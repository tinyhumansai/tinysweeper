//! Spotting self-promotion, in a diff and in an issue body.
//!
//! The pattern is familiar on any repository with a public issue tracker. A
//! pull request adds "support" for a service nobody asked for, and the support
//! is a base URL, an API key and a link; or an issue is a paragraph of product
//! copy with a signup link at the bottom. Both are cheap to write and expensive
//! to triage, because they *look* like contributions right up until somebody
//! reads them.
//!
//! ## What this is, and firmly is not
//!
//! It is a set of textual signals, and it is **advisory**. Nothing here can
//! close anything: [`crate::pr_triage::gate::decide`] refuses a close on this
//! verdict explicitly, and that refusal has a test. The reason is that the
//! honest version of this judgement is a judgement — "add Tavily as a BYOK
//! search provider" is a real contribution to one repository and an
//! advertisement on another, and no regular expression knows which. So the
//! signals produce a label and a comment naming exactly what was matched, and a
//! human decides.
//!
//! The signals are deliberately about *shape*, not about vendors. There is no
//! list of disallowed companies here and there must never be one: a denylist of
//! competitors is a different feature with a different name, and it would go
//! stale the week after it was written.
//!
//! ## Why it is safe to read prose here
//!
//! The rest of `pr_triage` never reads a title or a body, because those are
//! untrusted input and reading them into a prompt is how injection works. This
//! module reads them and is still safe, for a specific reason: there is no
//! prompt. It matches patterns and counts them. Text saying "ignore previous
//! instructions" matches nothing here and is simply text.
//!
//! Per the security boundary in `AGENTS.md`, a matched credential is reported
//! by **name and location only**. A value never reaches a label, a comment, or
//! a log line — a promotional pull request that happens to include a live key
//! must not have it echoed onto a public comment by the thing that noticed it.

use std::collections::BTreeSet;

use crate::forge::types::ChangedFile;
use crate::pr_triage::landed::hunks;

/// One reason something looks like an advertisement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Signal {
    /// A link carrying a referral, affiliate or campaign parameter.
    ///
    /// The strongest signal in the set, and the only one that is close to
    /// self-proving: a `?ref=` on a documentation link is not a technical
    /// requirement of anything.
    ReferralLink,
    /// A link whose host is the author's own — their login, or the site their
    /// GitHub profile advertises.
    AuthorsOwnLink,
    /// A new credential-shaped environment variable.
    ///
    /// Reported by *name*, never by value.
    NewCredential,
    /// A new outbound base URL: a vendor endpoint the change dials out to.
    NewEndpoint,
    /// Marketing register — superlatives and calls to action, in a diff that
    /// changes no code.
    MarketingCopy,
}

impl Signal {
    /// One phrase, for the comment and the log.
    pub fn reason(self) -> &'static str {
        match self {
            Signal::ReferralLink => "a link carrying a referral or campaign parameter",
            Signal::AuthorsOwnLink => "a link to a site the author is associated with",
            Signal::NewCredential => "a new API credential setting",
            Signal::NewEndpoint => "a new outbound service endpoint",
            Signal::MarketingCopy => "marketing language in a change that adds no code",
        }
    }
}

/// What the detector found, with enough detail to argue with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Finding {
    /// The signals matched, deduplicated and ordered.
    pub signals: BTreeSet<Signal>,
    /// The paths they were found in, deduplicated and ordered, capped.
    pub paths: BTreeSet<String>,
    /// The credential *names* matched. Never their values.
    pub credentials: BTreeSet<String>,
}

impl Finding {
    /// Whether this is worth flagging.
    ///
    /// Two independent signals, not one. Any single one of these fires on
    /// perfectly ordinary work — adding an integration legitimately does
    /// introduce an endpoint and a key — and a flag that cries wolf on every
    /// third pull request is a flag people learn to ignore. A referral link is
    /// the exception: nothing technical requires one.
    pub fn is_promotional(&self) -> bool {
        self.signals.contains(&Signal::ReferralLink) || self.signals.len() >= 2
    }

