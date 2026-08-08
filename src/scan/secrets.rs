//! The secret scanner.
//!
//! Deterministic, runs before any model call, and only ever looks at **added**
//! lines — a credential already present in the base revision is not this pull
//! request's problem, and re-reporting it every time someone touches the file
//! is exactly the noise we are trying not to make.
//!
//! Two layers:
//!
//! 1. A **rulepack** of known credential shapes. High confidence, so these are
//!    reported at `Critical` and fail the check on their own.
//! 2. An **entropy heuristic** for assignments to secret-looking names. Lower
//!    confidence, so these are `High` and phrased as a question — the model
//!    adjudicates them in the `commits` lane rather than the scanner asserting.
//!
//! Values never leave this module. Findings carry a redacted hint only.
//!
//! No regex crate: the rules are simple enough to express as prefix, length and
//! character-class checks, and hand-written matchers keep a dependency out of
//! the default build while staying easy to reason about.

use crate::config::types::Severity;
use crate::scan::types::{Finding, ScanKind, redact};

/// One known credential shape.
struct Rule {
    /// Stable id, used for suppression.
    id: &'static str,
    /// What to call it in the finding.
    label: &'static str,
    /// Literal prefix the token must start with.
    prefix: &'static str,
    /// Minimum total length, including the prefix.
    min_len: usize,
    /// Whether the remainder must be alphanumeric-ish (letters, digits, `-`,
    /// `_`). Rules for structured tokens like JWTs turn this off.
    alnum_body: bool,
}

/// Known credential shapes, most specific first.
const RULES: &[Rule] = &[
    Rule {
        id: "aws-access-key-id",
        label: "an AWS access key id",
        prefix: "AKIA",
        min_len: 20,
        alnum_body: true,
    },
    Rule {
        id: "aws-session-token-id",
        label: "an AWS temporary access key id",
        prefix: "ASIA",
        min_len: 20,
        alnum_body: true,
    },
    Rule {
        id: "github-personal-access-token",
        label: "a GitHub personal access token",
        prefix: "ghp_",
        min_len: 36,
        alnum_body: true,
    },
    Rule {
        id: "github-oauth-token",
        label: "a GitHub OAuth token",
        prefix: "gho_",
        min_len: 36,
        alnum_body: true,
    },
    Rule {
        id: "github-app-token",
        label: "a GitHub app token",
        prefix: "ghs_",
        min_len: 36,
        alnum_body: true,
    },
    Rule {
        id: "github-refresh-token",
        label: "a GitHub refresh token",
        prefix: "ghr_",
        min_len: 36,
        alnum_body: true,
    },
    Rule {
        id: "github-fine-grained-token",
        label: "a fine-grained GitHub token",
        prefix: "github_pat_",
        min_len: 40,
        alnum_body: true,
    },
    Rule {
        id: "openai-api-key",
        label: "an OpenAI API key",
        prefix: "sk-proj-",
        min_len: 40,
        alnum_body: true,
    },
    Rule {
        id: "openrouter-api-key",
        label: "an OpenRouter API key",
        prefix: "sk-or-v1-",
        min_len: 40,
        alnum_body: true,
    },
    Rule {
        id: "anthropic-api-key",
        label: "an Anthropic API key",
        prefix: "sk-ant-",
        min_len: 40,
        alnum_body: true,
    },
    Rule {
        id: "slack-token",
        label: "a Slack token",
        prefix: "xox",
        min_len: 24,
        alnum_body: true,
    },
    Rule {
        id: "stripe-secret-key",
        label: "a Stripe secret key",
        prefix: "sk_live_",
        min_len: 30,
        alnum_body: true,
    },
    Rule {
        id: "google-api-key",
        label: "a Google API key",
        prefix: "AIza",
        min_len: 39,
        alnum_body: true,
    },
    Rule {
        id: "sentry-user-token",
        label: "a Sentry auth token",
        prefix: "sntryu_",
        min_len: 40,
        alnum_body: true,
    },
    Rule {
        id: "npm-access-token",
        label: "an npm access token",
        prefix: "npm_",
        min_len: 36,
        alnum_body: true,
    },
];

