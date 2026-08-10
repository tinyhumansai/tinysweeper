//! Personal-data shapes, redacted from text that has already been through the
//! credential scrubber.
//!
//! ## Why this exists at all
//!
//! [`crate::scan::secrets::scrub`] is a **credential** rulepack: it matches
//! known prefixes — `AKIA`, `ghp_`, `sk-proj-`, `sk-or-v1-` and friends — and
//! nothing else. That is the right shape for its job, which is scanning diffs
//! for keys somebody committed. It does not know what an email address, a
//! bearer token, a session identifier or a card number looks like, and a quick
//! check confirms all four pass through it unchanged.
//!
//! For every other input tinysweeper reads that is fine, because everything
//! else it reads is source code and diffs, written to be published. A Sentry
//! event is not: it is captured from a running production process, and the one
//! allow-listed field that carries free text — the exception message — is
//! exactly where an application puts `"invalid card 4111111111111111 for
//! alice@example.com"`.
//!
//! So the allow-list in [`crate::sentry::types`] keeps personal data out of the
//! *structured* sections (`user`, `request`, `contexts`, `extra`, breadcrumbs,
//! frame `vars`), and this module keeps it out of the *free text* that survives.
//! Both are needed; neither is sufficient.
//!
//! ## Why it is local to `sentry` and not added to the shared scrubber
//!
//! Extending [`crate::scan::secrets`] would change what every review comment,
//! check-run summary and finding body renders across the whole product — an
//! email in a diff would start being redacted from ordinary review output.
//! That is a broad behaviour change with its own argument to make, and it is
//! not this one. `sentry.scrub_patterns` is documented as running "on top of
//! the always-on secret scrubbing, never instead of it"; this pass sits in the
//! same place, for the same reason.
//!
//! ## Every rule fails safe
//!
//! Each rule below redacts more than it strictly must, because the costs are
//! not symmetric. An over-redacted exception message is mildly less useful; an
//! under-redacted one is permanent, because GitHub keeps the edit history of
//! an issue body. There is deliberately no Luhn check on the card rule and no
//! entropy threshold on the opaque-token rule: both would be more precise, and
//! both fail *open* on the input they get wrong.

/// What replaces a redacted span. Names the shape so a reader can tell why the
/// text is gone — the same "type and location, never the value" rule the
/// scanners already follow.
const EMAIL: &str = "[redacted-email]";
const BEARER: &str = "[redacted-token]";
const DIGITS: &str = "[redacted-number]";
const OPAQUE: &str = "[redacted-opaque]";

/// Shortest digit run treated as card-shaped, ignoring separators.
const CARD_MIN_DIGITS: usize = 13;
/// Longest digit run treated as card-shaped.
const CARD_MAX_DIGITS: usize = 19;
/// Shortest run of token characters treated as an opaque secret.
const OPAQUE_MIN_LEN: usize = 32;
/// Shortest value after `Bearer ` treated as a credential rather than prose.
const BEARER_MIN_TOKEN_LEN: usize = 12;

/// Whether `token` looks like a credential rather than an ordinary word.
///
/// Used only by the `Bearer` rule, where the keyword has already narrowed the
/// context: the question is "is the next word a token or is this a sentence".
fn is_credential_shaped(token: &str) -> bool {
    token.len() >= BEARER_MIN_TOKEN_LEN
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '/' | '='))
}

/// Redact personal-data shapes from `text`.
///
/// Applied after the credential scrubber and before any size cap, so the
/// replacement markers are inside the budget rather than pushing text out of
/// it.
pub fn redact(text: &str) -> String {
    let stage = redact_bearer(text);
    let stage = redact_emails(&stage);
    let stage = redact_digit_runs(&stage);
    redact_opaque_tokens(&stage)
}

