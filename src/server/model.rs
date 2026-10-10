//! Deployment-owned model resolution and reuse, behind the `serve` feature.
//!
//! Adapter resolution lives here rather than in route workers. The resolved
//! handles share one deployment policy; repository overlays cannot change it.

use std::sync::Arc;

pub use crate::harness::factory::GatewayModelFactory;

use crate::config::Config;
use crate::config::types::Workload;
use crate::error::Result;
use crate::ports::model::Model;
use crate::ports::model_factory::{ModelFactory, ModelPurpose};

/// The adapters shared by the server and every cloned worker state.
#[derive(Clone)]
pub(crate) struct ResolvedModels {
    text: Arc<dyn Model>,
    vision: Option<(String, Arc<dyn Model>)>,
}

impl ResolvedModels {
    /// Resolve each required adapter once before any listener opens.
    pub(crate) async fn resolve(factory: &dyn ModelFactory, config: &Config) -> Result<Self> {
        let text = factory.create(&config.models, ModelPurpose::Text).await?;
        let vision = match config.model_for_vision() {
            Some(id) => Some((
                id.to_owned(),
                factory.create(&config.models, ModelPurpose::Vision).await?,
            )),
            None => None,
        };
        tracing::info!(vision = vision.is_some(), "server model adapters resolved");
        Ok(Self { text, vision })
    }

    /// Clone the deployment's text adapter, retaining its shared runtime state.
    pub(crate) fn text(&self) -> Arc<dyn Model> {
        self.text.clone()
    }

    /// Select the vision adapter, or text captions with the preview workload ID.
    pub(crate) fn caption<'a>(&'a self, config: &'a Config) -> (Arc<dyn Model>, &'a str, bool) {
        match &self.vision {
            Some((id, model)) => (model.clone(), id, true),
            None => (
                self.text(),
                config.model_for_workload(Workload::Preview),
                false,
            ),
        }
    }
}

#[cfg(test)]
#[path = "model_test.rs"]
mod tests;
