//! The agent harness: prompts, schemas, and the models that answer them.
//!
//! Built around one economic fact. Both model tiers price a cache read at a
//! fraction of a fresh input token and charge nothing to populate the cache, so
//! a re-review is cheap exactly to the extent that its prompt *prefix* is
//! unchanged. [`prompt`] exists to guarantee that; see its module docs before
//! changing how a prompt is assembled.

#[cfg(feature = "harness")]
pub mod agentic;
#[cfg(feature = "harness")]
pub(crate) mod budget;
pub mod cassette;
#[cfg(feature = "harness")]
pub mod embed;
#[cfg(feature = "harness")]
pub mod factory;
#[cfg(all(test, feature = "harness"))]
pub(crate) mod fake_gateway;
#[cfg(feature = "harness")]
pub mod langfuse;
pub mod mock;
#[cfg(all(test, feature = "harness"))]
mod parity_test;
pub mod pricing;
pub mod prompt;
pub mod schema;

pub use crate::harness::cassette::Cassette;
pub use crate::harness::mock::MockModel;
pub use crate::harness::prompt::{Prompt, PromptInputs};
pub use crate::harness::schema::{LaneResponse, RawFinding};