/// Replace `Bearer <token>` — the token, not the word.
///
/// Case-insensitive on the keyword, because `bearer`, `Bearer` and `BEARER`
/// all occur in real `Authorization` echoes.
fn redact_bearer(text: &str) -> String {
    let lower = text.to_lowercase();
    if lower.len() != text.len() {
        // Lowercasing changed the byte length, so offsets from `lower` do not
        // address `text`. Nothing here is worth an incorrect slice.
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;

    while let Some(found) = lower[cursor..].find("bearer ") {
        let keyword_start = cursor + found;
        let token_start = keyword_start + "bearer ".len();
        let token_end = text[token_start..]
            .find(|c: char| c.is_whitespace())
            .map(|offset| token_start + offset)
            .unwrap_or(text.len());

        // "the bearer of this token" is prose, not an `Authorization` header.
        // A credential is long and opaque; a word is neither. Anything this
        // rule declines that really was a secret is still caught by
        // `redact_opaque_tokens` at 32 characters, so the conservative choice
        // here narrows a gap rather than opening one.
        if !is_credential_shaped(&text[token_start..token_end]) {
            out.push_str(&text[cursor..token_end]);
            cursor = token_end;
            continue;
        }

        out.push_str(&text[cursor..token_start]);
        out.push_str(BEARER);
        cursor = token_end;
    }

    out.push_str(&text[cursor..]);
    out
}

/// Replace anything shaped like an email address.
///
/// Deliberately loose: a token containing `@` with a dot somewhere after it
/// and no whitespace. Tightening this towards RFC 5322 would only ever cause
/// it to miss addresses.
fn redact_emails(text: &str) -> String {
    rewrite_tokens(text, |token| {
        let (local, domain) = token.split_once('@')?;
        let local_ok = !local.is_empty() && local.chars().all(is_email_char);
        let domain_ok = domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && domain.chars().all(is_email_char);
        (local_ok && domain_ok).then(|| EMAIL.to_string())
    })
}

fn is_email_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '%')
}

/// Replace runs of 13–19 digits, ignoring `-` and space separators.
///
/// Scans the raw text rather than going through [`rewrite_tokens`], because a
/// space is a token separator and `4111 1111 1111 1111` therefore never
/// reaches a token rule as one value — the shape most likely to be typed into
/// a form and echoed into an error message.
///
/// No Luhn check: a card number that fails Luhn is still a card number
/// somebody typed, and a rule that lets it through is worse than one that
/// occasionally redacts a long identifier.
fn redact_digit_runs(text: &str) -> String {
    // Indexed by character rather than by byte: an exception message is
    // arbitrary UTF-8, and slicing it at a byte offset panics the moment it
    // carries an accent or an emoji.
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;

    while index < chars.len() {
        let (start_byte, ch) = chars[index];
        if !ch.is_ascii_digit() {
            out.push(ch);
            index += 1;
            continue;
        }

        // Extend over digits and single internal `-` / ` ` separators.
        let mut end = index;
        let mut digits = 0usize;
        while end < chars.len() {
            let current = chars[end].1;
            if current.is_ascii_digit() {
                digits += 1;
                end += 1;
            } else if matches!(current, '-' | ' ')
                && end + 1 < chars.len()
                && chars[end + 1].1.is_ascii_digit()
            {
                end += 1;
            } else {
                break;
            }
        }

        // Refuse to cut inside a longer alphanumeric token: `build1234567890123`
        // is an identifier, not a card.
        let bounded_left = index == 0 || !is_token_char(chars[index - 1].1);
        let bounded_right = end >= chars.len() || !is_token_char(chars[end].1);

        if bounded_left && bounded_right && (CARD_MIN_DIGITS..=CARD_MAX_DIGITS).contains(&digits) {
            out.push_str(DIGITS);
        } else {
            let end_byte = chars.get(end).map_or(text.len(), |(byte, _)| *byte);
            out.push_str(&text[start_byte..end_byte]);
        }
        index = end;
    }

    out
}

/// Whether `c` continues an identifier, for the digit rule's boundary test.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.')
}

/// Replace long opaque tokens: session ids, raw JWT segments, API keys with no
/// recognised prefix.
///
/// Requires both a letter and a digit so ordinary long identifiers —
/// `NullPointerExceptionHandlerFactory`, a deep module path — survive. A
/// hex-only string counts, because that is what session identifiers look like.
fn redact_opaque_tokens(text: &str) -> String {
    rewrite_tokens(text, |token| {
        if token.len() < OPAQUE_MIN_LEN {
            return None;
        }
        if !token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return None;
        }

        let has_digit = token.chars().any(|c| c.is_ascii_digit());
        let has_alpha = token.chars().any(|c| c.is_ascii_alphabetic());
        let all_hex = token.chars().all(|c| c.is_ascii_hexdigit());

        (all_hex || (has_digit && has_alpha)).then(|| OPAQUE.to_string())
    })
}

