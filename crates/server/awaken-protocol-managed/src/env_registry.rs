//! The self-hosted **environment registry** as a port, the sibling of
//! [`crate::work_queue::WorkQueue`]. An environment is user-created config (name,
//! description, metadata, networking policy) — not re-derivable — so a durable
//! backend must persist it for the work queue to stay usable across a restart or
//! on another node. The default [`InMemoryEnvRegistry`] preserves the exact
//! single-process behavior the routes had inline.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_provisioning_contract::NetworkPolicy;
use serde_json::Value;

use crate::types::environment::Environment;

/// The frozen presence timestamp the managed wire uses (parity with the queue).
const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

/// One environment record (the domain shape; [`EnvItem::project`] renders the
/// `BetaEnvironment` wire object).
#[derive(Clone, Debug)]
pub struct EnvItem {
    pub id: String,
    pub name: String,
    pub description: String,
    pub metadata: BTreeMap<String, String>,
    /// `BetaCloudConfig | BetaSelfHostedConfig`.
    pub config: Value,
    pub archived_at: Option<String>,
}

impl EnvItem {
    /// Project to the official `BetaEnvironment` shape. No `scope` on the wire:
    /// ownership is credential-implicit (authz enforces the workspace).
    #[must_use]
    pub fn project(&self) -> Environment {
        Environment {
            id: self.id.clone(),
            object_type: "environment",
            archived_at: self.archived_at.clone(),
            created_at: OBJECT_AT.to_string(),
            updated_at: OBJECT_AT.to_string(),
            name: self.name.clone(),
            description: self.description.clone(),
            metadata: self.metadata.clone(),
            config: self.config.clone(),
        }
    }

    /// Map the `networking` wire config onto the neutral [`NetworkPolicy`] the
    /// sandbox understands: `unrestricted → Unrestricted`, `limited{hosts} →
    /// Allowlist`, `none → None`. Absent networking (incl. `self_hosted`) or an
    /// unknown type shares the host network.
    #[must_use]
    pub fn network_policy(&self) -> NetworkPolicy {
        let Some(net) = self.config.get("networking") else {
            return NetworkPolicy::Unrestricted;
        };
        match net.get("type").and_then(Value::as_str) {
            Some("none") => NetworkPolicy::None,
            Some("limited") => {
                let hosts = net
                    .get("allowed_hosts")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|h| h.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                NetworkPolicy::Allowlist { hosts }
            }
            _ => NetworkPolicy::Unrestricted,
        }
    }

    /// Whether this is a self-hosted environment (`config.type == self_hosted`).
    #[must_use]
    pub fn is_self_hosted(&self) -> bool {
        self.config.get("type").and_then(Value::as_str) == Some("self_hosted")
    }
}

/// A metadata/config update patch: present fields replace; a `metadata` key mapped
/// to `null` deletes it.
#[derive(Debug, Default)]
pub struct EnvUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    pub config: Option<Value>,
    pub metadata: Option<BTreeMap<String, Option<String>>>,
}

/// The port the environment routes drive. In-memory by default; a durable impl
/// (sqlite / postgres) backs it at parity.
#[async_trait]
pub trait EnvRegistry: Send + Sync {
    /// Create an environment; mints and returns the record with its new id.
    async fn create(
        &self,
        name: String,
        description: String,
        metadata: BTreeMap<String, String>,
        config: Value,
    ) -> EnvItem;
    /// All non-archived environments, ascending by id.
    async fn list_active(&self) -> Vec<EnvItem>;
    /// The environment under `id` (archived or not).
    async fn get(&self, id: &str) -> Option<EnvItem>;
    /// Whether `id` exists (archived or not).
    async fn exists(&self, id: &str) -> bool;
    /// Apply an update patch; `None` when `id` does not exist.
    async fn update(&self, id: &str, patch: EnvUpdate) -> Option<EnvItem>;
    /// Delete `id`; returns whether it existed.
    async fn delete(&self, id: &str) -> bool;
    /// Archive `id` (stamps `archived_at`); `None` when it does not exist.
    async fn archive(&self, id: &str) -> Option<EnvItem>;
}

/// The default single-process environment registry.
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
        if let Some(name) = patch.name {
            item.name = name;
        }
        if let Some(description) = patch.description {
            item.description = description;
        }
        if let Some(config) = patch.config.filter(|v| !v.is_null()) {
            item.config = config;
        }
        if let Some(md) = patch.metadata {
            for (k, v) in md {
                match v {
                    Some(s) => {
                        item.metadata.insert(k, s);
                    }
                    None => {
                        item.metadata.remove(&k);
                    }
                }
            }
        }
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

    fn env_with(config: Value) -> EnvItem {
        EnvItem {
            id: "e".into(),
            name: "e".into(),
            description: String::new(),
            metadata: BTreeMap::new(),
            config,
            archived_at: None,
        }
    }

    #[test]
    fn network_policy_maps_all_wire_types() {
        // unrestricted → shares host network
        let open = env_with(json!({ "networking": { "type": "unrestricted" } }));
        assert_eq!(open.network_policy(), NetworkPolicy::Unrestricted);
        assert!(!open.network_policy().is_restricted());
        // limited{allowed_hosts} → typed Allowlist, denies under bwrap (fail-closed)
        let limited = env_with(json!({
            "networking": { "type": "limited", "allowed_hosts": ["api.anthropic.com"] }
        }));
        assert_eq!(
            limited.network_policy(),
            NetworkPolicy::Allowlist {
                hosts: vec!["api.anthropic.com".to_string()],
            }
        );
        assert!(limited.network_policy().is_restricted());
        // none → no egress
        let none = env_with(json!({ "networking": { "type": "none" } }));
        assert_eq!(none.network_policy(), NetworkPolicy::None);
        assert!(none.network_policy().is_restricted());
        // absent networking / self_hosted / unknown → Unrestricted (shares host)
        let self_hosted = env_with(json!({ "type": "self_hosted" }));
        assert_eq!(self_hosted.network_policy(), NetworkPolicy::Unrestricted);
        assert!(!self_hosted.network_policy().is_restricted());
    }

    #[tokio::test]
    async fn network_policy_and_delete() {
        let r = r();
        let closed = r
            .create(
                "c".into(),
                String::new(),
                BTreeMap::new(),
                json!({"networking":{"type":"none"}}),
            )
            .await;
        assert!(closed.network_policy().is_restricted());
        assert!(r.delete(&closed.id).await);
        assert!(!r.delete(&closed.id).await, "second delete is false");
    }
}
