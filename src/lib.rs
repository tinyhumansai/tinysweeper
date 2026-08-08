//! tinysweeper — a GitHub review bot that fans a pull request out across
//! independently-gated check runs.
//!
//! The shape of the crate follows ports & adapters: [`ports`] holds one trait
//! per file, every port has an always-compiled offline mock, and only the real
//! network-backed adapter sits behind a Cargo feature. That is what keeps
//! `cargo test` offline and makes `local-review` — the whole engine over a
//! local git range, with no GitHub token — possible.
//!
//! Modules land milestone by milestone; see `ROADMAP.md`.

pub mod app;
pub mod config;
pub mod error;
pub mod evidence;
pub mod findings;
pub mod forge;
pub mod harness;
pub mod lanes;
pub mod ports;
pub mod scan;
#[cfg(feature = "serve")]
pub mod server;

pub use crate::error::{Error, Result};

/// The crate version, as reported by `tinysweeper --version` and stamped into
/// the hidden markers written onto pull requests.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The marker prefix tinysweeper writes into its own GitHub comments.
///
/// Automation never parses prose: the review lane and the apply path talk to
/// each other exclusively through HTML-comment markers carrying this prefix.
pub const MARKER_PREFIX: &str = "tinysweeper:";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_the_package_version() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!VERSION.is_empty());
    }
}
