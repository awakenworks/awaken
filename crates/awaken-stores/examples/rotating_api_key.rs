//! Reference example: a `ConfigStore` wrapper that rotates an api_key
//! credential on a fixed schedule and emits a `ConfigChangeEvent` so that
//! a connected `ConfigRuntimeManager` re-applies the new value.
//!
//! Run with `cargo run --example rotating_api_key -p awaken-stores`.
//!
//! ## What this demonstrates
//!
//! - **Pattern**: implement `ConfigStore` (and optionally
//!   `ConfigChangeNotifier`) on a wrapper that delegates CRUD to an inner
//!   store and intercepts only the secret-bearing field.
//! - **Rotation seam**: a `TokenSource` trait abstracts whatever fetches a
//!   fresh credential — vault, IAM-issued STS token, OAuth refresh, etc.
//! - **Event emission**: rotation publishes a single per-rotation
//!   `ConfigChangeEvent`, which lets a downstream
//!   `ConfigRuntimeManager` re-apply quickly without polling.
//!
//! Copy this file as a starting point — none of the types here are part
//! of `awaken-stores`'s public API.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use awaken_contract::contract::config_store::{
    ConfigChangeEvent, ConfigChangeKind, ConfigChangeNotifier, ConfigChangeSubscriber, ConfigStore,
};
use awaken_contract::contract::storage::StorageError;
use awaken_stores::InMemoryStore;
use serde_json::{Value, json};
use tokio::sync::broadcast;

/// Plug-point for whatever vends fresh credentials. Implementations call
/// out to vault / STS / OAuth refresh and return the new bearer string.
#[async_trait]
trait TokenSource: Send + Sync {
    async fn fresh_token(&self) -> Result<String, StorageError>;
}

/// Demo source that returns `sk-rotated-N` where N counts up. A real
/// implementation would call a secret-management API instead.
struct CountingTokenSource {
    counter: AtomicU64,
}

#[async_trait]
impl TokenSource for CountingTokenSource {
    async fn fresh_token(&self) -> Result<String, StorageError> {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(format!("sk-rotated-{n:08}"))
    }
}

/// Wrapper that rotates the `api_key` field of one specific
/// `(namespace, id)` entry on demand or on a fixed interval.
///
/// Other CRUD operations and other namespaces pass through to the inner
/// store untouched.
struct RotatingApiKeyStore<S: ConfigStore + 'static> {
    inner: Arc<S>,
    notifier: broadcast::Sender<ConfigChangeEvent>,
    namespace: String,
    target_id: String,
    source: Arc<dyn TokenSource>,
}

impl<S: ConfigStore + 'static> RotatingApiKeyStore<S> {
    fn new(
        inner: Arc<S>,
        namespace: impl Into<String>,
        target_id: impl Into<String>,
        source: Arc<dyn TokenSource>,
    ) -> Self {
        let (notifier, _) = broadcast::channel(32);
        Self {
            inner,
            notifier,
            namespace: namespace.into(),
            target_id: target_id.into(),
            source,
        }
    }

    /// Fetch a fresh token, splice it into the target entry, and emit one
    /// change event. The rest of the entry is preserved so any non-secret
    /// fields (`base_url`, `adapter`, `timeout_secs`, `adapter_options`)
    /// keep their values.
    async fn rotate_once(&self) -> Result<(), StorageError> {
        let fresh = self.source.fresh_token().await?;
        let mut current = self
            .inner
            .get(&self.namespace, &self.target_id)
            .await?
            .unwrap_or_else(|| json!({"id": &self.target_id}));
        if let Some(object) = current.as_object_mut() {
            object.insert("api_key".into(), Value::String(fresh));
        }
        self.inner
            .put(&self.namespace, &self.target_id, &current)
            .await?;
        // `send` only fails if there are no live subscribers — the rotation
        // itself succeeded, so we don't propagate that as an error.
        let _ = self.notifier.send(ConfigChangeEvent {
            namespace: self.namespace.clone(),
            id: self.target_id.clone(),
            kind: ConfigChangeKind::Put,
        });
        Ok(())
    }

    /// Spawn a background task that rotates every `interval`. The handle
    /// can be aborted to stop rotation.
    fn start_rotation(self: &Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if let Err(error) = me.rotate_once().await {
                    tracing::warn!(error = %error, "credential rotation failed");
                }
            }
        })
    }
}

