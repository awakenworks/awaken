//! Test-support observation port; no product transitions or persistence owner.
//! Installed once by the scenario executable. Production builds omit the port.
use std::sync::{Arc, OnceLock};

#[async_trait::async_trait]
pub trait RealizationObserver: Send + Sync {
    async fn observe(&self, point: &'static str, session_id: &str, data: serde_json::Value);
}
static OBSERVER: OnceLock<Arc<dyn RealizationObserver>> = OnceLock::new();
pub fn install(observer: Arc<dyn RealizationObserver>) {
    assert!(
        OBSERVER.set(observer).is_ok(),
        "one realization observation owner"
    );
}
pub fn enabled() -> bool {
    OBSERVER.get().is_some()
}
pub async fn checkpoint(point: &'static str, session_id: &str, data: serde_json::Value) {
    if let Some(observer) = OBSERVER.get() {
        observer.observe(point, session_id, data).await;
    }
}
