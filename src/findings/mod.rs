//! Findings, and the rules that decide which of them a human ever sees.

pub mod anchor;
pub mod prior;
pub mod render;
pub mod suggest;
pub mod types;

pub use crate::findings::anchor::anchor_context;
pub use crate::findings::prior::PriorReview;
pub use crate::findings::suggest::applicable;
pub use crate::findings::types::{Finding, Suggestion};
