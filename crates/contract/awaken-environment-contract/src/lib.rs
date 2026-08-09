//! Control-owned Environment definitions and their persistence port.
//!
//! An Environment is immutable-by-revision configuration: identity, metadata,
//! packages, networking, and an exact sandbox-policy reference. Coordinator
//! receives an executable projection through a registration port; it never opens
//! this registry. The in-memory and durable adapters live in `awaken-env-store`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

/// Stable Control command for creating one Environment. The command id is
/// supplied by the ingress boundary; the fingerprint is derived from every
/// business input so a replay can be distinguished from conflicting reuse.
#[derive(Clone, Debug)]
pub struct CreateEnvironmentCommand {
    pub command_id: String,
    pub name: String,
    pub description: String,
    pub metadata: BTreeMap<String, String>,
    pub scope: Option<String>,
    pub config: EnvironmentConfig,
}

impl CreateEnvironmentCommand {
    #[must_use]
    pub fn fingerprint(&self) -> String {
        environment_facts_fingerprint(&(
            &self.name,
            &self.description,
            &self.metadata,
            &self.scope,
            &self.config,
        ))
    }
}

/// Deterministic equality evidence for facts inside the Environment context.
/// This is intentionally the canonical serialized fact set, not a security
/// digest; callers use it only for replay/conflict and corruption detection.
#[must_use]
pub fn environment_facts_fingerprint(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("Environment fingerprint facts serialize")
}

#[derive(Clone, Debug)]
pub enum CreateEnvironmentOutcome {
    Created(EnvItem),
    Replayed(EnvItem),
}

