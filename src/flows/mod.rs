//! Lane orchestration as a graph.
//!
//! Every model-calling lane runs as a tinyflows [`WorkflowGraph`] rather than as
//! hand-written concurrency. What that buys, in the order it matters:
//!
//! - **A panel instead of an oracle.** One expensive call per file is replaced
//!   by several cheap ones with different lenses, and only what more than one of
//!   them saw survives. Agreement is a cheaper noise filter than a better model,
//!   and it is one this crate can test offline.
//! - **Sub-agents, bounded.** A panellist that needs to know something about the
//!   codebase spawns a child workflow to answer it, exactly one level deep — see
//!   [`subagent`].
//! - **One concurrency story.** Per-file fan-out, the panel, and sub-agents are
//!   all node configuration, so the width of a review is data rather than a
//!   constant compiled into a semaphore.
//!
//! The graph orchestrates; it does not decide. Merging opinions into findings is
//! Rust in [`consensus`], not a `transform` node, because that is the step whose
//! behaviour the golden tests pin.

pub mod caps;
pub mod consensus;
pub mod panel;
pub mod runner;
pub mod subagent;
pub mod tier;
