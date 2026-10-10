//! Lane orchestration: how a lane's reviewers are asked.
//!
//! Every model-calling lane asks its reviewers through [`runner`], which makes
//! one structured call per reviewer, all at once, under the lane's shared
//! budget in [`caps`]. What that buys, in the order it matters:
//!
//! - **A panel instead of an oracle.** One expensive call per file can be
//!   replaced by several cheap ones with different lenses, and agreement ranks
//!   what they found. Agreement is a cheaper noise signal than a better model,
//!   and it is one this crate can test offline.
//! - **Asking instead of guessing, bounded.** A reviewer may look code up in
//!   the read-only tree ([`lookup`]) or ask a sub-agent a question
//!   ([`subagent`]), exactly one level deep.
//! - **A model that never acts.** A reviewer's only capability is answering a
//!   schema; repository reads are performed by the host, and there is no tool,
//!   network or code-execution path for a reviewer to reach.
//!
//! The runner orchestrates; it does not decide. Merging opinions into findings
//! is Rust in `council`, because that is the step whose behaviour the golden
//! tests pin.

pub mod caps;
pub mod lookup;
pub mod panel;
pub(crate) mod review_tree;
pub mod runner;
pub mod subagent;
