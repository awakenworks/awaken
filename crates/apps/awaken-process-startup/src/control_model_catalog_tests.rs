use super::*;
use awaken_admin_config_api::BrokeredCatalogDiscovery;
use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
use awaken_model_catalog::{ApiDialect, BrokeredCatalogProjection, BrokeredModelProjection};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Discovery {
    observation: Mutex<Result<BrokeredCatalogProjection, String>>,
    calls: AtomicUsize,
    hang: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl BrokeredCatalogDiscovery for Discovery {
    async fn projection(&self) -> Result<BrokeredCatalogProjection, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        self.observation.lock().unwrap().clone()
    }
}

fn observation(models: &[&str], revision: u64) -> BrokeredCatalogProjection {
    BrokeredCatalogProjection {
        broker_id: "cloud-fixture".into(),
        control_base_url: "https://gateway.invalid".into(),
        observed_at_unix_ms: revision * 1000,
        models: models
            .iter()
            .map(|model| BrokeredModelProjection {
                provider_id: "provider".into(),
                model_id: (*model).into(),
                dialect: ApiDialect::OpenAiChat,
                context_window: Some(32000),
                max_output_tokens: Some(1024),
                publication_revision: revision,
            })
            .collect(),
    }
}

fn discovery(value: Result<BrokeredCatalogProjection, String>) -> Arc<Discovery> {
    Arc::new(Discovery {
        observation: Mutex::new(value),
        calls: AtomicUsize::new(0),
        hang: std::sync::atomic::AtomicBool::new(false),
    })
}

// Cause/effect graph: S1 initial observation -> immediately visible Catalog;
// S2 no publication -> valid empty; S2b initial authority error -> no task;
// S3 new revision -> next periodic observation; S4 error -> retain last facts,
// then recovery -> converge; S5 repeat -> no duplicate; S6 shutdown (including
// an in-flight unavailable adapter) -> no further observations.
// The actual production start/loop is exercised with Tokio's virtual clock;
// repository durability and remote authority are exercised by Cloud HTTP E2E.
#[tokio::test(start_paused = true)]
async fn startup_refresh_recovery_and_cancellation_follow_one_lifecycle() {
    for initial in [Ok(observation(&[], 1)), Err("offline".into())] {
        let catalog = Arc::new(InMemoryCatalogRepo::new());
        let source = discovery(initial.clone());
        let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
        let result =
            start_hosted_model_catalog_reconciliation(catalog.clone(), source.clone(), &lifecycle)
                .await;
        assert_eq!(result.is_ok(), initial.is_ok(), "S2/S2b");
        assert!(catalog.snapshot().await.unwrap().offerings.is_empty());
        lifecycle.shutdown(Duration::from_secs(1)).await.unwrap();
        tokio::time::advance(Duration::from_secs(120)).await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    }
    let catalog = Arc::new(InMemoryCatalogRepo::new());
    let source = discovery(Ok(observation(&["a"], 1)));
    let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    start_hosted_model_catalog_reconciliation(catalog.clone(), source.clone(), &lifecycle)
        .await
        .unwrap();
    assert_eq!(catalog.snapshot().await.unwrap().offerings.len(), 1, "S1");
    tokio::task::yield_now().await;
    *source.observation.lock().unwrap() = Err("transient".into());
    let before = catalog.snapshot().await.unwrap();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        source.calls.load(Ordering::SeqCst),
        2,
        "S4 attempts refresh"
    );
    assert_eq!(
        catalog.snapshot().await.unwrap(),
        before,
        "S4 retains facts"
    );
    *source.observation.lock().unwrap() = Ok(observation(&["b"], 2));
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    let updated = catalog.snapshot().await.unwrap();
    assert_eq!(updated.offerings.len(), 2, "S3 retains unavailable history");
    assert!(updated.offerings.iter().any(
        |o| o.model_id == "a" && o.status == awaken_model_catalog::OfferingStatus::Unavailable
    ));
    assert!(
        updated
            .offerings
            .iter()
            .any(|o| o.model_id == "b" && o.status == awaken_model_catalog::OfferingStatus::Active)
    );
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(catalog.snapshot().await.unwrap(), updated, "S5");
    source.hang.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    lifecycle
        .shutdown(Duration::from_secs(1))
        .await
        .expect("S6 cancels in-flight discovery");
    let count = source.calls.load(Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(120)).await;
    assert_eq!(
        source.calls.load(Ordering::SeqCst),
        count,
        "S6 no orphan task"
    );
}
