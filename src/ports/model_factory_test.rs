//! Offline factory identity and capability selection tests.

use std::sync::Arc;

use super::model_factory::{ModelFactory, ModelPurpose, StaticModelFactory};
use crate::harness::MockModel;
use crate::ports::model::Model;

#[tokio::test]
async fn static_factory_reuses_the_supplied_text_and_vision_models() {
    let text: Arc<dyn Model> = Arc::new(MockModel::silent());
    let vision: Arc<dyn Model> = Arc::new(MockModel::silent());
    let factory = StaticModelFactory::new(text.clone()).with_vision(vision.clone());
    let models = crate::config::Config::default().models;
    for _ in 0..2 {
        assert!(Arc::ptr_eq(
            &factory.create(&models, ModelPurpose::Text).await.unwrap(),
            &text
        ));
        assert!(Arc::ptr_eq(
            &factory.create(&models, ModelPurpose::Vision).await.unwrap(),
            &vision
        ));
    }
}

#[tokio::test]
async fn missing_vision_never_silently_uses_the_text_model() {
    let factory = StaticModelFactory::new(Arc::new(MockModel::silent()));
    let error = match factory
        .create(
            &crate::config::Config::default().models,
            ModelPurpose::Vision,
        )
        .await
    {
        Ok(_) => panic!("vision unexpectedly resolved"),
        Err(error) => error,
    };
    assert!(matches!(error, crate::error::Error::Model(_)));
}