    /// The signals as one line, for a report.
    pub fn summary(&self) -> String {
        self.signals
            .iter()
            .map(|signal| signal.reason())
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// How many paths are named in a finding before it stops listing them.
const MAX_PATHS: usize = 5;

/// Query parameter *names* that exist to attribute a referral and nothing else.
///
/// Matched only where a query string can actually put them — after a `?` or an
/// `&` — and never as a bare substring. `ref=` as a substring matches `href=`,
/// which is in every HTML link ever written; that false positive was found by
/// running this over a real repository and it is exactly the kind a flag cannot
/// afford, because the flag reads as an accusation.
const REFERRAL_PARAMS: [&str; 5] = ["ref", "via", "utm_source", "utm_campaign", "aff"];

/// Referral markers that are not query parameters.
///
/// Matched inside a URL only, never in prose. "We do not allow affiliate
/// links" is a sentence about policy, and flagging the documentation that says
/// so as an advertisement is the most embarrassing false positive available.
const REFERRAL_WORDS: [&str; 3] = ["/affiliate", "affiliate=", "/r/?"];

/// Words that carry no technical content and a lot of register.
///
/// Only counted in a diff that adds no code, because "the fastest path" is a
/// normal thing to write in a comment about a fast path.
const MARKETING_WORDS: [&str; 12] = [
    "sign up",
    "signup",
    "free trial",
    "get started for free",
    "pricing",
    "our platform",
    "our product",
    "industry-leading",
    "best-in-class",
    "world's first",
    "revolutionary",
    "powered by our",
];

/// File extensions that are code rather than prose.
const CODE_EXTENSIONS: [&str; 16] = [
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "rb", "c", "h", "cpp", "swift", "kt", "sh",
    "toml",
];

/// Examine a pull request's diff.
///
/// `author_hosts` are hosts associated with whoever opened it — see
/// [`author_hosts`]. Empty is fine and simply retires one signal.
pub fn inspect_diff(files: &[ChangedFile], author_hosts: &[String]) -> Finding {
    let mut finding = Finding::default();
    let touches_code = files.iter().any(|file| is_code(&file.path));

    for file in files {
        let Some(patch) = file.patch.as_deref() else {
            continue;
        };
        // Only what the change *adds*. Deleting an advertisement is the
        // opposite of posting one, and a context line is somebody else's text.
        let added: Vec<String> = hunks(patch)
            .into_iter()
            .flat_map(|hunk| {
                hunk.after
                    .iter()
                    .filter(|line| !hunk.before.contains(line))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect();

        // Matched into a *per-file* finding and merged afterwards. Comparing
        // the size of the shared signal set before and after would miss the
        // second file to match a signal the first already raised — and then
        // name only the first in the comment, which is where a human goes to
        // look.
        let mut mine = Finding::default();
        inspect_lines(&added, author_hosts, touches_code, &mut mine);

        if !mine.signals.is_empty() && finding.paths.len() < MAX_PATHS {
            finding.paths.insert(file.path.clone());
        }
        finding.signals.extend(mine.signals);
        finding.credentials.extend(mine.credentials);
    }

    finding
}

/// Examine an issue's own text.
///
/// The same signals, over prose rather than over a diff. An issue has no code
/// to weigh the marketing signal against, so it is always eligible here — which
/// is the right reading: a bug report has no reason to contain a pricing page
/// link.
pub fn inspect_text(title: &str, body: &str, author_hosts: &[String]) -> Finding {
    let mut finding = Finding::default();
    let lines: Vec<String> = [title, body]
        .iter()
        .flat_map(|text| text.lines())
        .map(str::to_string)
        .collect();
    inspect_lines(&lines, author_hosts, false, &mut finding);
    finding
}

/// The shared matcher, over already-extracted lines.
fn inspect_lines(
    lines: &[String],
    author_hosts: &[String],
    touches_code: bool,
    finding: &mut Finding,
) {
    for line in lines {
        let lower = line.to_ascii_lowercase();

        // Inside the URLs on the line, never across the line. "See
        // https://example.test/policy for the /affiliate rule" is a sentence,
        // and `ReferralLink` alone is enough to flag — so a whole-line match
        // here puts an accusatory label on ordinary prose.
        for url in urls_in(&lower) {
            let (host, rest) = split_host(&url);

            if REFERRAL_PARAMS.iter().any(|name| {
                rest.contains(&format!("?{name}=")) || rest.contains(&format!("&{name}="))
            }) || REFERRAL_WORDS.iter().any(|word| rest.contains(word))
            {
                finding.signals.insert(Signal::ReferralLink);
            }
            // Against the URL's **host**, not the whole line. A substring match
            // calls an author named `docs` the owner of
            // `https://api.example.com/docs`, and two signals is all it takes
            // to put an accusatory label on an ordinary integration.
            if author_hosts.iter().any(|mine| host_matches(&host, mine)) {
                finding.signals.insert(Signal::AuthorsOwnLink);
            }
            if host.starts_with("api.") || rest.starts_with("/v1") {
                finding.signals.insert(Signal::NewEndpoint);
            }
        }

        if let Some(name) = credential_name(line) {
            finding.signals.insert(Signal::NewCredential);
            finding.credentials.insert(name);
        }

        // Weighed against the rest of the change: a superlative in a comment
        // beside real code is somebody being enthusiastic, where the same
        // sentence in a docs-only diff is a paragraph of copy.
        if !touches_code && MARKETING_WORDS.iter().any(|phrase| lower.contains(phrase)) {
            finding.signals.insert(Signal::MarketingCopy);
        }
    }
}

/// Split a URL into its host and everything after it.
fn split_host(url: &str) -> (String, String) {
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let authority_len = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_len];
    // Userinfo before an `@` is not the host, and `mine.example@evil.test` is
    // exactly how somebody would try to borrow one.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    (
        host.trim_start_matches("www.").to_string(),
        after_scheme[authority_len..].to_string(),
    )
}

/// The URLs on a line, from `http` to the first delimiter.
fn urls_in(line: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = line;

    while let Some(at) = rest.find("http") {
        rest = &rest[at..];
        if !(rest.starts_with("https://") || rest.starts_with("http://")) {
            rest = &rest[4..];
            continue;
        }
        let url: String = rest
            .chars()
            .take_while(|c| !matches!(c, '"' | '\'' | ')' | '>' | ' ' | ',' | '`'))
            .collect();
        rest = &rest[url.len().min(rest.len())..];
        urls.push(url);
    }

    urls
}

/// Whether `host` is, or is a subdomain of, `mine`.
fn host_matches(host: &str, mine: &str) -> bool {
    if mine.is_empty() {
        return false;
    }
    // The host itself, or a subdomain of it. Deliberately *not* "any label
    // equals the login": `author_hosts` admits any login of four characters or
    // more, and `docs` matching `docs.rs` — or `blog`, `test`, `demo`, `help`
    // matching half the internet — reaches two signals on one ordinary link.
    host == mine || host.ends_with(&format!(".{mine}"))
}

/// The credential-shaped identifier on a line, if there is one.
///
/// Matches the *name*, and only the name. Returning the value — or the line —
/// would put a secret somebody committed onto a public comment, which is the
/// one thing `AGENTS.md` says the scanners must never do.
fn credential_name(line: &str) -> Option<String> {
    const SUFFIXES: [&str; 4] = ["_API_KEY", "_SECRET", "_ACCESS_TOKEN", "_CLIENT_SECRET"];

    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .find(|word| {
            word.len() > 4
                && word
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && SUFFIXES.iter().any(|suffix| word.ends_with(suffix))
        })
        .map(str::to_string)
}

/// Whether a path is code rather than prose or configuration.
fn is_code(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, extension)| CODE_EXTENSIONS.contains(&extension))
}

/// The hosts to treat as the author's own.
///
/// Their login is included because a vanity domain usually echoes it, and
/// `blog` is whatever their GitHub profile advertises. Both are supplied by the
/// caller rather than fetched here, so this module stays a pure function.
pub fn author_hosts(login: &str, blog: Option<&str>) -> Vec<String> {
    let mut hosts = Vec::new();
    let login = login.trim().trim_end_matches("[bot]").to_ascii_lowercase();
    // Two characters is not a distinguishing host. A login like `jd` would
    // match `jd` inside any URL on earth.
    if login.len() >= 4 {
        hosts.push(login);
    }
    if let Some(blog) = blog {
        let host = blog
            .trim()
            .to_ascii_lowercase()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("www.")
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string();
        if host.len() >= 4 {
            hosts.push(host);
        }
    }
    hosts
}

#[cfg(test)]
#[path = "promo_test.rs"]
mod tests;
