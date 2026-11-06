// @formal entrypoint environment.registration_intent_is_atomic_recoverable_and_idempotent
// @formal entrypoint environment.prepared_packages_do_not_grant_runtime_registry_access
//! Control-owned Environment definitions and their persistence port.
//!
//! An Environment is immutable-by-revision configuration: identity, metadata,
//! packages, networking, and an exact sandbox-policy reference. Coordinator
//! receives an executable projection through a registration port; it never opens
//! this registry. The in-memory and durable adapters live in `awaken-env-store`.
//!
//! # Boundary
//!
//! Context: `environment`.
//!
//! Implemented by: `awaken-admin-assistant`, `awaken-env-store`, `awaken-environment-application`.
//!
//! Called by: `awaken-admin-assistant`, `awaken-container-contract`, `awaken-control`,
//! `awaken-coordinator`, `awaken-env-store`, `awaken-environment-application`,
//! `awaken-environment-package-image-builder`, `awaken-executable-environment-catalog`,
//! `awaken-executable-environment-contract`, `awaken-package-image`, `awaken-process-startup`,
//! `awaken-protocol-awaken`, `awaken-protocol-managed`, `awaken-protocol-managed-testkit`,
//! `awaken-provisioning-contract`, `awaken-sandbox-container`, `awaken-session-application`,
//! `awaken-session-configuration-contract`, `awaken-store-conformance`, `awaken-work-store`.

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
    pub description: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub scope: Option<String>,
    pub config: EnvironmentConfig,
    /// Optional exact sandbox-policy version selected atomically with this
    /// Environment revision. Protocols that do not expose Awaken extensions
    /// leave it absent and retain their existing wire contract.
    pub sandbox_policy: Option<EnvironmentSandboxPolicyRef>,
}

/// Control command port for authoring canonical Environment definitions.
///
/// Ingress adapters construct this context-owned command directly. The Control
/// application implements the port, so callers cannot introduce a parallel
/// Environment draft model or a second translation layer.
#[async_trait]
pub trait EnvironmentAuthor: Send + Sync {
    async fn create_environment(&self, command: CreateEnvironmentCommand)
    -> Result<String, String>;
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
            &self.sandbox_policy,
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