impl CreateEnvironmentOutcome {
    #[must_use]
    pub fn item(&self) -> &EnvItem {
        match self {
            Self::Created(item) | Self::Replayed(item) => item,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CreateEnvironmentError {
    #[error("Environment command id was reused with different input")]
    IdempotencyConflict,
    #[error("Environment store failed: {0}")]
    Store(String),
}

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

/// Canonical static Environment configuration owned by Control. Protocol
/// adapters translate their wire unions into this closed vocabulary once; stores
/// and executable-projection compilation never inspect arbitrary JSON.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentConfig {
    Cloud {
        #[serde(default)]
        networking: EnvironmentNetworking,
        #[serde(default)]
        packages: EnvironmentPackages,
    },
    #[default]
    SelfHosted,
}

impl EnvironmentConfig {
    #[must_use]
    pub fn is_self_hosted(&self) -> bool {
        matches!(self, Self::SelfHosted)
    }

    #[must_use]
    pub fn packages(&self) -> EnvironmentPackages {
        match self {
            Self::Cloud { packages, .. } => packages.clone(),
            Self::SelfHosted => EnvironmentPackages::default(),
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

/// Canonical public registries represented by Anthropic's
/// `allow_package_managers` switch. Keeping this catalog at the static
/// Environment owner prevents projection compilers and sandbox providers from
/// growing competing interpretations.
pub const PUBLIC_PACKAGE_REGISTRY_HOSTS: &[&str] = &[
    "archive.ubuntu.com",
    "crates.io",
    "deb.debian.org",
    "files.pythonhosted.org",
    "index.crates.io",
    "proxy.golang.org",
    "pypi.org",
    "registry.npmjs.org",
    "rubygems.org",
    "security.debian.org",
    "security.ubuntu.com",
    "static.crates.io",
    "sum.golang.org",
];

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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnvItem {
    pub id: String,
    pub revision: EnvironmentRevision,
    pub name: String,
    pub description: String,
    pub metadata: BTreeMap<String, String>,
    /// Anthropic visibility scope (`organization` or `account`).
    pub scope: Option<String>,
    pub config: EnvironmentConfig,
    /// Exact Control-owned sandbox policy version selected by this immutable
    /// Environment revision. The policy body is resolved only while publishing
    /// the executable projection; Coordinator never opens the policy store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<EnvironmentSandboxPolicyRef>,
    pub archived_at: Option<String>,
}

/// Neutral identity of one immutable sandbox-policy version. This value lives
/// in the Environment aggregate so the binding and revision advance atomically;
/// the Worker provisioning contract owns the policy body and validation rules.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentSandboxPolicyRef {
    pub policy_id: String,
    pub version: u64,
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
            self.config.apply(config);
        }
        if let Some(scope) = patch.scope {
            self.scope = scope;
        }
        if let Some(sandbox_policy) = patch.sandbox_policy {
            self.sandbox_policy = sandbox_policy;
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
    pub config: Option<EnvironmentConfigMutation>,
    /// Outer `Some` means the field was supplied; inner `None` clears it.
    pub scope: Option<Option<String>>,
    pub metadata: Option<BTreeMap<String, Option<String>>>,
    /// Outer `Some` means the binding was supplied; inner `None` clears it.
    pub sandbox_policy: Option<Option<EnvironmentSandboxPolicyRef>>,
}

/// Atomic mutation vocabulary for the official Environment update semantics.
/// It is deliberately owned beside `EnvironmentConfig` so every store applies
/// omitted-field preservation identically under its existing update lock.
#[derive(Debug)]
pub enum EnvironmentConfigMutation {
    Replace(EnvironmentConfig),
    PatchCloud {
        networking: Option<EnvironmentNetworkingMutation>,
        packages: Option<EnvironmentPackagesMutation>,
    },
}

#[derive(Debug)]
pub enum EnvironmentNetworkingMutation {
    Reset,
    Unrestricted,
    Limited {
        allowed_hosts: Option<Option<Vec<String>>>,
        allow_mcp_servers: Option<Option<bool>>,
        allow_package_managers: Option<Option<bool>>,
    },
}

#[derive(Debug, Default)]
pub struct EnvironmentPackagesMutation {
    pub reset: bool,
    pub apt: Option<Option<Vec<String>>>,
    pub cargo: Option<Option<Vec<String>>>,
    pub gem: Option<Option<Vec<String>>>,
    pub go: Option<Option<Vec<String>>>,
    pub npm: Option<Option<Vec<String>>>,
    pub pip: Option<Option<Vec<String>>>,
}

impl EnvironmentConfig {
    fn apply(&mut self, mutation: EnvironmentConfigMutation) {
        match mutation {
            EnvironmentConfigMutation::Replace(config) => *self = config,
            EnvironmentConfigMutation::PatchCloud {
                networking,
                packages,
            } => {
                if !matches!(self, Self::Cloud { .. }) {
                    *self = Self::Cloud {
                        networking: EnvironmentNetworking::default(),
                        packages: EnvironmentPackages::default(),
                    };
                }
                let Self::Cloud {
                    networking: current_networking,
                    packages: current_packages,
                } = self
                else {
                    unreachable!("Cloud initialized above")
                };
                if let Some(mutation) = networking {
                    current_networking.apply(mutation);
                }
                if let Some(mutation) = packages {
                    current_packages.apply(mutation);
                }
            }
        }
    }
}

impl EnvironmentNetworking {
    fn apply(&mut self, mutation: EnvironmentNetworkingMutation) {
        match mutation {
            EnvironmentNetworkingMutation::Reset | EnvironmentNetworkingMutation::Unrestricted => {
                *self = Self::Unrestricted
            }
            EnvironmentNetworkingMutation::Limited {
                allowed_hosts,
                allow_mcp_servers,
                allow_package_managers,
            } => {
                if !matches!(self, Self::Limited { .. }) {
                    *self = Self::Limited {
                        allowed_hosts: Vec::new(),
                        allow_mcp_servers: false,
                        allow_package_managers: false,
                    };
                }
                let Self::Limited {
                    allowed_hosts: current_hosts,
                    allow_mcp_servers: current_mcp,
                    allow_package_managers: current_packages,
                } = self
                else {
                    unreachable!("Limited initialized above")
                };
                if let Some(value) = allowed_hosts {
                    *current_hosts = value.unwrap_or_default();
                }
                if let Some(value) = allow_mcp_servers {
                    *current_mcp = value.unwrap_or(false);
                }
                if let Some(value) = allow_package_managers {
                    *current_packages = value.unwrap_or(false);
                }
            }
        }
    }
}

impl EnvironmentPackages {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.apt.is_empty()
            && self.cargo.is_empty()
            && self.gem.is_empty()
            && self.go.is_empty()
            && self.npm.is_empty()
            && self.pip.is_empty()
    }

    /// Canonical package-manager ordering and values. Consumers project this
    /// definition vocabulary into their own execution contracts without
    /// repeating the closed manager catalog.
    #[must_use]
    pub fn manager_packages(&self) -> [(&'static str, &[String]); 6] {
        [
            ("apt", &self.apt),
            ("cargo", &self.cargo),
            ("gem", &self.gem),
            ("go", &self.go),
            ("npm", &self.npm),
            ("pip", &self.pip),
        ]
    }

    fn apply(&mut self, mutation: EnvironmentPackagesMutation) {
        if mutation.reset {
            *self = Self::default();
            return;
        }
        macro_rules! apply_field {
            ($field:ident) => {
                if let Some(value) = mutation.$field {
                    self.$field = value.unwrap_or_default();
                }
            };
        }
        apply_field!(apt);
        apply_field!(cargo);
        apply_field!(gem);
        apply_field!(go);
        apply_field!(npm);
        apply_field!(pip);
    }
}

/// The port the environment routes drive. In-memory by default; a durable impl
/// (sqlite / postgres) backs it at parity.
#[async_trait]
pub trait EnvRegistry: Send + Sync {
    /// Atomically persist or replay one create command. A command id can name
    /// exactly one fingerprint for the lifetime of the authority store.
    async fn create_once(
        &self,
        command: CreateEnvironmentCommand,
    ) -> Result<CreateEnvironmentOutcome, CreateEnvironmentError>;
    /// Test/embedding convenience for callers that intentionally request a new
    /// identity on every invocation. Production ingress must use `create_once`.
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
    /// Test/embedding convenience counterpart of [`Self::create`].
    async fn create_scoped(
        &self,
        name: String,
        description: String,
        metadata: BTreeMap<String, String>,
        scope: Option<String>,
        config: EnvironmentConfig,
    ) -> EnvItem {
        static NEXT_COMMAND: AtomicU64 = AtomicU64::new(0);
        self.create_once(CreateEnvironmentCommand {
            command_id: format!(
                "unkeyed:{}:{}",
                std::process::id(),
                NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
            ),
            name,
            description,
            metadata,
            scope,
            config,
        })
        .await
        .expect("unkeyed Environment create")
        .item()
        .clone()
    }
    /// All non-archived environments, ascending by id.
    async fn list_active(&self) -> Vec<EnvItem>;
    /// Every current Environment projection, including archived definitions.
    /// Reconciliation uses this to replay withdrawals after a failed publish.
    async fn list_all(&self) -> Vec<EnvItem>;
    /// The environment under `id` (archived or not).
    async fn get(&self, id: &str) -> Option<EnvItem>;
    /// One immutable authored revision. Implementations append this record in
    /// the same transaction that advances the current row; they never reconstruct
    /// history from the mutable current projection.
    async fn get_revision(&self, id: &str, revision: EnvironmentRevision) -> Option<EnvItem>;
    /// Whether `id` exists (archived or not).
    async fn exists(&self, id: &str) -> bool;
    /// Apply an update patch; `None` when `id` does not exist.
    async fn update(&self, id: &str, patch: EnvUpdate) -> Option<EnvItem>;
    /// Archive `id` (stamps `archived_at`); `None` when it does not exist.
    async fn archive(&self, id: &str) -> Option<EnvItem>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_default_is_the_self_hosted_decision() {
        assert_eq!(EnvironmentConfig::default(), EnvironmentConfig::SelfHosted);
    }

    #[test]
    fn environment_facts_fingerprint_is_stable_and_sensitive() {
        // Cause/effect decision table: R1 equal ordered facts -> equal replay
        // evidence; R2 one changed fact -> different evidence. The value is not
        // used as a secret, authorization proof, or content-addressed identity.
        assert_eq!(
            environment_facts_fingerprint(&(1_u64, "same")),
            environment_facts_fingerprint(&(1_u64, "same")),
            "R1"
        );
        assert_ne!(
            environment_facts_fingerprint(&(1_u64, "same")),
            environment_facts_fingerprint(&(2_u64, "same")),
            "R2"
        );
    }
}
