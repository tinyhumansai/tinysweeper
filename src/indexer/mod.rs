//! Keeping a repository's chunk index current, and knowing what it cost.
//!
//! Always compiled; the MongoDB manifest adapter is behind `serve`.
//!
//! The [`chunk`](crate::chunk) module decides *what* a chunk is. This one
//! decides *when* to make one, and the answer it is built around is "as rarely
//! as possible". Three mechanisms do that work, and each of them exists because
//! its absence has a specific, observed failure:
//!
//! - **Content hashing.** A chunk's id contains the hash of its text, so an
//!   unchanged file's chunks are already present and are skipped without an
//!   embedding call. Without it, every push re-embeds the whole repository and
//!   the bill is proportional to the number of pushes rather than to the number
//!   of changes.
//! - **Write-then-delete.** New chunks are upserted before superseded ones are
//!   removed, so an interrupted run leaves a slightly stale index rather than
//!   an empty one. See [`run`] for the ordering in full.
//! - **A claim, not a lock.** Two workers must not index one repository at
//!   once, and a worker that loses the race requeues the job instead of waiting
//!   on it.
//!
//! And it prices itself: embedding spend is counted in [`cost`] because nothing
//! upstream counts it at all.

pub mod cost;
#[cfg(feature = "serve")]
pub mod fetch;
pub mod mock;
#[cfg(feature = "serve")]
pub mod mongo;
pub mod run;
pub mod types;

pub use crate::indexer::cost::EmbedUsage;
pub use crate::indexer::mock::{CountingEmbedder, MockManifest};
pub use crate::indexer::run::Indexer;
pub use crate::indexer::types::{
    Claim, IndexLease, IndexOutcome, IndexReport, IndexState, IndexedFile, RepoIndex, Settled,
};

#[cfg(feature = "serve")]
pub use crate::indexer::fetch::Checkout;
#[cfg(feature = "serve")]
pub use crate::indexer::mongo::MongoManifest;