/// Private-key armour, matched on the line rather than on a token.
const PEM_MARKERS: &[(&str, &str)] = &[
    ("-----BEGIN RSA PRIVATE KEY-----", "an RSA private key"),
    ("-----BEGIN DSA PRIVATE KEY-----", "a DSA private key"),
    ("-----BEGIN EC PRIVATE KEY-----", "an EC private key"),
    (
        "-----BEGIN OPENSSH PRIVATE KEY-----",
        "an OpenSSH private key",
    ),
    ("-----BEGIN PGP PRIVATE KEY BLOCK-----", "a PGP private key"),
    ("-----BEGIN PRIVATE KEY-----", "a private key"),
];

/// Variable names that make a high-entropy value on the right-hand side
/// suspicious.
/// Deliberately excludes a bare `auth`: it matches `authenticate`, `author` and
/// `authorization`, which turned every function signature containing a colon
/// into a candidate. Specificity here is worth more than reach, because the
/// rulepack already covers the shapes that matter most.
const SECRET_NAMES: &[&str] = &[
    "secret",
    "token",
    "passwd",
    "password",
    "apikey",
    "api_key",
    "access_key",
    "private_key",
    "client_secret",
    "credential",
];

/// Values that look like credentials but are conventional placeholders.
///
/// Missing one of these produces a false positive on almost every repository's
/// example config, which is the fastest way to teach a team to ignore the bot.
const PLACEHOLDERS: &[&str] = &[
    "xxx",
    "your",
    "example",
    "changeme",
    "placeholder",
    "redacted",
    "dummy",
    "sample",
    "test",
    "fake",
    "notreal",
    "insert",
    "todo",
    "abc123",
    "0000",
    "1234",
];

/// Above this length a line is treated as machine-generated, and only the
/// deterministic rulepack runs on it. The entropy heuristic on a minified
/// bundle reports nothing but noise.
const MAX_HEURISTIC_LINE: usize = 4096;

/// Paths whose contents are expected to contain credential-shaped strings.
const EXPECTED_PATHS: &[&str] = &[
    ".env.example",
    ".env.sample",
    ".env.template",
    "example.env",
];

/// Scan the added lines of one file.
///
/// `path` is the head-revision path; `added` yields `(line number, text)`.
pub fn scan_added_lines<'a>(
    path: &str,
    added: impl Iterator<Item = (u64, &'a str)>,
) -> Vec<Finding> {
    let expected = is_expected_path(path);
    let mut findings = Vec::new();

    for (line_no, text) in added {
        // A minified bundle or a base64 asset makes the *entropy heuristic*
        // useless — but a real credential pasted into a long config line is
        // still a real credential, so the rulepack keeps running. Skipping the
        // whole line was a hole: one `JSON.stringify` and a leaked key becomes
        // invisible.
        let too_long_for_heuristics = text.len() > MAX_HEURISTIC_LINE;

        for (marker, label) in PEM_MARKERS {
            if text.contains(marker) {
                findings.push(
                    Finding::new(
                        ScanKind::Secret,
                        Severity::Critical,
                        path,
                        "private-key",
                        format!("Remove the committed private key ({label})"),
                        "Rotate the key, remove it from the working tree, and purge it from history — \
                         a force-push alone does not remove it from forks or from anyone who already fetched.",
                    )
                    .at_line(line_no),
                );
            }
        }

        for token in tokenize(text) {
            if let Some(rule) = match_rule(token) {
                // A placeholder in an example file is the file doing its job.
                if expected && looks_like_placeholder(token) {
                    continue;
                }
                findings.push(
                    Finding::new(
                        ScanKind::Secret,
                        Severity::Critical,
                        path,
                        rule.id,
                        format!("Remove the committed credential ({})", rule.label),
                        "Rotate the credential first, then remove it from the working tree and purge it \
                         from history. Treat it as compromised: it is in the push, whatever happens next.",
                    )
                    .at_line(line_no)
                    .with_hint(redact(token)),
                );
            }
        }

        // Every condition here is a false-positive filter earned the hard way:
        // the value has to *look* like an opaque credential (one token, no
        // spaces, base64/hex alphabet), be long enough to be one, and carry
        // more entropy than an identifier or a sentence would.
        if !expected
            && !too_long_for_heuristics
            && let Some((name, value)) = secret_assignment(text)
            && !looks_like_placeholder(value)
            && is_opaque_token(value)
            && value.len() >= 20
            && shannon_entropy(value) >= ENTROPY_THRESHOLD
        {
            findings.push(
                Finding::new(
                    ScanKind::Secret,
                    Severity::High,
                    path,
                    "high-entropy-assignment",
                    format!("`{name}` is assigned a high-entropy literal"),
                    "This looks like a credential rather than a constant. If it is one, rotate it and read \
                     it from the environment instead. If it is not — a hash, a fixture, a test vector — say \
                     so and this will be suppressed.",
                )
                .at_line(line_no)
                .with_hint(redact(value)),
            );
        }
    }

    findings
}

