//! The calls a lane's reviewers make.
//!
//! `src/council` decides *who* reviews and what becomes of their findings. This
//! module is *what each reviewer is asked*: one [`Call`] per reviewer, all of
//! them made at once by `flows::runner` and joined before anything is read.
//!
//! ```text
//!   evidence ─┬─ reviewer-a ─┐
//!             ├─ reviewer-b ─┼─ join ─► one answer per reviewer
//!             └─ reviewer-c ─┘
//! ```
//!
//! ## Why concurrent rather than a loop
//!
//! A council multiplies calls: files × reviewers. Run serially — which is what
//! it was, because a budget can only be checked once a call has returned — a
//! three-agent council on a twenty-file pull request is sixty round trips end
//! to end. The ceiling lives in [`crate::flows::caps::ModelCapability`], which
//! refuses a call however many are in flight, so the width is free to be real.
//!
//! ## What is deliberately *not* here
//!
//! There is no verification round. An earlier version of this module ran one:
//! every finding put to independent judges, majority keeps it. `src/falsify`
//! argues at length why that shape is wrong — a checker that sees less than the
//! reviewer did rejects whatever it cannot confirm, which deletes exactly the
//! findings that needed context to notice — and it is right. Removal is
//! `falsify`'s job, it rejects only what it can *prove* wrong, and it fails
//! open. Agreement between reviewers is a ranking signal, handled by
//! `council::merge`, and it never removes anything here.

/// How many files a lane reviews at once.
///
/// Inherited from the semaphore this replaced. The number is about spend and
/// provider rate limits, not CPU: these tasks are almost entirely waiting on a
/// model.
pub const MAX_CONCURRENT_FILES: usize = 8;

/// One model call the graph should make.
///
/// Assembled by the lane, because prompt layering is the lane's business and
/// which half of it is cacheable is `harness::prompt`'s — see its module docs
/// before moving anything between `system` and `prompt`.
#[derive(Debug, Clone)]
pub struct Call {
    /// The reviewer's id. It is what a failure names.
    pub id: String,
    /// The model id, already resolved from tier by `council::reviewers`.
    pub model: String,
    /// The cacheable prefix.
    pub system: String,
    /// The volatile suffix: the evidence.
    pub prompt: String,
    /// The schema name this answer is reported under.
    pub schema_name: String,
}