    #[must_use]
    pub fn into_item(self) -> EnvItem {
        match self {
            Self::Created(item) | Self::Replayed(item) => item,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CreateEnvironmentError {
    #[error("Environment command id was reused with different input")]
    IdempotencyConflict,
    /// Legacy authorities can retain a command identity without its original
    /// revision. Never replace that missing receipt with the mutable head.
    #[error(
        "Environment {environment_id} was created, but its original creation result is unavailable; retrieve the current definition"
    )]
    OriginalResultUnavailable { environment_id: String },
    #[error(transparent)]
    InvalidConfig(#[from] InvalidEnvironmentConfig),
    #[error("Environment store failed: {0}")]
    Store(String),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EnvironmentStoreError {
    #[error(transparent)]
    InvalidConfig(#[from] InvalidEnvironmentConfig),
    #[error("Environment revision changed before the update was committed")]
    RevisionConflict,
    #[error("Environment revision exhausted")]
    RevisionExhausted,
    #[error("Environment store failed: {0}")]
    Backend(String),
}

/// A cross-field Environment rule that every ingress and persistence backend
/// enforces. Keeping the invariant in the domain contract gives protocol,
/// in-memory, SQLite, and PostgreSQL paths one authoritative interpretation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidEnvironmentConfig {
    #[error("invalid {manager} package requirement")]
    InvalidPackageRequirement { manager: &'static str },
}

/// A rejected mutation leaves the Environment and its revision unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EnvironmentMutationError {
    #[error(transparent)]
    InvalidConfig(#[from] InvalidEnvironmentConfig),
    #[error("Environment revision exhausted")]
    RevisionExhausted,
}

impl From<EnvironmentMutationError> for EnvironmentStoreError {
    fn from(error: EnvironmentMutationError) -> Self {
        match error {
            EnvironmentMutationError::InvalidConfig(error) => Self::InvalidConfig(error),
            EnvironmentMutationError::RevisionExhausted => Self::RevisionExhausted,
        }
    }
}

/// Installation-global identity of the immutable built-in local Environment.
/// Every ingress, Session default, and executable projection must reuse this
/// value instead of maintaining a parallel literal convention.
pub const BUILTIN_LOCAL_ENVIRONMENT_ID: &str = "env_local";

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

#[derive(Clone, Copy)]
enum EnvironmentNetworkStage {
    Runtime,
    PackageBuild,
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

    /// Package preparation precedes execution. Runtime registry permission
    /// cannot invalidate an explicitly declared build input; malformed operands
    /// still fail at each ingress and persistence boundary.
    pub fn validate(&self) -> Result<(), InvalidEnvironmentConfig> {
        for (manager, requirements) in self.packages().manager_packages() {
            if requirements
                .iter()
                .any(|requirement| !PackageRequirements::requirement_is_valid(requirement))
            {
                return Err(InvalidEnvironmentConfig::InvalidPackageRequirement { manager });
            }
        }
        Ok(())
    }

    /// Base runtime hosts. MCP custody is added by its existing execution owner.
    /// None denotes unrestricted egress; an empty Some denotes no egress.
    #[must_use]
    pub fn runtime_network_hosts(&self) -> Option<Vec<String>> {
        self.network_hosts(EnvironmentNetworkStage::Runtime)
    }

    /// Package declarations authorize build-stage registry access independently
    /// of the frozen runtime flag. Builds never derive MCP upstream access.
    #[must_use]
    pub fn package_build_network_hosts(&self) -> Option<Vec<String>> {
        self.network_hosts(EnvironmentNetworkStage::PackageBuild)
    }

    fn package_registry_access(&self, stage: EnvironmentNetworkStage) -> bool {
        match (self, stage) {
            (
                Self::Cloud {
                    networking:
                        EnvironmentNetworking::Limited {
                            allow_package_managers,
                            ..
                        },
                    ..
                },
                EnvironmentNetworkStage::Runtime,
            ) => *allow_package_managers,
            (Self::Cloud { packages, .. }, EnvironmentNetworkStage::PackageBuild) => {
                !packages.is_empty()
            }
            _ => true,
        }
    }

    fn network_hosts(&self, stage: EnvironmentNetworkStage) -> Option<Vec<String>> {
        let Self::Cloud {
            networking: EnvironmentNetworking::Limited { allowed_hosts, .. },
            ..
        } = self
        else {
            return None;
        };
        let mut hosts = allowed_hosts.clone();
        if self.package_registry_access(stage) {
            hosts.extend(
                PUBLIC_PACKAGE_REGISTRY_HOSTS
                    .iter()
                    .map(ToString::to_string),
            );
        }
        hosts.sort();
        hosts.dedup();
        Some(hosts)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentNetworking {
    /// `{}` and not a bare unit. `deny_unknown_fields` on an internally tagged
    /// enum governs its struct variants only: serde reads a unit variant's tag
    /// and ignores every sibling key, so `{"type":"unrestricted",
    /// "allowed_hosts":[…]}` would decode to unrestricted egress and the
    /// authored allowlist would vanish without an error. The empty brace form
    /// is what subjects the variant to the container attribute. `Default` is
    /// written by hand because `#[default]` accepts unit variants only.
    Unrestricted {},
    Limited {
        #[serde(default)]
        allowed_hosts: Vec<String>,
        #[serde(default)]
        allow_mcp_servers: bool,
        #[serde(default)]
        allow_package_managers: bool,
    },
}

impl Default for EnvironmentNetworking {
    fn default() -> Self {
        Self::Unrestricted {}
    }
}

/// Exact package-manager inputs provisioned before any workload process starts.
/// Manager names remain open for provider extensibility; protocol adapters own
/// their closed public enums and providers reject unsupported managers.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageRequirements {
    #[serde(default)]
    pub managers: std::collections::BTreeMap<String, Vec<String>>,
    /// Frozen Environment identity used only when at least one requirement is
    /// unpinned. This gives "latest" a precise lifecycle: it resolves once for
    /// one Environment snapshot, is cached across that snapshot's Sessions, and
    /// is resolved again after the Environment revision changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_id: Option<String>,
}

impl PackageRequirements {
    /// One operand, never a command option. Managers interpret the value as data;
    /// callers must still pass it as one argv element rather than interpolate it.
    #[must_use]
    pub fn requirement_is_valid(requirement: &str) -> bool {
        !requirement.is_empty()
            && requirement.trim() == requirement
            && !requirement.starts_with('-')
            && !requirement.chars().any(char::is_control)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.managers.values().all(Vec::is_empty)
    }
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
    pub description: Option<String>,
    pub metadata: BTreeMap<String, String>,
    /// Anthropic visibility scope (`organization` or `account`).
    pub scope: Option<String>,
    pub config: EnvironmentConfig,
    /// Exact Control-owned sandbox policy version selected by this immutable
    /// Environment revision. The policy body is resolved only while publishing
    /// the executable projection; Coordinator never opens the policy store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<EnvironmentSandboxPolicyRef>,
    /// When the registry accepted this Environment, read from the store's
    /// injected wall-clock port inside the create transaction and never
    /// rewritten afterwards.
    ///
    /// `None` only for a projection that was never registered — the built-in
    /// local Environment exists by installation, so there is no acceptance
    /// event to report and the absence is stated rather than invented.
    pub created_at: Option<String>,
    /// When the registry last committed a revision of this Environment (the
    /// create, each update, and the archive). Same clock, same transaction,
    /// and `None` for the same reason as [`Self::created_at`].
    pub updated_at: Option<String>,
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

/// Durable delivery operation appended atomically with one authored revision.
/// The operation is a fact of the Environment aggregate; projection adapters
/// may retry it, but must never infer a second work list from current rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvironmentRegistrationOperation {
    Register,
    Withdraw,
}

/// One immutable entry in the Control-owned executable-registration outbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentRegistrationIntent {
    pub environment_id: String,
    pub revision: EnvironmentRevision,
    pub operation: EnvironmentRegistrationOperation,
    pub delivered: bool,
}

impl EnvironmentRegistrationIntent {
    #[must_use]
    pub fn for_item(item: &EnvItem) -> Self {
        Self {
            environment_id: item.id.clone(),
            revision: item.revision,
            operation: registration_operation(item.archived_at.is_some()),
            delivered: false,
        }
    }

    /// Whether the one durable intent must cross the executable-projection
    /// boundary in this drain mode. Normal draining stutters after an exact
    /// acknowledgement; startup recovery deliberately replays the same intent
    /// so an empty disposable projection can be rebuilt without deriving work
    /// from current Environment rows.
    #[must_use]
    pub const fn requires_delivery(&self, replay_delivered: bool) -> bool {
        !self.delivered || replay_delivered
    }
}

const fn registration_operation(archived: bool) -> EnvironmentRegistrationOperation {
    if archived {
        EnvironmentRegistrationOperation::Withdraw
    } else {
        EnvironmentRegistrationOperation::Register
    }
}

#[cfg(kani)]
mod registration_intent_proofs {
    use super::*;

    /// @obligation environment.registration_intent_is_atomic_recoverable_and_idempotent
    /// @proof-input-debt free_symbol registration_operation archived
    #[kani::proof]
    fn environment_registration_intent_is_exact_idempotent_and_recoverable() {
        // Cause/effect table: C1 the committed Environment is active/archived;
        // C2 its exact intent is pending/delivered; C3 the caller is the normal
        // pending drainer/startup recovery. E1 maps active only to Register and
        // archived only to Withdraw; E2 normal drain delivers only pending;
        // E3 recovery delivers both states; E4 no branch changes the operation.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // |---|---|---|---|---|
        // | R1 | active | any | any | Register |
        // | R2 | archived | any | any | Withdraw |
        // | R3 | any | pending | normal/recovery | deliver |
        // | R4 | any | delivered | normal | stutter |
        // | R5 | any | delivered | recovery | redeliver exact intent |
        let archived = kani::any::<bool>();
        let delivered = kani::any::<bool>();
        let replay_delivered = kani::any::<bool>();
        let operation = registration_operation(archived);
        assert_eq!(
            operation == EnvironmentRegistrationOperation::Withdraw,
            archived
        );
        let intent = EnvironmentRegistrationIntent {
            environment_id: String::new(),
            revision: EnvironmentRevision(kani::any()),
            operation,
            delivered,
        };
        assert_eq!(
            intent.requires_delivery(replay_delivered),
            !delivered || replay_delivered
        );
        assert_eq!(intent.operation, operation);
    }
}

/// Selects either normal retry work or the same durable log for rebuilding an
/// empty executable projection after process recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvironmentRegistrationIntentFilter {
    Pending,
    All,
}

impl EnvItem {
    /// Whether this is a self-hosted environment (`config.type == self_hosted`).
    #[must_use]
    pub fn is_self_hosted(&self) -> bool {
        self.config.is_self_hosted()
    }

    /// Apply an [`EnvUpdate`] in place: present fields replace, a `metadata` key
    /// mapped to `null` deletes it. Returns whether canonical facts changed; a
    /// no-op replay retains the revision and produces no new delivery intent.
    /// One patch definition is shared by every backend so semantics cannot drift.
    pub fn apply(&mut self, patch: EnvUpdate) -> Result<bool, EnvironmentMutationError> {
        let mut next = self.clone();
        if let Some(name) = patch.name {
            next.name = name;
        }
        if let Some(description) = patch.description {
            next.description = match description {
                EnvironmentFieldUpdate::Clear => None,
                EnvironmentFieldUpdate::Replace(description) => Some(description),
            };
        }
        if let Some(config) = patch.config {
            next.config.apply(config);
        }
        if let Some(scope) = patch.scope {
            next.scope = match scope {
                EnvironmentFieldUpdate::Clear => None,
                EnvironmentFieldUpdate::Replace(scope) => Some(scope),
            };
        }
        if let Some(sandbox_policy) = patch.sandbox_policy {
            next.sandbox_policy = match sandbox_policy {
                EnvironmentFieldUpdate::Clear => None,
                EnvironmentFieldUpdate::Replace(policy) => Some(policy),
            };
        }
        if let Some(md) = patch.metadata {
            for (k, v) in md {
                match v {
                    Some(s) => {
                        next.metadata.insert(k, s);
                    }
                    None => {
                        next.metadata.remove(&k);
                    }
                }
            }
        }
        next.config.validate()?;
        if next == *self {
            return Ok(false);
        }
        next.revision = EnvironmentRevision(
            next.revision
                .0
                .checked_add(1)
                .ok_or(EnvironmentMutationError::RevisionExhausted)?,
        );
        *self = next;
        Ok(true)
    }
}

/// A metadata/config update patch: present fields replace; a `metadata` key mapped
/// to `null` deletes it.
#[derive(Clone, Debug, Default)]
pub struct EnvUpdate {
    pub name: Option<String>,
    /// Omission preserves, `Clear` stores absence, and `Replace` stores text.
    pub description: Option<EnvironmentFieldUpdate<String>>,
    pub config: Option<EnvironmentConfigMutation>,
    pub scope: Option<EnvironmentFieldUpdate<String>>,
    pub metadata: Option<BTreeMap<String, Option<String>>>,
    pub sandbox_policy: Option<EnvironmentFieldUpdate<EnvironmentSandboxPolicyRef>>,
}

/// Explicit update of an optional Environment field. Command absence means
/// unchanged; clearing and replacement are distinct domain intents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentFieldUpdate<T> {
    Clear,
    Replace(T),
}

/// Atomic mutation vocabulary for the official Environment update semantics.
/// It is deliberately owned beside `EnvironmentConfig` so every store applies
/// omitted-field preservation identically under its existing update lock.
#[derive(Clone, Debug)]
pub enum EnvironmentConfigMutation {
    Replace(EnvironmentConfig),
    PatchCloud {
        networking: Option<EnvironmentNetworkingMutation>,
        packages: Option<EnvironmentPackagesMutation>,
    },
}

#[derive(Clone, Debug)]
pub enum EnvironmentNetworkingMutation {
    Reset,
    Unrestricted,
    Limited {
        allowed_hosts: Option<EnvironmentFieldUpdate<Vec<String>>>,
        allow_mcp_servers: Option<EnvironmentFieldUpdate<bool>>,
        allow_package_managers: Option<EnvironmentFieldUpdate<bool>>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct EnvironmentPackagesMutation {
    pub reset: bool,
    pub apt: Option<EnvironmentFieldUpdate<Vec<String>>>,
    pub cargo: Option<EnvironmentFieldUpdate<Vec<String>>>,
    pub gem: Option<EnvironmentFieldUpdate<Vec<String>>>,
    pub go: Option<EnvironmentFieldUpdate<Vec<String>>>,
    pub npm: Option<EnvironmentFieldUpdate<Vec<String>>>,
    pub pip: Option<EnvironmentFieldUpdate<Vec<String>>>,
}

impl EnvironmentConfig {
    fn apply(&mut self, mutation: EnvironmentConfigMutation) {
        match mutation {
            EnvironmentConfigMutation::Replace(config) => *self = config,
            EnvironmentConfigMutation::PatchCloud {
                networking,
                packages,
            } => {
                let (mut current_networking, mut current_packages) =
                    match std::mem::replace(self, Self::SelfHosted) {
                        Self::Cloud {
                            networking,
                            packages,
                        } => (networking, packages),
                        Self::SelfHosted => (
                            EnvironmentNetworking::default(),
                            EnvironmentPackages::default(),
                        ),
                    };
                if let Some(mutation) = networking {
                    current_networking.apply(mutation);
                }
                if let Some(mutation) = packages {
                    current_packages.apply(mutation);
                }
                *self = Self::Cloud {
                    networking: current_networking,
                    packages: current_packages,
                };
            }
        }
    }
}

impl EnvironmentNetworking {
    fn apply(&mut self, mutation: EnvironmentNetworkingMutation) {
        match mutation {
            EnvironmentNetworkingMutation::Reset | EnvironmentNetworkingMutation::Unrestricted => {
                *self = Self::Unrestricted {}
            }
            EnvironmentNetworkingMutation::Limited {
                allowed_hosts,
                allow_mcp_servers,
                allow_package_managers,
            } => {
                let (mut current_hosts, mut current_mcp, mut current_packages) =
                    match std::mem::replace(self, Self::Unrestricted {}) {
                        Self::Limited {
                            allowed_hosts,
                            allow_mcp_servers,
                            allow_package_managers,
                        } => (allowed_hosts, allow_mcp_servers, allow_package_managers),
                        Self::Unrestricted {} => (Vec::new(), false, false),
                    };
                if let Some(value) = allowed_hosts {
                    current_hosts = match value {
                        EnvironmentFieldUpdate::Clear => Vec::new(),
                        EnvironmentFieldUpdate::Replace(value) => value,
                    };
                }
                if let Some(value) = allow_mcp_servers {
                    current_mcp = match value {
                        EnvironmentFieldUpdate::Clear => false,
                        EnvironmentFieldUpdate::Replace(value) => value,
                    };
                }
                if let Some(value) = allow_package_managers {
                    current_packages = match value {
                        EnvironmentFieldUpdate::Clear => false,
                        EnvironmentFieldUpdate::Replace(value) => value,
                    };
                }
                *self = Self::Limited {
                    allowed_hosts: current_hosts,
                    allow_mcp_servers: current_mcp,
                    allow_package_managers: current_packages,
                };
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
                    self.$field = match value {
                        EnvironmentFieldUpdate::Clear => Vec::new(),
                        EnvironmentFieldUpdate::Replace(value) => value,
                    };
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

/// Read current definitions and their immutable authored revisions.
#[async_trait]
pub trait EnvironmentDefinitionQueries: Send + Sync {
    /// All non-archived environments, ascending by id.
    async fn list_active(&self) -> Result<Vec<EnvItem>, EnvironmentStoreError>;

    /// Every current Environment projection, including archived definitions.
    /// This is a query surface only; executable delivery consumes the outbox.
    async fn list_all(&self) -> Result<Vec<EnvItem>, EnvironmentStoreError>;

    /// The environment under `id` (archived or not).
    async fn get(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError>;

    /// One immutable authored revision. Implementations append this record in
    /// the same transaction that advances the current row; they never reconstruct
    /// history from the mutable current projection.
    async fn get_revision(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvItem>, EnvironmentStoreError>;

    /// Whether `id` exists (archived or not).
    async fn exists(&self, id: &str) -> Result<bool, EnvironmentStoreError>;
}

/// Atomically author definitions, immutable revisions, and their registration intents.
/// Every mutation commits all three facts under one backend transaction or lock.
#[async_trait]
pub trait EnvironmentDefinitionStore: Send + Sync {
    /// Read the original immutable result of this exact command before admitting
    /// a new mutation. None means no committed command exists; mismatched input
    /// is an idempotency conflict. This lookup creates no definition or intent.
    /// It cannot substitute the current head for a missing original revision.
    async fn find_create_result(
        &self,
        command: &CreateEnvironmentCommand,
    ) -> Result<Option<EnvItem>, CreateEnvironmentError>;

    /// Atomically persist or replay one create command. A command id can name
    /// exactly one fingerprint for the lifetime of the authority store.
    /// Replay returns the original immutable revision, even after updates or
    /// archive advance the current definition. It creates no new intent.
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
    ) -> Result<EnvItem, CreateEnvironmentError> {
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
    ) -> Result<EnvItem, CreateEnvironmentError> {
        static NEXT_COMMAND: AtomicU64 = AtomicU64::new(0);
        self.create_once(CreateEnvironmentCommand {
            command_id: format!(
                "unkeyed:{}:{}",
                std::process::id(),
                NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
            ),
            name,
            description: Some(description),
            metadata,
            scope,
            config,
            sandbox_policy: None,
        })
        .await
        .map(CreateEnvironmentOutcome::into_item)
    }

    /// Compare-and-apply an update patch to an active definition. The store must
    /// reject a stale `expected_revision` in the same transaction or lock that
    /// advances the current row. `None` means `id` does not exist or is archived.
    async fn update(
        &self,
        id: &str,
        expected_revision: EnvironmentRevision,
        patch: EnvUpdate,
    ) -> Result<Option<EnvItem>, EnvironmentStoreError>;

    /// Archive `id` (stamps `archived_at`); `None` when it does not exist.
    async fn archive(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError>;
}

/// Read and acknowledge the exact registration facts committed with definitions.
/// Delivery never derives new facts from the mutable current projection.
#[async_trait]
pub trait EnvironmentRegistrationOutbox: Send + Sync {
    /// Read one exact outbox fact. Every authored revision has exactly one.
    async fn registration_intent(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvironmentRegistrationIntent>, EnvironmentStoreError>;

    /// Read the authoritative outbox in deterministic per-Environment revision
    /// order. `All` is used only to rebuild an empty projection at startup.
    async fn registration_intents(
        &self,
        filter: EnvironmentRegistrationIntentFilter,
    ) -> Result<Vec<EnvironmentRegistrationIntent>, EnvironmentStoreError>;

    /// Acknowledge successful idempotent projection delivery. Returning false
    /// means the exact intent did not exist and is an invariant violation.
    async fn mark_registration_intent_delivered(
        &self,
        intent: &EnvironmentRegistrationIntent,
    ) -> Result<bool, EnvironmentStoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_default_is_the_self_hosted_decision() {
        assert_eq!(EnvironmentConfig::default(), EnvironmentConfig::SelfHosted);
    }

    #[test]
    fn registration_intent_delivery_uses_one_exact_recovery_rule() {
        // Cause/effect decision table shared with the production method:
        // pending x normal/recovery -> deliver; delivered x normal -> stutter;
        // delivered x recovery -> redeliver the same Register/Withdraw fact.
        // The operation is crossed independently so replay cannot reclassify it.
        for operation in [
            EnvironmentRegistrationOperation::Register,
            EnvironmentRegistrationOperation::Withdraw,
        ] {
            for delivered in [false, true] {
                for replay_delivered in [false, true] {
                    let intent = EnvironmentRegistrationIntent {
                        environment_id: "env".into(),
                        revision: EnvironmentRevision(7),
                        operation,
                        delivered,
                    };
                    assert_eq!(
                        intent.requires_delivery(replay_delivered),
                        !delivered || replay_delivered
                    );
                    assert_eq!(intent.operation, operation);
                }
            }
        }
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

    #[test]
    fn unrestricted_networking_refuses_the_allowlist_it_does_not_model() {
        // `deny_unknown_fields` sits on this enum, yet it governs `Limited`
        // only: serde reads a *unit* variant's tag and discards every sibling
        // key. `EnvItem.config` reaches this type from an externally authored
        // `ExecutableEnvironmentRegistration` body, so an operator who writes
        // `{"type":"unrestricted","allowed_hosts":[…]}` registered an
        // environment with unrestricted egress and no error to read.
        //
        // Causes over one networking object: C1 tag only; C2 tag plus a key
        // `Limited` models; C3 tag plus a key no variant models.
        // Effects: E1 decode, E2 refuse.
        // | Rule | variant      | body | Effect |
        // | U1   | unrestricted | C1   | E1     |
        // | U2   | unrestricted | C2   | E2     |
        // | U3   | unrestricted | C3   | E2     |
        // | U4   | limited      | C2   | E1     |
        // | U5   | limited      | C3   | E2     |
        let decode =
            |value: serde_json::Value| serde_json::from_value::<EnvironmentNetworking>(value);
        assert_eq!(
            decode(serde_json::json!({"type": "unrestricted"})).unwrap(),
            EnvironmentNetworking::Unrestricted {},
            "U1"
        );
        assert!(
            decode(serde_json::json!({"type": "unrestricted", "allowed_hosts": ["api.test"]}))
                .is_err(),
            "U2: an authored egress allowlist must not be dropped into unrestricted egress"
        );
        assert!(
            decode(serde_json::json!({"type": "unrestricted", "future": true})).is_err(),
            "U3"
        );
        assert!(
            decode(serde_json::json!({"type": "limited", "allowed_hosts": ["api.test"]})).is_ok(),
            "U4"
        );
        assert!(
            decode(serde_json::json!({"type": "limited", "future": true})).is_err(),
            "U5"
        );
        // The closed form must not change the encoding any peer already reads.
        assert_eq!(
            serde_json::to_value(EnvironmentNetworking::Unrestricted {}).unwrap(),
            serde_json::json!({"type": "unrestricted"}),
            "round trip: the wire spelling is unchanged"
        );
        assert_eq!(
            EnvironmentNetworking::default(),
            EnvironmentNetworking::Unrestricted {},
            "the hand-written Default replaces `#[default]`, which unit variants alone accept"
        );
    }

    #[test]
    fn package_network_invariant_exhausts_the_closed_state_space() {
        // DT-NET: C1 packages absent/present; C2 runtime registry permission
        // off/on; C3 unrestricted/limited; C4 operand valid/invalid. N1 valid
        // inputs admit independent preparation. N2 limited runtime permission
        // alone controls runtime hosts. N3 declared packages alone enable
        // build registry hosts. N4 malformed operands always reject. These
        // projections grant no Pod policy or build completion receipt.
        for present in [false, true] {
            for allow_package_managers in [false, true] {
                let config = EnvironmentConfig::Cloud {
                    networking: EnvironmentNetworking::Limited {
                        allowed_hosts: Vec::new(),
                        allow_mcp_servers: false,
                        allow_package_managers,
                    },
                    packages: EnvironmentPackages {
                        npm: present.then(|| "tsx".into()).into_iter().collect(),
                        ..Default::default()
                    },
                };
                assert!(config.validate().is_ok(), "N1");
                assert_eq!(
                    !config.runtime_network_hosts().unwrap().is_empty(),
                    allow_package_managers,
                    "N2"
                );
                assert_eq!(
                    !config.package_build_network_hosts().unwrap().is_empty(),
                    present,
                    "N3"
                );
            }
        }
        for networking in [
            EnvironmentNetworking::Unrestricted {},
            EnvironmentNetworking::Limited {
                allowed_hosts: Vec::new(),
                allow_mcp_servers: false,
                allow_package_managers: false,
            },
        ] {
            for operand in ["", "--option", " tsx", "tsx\n"] {
                let config = EnvironmentConfig::Cloud {
                    networking: networking.clone(),
                    packages: EnvironmentPackages {
                        npm: vec![operand.into()],
                        ..Default::default()
                    },
                };
                assert_eq!(
                    config.validate(),
                    Err(InvalidEnvironmentConfig::InvalidPackageRequirement { manager: "npm" }),
                    "N4"
                );
            }
        }
        assert_eq!(EnvironmentConfig::SelfHosted.runtime_network_hosts(), None);
        assert_eq!(
            EnvironmentConfig::Cloud {
                networking: EnvironmentNetworking::Unrestricted {},
                packages: EnvironmentPackages::default()
            }
            .package_build_network_hosts(),
            None,
            "N1 unrestricted unchanged"
        );
    }

    #[test]
    fn package_operands_are_data_and_options_or_torn_values_are_refused() {
        // DT-OP: O1 ordinary/scoped/pinned/URL values (including punctuation)
        // remain one data operand; O2 empty, surrounding whitespace, leading
        // option and embedded control bytes reject. This predicate does not
        // authorize a registry or interpolate/execute a shell expression.
        for value in [
            "tsx",
            "@org/pkg@1.0.0",
            "pkg;literal",
            "https://registry.test/archive.tgz",
        ] {
            assert!(
                PackageRequirements::requirement_is_valid(value),
                "O1 {value:?}"
            );
        }
        for value in ["", " pkg", "pkg ", "--option", "pkg\nnext", "pkg\0next"] {
            assert!(
                !PackageRequirements::requirement_is_valid(value),
                "O2 {value:?}"
            );
        }
    }

    #[test]
    fn network_projections_preserve_authored_hosts_without_mutating_the_definition() {
        // DT-HOST: C1 duplicate, unsorted authored hosts including one registry;
        // C2 each of six managers independently declares one package; C3 runtime
        // registry permission is false. H1 runtime preserves only authored hosts;
        // H2 preparation adds the complete canonical catalog; H3 both outputs
        // sort/deduplicate and leave the immutable definition unchanged. Explicit
        // authored registry access is preserved, so H1 forbids implicit grants.
        for manager in 0..6 {
            let mut packages = EnvironmentPackages::default();
            match manager {
                0 => &mut packages.apt,
                1 => &mut packages.cargo,
                2 => &mut packages.gem,
                3 => &mut packages.go,
                4 => &mut packages.npm,
                _ => &mut packages.pip,
            }
            .push("package@1".into());
            let config = EnvironmentConfig::Cloud {
                networking: EnvironmentNetworking::Limited {
                    allowed_hosts: vec![
                        "z.example".into(),
                        "registry.npmjs.org".into(),
                        "a.example".into(),
                        "z.example".into(),
                    ],
                    allow_mcp_servers: false,
                    allow_package_managers: false,
                },
                packages,
            };
            let before = config.clone();
            assert_eq!(
                config.runtime_network_hosts().unwrap(),
                ["a.example", "registry.npmjs.org", "z.example"],
                "H1/H3 manager {manager}"
            );
            let mut expected: Vec<String> = PUBLIC_PACKAGE_REGISTRY_HOSTS
                .iter()
                .map(ToString::to_string)
                .chain(["a.example".into(), "z.example".into()])
                .collect();
            expected.sort();
            assert_eq!(
                config.package_build_network_hosts().unwrap(),
                expected,
                "H2/H3 manager {manager}"
            );
            assert_eq!(config, before, "H3 immutable source");
        }
    }

    #[test]
    fn invalid_patch_restores_the_entire_aggregate() {
        // C1 a valid aggregate has declared packages; C2 one patch supplies an
        // invalid operand and changes networking; E1 rejection; E2 every field and revision equals
        // the pre-state. This proves rollback is aggregate-wide, not field-local.
        let mut item = EnvItem {
            id: "env".into(),
            revision: EnvironmentRevision(7),
            name: "before".into(),
            description: None,
            metadata: BTreeMap::new(),
            scope: None,
            config: EnvironmentConfig::Cloud {
                networking: EnvironmentNetworking::Limited {
                    allowed_hosts: Vec::new(),
                    allow_mcp_servers: false,
                    allow_package_managers: true,
                },
                packages: EnvironmentPackages {
                    npm: vec!["tsx".into()],
                    ..Default::default()
                },
            },
            sandbox_policy: None,
            archived_at: None,
            created_at: Some("2026-02-03T04:05:06.007Z".into()),
            updated_at: Some("2026-02-03T04:05:06.007Z".into()),
        };
        let before = item.clone();
        assert_eq!(
            item.apply(EnvUpdate {
                name: Some("must roll back too".into()),
                config: Some(EnvironmentConfigMutation::PatchCloud {
                    networking: Some(EnvironmentNetworkingMutation::Limited {
                        allowed_hosts: None,
                        allow_mcp_servers: None,
                        allow_package_managers: Some(EnvironmentFieldUpdate::Replace(false)),
                    }),
                    packages: Some(EnvironmentPackagesMutation {
                        npm: Some(EnvironmentFieldUpdate::Replace(vec!["--invalid".into()])),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            }),
            Err(EnvironmentMutationError::InvalidConfig(
                InvalidEnvironmentConfig::InvalidPackageRequirement { manager: "npm" }
            ))
        );
        assert_eq!(item, before);
    }

    #[test]
    fn revision_boundary_rejects_changes_atomically_and_admits_noop_replay() {
        // R1 MAX-1 accepts one change; R2 MAX admits an empty/equal-fact replay;
        // R3 MAX rejects a real change; R4 invalid configuration wins before
        // revision admission at either boundary. Every rejection preserves all facts.
        let invalid = EnvUpdate {
            name: Some("must roll back".into()),
            config: Some(EnvironmentConfigMutation::Replace(
                EnvironmentConfig::Cloud {
                    networking: EnvironmentNetworking::Limited {
                        allowed_hosts: Vec::new(),
                        allow_mcp_servers: false,
                        allow_package_managers: false,
                    },
                    packages: EnvironmentPackages {
                        npm: vec!["--invalid".into()],
                        ..Default::default()
                    },
                },
            )),
            ..Default::default()
        };
        for (revision, patch, expected) in [
            (
                u64::MAX - 1,
                EnvUpdate {
                    name: Some("after".into()),
                    ..Default::default()
                },
                Ok(true),
            ),
            (u64::MAX, EnvUpdate::default(), Ok(false)),
            (
                u64::MAX,
                EnvUpdate {
                    name: Some("before".into()),
                    ..Default::default()
                },
                Ok(false),
            ),
            (
                u64::MAX,
                EnvUpdate {
                    name: Some("after".into()),
                    ..Default::default()
                },
                Err(EnvironmentMutationError::RevisionExhausted),
            ),
            (
                u64::MAX - 1,
                invalid.clone(),
                Err(EnvironmentMutationError::InvalidConfig(
                    InvalidEnvironmentConfig::InvalidPackageRequirement { manager: "npm" },
                )),
            ),
            (
                u64::MAX,
                invalid,
                Err(EnvironmentMutationError::InvalidConfig(
                    InvalidEnvironmentConfig::InvalidPackageRequirement { manager: "npm" },
                )),
            ),
        ] {
            let mut item = EnvItem {
                id: "env-boundary".into(),
                revision: EnvironmentRevision(revision),
                name: "before".into(),
                description: Some("retained".into()),
                metadata: BTreeMap::from([("retained".into(), "value".into())]),
                scope: Some("workspace".into()),
                config: EnvironmentConfig::SelfHosted,
                sandbox_policy: None,
                archived_at: None,
                created_at: Some("2026-02-03T04:05:06.007Z".into()),
                updated_at: Some("2026-02-03T04:05:06.007Z".into()),
            };
            let before = item.clone();
            assert_eq!(item.apply(patch), expected);
            if expected == Ok(true) {
                let mut after = before;
                after.name = "after".into();
                after.revision = EnvironmentRevision(u64::MAX);
                assert_eq!(item, after);
            } else {
                assert_eq!(item, before);
            }
        }
        assert_eq!(
            EnvironmentStoreError::from(EnvironmentMutationError::RevisionExhausted),
            EnvironmentStoreError::RevisionExhausted,
        );
    }
}

#[cfg(test)]
mod network_projection_exhaustive {
    use super::*;

    /// @exhaustive bool(2) x bool(2) x bool(2) x bool(2) x package manager(6) = 96
    /// @obligation environment.prepared_packages_do_not_grant_runtime_registry_access
    #[test]
    fn declared_build_packages_never_expand_runtime_hosts() {
        // Raw configuration, then the same stage decision used by both host
        // projections. The six managers and empty/nonempty declarations cover
        // the presence predicate; no free authorization Boolean is supplied.
        for restricted in [false, true] {
            for runtime_permission in [false, true] {
                for allow_mcp_servers in [false, true] {
                    for present in [false, true] {
                        for manager in 0_u8..6 {
                            let networking = if restricted {
                                EnvironmentNetworking::Limited {
                                    allowed_hosts: Vec::new(),
                                    allow_mcp_servers,
                                    allow_package_managers: runtime_permission,
                                }
                            } else {
                                EnvironmentNetworking::Unrestricted {}
                            };
                            let mut config = EnvironmentConfig::Cloud {
                                networking,
                                packages: EnvironmentPackages::default(),
                            };
                            let before =
                                config.package_registry_access(EnvironmentNetworkStage::Runtime);
                            if present {
                                let EnvironmentConfig::Cloud { packages, .. } = &mut config else {
                                    unreachable!()
                                };
                                match manager {
                                    0 => &mut packages.apt,
                                    1 => &mut packages.cargo,
                                    2 => &mut packages.gem,
                                    3 => &mut packages.go,
                                    4 => &mut packages.npm,
                                    _ => &mut packages.pip,
                                }
                                .push("package@1".into());
                            }
                            assert_eq!(
                                before,
                                config.package_registry_access(EnvironmentNetworkStage::Runtime)
                            );
                            assert_eq!(before, !restricted || runtime_permission);
                            assert_eq!(
                                config
                                    .package_registry_access(EnvironmentNetworkStage::PackageBuild),
                                present
                            );
                        }
                    }
                }
            }
        }
    }
}