/// Whether `path` is expected to contain credential-shaped placeholders.
fn is_expected_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    EXPECTED_PATHS.iter().any(|p| lower.ends_with(p))
}

/// Split a line into candidate tokens.
/// `=` and `:` are separators, not token characters: without them
/// `AWS_ACCESS_KEY_ID=AKIA…` is one token starting with `AWS`, and every
/// rulepack prefix check misses the credential sitting right there.
fn tokenize(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| {
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
            )
    })
    .filter(|t| !t.is_empty())
}

/// Whether a value looks like one opaque credential rather than a phrase.
///
/// Real credentials are a single run of base64/hex-ish characters. Requiring
/// that shape is what stops a function signature, a sentence, or a formatted
/// expression from ever reaching the entropy check — short strings with diverse
/// characters score deceptively high, so entropy alone is not a filter.
fn is_opaque_token(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(char::is_whitespace)
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-' | '.'))
}

fn match_rule(token: &str) -> Option<&'static Rule> {
    RULES.iter().find(|rule| {
        token.starts_with(rule.prefix)
            && token.len() >= rule.min_len
            && (!rule.alnum_body
                || token[rule.prefix.len()..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    })
}

/// Whether a value is a conventional placeholder rather than a real secret.
fn looks_like_placeholder(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if PLACEHOLDERS.iter().any(|p| lower.contains(p)) {
        return true;
    }
    // `<something>`, `${SOMETHING}`, `{{ something }}` are all templates.
    if value.starts_with('$') || value.starts_with('<') || value.starts_with("{{") {
        return true;
    }
    // A single repeated character carries no information.
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => chars.all(|c| c == first),
        None => true,
    }
}

