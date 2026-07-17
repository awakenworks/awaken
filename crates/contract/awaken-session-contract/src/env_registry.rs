//! The self-hosted **environment registry** as a port, the sibling of
//! [`crate::work_queue::WorkQueue`]. An environment is user-created config (name,
//! description, metadata, networking policy) — not re-derivable — so a durable
//! backend must persist it for the work queue to stay usable across a restart or
//! on another node. The in-memory reference backend lives outward in
//! `awaken-env-store`, beside the durable sqlite/postgres sibling.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;

/// The frozen presence timestamp stamped on a record (parity with the work queue).
/// The Managed wire adapter reuses it in its `BetaEnvironment` projection.
pub const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

/// One environment record (the neutral domain shape). The Managed wire adapter
/// renders the `BetaEnvironment` object and derives the sandbox `NetworkPolicy` from
/// `config` — this crate names neither the wire nor the provisioning vocabulary.
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
    /// Whether this is a self-hosted environment (`config.type == self_hosted`).
    #[must_use]
    pub fn is_self_hosted(&self) -> bool {
        self.config.get("type").and_then(Value::as_str) == Some("self_hosted")
    }

    /// Apply an [`EnvUpdate`] in place: present fields replace, a `metadata` key
    /// mapped to `null` deletes it. One patch definition shared by every backend
    /// (in-memory, sqlite, postgres) so the merge semantics can never drift.
    pub fn apply(&mut self, patch: EnvUpdate) {
        if let Some(name) = patch.name {
            self.name = name;
        }
        if let Some(description) = patch.description {
            self.description = description;
        }
        if let Some(config) = patch.config.filter(|v| !v.is_null()) {
            self.config = config;
        }
        if let Some(md) = patch.metadata {
            for (k, v) in md {
                match v {
                    Some(s) => {
                        self.metadata.insert(k, s);
                    }
                    None => {
                        self.metadata.remove(&k);
                    }
                }
            }
        }
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