#[async_trait]
impl<S: ConfigStore + 'static> ConfigStore for RotatingApiKeyStore<S> {
    async fn get(&self, namespace: &str, id: &str) -> Result<Option<Value>, StorageError> {
        self.inner.get(namespace, id).await
    }

    async fn list(
        &self,
        namespace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, Value)>, StorageError> {
        self.inner.list(namespace, offset, limit).await
    }

    async fn put(&self, namespace: &str, id: &str, value: &Value) -> Result<(), StorageError> {
        self.inner.put(namespace, id, value).await
    }

    async fn delete(&self, namespace: &str, id: &str) -> Result<(), StorageError> {
        self.inner.delete(namespace, id).await
    }
}

#[async_trait]
impl<S: ConfigStore + 'static> ConfigChangeNotifier for RotatingApiKeyStore<S> {
    async fn subscribe(&self) -> Result<Box<dyn ConfigChangeSubscriber>, StorageError> {
        Ok(Box::new(BroadcastSubscriber {
            receiver: self.notifier.subscribe(),
        }))
    }
}

struct BroadcastSubscriber {
    receiver: broadcast::Receiver<ConfigChangeEvent>,
}

#[async_trait]
impl ConfigChangeSubscriber for BroadcastSubscriber {
    async fn next(&mut self) -> Result<ConfigChangeEvent, StorageError> {
        self.receiver
            .recv()
            .await
            .map_err(|error| StorageError::Io(format!("rotation broadcast: {error}")))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Seed the inner store with an initial provider entry.
    let inner = Arc::new(InMemoryStore::new());
    inner
        .put(
            "providers",
            "openai",
            &json!({
                "id": "openai",
                "adapter": "openai",
                "api_key": "sk-initial-12345",
            }),
        )
        .await?;

    let source = Arc::new(CountingTokenSource {
        counter: AtomicU64::new(0),
    });
    let rotating = Arc::new(RotatingApiKeyStore::new(
        Arc::clone(&inner),
        "providers",
        "openai",
        source,
    ));

    let mut subscriber = rotating.subscribe().await?;

    // Manual one-shot rotation (e.g. on app startup).
    rotating.rotate_once().await?;
    let event = subscriber.next().await?;
    println!("manual rotation emitted: {event:?}");

    // Background rotation on a fixed interval. In production this would
    // be every few minutes; here we use a short interval so the example
    // exits quickly.
    let handle = rotating.start_rotation(Duration::from_millis(100));
    tokio::time::sleep(Duration::from_millis(250)).await;
    handle.abort();

    let after = rotating
        .get("providers", "openai")
        .await?
        .expect("entry should be present after rotation");
    println!("provider after several rotations: {after}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rotate_once_replaces_api_key_and_emits_event() {
        let inner = Arc::new(InMemoryStore::new());
        inner
            .put(
                "providers",
                "openai",
                &json!({
                    "id": "openai",
                    "adapter": "openai",
                    "api_key": "sk-initial",
                    "base_url": "https://example.com/v1",
                }),
            )
            .await
            .unwrap();

        let rotating = Arc::new(RotatingApiKeyStore::new(
            Arc::clone(&inner),
            "providers",
            "openai",
            Arc::new(CountingTokenSource {
                counter: AtomicU64::new(7),
            }),
        ));

        let mut subscriber = rotating.subscribe().await.unwrap();
        rotating.rotate_once().await.unwrap();

        let event = subscriber.next().await.unwrap();
        assert_eq!(event.namespace, "providers");
        assert_eq!(event.id, "openai");
        assert_eq!(event.kind, ConfigChangeKind::Put);

        let stored = rotating.get("providers", "openai").await.unwrap().unwrap();
        let api_key = stored
            .get("api_key")
            .and_then(Value::as_str)
            .expect("api_key should be present");
        assert_eq!(api_key, "sk-rotated-00000007");
        // Non-secret fields must survive rotation untouched.
        assert_eq!(
            stored.get("base_url").and_then(Value::as_str),
            Some("https://example.com/v1")
        );
    }

    #[tokio::test]
    async fn rotate_once_creates_entry_when_missing() {
        let inner = Arc::new(InMemoryStore::new());
        let rotating = Arc::new(RotatingApiKeyStore::new(
            Arc::clone(&inner),
            "providers",
            "fresh",
            Arc::new(CountingTokenSource {
                counter: AtomicU64::new(0),
            }),
        ));

        rotating.rotate_once().await.unwrap();

        let stored = rotating.get("providers", "fresh").await.unwrap().unwrap();
        assert_eq!(
            stored.get("api_key").and_then(Value::as_str),
            Some("sk-rotated-00000000")
        );
    }
}
