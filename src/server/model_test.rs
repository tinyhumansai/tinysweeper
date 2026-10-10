//! Server-owned model resolution and reuse without credentials or network.

use std::sync::Mutex;

use super::*;
use crate::config::types::Models;
use crate::harness::MockModel;
use crate::ports::model_factory::StaticModelFactory;

struct RecordingFactory {
    delegate: StaticModelFactory,
    calls: Mutex<Vec<ModelPurpose>>,
    fail: Option<ModelPurpose>,
}

#[async_trait::async_trait]
impl ModelFactory for RecordingFactory {
    async fn create(&self, models: &Models, purpose: ModelPurpose) -> Result<Arc<dyn Model>> {
        self.calls.lock().unwrap().push(purpose);
        if self.fail == Some(purpose) {
            return Err(crate::error::Error::Model("factory unavailable".into()));
        }
        self.delegate.create(models, purpose).await
    }
}

fn factory() -> RecordingFactory {
    RecordingFactory {
        delegate: StaticModelFactory::new(Arc::new(MockModel::silent()))
            .with_vision(Arc::new(MockModel::silent())),
        calls: Mutex::new(vec![]),
        fail: None,
    }
}

#[tokio::test]
async fn text_model_is_resolved_once_and_reused_across_worker_clones_and_captions() {
    let factory = factory();
    let config = Config::default();
    let models = ResolvedModels::resolve(&factory, &config).await.unwrap();
    let worker = models.clone();
    assert!(Arc::ptr_eq(&models.text(), &worker.text()));
    let (caption, id, vision) = worker.caption(&config);
    assert!(Arc::ptr_eq(&models.text(), &caption));
    assert_eq!(
        id,
        config.model_for_workload(crate::config::types::Workload::Preview)
    );
    assert!(!vision);
    assert_eq!(*factory.calls.lock().unwrap(), vec![ModelPurpose::Text]);
}

#[tokio::test]
async fn configured_vision_has_its_own_reused_handle_and_trimmed_model_id() {
    let factory = factory();
    let mut config = Config::default();
    config.models.vision = Some("  vendor/vision  ".into());
    let models = ResolvedModels::resolve(&factory, &config).await.unwrap();
    let worker = models.clone();
    let (first, id, vision) = models.caption(&config);
    let (second, _, _) = worker.caption(&config);
    assert_eq!(id, "vendor/vision");
    assert!(vision);
    assert!(Arc::ptr_eq(&first, &second));
    assert!(!Arc::ptr_eq(&first, &models.text()));
    assert_eq!(
        *factory.calls.lock().unwrap(),
        vec![ModelPurpose::Text, ModelPurpose::Vision]
    );
}

#[tokio::test]
async fn blank_vision_uses_text_and_does_not_construct_a_vision_adapter() {
    let factory = factory();
    let mut config = Config::default();
    config.models.vision = Some("  ".into());
    let models = ResolvedModels::resolve(&factory, &config).await.unwrap();
    assert!(!models.caption(&config).2);
    assert_eq!(*factory.calls.lock().unwrap(), vec![ModelPurpose::Text]);
}

#[tokio::test]
async fn failed_text_or_configured_vision_resolution_refuses_startup() {
    for purpose in [ModelPurpose::Text, ModelPurpose::Vision] {
        let mut factory = factory();
        factory.fail = Some(purpose);
        let mut config = Config::default();
        config.models.vision = Some("vendor/vision".into());
        let error = match ResolvedModels::resolve(&factory, &config).await {
            Ok(_) => panic!("failed factory produced models"),
            Err(error) => error,
        };
        assert!(matches!(error, crate::error::Error::Model(_)));
    }
}