/// Split `text` on whitespace-and-punctuation boundaries, letting `rule`
/// replace whole tokens, and reassemble it with the original separators
/// intact.
///
/// Token-wise rather than character-wise so a rule cannot cut a token in half
/// and leave a readable remainder — which is how partial redactions leak.
fn rewrite_tokens(text: &str, rule: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut token_start: Option<usize> = None;

    for (index, ch) in text.char_indices() {
        if is_separator(ch) {
            if let Some(start) = token_start.take() {
                push_token(&mut out, &text[start..index], &rule);
            }
            out.push(ch);
        } else if token_start.is_none() {
            token_start = Some(index);
        }
    }

    if let Some(start) = token_start {
        push_token(&mut out, &text[start..], &rule);
    }

    out
}

fn push_token(out: &mut String, token: &str, rule: &impl Fn(&str) -> Option<String>) {
    match rule(token) {
        Some(replacement) => out.push_str(&replacement),
        None => out.push_str(token),
    }
}

/// Characters that end a token.
///
/// Whitespace included, which is why [`redact_digit_runs`] does not use
/// [`rewrite_tokens`]: a space-separated card number would arrive as four
/// four-digit tokens, none of which is card-shaped.
fn is_separator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '<' | '>' | '='
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::secrets;

    /// The premise this module rests on. If the shared scrubber ever grows
    /// these rules, this test fails and the duplication here can be removed.
    #[test]
    fn the_credential_scrubber_does_not_catch_personal_data() {
        for input in [
            "contact alice@example.com",
            "card 4111111111111111 declined",
            "session 9f2b7c1e4a8d3f6b0c5e2a9d7f4b1e8c",
        ] {
            assert_eq!(
                secrets::scrub(input),
                input,
                "the shared scrubber now handles `{input}` — reconsider this module"
            );
        }
    }

    #[test]
    fn an_email_is_replaced_and_its_sentence_survives() {
        let out = redact("failed to charge alice.smith+billing@example.co.uk on retry");
        assert!(!out.contains("alice.smith"), "{out}");
        assert!(!out.contains("example.co.uk"), "{out}");
        assert!(out.contains(EMAIL), "{out}");
        assert!(out.contains("failed to charge"), "{out}");
        assert!(out.contains("on retry"), "{out}");
    }

    #[test]
    fn a_bearer_token_is_replaced_but_the_keyword_stays() {
        let out = redact("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.QUJDREVG.c2lnbmF0dXJl failed");
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"), "{out}");
        assert!(out.contains(BEARER), "{out}");
        assert!(out.contains("failed"), "{out}");
    }

    #[test]
    fn bearer_as_prose_is_left_alone() {
        let out = redact("the bearer of this token");
        assert!(out.contains("the bearer of this token"), "{out}");
    }

    #[test]
    fn card_shaped_numbers_go_with_or_without_separators() {
        for card in [
            "4111111111111111",
            "4111-1111-1111-1111",
            "4111 1111 1111 1111",
            "378282246310005",
        ] {
            let out = redact(&format!("declined {card} for order 42"));
            assert!(!out.contains(card), "`{card}` survived: {out}");
            assert!(out.contains("order 42"), "{out}");
        }
    }

    #[test]
    fn ordinary_numbers_survive() {
        let out = redact("retried 12 times over 3600 seconds, request 8675309");
        assert!(out.contains("12"), "{out}");
        assert!(out.contains("3600"), "{out}");
        assert!(out.contains("8675309"), "{out}");
    }

    #[test]
    fn an_opaque_session_blob_goes() {
        let out = redact("session=9f2b7c1e4a8d3f6b0c5e2a9d7f4b1e8c expired");
        assert!(!out.contains("9f2b7c1e4a8d3f6b0c5e2a9d7f4b1e8c"), "{out}");
        assert!(out.contains(OPAQUE), "{out}");
        assert!(out.contains("expired"), "{out}");
    }

    /// The rule that would make this module unusable if it were wrong: long
    /// symbol names are not secrets, and a stack trace is full of them.
    #[test]
    fn long_identifiers_and_paths_survive() {
        for identifier in [
            "NullPointerExceptionHandlerFactoryBuilder",
            "src/very/deeply/nested/module/path/handler.rs",
            "com.example.service.internal.OrderProcessorImpl",
        ] {
            let out = redact(identifier);
            assert_eq!(out, identifier, "`{identifier}` was redacted");
        }
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = redact("alice@example.com paid with 4111111111111111");
        assert_eq!(redact(&once), once);
    }

    #[test]
    fn ordinary_prose_is_untouched() {
        let prose = "TypeError: cannot read property 'id' of undefined";
        assert_eq!(redact(prose), prose);
    }
}