/// Extract `name = value` where the name looks secret-ish.
///
/// Handles the shapes that actually occur: `NAME=value`, `name: "value"`,
/// `name = 'value'`, and `"name": "value"`.
fn secret_assignment(text: &str) -> Option<(&str, &str)> {
    let (name_part, value_part) = text.split_once('=').or_else(|| text.split_once(':'))?;

    let name = name_part
        .trim()
        .trim_end_matches(['"', '\''])
        .rsplit(|c: char| c.is_whitespace() || c == '.' || c == '"' || c == '\'')
        .next()?
        .trim();

    let lower = name.to_ascii_lowercase();
    if !SECRET_NAMES.iter().any(|n| lower.contains(n)) {
        return None;
    }

    let value = value_part
        .trim()
        .trim_end_matches(&[',', ';'][..])
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`')
        .trim();

    if value.is_empty() {
        return None;
    }

    Some((name, value))
}

/// Bits-per-character a value must carry before the heuristic will flag it.
///
/// Set from measurement, not intuition: a 32-character base64 credential lands
/// around 4.6–5.0, while `snake_case_identifiers_like_this` sits near 3.7.
/// Short strings with diverse characters score higher than their information
/// content deserves, which is why [`is_opaque_token`] and a length floor guard
/// this check rather than the other way round.
const ENTROPY_THRESHOLD: f64 = 4.2;

/// Shannon entropy in bits per character.
fn shannon_entropy(value: &str) -> f64 {
    if value.is_empty() {
        return 0.0;
    }

    let mut counts = [0usize; 256];
    let mut total = 0usize;
    for byte in value.bytes() {
        counts[byte as usize] += 1;
        total += 1;
    }

    let total = total as f64;
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let p = count as f64 / total;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(path: &str, line: &str) -> Vec<Finding> {
        scan_added_lines(path, std::iter::once((1u64, line)))
    }

    /// Assemble a credential-shaped fixture at run time.
    ///
    /// A secret scanner's test corpus is, by construction, full of strings that
    /// look exactly like credentials — and GitHub's push protection scans this
    /// file's text, so writing them as literals makes the repository
    /// unpushable. Splitting each one across two fragments that do not match on
    /// their own keeps the runtime value identical and the file clean.
    ///
    /// Do not "simplify" these back into literals.
    fn token(prefix: &str, body: &str) -> String {
        format!("{prefix}{body}")
    }

    #[test]
    fn a_real_aws_key_is_critical() {
        let key = token("AKIA", "IOSFODNN7EXAMPLE");
        let findings = scan("src/config.rs", &format!("const KEY: &str = \"{key}\";"));

        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].severity, Severity::Critical);
        assert_eq!(findings[0].rule, "aws-access-key-id");
    }

    #[test]
    fn the_matched_value_never_appears_in_the_finding() {
        // Synthetic, and it must stay synthetic: a real credential in a fixture is
        // still a committed credential.
        let secret = token(
            "sk-or-",
            "v1-0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0",
        );
        let findings = scan("src/main.rs", &format!("let key = \"{secret}\";"));

        assert_eq!(findings.len(), 1, "{findings:#?}");
        let rendered = serde_json::to_string(&findings[0]).expect("serialises");
        assert!(
            !rendered.contains("0f1e2d3c"),
            "the secret body leaked into the finding: {rendered}"
        );
        assert!(
            rendered.contains("sk-o"),
            "the prefix is kept for recognition"
        );
    }

    #[test]
    fn github_and_vendor_tokens_are_recognised() {
        for (line, rule) in [
            (
                format!(
                    "token = \"{}\"",
                    token("ghp_", "9f8e7d6c5b4a39281706fedcba9876543210")
                ),
                "github-personal-access-token",
            ),
            (
                format!(
                    "GOOGLE={}",
                    token("AIza", "SyA9f8e7d6c5b4a39281706fedcba98765432")
                ),
                "google-api-key",
            ),
            (
                format!(
                    "stripe = \"{}\"",
                    token("sk_", "live_9f8e7d6c5b4a39281706fedcba")
                ),
                "stripe-secret-key",
            ),
        ] {
            let findings = scan("src/main.rs", &line);
            assert!(
                findings.iter().any(|f| f.rule == rule),
                "expected {rule} from `{line}`, got {findings:#?}"
            );
        }
    }

    #[test]
    fn private_key_armour_is_caught() {
        let findings = scan("deploy/key.pem", "-----BEGIN OPENSSH PRIVATE KEY-----");

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, "private-key");
        assert_eq!(findings[0].severity, Severity::Critical);
        assert!(findings[0].detail.contains("force-push alone does not"));
    }

    #[test]
    fn a_high_entropy_assignment_to_a_secret_name_is_flagged_at_high_not_critical() {
        let value = token("f3Kq9zR2", "mW7pL4xN8vB1cY6tH0jD5sG");
        let findings = scan("src/main.rs", &format!("let api_key = \"{value}\";"));

        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].rule, "high-entropy-assignment");
        assert_eq!(
            findings[0].severity,
            Severity::High,
            "the heuristic is not certain, so it must not claim to be"
        );
    }

    #[test]
    fn ordinary_code_produces_nothing() {
        for line in [
            "let user_count = 42;",
            "fn authenticate(user: &User) -> Result<Session> {",
            "// TODO: rotate the token before release",
            "pub const MAX_RETRIES: usize = 5;",
            "use crate::config::types::Severity;",
        ] {
            let findings = scan("src/main.rs", line);
            assert!(findings.is_empty(), "`{line}` produced {findings:#?}");
        }
    }

    #[test]
    fn placeholders_are_not_secrets() {
        for line in [
            "API_KEY=your-api-key-here",
            "password = \"changeme\"",
            "secret: \"xxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"",
            "token = \"${GITHUB_TOKEN}\"",
            "client_secret: <your-client-secret>",
            "api_key = \"EXAMPLE_KEY_DO_NOT_USE_1234567890\"",
        ] {
            let findings = scan("src/config.rs", line);
            assert!(findings.is_empty(), "`{line}` produced {findings:#?}");
        }
    }

    #[test]
    fn an_env_example_file_is_allowed_its_placeholders() {
        let findings = scan(".env.example", "OPENROUTER_API_KEY=your-key-here");
        assert!(findings.is_empty(), "{findings:#?}");
    }

    #[test]
    fn a_real_key_in_an_env_example_is_still_reported() {
        // The exemption covers placeholders, not a genuine leak into the file
        // people are most likely to commit carelessly.
        let key = token("AKIA", "ZZ7QWERTYUIOPASD");
        let findings = scan(".env.example", &format!("AWS_ACCESS_KEY_ID={key}"));
        assert_eq!(findings.len(), 1, "{findings:#?}");
    }

    #[test]
    fn lockfile_hashes_do_not_trip_the_entropy_heuristic() {
        // The name is what gates the heuristic, and `checksum` is not a secret
        // name — this is the single most common false positive class.
        let findings = scan(
            "Cargo.lock",
            "checksum = \"d5f1c3e8a9b04c7e2f6a8d3b1e9c5a7f2d4b6e8c0a2f4d6b8e0c2a4f6d8b0e2c\"",
        );
        assert!(findings.is_empty(), "{findings:#?}");
    }

    #[test]
    fn base64_asset_lines_do_not_trip_the_entropy_heuristic() {
        let line = format!("const LOGO: &str = \"{}\";", "QUJDRA".repeat(1000));
        let findings = scan("src/assets.rs", &line);
        assert!(findings.is_empty(), "{findings:#?}");
    }

    #[test]
    fn a_real_credential_on_a_minified_line_is_still_caught() {
        // Skipping long lines entirely was a hole: one `JSON.stringify` and a
        // leaked key becomes invisible to the scanner.
        let key = token("AKIA", "IOSFODNN7EXAMPLE");
        let line = format!("{{\"padding\":\"{}\",\"aws\":\"{key}\"}}", "x".repeat(5000));
        let findings = scan("config/bundle.json", &line);

        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].rule, "aws-access-key-id");
    }

    #[test]
    fn entropy_separates_credentials_from_identifiers() {
        assert!(shannon_entropy(&token("f3Kq9zR2", "mW7pL4xN8vB1cY6tH0jD5sG")) > ENTROPY_THRESHOLD);
        assert!(shannon_entropy("snake_case_identifier_like_this") < ENTROPY_THRESHOLD);
        assert!(shannon_entropy("aaaaaaaaaaaaaaaaaaaaaaaa") < 1.0);
        assert_eq!(shannon_entropy(""), 0.0);
    }

    #[test]
    fn only_opaque_single_token_values_reach_the_entropy_check() {
        // Entropy alone is not a filter: a short string with diverse characters
        // scores high, so the shape check has to come first.
        assert!(is_opaque_token(&token(
            "f3Kq9zR2",
            "mW7pL4xN8vB1cY6tH0jD5sG"
        )));
        assert!(is_opaque_token("YWJjZGVmZ2hpamtsbW5vcA=="));
        assert!(!is_opaque_token("&User) -> Result<Session> {"));
        assert!(!is_opaque_token("the quick brown fox"));
        assert!(!is_opaque_token(""));
    }

    #[test]
    fn a_function_signature_containing_a_colon_is_not_a_secret() {
        // This shape reached the entropy heuristic through a bare `auth` name
        // match and a value that was never a single token.
        for line in [
            "fn authenticate(user: &User) -> Result<Session> {",
            "pub fn author_of(commit: &Commit) -> String {",
            "  authorization: Authorization::Bearer(token),",
        ] {
            assert!(scan("src/main.rs", line).is_empty(), "`{line}` was flagged");
        }
    }

    #[test]
    fn assignments_are_extracted_from_the_shapes_that_actually_occur() {
        for line in [
            "API_TOKEN=abc",
            "api_token: \"abc\"",
            "let api_token = 'abc';",
            "\"api_token\": \"abc\",",
        ] {
            assert!(
                secret_assignment(line).is_some(),
                "failed to extract from `{line}`"
            );
        }
        assert!(secret_assignment("let count = 5;").is_none());
    }

    #[test]
    fn only_added_lines_are_ever_offered_to_the_scanner() {
        // Guarded by the caller, but asserted here so the contract is written
        // down somewhere a reader of this module will see it.
        let findings = scan_added_lines("src/main.rs", std::iter::empty());
        assert!(findings.is_empty());
    }
}
