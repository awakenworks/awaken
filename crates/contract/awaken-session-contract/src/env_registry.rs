//! The self-hosted **environment registry** as a port, the sibling of
//! [`crate::work_queue::WorkQueue`]. An environment is user-created config (name,
//! description, metadata, networking policy) — not re-derivable — so a durable
//! backend must persist it for the work queue to stay usable across a restart or
//! on another node. The in-memory reference backend lives outward in
//! `awaken-env-store`, beside the durable sqlite/postgres sibling.

use std::collections::BTreeMap;

use crate::SessionNetworkPolicy;
use async_trait::async_trait;

/// The frozen presence timestamp stamped on a record (parity with the work queue).
/// The Managed wire adapter reuses it in its `BetaEnvironment` projection.
pub const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

/// Monotonic authored Environment revision. A Session freezes this value with
/// the normalized snapshot so later registry edits cannot change its meaning.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct EnvironmentRevision(pub u64);

/// Canonical Environment configuration owned by the Session domain. Protocol
/// adapters translate their wire unions into this closed vocabulary once; stores
/// and snapshot compilation never inspect arbitrary JSON.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentConfig {
    Cloud {
        #[serde(default)]
        networking: EnvironmentNetworking,
        #[serde(default)]
        packages: EnvironmentPackages,
    },
    SelfHosted,
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self::SelfHosted
    }
}

impl EnvironmentConfig {
    #[must_use]
    pub fn is_self_hosted(&self) -> bool {
        matches!(self, Self::SelfHosted)
    }

    #[must_use]
    pub fn network_policy(&self) -> SessionNetworkPolicy {
        match self {
            Self::Cloud { networking, .. } => networking.network_policy(),
            Self::SelfHosted => SessionNetworkPolicy::Unrestricted,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentNetworking {
    #[default]
    Unrestricted,
    Limited {
        #[serde(default)]
        allowed_hosts: Vec<String>,
        #[serde(default)]
        allow_mcp_servers: bool,
        #[serde(default)]
        allow_package_managers: bool,
    },
}

impl EnvironmentNetworking {
    #[must_use]
    pub fn network_policy(&self) -> SessionNetworkPolicy {
        match self {
            Self::Unrestricted => SessionNetworkPolicy::Unrestricted,
            Self::Limited { allowed_hosts, .. } => SessionNetworkPolicy::Allowlist {
                hosts: allowed_hosts.clone(),
            }
            .normalized(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentPackages {
    #[serde(rename = "type", default)]
    pub kind: EnvironmentPackagesKind,
    #[serde(default)]
    pub apt: Vec<String>,
    #[serde(default)]
    pub cargo: Vec<String>,
    #[serde(default)]
    pub gem: Vec<String>,
    #[serde(default)]
    pub go: Vec<String>,
    #[serde(default)]
    pub npm: Vec<String>,
    #[serde(default)]
    pub pip: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentPackagesKind {
    #[default]
    Packages,
}

impl Default for EnvironmentPackages {
    fn default() -> Self {
        Self {
            kind: EnvironmentPackagesKind::Packages,
            apt: Vec::new(),
            cargo: Vec::new(),
            gem: Vec::new(),
            go: Vec::new(),
            npm: Vec::new(),
            pip: Vec::new(),
        }
    }
}

/// One environment record (the neutral domain shape). The Managed wire adapter
/// renders the `BetaEnvironment` object and derives the sandbox `NetworkPolicy` from
/// `config` — this crate names neither the wire nor the provisioning vocabulary.
#[derive(Clone, Debug)]
pub struct EnvItem {
    pub id: String,
    pub revision: EnvironmentRevision,
    pub name: String,
    pub description: String,
    pub metadata: BTreeMap<String, String>,
    /// Anthropic visibility scope (`organization` or `account`).
    pub scope: Option<String>,
    pub config: EnvironmentConfig,
    pub archived_at: Option<String>,
}

impl EnvItem {
    /// Whether this is a self-hosted environment (`config.type == self_hosted`).
    #[must_use]
    pub fn is_self_hosted(&self) -> bool {
        self.config.is_self_hosted()
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
        if let Some(config) = patch.config {
            self.config = config;
        }
        if let Some(scope) = patch.scope {
            self.scope = scope;
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
        self.revision = EnvironmentRevision(
            self.revision
                .0
                .checked_add(1)
                .expect("Environment revision exhausted"),
        );
    }
}

/// A metadata/config update patch: present fields replace; a `metadata` key mapped
/// to `null` deletes it.
#[derive(Debug, Default)]
pub struct EnvUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    pub config: Option<EnvironmentConfig>,
    /// Outer `Some` means the field was supplied; inner `None` clears it.
    pub scope: Option<Option<String>>,
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
        config: EnvironmentConfig,
    ) -> EnvItem {
        self.create_scoped(name, description, metadata, None, config)
            .await
    }

    async fn create_scoped(
        &self,
        name: String,
        description: String,
        metadata: BTreeMap<String, String>,
        scope: Option<String>,
        config: EnvironmentConfig,
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
