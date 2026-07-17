//! In-memory reference [`EnvRegistry`] backend.
//!
//! [`InMemoryEnvRegistry`] is the open-tier single-process default the environments
//! routes wire when no durable backend is configured; it lives here beside the durable
//! sqlite/postgres sibling. The neutral port + value objects (`EnvItem`/`EnvUpdate`)
//! it operates on live inward in `awaken-session-contract`.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_session_contract::env_registry::{EnvItem, EnvRegistry, EnvUpdate, OBJECT_AT};
use serde_json::Value;

pub struct InMemoryEnvRegistry {
    envs: Mutex<BTreeMap<String, EnvItem>>,
    seq: AtomicU64,
}

impl Default for InMemoryEnvRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryEnvRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            envs: Mutex::new(BTreeMap::new()),
            seq: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl EnvRegistry for InMemoryEnvRegistry {
    async fn create(
        &self,
        name: String,
        description: String,
        metadata: BTreeMap<String, String>,
        config: Value,
    ) -> EnvItem {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let id = format!("env_{n:016}");
        let item = EnvItem {
            id: id.clone(),
            name,
            description,
            metadata,
            config,
            archived_at: None,
        };
        self.envs.lock().unwrap().insert(id, item.clone());
        item
    }

    async fn list_active(&self) -> Vec<EnvItem> {
        self.envs
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.archived_at.is_none())
            .cloned()
            .collect()
    }

    async fn get(&self, id: &str) -> Option<EnvItem> {
        self.envs.lock().unwrap().get(id).cloned()
    }

    async fn exists(&self, id: &str) -> bool {
        self.envs.lock().unwrap().contains_key(id)
    }

    async fn update(&self, id: &str, patch: EnvUpdate) -> Option<EnvItem> {
        let mut envs = self.envs.lock().unwrap();
        let item = envs.get_mut(id)?;
        item.apply(patch);
        Some(item.clone())
    }

    async fn delete(&self, id: &str) -> bool {
        self.envs.lock().unwrap().remove(id).is_some()
    }

    async fn archive(&self, id: &str) -> Option<EnvItem> {
        let mut envs = self.envs.lock().unwrap();
        let item = envs.get_mut(id)?;
        item.archived_at = Some(OBJECT_AT.to_string());
        Some(item.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r() -> InMemoryEnvRegistry {
        InMemoryEnvRegistry::new()
    }

    #[tokio::test]
    async fn create_get_list_and_archive() {
        let r = r();
        let e = r
            .create(
                "prod".into(),
                String::new(),
                BTreeMap::new(),
                json!({"type":"self_hosted"}),
            )
            .await;
        assert!(r.exists(&e.id).await);
        assert!(e.is_self_hosted());
        assert_eq!(r.list_active().await.len(), 1);
        r.archive(&e.id).await.expect("archive");
        assert_eq!(r.list_active().await.len(), 0, "archived drops from active");
        assert!(r.get(&e.id).await.is_some(), "still retrievable");
    }

    /// Cause-effect on the archive-vs-delete + existence axes: archive is SOFT (the
    /// record stays retrievable by `get`, only leaves `list_active`) while delete is
    /// HARD (`get` returns `None` after); and archiving a non-existent id fails closed
    /// with `None` rather than fabricating a record. Pins the two distinctions a
    /// refactor of the registry could blur.
    #[tokio::test]
    async fn archive_is_soft_delete_is_hard_and_missing_id_fails_closed() {
        let r = r();
        // C: id does not exist -> archive returns None (fail-closed, no fabrication).
        assert!(
            r.archive("env_missing").await.is_none(),
            "archive of missing id"
        );
        let e = r
            .create("prod".into(), String::new(), BTreeMap::new(), json!({}))
            .await;
        // Archive is soft: get still returns the (now archived) record.
        assert!(r.archive(&e.id).await.is_some());
        assert!(r.get(&e.id).await.is_some(), "archive keeps the record");
        // Delete is hard: get returns None afterwards.
        assert!(r.delete(&e.id).await, "delete reports it existed");
        assert!(r.get(&e.id).await.is_none(), "delete removes the record");
    }

    #[tokio::test]
    async fn update_patches_fields_and_metadata_null_deletes() {
        let r = r();
        let e = r
            .create(
                "e".into(),
                String::new(),
                BTreeMap::from([("keep".into(), "1".into()), ("drop".into(), "2".into())]),
                json!({}),
            )
            .await;
        let up = r
            .update(
                &e.id,
                EnvUpdate {
                    name: Some("renamed".into()),
                    metadata: Some(BTreeMap::from([("drop".into(), None)])),
                    ..Default::default()
                },
            )
            .await
            .expect("updated");
        assert_eq!(up.name, "renamed");
        assert!(up.metadata.contains_key("keep"));
        assert!(!up.metadata.contains_key("drop"), "null deletes the key");
    }

    // (`network_policy`/`project` are wire/provisioning projections; they moved to the
    // Managed adapter along with their tests. This crate keeps only the neutral registry.)

    #[tokio::test]
    async fn create_delete_is_idempotent() {
        let r = r();
        let closed = r
            .create(
                "c".into(),
                String::new(),
                BTreeMap::new(),
                json!({"networking":{"type":"none"}}),
            )
            .await;
        assert!(r.exists(&closed.id).await);
        assert!(r.delete(&closed.id).await);
        assert!(!r.delete(&closed.id).await, "second delete is false");
    }
}
