//! The always-compiled model-construction seam and its offline implementation.
//!
//! Factories resolve deployment-owned model adapters once. A worker gets an
//! already-resolved `Model`, never provider constructors or write credentials.

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::types::Models;
use crate::error::{Error, Result};
use crate::ports::model::Model;

/// Which provider policy the resolved model needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPurpose {
    /// Text workloads, including their configured routing and fallback ladder.
    Text,
    /// Image captions, without text-only routes, pins or fallbacks.
    Vision,
}

/// Resolve a reusable model from deployment-owned provider configuration.
///
/// The async boundary accommodates adapters that initialize an embedded runtime.
/// No reviewed repository content or forge credentials reach this port.
#[async_trait]
pub trait ModelFactory: Send + Sync {
    /// Construct the adapter for `purpose`; failures refuse server startup.
    async fn create(&self, models: &Models, purpose: ModelPurpose) -> Result<Arc<dyn Model>>;
}

/// An offline factory returning handles supplied by its caller.
///
/// Pair with `harness::MockModel` for tests. It ignores provider configuration
/// and opens no files, processes or network connections. Missing vision is an
/// error rather than silently handing images to the text model.
#[derive(Clone)]
pub struct StaticModelFactory {
    text: Arc<dyn Model>,
    vision: Option<Arc<dyn Model>>,
}

impl StaticModelFactory {
    /// Supply the reusable text model.
    pub fn new(text: Arc<dyn Model>) -> Self {
        Self { text, vision: None }
    }

    /// Supply a separate reusable vision model.
    pub fn with_vision(mut self, vision: Arc<dyn Model>) -> Self {
        self.vision = Some(vision);
        self
    }
}

#[async_trait]
impl ModelFactory for StaticModelFactory {
    async fn create(&self, _: &Models, purpose: ModelPurpose) -> Result<Arc<dyn Model>> {
        match purpose {
            ModelPurpose::Text => Ok(self.text.clone()),
            ModelPurpose::Vision => self.vision.clone().ok_or_else(|| {
                Error::Model("no vision model supplied to the static factory".into())
            }),
        }
    }
}
