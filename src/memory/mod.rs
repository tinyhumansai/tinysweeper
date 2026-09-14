//! The memory layer: what the reviewer remembers about a repository over time.
//!
//! Always compiled. Everything goes through the [`Memory`] port, so ingest,
//! recall and rendering all run offline against [`MockMemory`]; the CortexDB
//! adapter in [`cortex`] is the one file behind the `cortex` feature.
//!
//! # Why a memory, when there is already an index
//!
//! `src/retrieve` finds the code that reads like the diff and the code the diff
//! reaches. `src/knowledge` reads the instruction files on the branch under
//! review. Both are recomputed from the tree, and both forget everything the
//! moment the review ends. What neither can answer:
//!
//! - *"Has this reviewer raised this before, and what did the maintainers
//!   say?"* — the single largest source of noise in a long-running bot is a
//!   finding that was rejected on pull request 40 coming back on pull request
//!   41, phrased slightly differently.
//! - *"Which rule in this repository's own guides applies to these paths?"* —
//!   asked as a question, and answered with a citation, rather than as a
//!   similarity query that returns whichever paragraph shares the most words.
//!
//! So the memory holds three sections per repository — code, conventions,
//! reviews ([`types::MemorySection`]) — and a review consults it twice: by
//! *query*, composed from the diff the same way the retrieval query is, and by
//! *question*, a configurable list templated over the changed paths and asked
//! of the engine's grounded-answer route.
//!
//! # Where it sits in the prompt
//!
//! In the volatile suffix, fenced as `repository-memory`, after the retrieved
//! code and before the diff. Everything in it is prose somebody other than the
//! operator wrote — a merged `AGENTS.md`, a maintainer's reply, the engine's
//! own synthesis — so it goes where the model is told to treat text as data.
//! The framing tells the lane what a *rejected* outcome means: do not raise it
//! again unless the code is materially different.
//!
//! # What it never does
//!
//! Fail the review. Every call in the review path is best-effort; an
//! unreachable engine produces a [`recall::MemoryContext`] whose status says
//! so, and the check-run summary states it.

pub mod discussions;
pub mod ingest;
pub mod mock;
pub mod recall;
pub mod types;

#[cfg(feature = "cortex")]
pub mod cortex;

pub use crate::memory::discussions::{DiscussionReport, Discussions, Subject};
pub use crate::memory::ingest::{IngestReport, Ingestor};
pub use crate::memory::mock::MockMemory;
pub use crate::memory::recall::{MemoryContext, MemoryStatus, Recaller};
pub use crate::memory::types::{
    Citation, MemoryAnswer, MemoryItem, MemoryKind, MemoryScope, MemorySection, Recollection,
    RememberReport,
};

/// Whether a memory endpoint may carry a bearer token.
///
/// HTTPS anywhere; plain HTTP only to a literal loopback host, which is what a
/// local engine and the test harness use. Anything else is refused by name,
/// because the failure it prevents — the engine's credential crossing a
/// network in the clear — is silent.
///
/// Parsed with [`url::Url`] — the same crate `reqwest` resolves the request
/// against — rather than a hand-rolled split on `/` and `@`. A manual parser
/// that disagrees with the HTTP client about where the authority ends is
/// exactly the gap that let `http://localhost@evil.example` read as host
/// `localhost` here while the client actually dialled `evil.example` in the
/// clear.
pub fn endpoint_allowed(endpoint: &str) -> std::result::Result<(), String> {
    endpoint_allowed_with(endpoint, false)
}

/// [`endpoint_allowed`], with the operator's private-network statement.
///
/// `allow_private_http` widens plain `http://` from loopback to any host —
/// the case is an engine on the server's own Docker network, which never
/// leaves the box. The scheme still has to be `http` or `https` and the host
/// still has to exist; a userinfo-carrying URL is parsed, not prefix-matched,
/// so `http://127.0.0.1@evil` remains the host `evil`.
pub fn endpoint_allowed_with(
    endpoint: &str,
    allow_private_http: bool,
) -> std::result::Result<(), String> {
    let endpoint = endpoint.trim();
    let url = url::Url::parse(endpoint).map_err(|err| format!("not a URL: {err}"))?;
    match url.scheme() {
        "https" => {
            if url.host_str().is_none_or(str::is_empty) {
                return Err("no host after `https://`".into());
            }
            Ok(())
        }
        "http" => {
            // Parsed, not prefix-matched: `127.0.0.1.evil.com` starts with
            // `127.` and is not loopback.
            let loopback = match url.host() {
                Some(url::Host::Domain(domain)) => domain == "localhost",
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                None => false,
            };
            let named = url.host_str().is_some_and(|h| !h.is_empty());
            if loopback || (allow_private_http && named) {
                Ok(())
            } else {
                Err(
                    "plain `http://` is only allowed to a loopback host; use `https://`, or set \
                     `memory.allow_private_http = true` for an engine on a private network"
                        .into(),
                )
            }
        }
        _ => Err("must start with `https://` (or `http://` for loopback)".into()),
    }
}

/// The first `max` characters of `text`, on a character boundary, with an
/// ellipsis when anything was cut.
pub fn excerpt(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_is_allowed_anywhere_and_http_only_to_loopback() {
        assert!(endpoint_allowed("https://api-v1.cortexdb.ai").is_ok());
        assert!(endpoint_allowed("http://127.0.0.1:3141").is_ok());
        assert!(endpoint_allowed("http://localhost:3141/").is_ok());
        assert!(endpoint_allowed("http://[::1]:3141").is_ok());
        assert!(endpoint_allowed("http://cortex.internal:3141").is_err());
        assert!(endpoint_allowed("http://127.0.0.1.evil.com").is_err());
        assert!(endpoint_allowed("ftp://x").is_err());
        // The operator's statement widens http to any named host, and only that.
        assert!(endpoint_allowed_with("http://cortexdb:3141", true).is_ok());
        assert!(endpoint_allowed_with("http://10.0.0.5:3141", true).is_ok());
        assert!(endpoint_allowed_with("ftp://cortexdb", true).is_err());
        assert!(endpoint_allowed_with("http://", true).is_err());
        assert!(endpoint_allowed("https://").is_err());
    }

    #[test]
    fn userinfo_in_the_authority_does_not_disguise_the_real_host() {
        // The URL client resolves the host after the last `@`; the loopback
        // check must reject anything this could actually reach off-box.
        assert!(endpoint_allowed("http://localhost:80@evil.example").is_err());
        assert!(endpoint_allowed("http://localhost@evil.example").is_err());
        assert!(endpoint_allowed("http://127.0.0.1@evil.example").is_err());
        // A userinfo-prefixed loopback host is still loopback.
        assert!(endpoint_allowed("http://user:pass@localhost:3141").is_ok());
        assert!(endpoint_allowed("http://user:pass@127.0.0.1:3141").is_ok());
    }

    #[test]
    fn excerpts_cut_on_character_boundaries() {
        assert_eq!(excerpt("short", 10), "short");
        assert_eq!(excerpt("héllo wörld", 6), "héllo…");
        assert_eq!(excerpt("  padded  ", 10), "padded");
    }
}
