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
/// Case-insensitive substring search that returns a byte range in `haystack`.
///
/// Exists because `to_lowercase()` is the wrong tool for *locating* a literal.
/// It is Unicode-aware, so a character's lowercase form can occupy a different
/// number of bytes, and an offset found in the lowered copy then addresses a
/// different position in the original. Comparing `lower.len() != text.len()`
/// does **not** rule that out: `İİK` is 7 bytes before and after lowercasing
/// (`İ` grows by one byte twice, the Kelvin sign `K` shrinks by two), and every
/// internal offset still moves. Attacker-influenced text — which a Sentry
/// exception message is — could therefore steer a slice.
///
/// So the walk happens over the *original*: candidate starts are real character
/// boundaries of `haystack`, and the returned range is measured in its bytes.
/// Folding is per character via [`char::to_lowercase`], which keeps this
/// Unicode-correct — `RÉSUMÉ` matches `résumé` — rather than ASCII-only, since
/// `sentry.scrub_patterns` is documented as case-insensitive and an operator
/// may well configure a non-ASCII pattern.
///
/// The end offset is returned rather than derived from `needle.len()`, because
/// a case-insensitive match can span a different number of bytes than the
/// needle does.
pub(super) fn find_ci(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    for (start, _) in haystack.char_indices() {
        if let Some(end) = match_at(haystack, start, needle) {
            return Some((start, end));
        }
    }
    None
}

/// Whether `needle` matches `haystack` at `start`, case-insensitively.
///
/// Returns the byte offset in `haystack` just past the match. Compares the
/// lowercase expansion of each side character by character, so a mapping that
/// yields more than one character (`İ` → `i` + a combining dot) is handled
/// without either side being materialised as a string.
fn match_at(haystack: &str, start: usize, needle: &str) -> Option<usize> {
    let mut hay = haystack[start..].chars();
    let mut pat = needle.chars().flat_map(char::to_lowercase);
    let mut consumed = 0usize;
    let mut pending = None;

    loop {
        let Some(want) = pending.take().or_else(|| pat.next()) else {
            return Some(start + consumed);
        };
        let got = hay.next()?;
        consumed += got.len_utf8();

        let mut folded = got.to_lowercase();
        let first = folded.next()?;
        if first != want {
            return None;
        }
        // A character whose lowercase expands to several must match that many
        // of the pattern's characters before the next haystack character.
        for extra in folded {
            let next_want = pat.next()?;
            if extra != next_want {
                return None;
            }
        }
        pending = None;
    }
}

fn redact_bearer(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;

    // Searches `text` itself rather than a lowered copy, so every offset below
    // addresses the string being sliced. The previous version bailed out
    // whenever lowercasing changed the byte length, which meant one `İ`
    // anywhere in a message silently disabled bearer redaction for all of it.
    while let Some((_, found_end)) = find_ci(&text[cursor..], "bearer ") {
        let token_start = cursor + found_end;
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
///
/// `:` and `/` are separators because the rules downstream
/// ([`redact_emails`], [`redact_opaque_tokens`]) require a token to be made
/// only of their allowed charset, and reject anything else outright. Without
/// these two, `mailto:alice@example.com`, `to:alice@example.com` and
/// `session:9f2b7c1e4a8d3f6b0c5e2a9d7f4b1e8c` each arrive as a *single* token
/// containing a rejected character — so the address and the token pass through
/// untouched. Splitting on them is what makes the prefix irrelevant instead of
/// protective.
fn is_separator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\''
                | '`'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | ','
                | ';'
                | '<'
                | '>'
                | '='
                | ':'
                | '/'
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

    /// A message is attacker-influenced text, so a character that merely
    /// *lowercases oddly* must not switch redaction off.
    ///
    /// The previous implementation searched a `to_lowercase()` copy and gave up
    /// whenever that changed the byte length — so a single `İ` anywhere in the
    /// message disabled bearer redaction for the whole of it.
    #[test]
    fn a_bearer_token_still_goes_when_the_message_lowercases_to_a_different_length() {
        let out = redact("İ Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.QUJDREVG.c2lnbmF0dXJl");
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"), "{out}");
        assert!(out.contains(BEARER), "{out}");
    }

    /// The same hazard with the byte length preserved, which the old length
    /// guard could not catch: `İİK` is 7 bytes before and after lowercasing
    /// (two `İ` grow by one byte each, the Kelvin sign shrinks by two), while
    /// every internal offset moves.
    #[test]
    fn offsets_are_not_taken_from_a_lowercased_copy() {
        let out = redact("İİK Bearer eyJhbGciOiJIUzI1NiJ9.QUJDREVG.c2lnbmF0dXJl");
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"), "{out}");
        assert!(out.contains(BEARER), "{out}");
        assert!(
            out.contains("İİK"),
            "the surrounding text must survive: {out}"
        );
    }

    /// `scrub_patterns` is documented as case-insensitive, and an operator may
    /// reasonably configure a non-ASCII pattern. ASCII-only folding would
    /// silently stop matching it — a scrubber that fails open.
    #[test]
    fn case_insensitive_search_folds_beyond_ascii() {
        assert!(find_ci("a RÉSUMÉ-SECRET here", "résumé-secret").is_some());
        assert!(find_ci("a résumé-secret here", "RÉSUMÉ-SECRET").is_some());
        // And the returned range addresses the ORIGINAL string.
        let (start, end) = find_ci("xx RÉSUMÉ yy", "résumé").expect("matches");
        assert_eq!(&"xx RÉSUMÉ yy"[start..end], "RÉSUMÉ");
    }

    /// A prefix must not protect an address. Without `:` as a separator the
    /// whole `mailto:alice@example.com` is one token, and the email rule
    /// rejects it for containing a character outside its charset — so it
    /// passed through untouched.
    #[test]
    fn an_email_behind_a_prefix_still_goes() {
        for text in [
            "to:alice@example.com",
            "mailto:alice@example.com",
            "contact=alice@example.com/profile",
        ] {
            let out = redact(text);
            assert!(!out.contains("alice@example.com"), "{text} -> {out}");
            assert!(out.contains(EMAIL), "{text} -> {out}");
        }
    }

    /// The same for an opaque token behind a prefix or in a URL path.
    #[test]
    fn an_opaque_token_behind_a_prefix_still_goes() {
        let token = "9f2b7c1e4a8d3f6b0c5e2a9d7f4b1e8c";
        for text in [
            format!("session:{token}"),
            format!("https://example.com/reset/{token}"),
        ] {
            let out = redact(&text);
            assert!(!out.contains(token), "{text} -> {out}");
            assert!(out.contains(OPAQUE), "{text} -> {out}");
        }
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
