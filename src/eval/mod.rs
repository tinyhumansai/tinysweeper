//! Measuring review quality against a labelled corpus.
//!
//! Always compiled. Everything except the live recording path runs offline on
//! default features, which is the property that lets a scoring rule be
//! rewritten without spending a cent.
//!
//! # Why this exists
//!
//! Before it, nothing in tinysweeper could say whether a change to a prompt, a
//! rule document, a threshold or a lane made the review better. The test suite
//! proves the machinery behaves; it cannot prove the reviewer is any good, and
//! the two questions are not related. Every prompt in `presets/rules/` was
//! written from judgement and validated by reading the output on live pull
//! requests, which measures the reviewer against the memory of whoever last
//! looked at it.
//!
//! # The shape
//!
//! ```text
//!   eval run    corpus + model ──► proposals + cassettes on disk   [costs money]
//!   eval score  proposals + labels ──► scorecard                   [free, offline]
//!   eval report scorecard (+ baseline) ──► markdown / json         [free, offline]
//! ```
//!
//! Running and scoring are separated by files on disk, for the same reason
//! `review` and `apply` are: the expensive, irreversible half happens once, and
//! everything downstream is a pure function of what it wrote. A scoring rule
//! gets rewritten ten times before it is right, and welding it to the run would
//! price each rewrite at another live corpus.
//!
//! # What it deliberately does not do
//!
//! There is no single composite score. A number that folds recall, precision
//! and cost together can be improved by trading the one that matters for the
//! two that do not, and nobody reading it can tell which happened. The gate is
//! a conjunction — recall at or above baseline, forbidden hits at or below it,
//! cost inside budget — and each is reported on its own line.

#[cfg(test)]
#[path = "committed_test.rs"]
mod committed_test;
pub mod corpus;
pub mod report;
pub mod runner;
pub mod score;
pub mod types;

pub use crate::eval::corpus::{Corpus, LoadedCase, load};
pub use crate::eval::report::{Comparison, compare, markdown};
pub use crate::eval::runner::{
    RunOptions, RunOutcome, read_scorecard, rescore, run, write_scorecard,
};
pub use crate::eval::score::score;
pub use crate::eval::types::{Case, CaseScore, Fixture, Scorecard, Verdict};
