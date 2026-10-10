//! Shared OpenHuman model construction for CLI and server hosts (`harness`).

use crate::config::types::Models;
use crate::error::Result;
use crate::ports::model::Model;
use crate::ports::model_factory::{ModelFactory, ModelPurpose};
use async_trait::async_trait;
use std::sync::Arc;

/// The shared default OpenHuman-backed gateway factory.
#[derive(Debug, Default)]
pub struct GatewayModelFactory;

#[async_trait]
impl ModelFactory for GatewayModelFactory {
    async fn create(&self, models: &Models, purpose: ModelPurpose) -> Result<Arc<dyn Model>> {
        let model = match purpose {
            ModelPurpose::Text => crate::harness::embed::GatewayModel::from_config(models)?,
            ModelPurpose::Vision => crate::harness::embed::GatewayModel::for_vision(models)?,
        };
        Ok(Arc::new(model))
    }
}
