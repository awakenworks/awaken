//! The provisioning ports: [`SandboxProvider`] realizes a [`SandboxSpec`] into a
//! live [`Sandbox`]; [`Sandbox`] launches processes and moves files. Concrete
//! backends (lexical / bubblewrap / container) implement these in their own
//! crates and are selected by [`SandboxCapabilities`].

use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::spec::{Command, SandboxSpec};
use crate::vocab::{Artifact, MountAccess, MountRequirement, Realization, RealizedMount};

/// Provisioning failure. String-carried at the boundary (like the runtime's other
/// neutral errors); a backend maps its own error into this.
#[derive(Debug, thiserror::Error)]
#[error("sandbox error: {0}")]
pub struct SandboxError(pub String);

impl SandboxError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// A content-addressed byte source a provider consults to resolve a `File`/`Resource`
/// mount's bytes by id. Dependency-inverted so the worker-tier sandbox providers stay
/// free of any durable store: the composition root injects an adapter over the
/// resources-tier content store (A-G17 — the isolated exec tier links no store).
#[async_trait]
pub trait BlobSource: Send + Sync {
    /// The bytes for content id `id`, or `None` if absent (errors are folded to
    /// `None`; a required mount that resolves to nothing fails closed downstream).
    async fn get(&self, id: &str) -> Option<Vec<u8>>;
}

/// Last-mile credential broker shared by secret files and process-secret
/// requirements. The two operations are intentionally distinct: a durable file
/// reference may support refresh/write-back, while a process reference is normally
/// short-lived, claim-fenced, one-shot, and never valid as a file identifier. Secret
/// bytes never enter a [`SandboxSpec`].
#[async_trait]
pub trait SecretBroker: Send + Sync {
    /// Materialize the current credential file bytes for `reference`.
    async fn materialize(&self, reference: &str) -> Result<Vec<u8>, SandboxError>;

    /// Consume a process-scoped requirement immediately before launch. An
    /// implementation must validate the reference as a process capability; it must
    /// not silently reinterpret an arbitrary durable file/credential id.
    async fn materialize_process(&self, reference: &str) -> Result<Vec<u8>, SandboxError>;

    /// Atomically persist a CLI-refreshed credential file under `reference`.
    async fn write_back(&self, reference: &str, bytes: Vec<u8>) -> Result<(), SandboxError>;
}

/// Realizes a [`MountSource::MemoryStore`](crate::vocab::MountSource::MemoryStore)
/// into a sandbox at a provider-resolved host path — the path-addressed counterpart
/// of [`BlobSource`] (a store is a keyed filesystem, not one blob). The FUSE / copy
/// impl lives in the worker tier (`awaken-sandbox-memoryd`); the composition root
/// injects it, so the providers stay free of the store and FUSE deps (A-G17). A
/// provider with no mounter fails a `MemoryStore` mount loud rather than fake it.
#[async_trait]
pub trait MemoryMounter: Send + Sync {
    /// Expose `store_id` at `host_path` with `access`, returning a live handle whose
    /// [`realization`](MemoryMount::realization) the provider records. The handle is
    /// held for the sandbox's life; dropping it (via [`teardown`](MemoryMount::teardown))
    /// unmounts a FUSE mount or harvests a writable copy back to the store.
    async fn mount(
        &self,
        store_id: &str,
        host_path: &Path,
        access: MountAccess,
    ) -> Result<Box<dyn MemoryMount>, SandboxError>;

    /// Reconcile the current files read from an adopted copy-backed sandbox.
    /// A hard process crash loses the original in-process [`MemoryMount`] guard,
    /// while the long-lived sandbox and its files remain. Providers call this only
    /// at the recovered Session's terminal edge; implementations retain the same
    /// conflict-safe durable-head rules as ordinary copy teardown.
    async fn reconcile_recovered_copy(
        &self,
        _store_id: &str,
        _files: &[(String, Vec<u8>)],
        _access: MountAccess,
    ) -> Result<(), SandboxError> {
        Err(SandboxError::new(
            "recovered Memory copy reconciliation is unsupported",
        ))
    }
}

/// A live memory-store mount, held for the sandbox's lifetime.
#[async_trait]
pub trait MemoryMount: Send + Sync {
    /// How the store was exposed (`Fuse` where the kernel supports it, else `Copy`).
    fn realization(&self) -> Realization;

    /// Tear down: unmount the FUSE, or (for a writable copy) harvest edits back to
    /// the durable store. Idempotent and best-effort — a teardown fault is logged,
    /// not surfaced, since the sandbox is already being disposed.
    async fn teardown(self: Box<Self>);
}

/// Secret-free, per-Sandbox projection of one already-resolved Repository config.
/// It pins configuration semantics only. Branch is a clone preference; commit is
/// an exact immutable checkout. Principal, role, API key, policy, Workspace
/// hierarchy, and credential bytes are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRealizationPlan {
    pub repository_id: String,
    pub mount_path: String,
    pub remote_url: String,
    pub initial_branch: Option<String>,
    pub initial_commit: Option<String>,
    pub access: MountAccess,
}

/// Ephemeral Basic-auth value translated from an already-admitted credential at
/// the Repository boundary. It is deliberately non-serializable and absent from
/// [`RepositoryRealizationPlan`]; only the target adapter may expose its fields.
#[derive(Debug, Clone)]
pub struct RepositoryHttpBasicCredential {
    username: awaken_agent_contract::RedactedString,
    password: awaken_agent_contract::RedactedString,
}

impl RepositoryHttpBasicCredential {
    #[must_use]
    pub fn new(
        username: impl Into<awaken_agent_contract::RedactedString>,
        password: impl Into<awaken_agent_contract::RedactedString>,
    ) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }

    #[must_use]
    pub fn expose_username(&self) -> &str {
        self.username.expose_secret()
    }

    #[must_use]
    pub fn expose_password(&self) -> &str {
        self.password.expose_secret()
    }
}

/// Environment-side adapter for a mutable Repository input. Authorization and
/// configuration resolution happen before this port is called; implementations
/// only construct/use a working tree. A credential value is injected ephemerally
/// for the transport operation and must never be persisted in the plan, origin URL,
/// or sandbox.
#[async_trait]
pub trait RepositoryRealizer: Send + Sync {
    /// Clone the current remote content into this realizer's Sandbox.
    async fn realize_repository(
        &self,
        plan: &RepositoryRealizationPlan,
        credential: Option<&RepositoryHttpBasicCredential>,
    ) -> Result<(), SandboxError>;

    /// Publish Agent-authored commits from the current working branch. The caller
    /// decides whether publishing is allowed/required; this adapter owns only Git
    /// transport mechanics. Returns false when there is nothing to publish.
    async fn publish_repository(
        &self,
        plan: &RepositoryRealizationPlan,
        credential: Option<&RepositoryHttpBasicCredential>,
    ) -> Result<bool, SandboxError>;
}

/// A serializable, **durable** reference to a realized sandbox. Persist it the
/// moment a sandbox is created; a live `Box<dyn Sandbox>` cannot survive a host
/// restart, but the handle can be stored and later passed to
/// [`SandboxProvider::adopt`] to reconnect to a still-running remote sandbox
/// (k8s pod / container on another host). For a local sandbox it is just the
/// directory id. The closed, versioned payload enum makes every durable locator
/// explicit and rejects unknown or cross-provider shapes during deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxHandle {
    pub provider_kind: String,
    pub sandbox_id: String,
    payload: SandboxHandlePayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "schema", rename_all = "snake_case", deny_unknown_fields)]
enum SandboxHandlePayload {
    Unmanaged,
    LocalV1(LocalSandboxHandleV1),
    NamespaceV1(NamespaceSandboxHandleV1),
    ContainerV1(ContainerSandboxHandleV1),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSandboxHandleV1 {
    pub outputs_path: String,
    pub base_env: Vec<crate::EnvVar>,
    pub continuation_excluded_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceSandboxHandleV1 {
    pub outputs_path: String,
    pub base_env: Vec<crate::EnvVar>,
    pub network: crate::NetworkPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceProviderKind {
    Bubblewrap,
    Seatbelt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerSandboxHandleV1 {
    pub container_id: String,
    pub outputs_path: String,
    pub base_env: Vec<crate::EnvVar>,
    pub live_input_projection: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_handle: Option<ContainerContinuationHandle>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContainerContinuationHandle {
    KubernetesContinuation { claim_uid: String },
}

impl SandboxHandle {
    /// Construct a deliberately non-resumable handle for ephemeral providers and
    /// test doubles. Durable built-in providers use one of the typed constructors.
    pub fn new(provider_kind: impl Into<String>, sandbox_id: impl Into<String>) -> Self {
        Self {
            provider_kind: provider_kind.into(),
            sandbox_id: sandbox_id.into(),
            payload: SandboxHandlePayload::Unmanaged,
        }
    }

    #[must_use]
    pub fn local(sandbox_id: impl Into<String>, payload: LocalSandboxHandleV1) -> Self {
        Self {
            provider_kind: "local".into(),
            sandbox_id: sandbox_id.into(),
            payload: SandboxHandlePayload::LocalV1(payload),
        }
    }

    #[must_use]
    pub fn namespace(
        provider: NamespaceProviderKind,
        sandbox_id: impl Into<String>,
        payload: NamespaceSandboxHandleV1,
    ) -> Self {
        Self {
            provider_kind: match provider {
                NamespaceProviderKind::Bubblewrap => "bwrap",
                NamespaceProviderKind::Seatbelt => "seatbelt",
            }
            .into(),
            sandbox_id: sandbox_id.into(),
            payload: SandboxHandlePayload::NamespaceV1(payload),
        }
    }

    #[must_use]
    pub fn container(sandbox_id: impl Into<String>, payload: ContainerSandboxHandleV1) -> Self {
        Self {
            provider_kind: "container".into(),
            sandbox_id: sandbox_id.into(),
            payload: SandboxHandlePayload::ContainerV1(payload),
        }
    }

    pub fn local_payload(&self) -> Result<&LocalSandboxHandleV1, SandboxError> {
        match (&*self.provider_kind, &self.payload) {
            ("local", SandboxHandlePayload::LocalV1(payload)) => Ok(payload),
            _ => Err(self.payload_mismatch("local")),
        }
    }

    pub fn namespace_payload(
        &self,
        expected_provider: &str,
    ) -> Result<&NamespaceSandboxHandleV1, SandboxError> {
        match (&*self.provider_kind, &self.payload) {
            (provider, SandboxHandlePayload::NamespaceV1(payload))
                if provider == expected_provider =>
            {
                Ok(payload)
            }
            _ => Err(self.payload_mismatch(expected_provider)),
        }
    }

    pub fn container_payload(&self) -> Result<&ContainerSandboxHandleV1, SandboxError> {
        match (&*self.provider_kind, &self.payload) {
            ("container", SandboxHandlePayload::ContainerV1(payload)) => Ok(payload),
            _ => Err(self.payload_mismatch("container")),
        }
    }

    fn payload_mismatch(&self, expected_provider: &str) -> SandboxError {
        SandboxError::new(format!(
            "{expected_provider} provider cannot adopt {:?} handle payload",
            self.provider_kind
        ))
    }
}

/// The lifecycle state of a sandbox, queryable idempotently (survives reconnect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxStatus {
    /// Being realized (image pull, mounts binding).
    Provisioning,
    /// Realized and usable — processes may be spawned.
    Ready,
    /// Torn down, reaped, or lease-expired; no longer usable.
    Terminated,
}

/// Isolation strength, ordered `Workdir < Namespace < Container`. A provider
/// admits a spec only when its class is `>=` the requested one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationClass {
    /// Working-directory selection only; no OS isolation (dev/CI/trusted).
    #[default]
    Workdir,
    /// OS-namespace isolation (bubblewrap / sandbox-exec).
    Namespace,
    /// Full container/VM isolation.
    Container,
}

/// Minimum enforceable Sandbox properties required before a workload may be
/// placed on a Worker. This is the one requirement vocabulary shared by
/// provider admission and distributed Worker placement; it deliberately omits
/// live handles, paths, mounts, credentials, and provider implementation names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxRequirements {
    #[serde(default)]
    pub isolation: IsolationClass,
    #[serde(default)]
    pub tool_transparent: bool,
    #[serde(default)]
    pub path_fidelity: bool,
    #[serde(default)]
    pub enforced_readonly: bool,
    #[serde(default)]
    pub network_isolation: bool,
    #[serde(default)]
    pub enforced_network_allowlist: bool,
    #[serde(default)]
    pub resource_limits: bool,
    #[serde(default)]
    pub custom_rootfs: bool,
    #[serde(default)]
    pub package_provisioning: bool,
}

impl SandboxRequirements {
    /// Derive placement requirements from the exact neutral realization spec.
    /// `opaque_process` is true for ACP and for Native Hand execution because
    /// both must remain correct without cooperative lexical path rewriting.
    #[must_use]
    pub fn from_spec(spec: &SandboxSpec, opaque_process: bool) -> Self {
        use crate::vocab::NetworkPolicy;

        let custom_rootfs = spec.environment.is_some();
        Self {
            isolation: if opaque_process {
                spec.isolation.max(IsolationClass::Namespace)
            } else {
                spec.isolation
            },
            tool_transparent: opaque_process,
            path_fidelity: opaque_process,
            enforced_readonly: spec
                .mounts
                .iter()
                .any(|mount| mount.access == MountAccess::ReadOnly),
            network_isolation: spec.network.is_restricted(),
            enforced_network_allowlist: matches!(spec.network, NetworkPolicy::Allowlist { .. }),
            resource_limits: spec.limits.is_set(),
            custom_rootfs,
            package_provisioning: !spec.packages.is_empty(),
        }
    }
}

/// What a backend can actually enforce — the host probes this to pick a provider
/// and to fail closed when a spec asks for more than a backend can give.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    pub isolation: IsolationClass,
    /// **The load-bearing flag.** True when isolation is OS-enforced on an
    /// arbitrary launched process (so it holds for Claude Code / any CLI); false
    /// for a cooperating-tool-only jail (lexical), which must never host an opaque
    /// agent process.
    pub tool_transparent: bool,
    /// Sandbox-absolute paths are real to launched processes (vs. lexical rewrite).
    pub path_fidelity: bool,
    /// Read-only mounts are OS-enforced.
    pub enforced_readonly: bool,
    /// Egress can be isolated/controlled.
    pub network_isolation: bool,
    /// Host allowlists are enforced for arbitrary workload traffic at a
    /// no-bypass network boundary. A process proxy environment variable is not
    /// sufficient evidence because the workload can remove or ignore it.
    #[serde(default)]
    pub enforced_network_allowlist: bool,
    /// `EnvVisibility::EgressOnly` secrets can be honored.
    pub secret_egress_substitution: bool,
    /// Resource limits are enforced.
    pub resource_limits: bool,
    /// Provides its own userland/rootfs (vs. borrowing the host's binaries).
    pub custom_rootfs: bool,
    /// Can materialize exact package requirements before workload launch and
    /// preserve them across adoption of the same sandbox handle.
    #[serde(default)]
    pub package_provisioning: bool,
}

impl SandboxCapabilities {
    /// One monotonic compatibility predicate used by local provider selection
    /// and remote Worker admission. Ranking policy runs only after this succeeds.
    #[must_use]
    pub fn satisfies_requirements(&self, required: &SandboxRequirements) -> bool {
        capability_requirements_satisfied(
            self.isolation,
            required.isolation,
            self.network_isolation,
            required.network_isolation,
            self.resource_limits,
            required.resource_limits,
        ) && (!required.tool_transparent || self.tool_transparent)
            && (!required.path_fidelity || self.path_fidelity)
            && (!required.enforced_readonly || self.enforced_readonly)
            && (!required.enforced_network_allowlist || self.enforced_network_allowlist)
            && (!required.custom_rootfs || self.custom_rootfs)
            && (!required.package_provisioning || self.package_provisioning)
    }

    /// Whether this provider can keep a real secret outside an arbitrary
    /// workload while forcing traffic through the substitution boundary.
    /// Neither substitution nor an allowlist alone is custody evidence.
    #[must_use]
    pub const fn supports_secret_egress_without_bypass(&self) -> bool {
        self.secret_egress_substitution && self.enforced_network_allowlist
    }

    /// Fail-closed backend selection (ADR-0021 §8): does this backend meet
    /// **everything** `spec` requires? A router filters candidate providers by this
    /// before applying any load/region/affinity policy, so a spec is never placed on
    /// a backend that cannot honor it.
    ///
    /// Matches the two load-bearing axes the vocabulary makes selectable: isolation
    /// class (the provider must *meet or exceed* the requested minimum) and network
    /// isolation (required for anything stricter than
    /// [`NetworkPolicy::Unrestricted`](crate::vocab::NetworkPolicy::Unrestricted)).
    #[must_use]
    pub fn satisfies(&self, spec: &crate::spec::SandboxSpec) -> bool {
        self.satisfies_requirements(&SandboxRequirements::from_spec(spec, false))
    }
}

/// Representation-free admission kernel shared by production provider selection
/// and the bounded proof harnesses. Every load-bearing requirement is conjunctive:
/// adding a requirement can only remove candidates, never make a weaker backend
/// admissible.
#[must_use]
pub const fn capability_requirements_satisfied(
    actual_isolation: IsolationClass,
    required_isolation: IsolationClass,
    has_network_isolation: bool,
    requires_network_isolation: bool,
    has_resource_limits: bool,
    requires_resource_limits: bool,
) -> bool {
    isolation_rank(actual_isolation) >= isolation_rank(required_isolation)
        && (!requires_network_isolation || has_network_isolation)
        && (!requires_resource_limits || has_resource_limits)
}

const fn isolation_rank(class: IsolationClass) -> u8 {
    match class {
        IsolationClass::Workdir => 0,
        IsolationClass::Namespace => 1,
        IsolationClass::Container => 2,
    }
}

/// Whether placing below the requested floor is explicitly authorized. This is
/// kept separate from readiness/ranking so a fail-closed policy can never silently
/// turn into a downgrade while candidate ordering changes.
#[must_use]
pub const fn degradation_is_authorized(
    actual: IsolationClass,
    required: IsolationClass,
    on_unmet: OnUnmet,
) -> bool {
    isolation_rank(actual) >= isolation_rank(required)
        || matches!(on_unmet, OnUnmet::DegradeWithConsent)
}

/// Why no backend could be selected for a spec.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SelectionError {
    /// No configured backend both satisfies the spec and passed its readiness probe.
    #[error("no configured backend can satisfy the requested isolation/network/limits")]
    NoCapableBackend,
}

/// Fail-closed provider selection: the first candidate whose capabilities
/// [`satisfies`](SandboxCapabilities::satisfies) the spec **and** whose
/// [`probe_ready`](SandboxProvider::probe_ready) check passes. It never downgrades to
/// a weaker tier — if nothing qualifies it returns [`SelectionError::NoCapableBackend`]
/// (the host maps this to a `Gated` outcome), so a spec is never silently placed on an
/// under-isolating or unavailable backend. This is the deliberate divergence from a
/// "degrade to a portable scope" policy: in a managed/multi-tenant plane a silent
/// isolation downgrade is a security regression, not a convenience.
pub async fn select_provider<'a>(
    candidates: &'a [Box<dyn SandboxProvider>],
    spec: &SandboxSpec,
) -> Result<&'a dyn SandboxProvider, SelectionError> {
    for provider in candidates {
        if provider.capabilities().satisfies(spec) && provider.probe_ready().await.is_ok() {
            return Ok(provider.as_ref());
        }
    }
    Err(SelectionError::NoCapableBackend)
}

/// What to do when no configured backend meets the isolation floor (ADR-0056 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnUnmet {
    /// Never place below the floor — a silent isolation downgrade is a security
    /// regression, so an unmet floor fails closed (the never-downgrade default).
    FailClosed,
    /// Place on the strongest available weaker tier, but ONLY as a *recorded*
    /// degradation: the caller must emit the audit event + metric + run marker
    /// ([`PolicySelection::degraded_to`]). Degradation becomes representable and
    /// logged, never invisible.
    DegradeWithConsent,
}

/// The isolation floor as a policy input, so one selection mechanism serves two trust
/// models (ADR-0056 §5): local single-user (`require = Workdir`, soft) and multi-tenant
/// hosting (`require = Namespace|Container`, `on_unmet = FailClosed`). The floor is a
/// parameter, not a hardcoded default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsolationPolicy {
    /// The minimum isolation to place on; a weaker backend is used only under
    /// [`OnUnmet::DegradeWithConsent`].
    pub require: IsolationClass,
    /// The preferred isolation when several qualify — the exact-`prefer` tier wins a
    /// tie, else the strongest floor-meeting tier is chosen.
    pub prefer: IsolationClass,
    /// How to handle a spec no backend can place at or above `require`.
    pub on_unmet: OnUnmet,
}

/// The outcome of a policy-driven selection: the chosen provider, and — when the floor
/// could not be met and [`OnUnmet::DegradeWithConsent`] allowed it — the weaker
/// isolation class actually placed on. `degraded_to = Some(..)` obliges the caller to
/// emit the degradation audit event + metric + run marker (never silent).
pub struct PolicySelection<'a> {
    pub provider: &'a dyn SandboxProvider,
    pub degraded_to: Option<IsolationClass>,
}

/// Metadata handed to the one injected checkpoint object adapter. It contains
/// no storage URL, credential, or encryption material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointObjectMetadata {
    /// Workspace ownership scope used by hosted adapters to resolve the tenant
    /// through their existing placement authority. It is not a storage key.
    pub workspace_id: String,
    pub session_id: String,
    pub generation_id: String,
    pub suspend_effect_id: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

/// Result of an atomic object write. The adapter must expose the digest of the
/// exact durable plaintext so providers can verify reads after process loss.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCheckpointObject {
    pub id: String,
    pub digest: String,
    pub size_bytes: u64,
}

/// Region/deployment-owned checkpoint byte custody. A filesystem adapter is
/// suitable for standalone deployments; hosted composition injects encrypted
/// object storage. This is a byte port, not a lifecycle state store.
#[async_trait]
pub trait SandboxCheckpointStore: Send + Sync {
    async fn put(
        &self,
        metadata: &CheckpointObjectMetadata,
        bytes: Vec<u8>,
    ) -> Result<StoredCheckpointObject, SandboxError>;

    async fn get(&self, id: &str) -> Result<Vec<u8>, SandboxError>;

    async fn delete(&self, id: &str) -> Result<(), SandboxError>;
}

/// Exact, provider-neutral request for one idempotent filesystem checkpoint.
///
/// Session lifecycle types deliberately do not cross this port. The Runtime
/// adapter projects its operation/generation into these immutable facts and
/// later wraps the returned artifact in a Session-owned receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxCheckpointRequest {
    pub workspace_id: String,
    pub session_id: String,
    pub generation_id: String,
    pub environment_fingerprint: String,
    pub base_image_fingerprint: String,
    pub effect_id: String,
    pub format: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub max_bytes: u64,
}

/// Opaque, secret-free evidence for one verified durable checkpoint object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCheckpointRef {
    pub id: String,
    pub format: String,
    pub digest: String,
    pub size_bytes: u64,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub environment_fingerprint: String,
    pub base_image_fingerprint: String,
    #[serde(default)]
    pub excluded_mounts: Vec<String>,
    pub suspend_effect_id: String,
}

impl SandboxCheckpointRef {
    #[must_use]
    pub const fn expired_at(&self, now_unix_ms: u64) -> bool {
        now_unix_ms >= self.expires_at_unix_ms
    }
}

// A backend honors the spec's non-isolation requirements (network) — the isolation
// floor is decided by the policy, so it is checked separately here.
fn non_isolation_ok(caps: &SandboxCapabilities, spec: &SandboxSpec) -> bool {
    let network_ok =
        matches!(spec.network, crate::vocab::NetworkPolicy::Unrestricted) || caps.network_isolation;
    // Resource caps are load-bearing like isolation: a spec asking for cgroup limits
    // must not be placed on a tier that cannot enforce them, even under a degrade.
    let limits_ok = !spec.limits.is_set() || caps.resource_limits;
    network_ok && limits_ok
}

/// Fail-closed provider selection with an explicit **policy floor** (ADR-0056 §5). It
/// first places on the strongest ready backend that meets `policy.require` (the exact
/// `prefer` class winning a tie). If none meets the floor, `on_unmet` decides: `FailClosed`
/// returns [`SelectionError::NoCapableBackend`] (the never-downgrade guarantee);
/// `DegradeWithConsent` places on the strongest ready backend *below* the floor and
/// reports `degraded_to` so the caller records the degradation. A spec is never silently
/// placed below its floor.
pub async fn select_provider_with_policy<'a>(
    candidates: &'a [Box<dyn SandboxProvider>],
    spec: &SandboxSpec,
    policy: &IsolationPolicy,
) -> Result<PolicySelection<'a>, SelectionError> {
    // Ready backends meeting the floor (isolation >= require) and the spec's network.
    let mut at_or_above: Vec<&dyn SandboxProvider> = Vec::new();
    // Ready backends below the floor but network-sound — the degrade candidates.
    let mut below: Vec<&dyn SandboxProvider> = Vec::new();
    for provider in candidates {
        let caps = provider.capabilities();
        if !non_isolation_ok(&caps, spec) || provider.probe_ready().await.is_err() {
            continue;
        }
        if caps.isolation >= policy.require {
            at_or_above.push(provider.as_ref());
        } else if degradation_is_authorized(caps.isolation, policy.require, policy.on_unmet) {
            below.push(provider.as_ref());
        }
    }

    if !at_or_above.is_empty() {
        // Prefer the exact `prefer` tier, else the strongest available.
        let chosen = at_or_above
            .iter()
            .find(|p| p.capabilities().isolation == policy.prefer)
            .copied()
            .unwrap_or_else(|| {
                *at_or_above
                    .iter()
                    .max_by_key(|p| p.capabilities().isolation)
                    .expect("non-empty")
            });
        return Ok(PolicySelection {
            provider: chosen,
            degraded_to: None,
        });
    }

    match policy.on_unmet {
        OnUnmet::FailClosed => Err(SelectionError::NoCapableBackend),
        OnUnmet::DegradeWithConsent => below
            .iter()
            .max_by_key(|p| p.capabilities().isolation)
            .map(|p| PolicySelection {
                provider: *p,
                degraded_to: Some(p.capabilities().isolation),
            })
            .ok_or(SelectionError::NoCapableBackend),
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn isolation(tag: u8) -> IsolationClass {
        match tag % 3 {
            0 => IsolationClass::Workdir,
            1 => IsolationClass::Namespace,
            _ => IsolationClass::Container,
        }
    }

    #[kani::proof]
    fn sandbox_admission_never_weakens_the_isolation_floor() {
        let actual = isolation(kani::any());
        let required = isolation(kani::any());
        let admitted = capability_requirements_satisfied(
            actual,
            required,
            kani::any(),
            kani::any(),
            kani::any(),
            kani::any(),
        );
        if admitted {
            assert!(isolation_rank(actual) >= isolation_rank(required));
        }
    }

    #[kani::proof]
    fn sandbox_admission_requires_every_requested_capability() {
        let has_network = kani::any();
        let needs_network = kani::any();
        let has_limits = kani::any();
        let needs_limits = kani::any();
        let admitted = capability_requirements_satisfied(
            isolation(kani::any()),
            IsolationClass::Workdir,
            has_network,
            needs_network,
            has_limits,
            needs_limits,
        );
        if admitted {
            assert!(!needs_network || has_network);
            assert!(!needs_limits || has_limits);
        }
    }

    #[kani::proof]
    fn fail_closed_sandbox_policy_never_authorizes_a_downgrade() {
        let actual = isolation(kani::any());
        let required = isolation(kani::any());
        if degradation_is_authorized(actual, required, OnUnmet::FailClosed) {
            assert!(isolation_rank(actual) >= isolation_rank(required));
        }
    }
}

/// Realizes environments. The local impl lives in `awaken-sandbox-local`; a
/// remote/container impl lives in a distributed repo and plugs in here.
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    /// What this backend can enforce (probed at startup for selection).
    fn capabilities(&self) -> SandboxCapabilities;

    /// Portable filesystem checkpoint formats this concrete provider fully
    /// implements. Worker composition projects these into the existing
    /// `WorkerManifest.checkpoint_formats` authority; no second capability field
    /// is maintained on `SandboxCapabilities`.
    fn checkpoint_formats(&self) -> Vec<String> {
        Vec::new()
    }

    /// A cheap liveness probe run at selection time: `Ok` iff this backend is
    /// actually usable *right now* — bwrap/unprivileged-userns available, a container
    /// daemon reachable, etc. The default assumes readiness; the namespace/container
    /// providers override it with a real check so `select_provider` fails closed
    /// (never a silent unisolated run) rather than deferring the failure to `create`.
    async fn probe_ready(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Realize a validated spec into a live sandbox (bind mounts, apply ro/env/
    /// network/limits). Callers should validate first via `prepare_environment`.
    async fn create(&self, spec: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError>;

    /// Reconnect to an already-realized sandbox from a persisted [`SandboxHandle`]
    /// — the recovery path after a host restart, and the takeover path across
    /// hosts. For a local backend this re-opens the directory; for a remote one it
    /// rebuilds a client against the still-running pod/container.
    async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError>;

    /// Create a distinct environment from one verified filesystem checkpoint.
    /// The default fails closed so out-of-tree providers cannot accidentally
    /// advertise continuation without implementing it.
    async fn restore(
        &self,
        _spec: &SandboxSpec,
        _checkpoint: &SandboxCheckpointRef,
        _store: &dyn SandboxCheckpointStore,
    ) -> Result<Box<dyn Sandbox>, SandboxError> {
        Err(SandboxError::new(
            "sandbox provider does not implement checkpoint restore",
        ))
    }
}

/// A live sandbox environment. **Execute** (`spawn`), **mount/inject** (`attach`),
/// **retrieve** (`artifacts`/`read_artifact`), and — for sandboxes that outlive the
/// owning host — **reconnect** (`handle`/`process`), **observe** (`status`), and
/// **keep alive** (`renew_lease`). `spawn` is primary and tool-transparent: the
/// runtime's `RawTool` model is a separate crate's adapter over `spawn`, not a
/// method here.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// The environment id (= the spec scope).
    fn id(&self) -> &str;

    /// A durable, serializable reference for reconnecting later (persist this).
    fn handle(&self) -> SandboxHandle;

    /// Persist the complete mutable filesystem before disposal. Implementations
    /// must omit independently governed mounts and credential material, enforce
    /// `max_bytes`, and return only after the object adapter reports durability.
    async fn checkpoint(
        &self,
        _request: &SandboxCheckpointRequest,
        _store: &dyn SandboxCheckpointStore,
    ) -> Result<SandboxCheckpointRef, SandboxError> {
        Err(SandboxError::new(
            "sandbox does not implement filesystem checkpointing",
        ))
    }

    /// **EXECUTE** — launch any process under OS-enforced isolation. Isolation is
    /// transparent to what the process does inside.
    async fn spawn(&self, command: Command) -> Result<Box<dyn ProcessHandle>, SandboxError>;

    /// **INJECT** — attach a mount after creation (mirrors adding a session
    /// resource). Fails closed when the backend cannot honor the access mode.
    async fn attach(&self, req: MountRequirement) -> Result<RealizedMount, SandboxError>;

    /// **RETRIEVE (list)** — artifacts the agent wrote under the outputs path.
    /// The backend decides how (directory scan / copy-out / volume read).
    async fn artifacts(&self) -> Result<Vec<Artifact>, SandboxError>;

    /// **RETRIEVE (read)** — the bytes of one artifact by id.
    async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, SandboxError>;

    /// The mounts realized so far — logical refs + content hashes (G3), for audit
    /// and replay.
    fn realized(&self) -> &[RealizedMount];

    /// Reconnect to a process launched earlier in this sandbox, by its id — the
    /// recovery path after a dropped connection or host restart (pair with
    /// [`ProcessHandle::poll`] to learn its outcome idempotently).
    async fn process(&self, process_id: &str) -> Result<Box<dyn ProcessHandle>, SandboxError>;

    /// The sandbox's current lifecycle state — an idempotent query, safe to call
    /// from any host after a reconnect.
    async fn status(&self) -> Result<SandboxStatus, SandboxError>;

    /// Renew the lease (the dead-man's switch). The owner calls this within the
    /// spec's `lease_ttl_secs`; if the owner vanishes and the lease expires, the
    /// backend reaps the sandbox. A local backend implements this as a no-op.
    async fn renew_lease(&self) -> Result<(), SandboxError>;

    /// Tear down the environment. Idempotent; `Durable` mounts persist.
    async fn dispose(&self) -> Result<(), SandboxError>;
}

/// A handle to a process launched by [`Sandbox::spawn`]. Lifecycle only — piped
/// stdio (for a protocol bridge such as ACP) is exposed by the provider's own
/// handle type, so the neutral contract needn't bind an async-IO abstraction.
#[async_trait]
pub trait ProcessHandle: Send + Sync {
    /// Provider-assigned process id.
    fn id(&self) -> &str;

    /// Await exit. Over a lossy transport this connection may drop mid-run; treat a
    /// transport error as "unknown" and re-establish via [`Sandbox::process`] +
    /// [`ProcessHandle::poll`] rather than assuming failure.
    async fn wait(&self) -> Result<ExitStatus, SandboxError>;

    /// Non-blocking, idempotent status: `None` while still running, `Some(status)`
    /// once exited. Safe to call repeatedly from any host after a reconnect — this
    /// is how you resolve an indeterminate outcome without re-running the process.
    async fn poll(&self) -> Result<Option<ExitStatus>, SandboxError>;

    /// Deliver a signal (terminate/kill/interrupt). This operation is idempotent:
    /// if the owned process exits before or during delivery, implementations return
    /// success after confirming that exit. Tearing down the sandbox reaps the whole
    /// process group regardless.
    async fn signal(&self, signal: Signal) -> Result<(), SandboxError>;
}

/// How a launched process ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    /// Exit code, when it exited normally.
    pub code: Option<i32>,
    /// True when terminated by a signal.
    pub signaled: bool,
}

/// A signal to deliver to a launched process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// Graceful terminate (SIGTERM).
    Term,
    /// Force kill (SIGKILL).
    Kill,
    /// Interrupt (SIGINT).
    Int,
}

#[cfg(test)]
mod tests {
    //! A trivial fake exercises the full lifecycle — create → persist handle →
    //! (simulated host restart) adopt → spawn → poll → renew_lease → dispose —
    //! which also proves the ports stay object-safe (`Box<dyn …>`).

    use super::*;
    use crate::spec::{Command, SandboxSpec};
    use crate::vocab::NetworkPolicy;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct FakeProcess {
        id: String,
    }

    #[async_trait]
    impl ProcessHandle for FakeProcess {
        fn id(&self) -> &str {
            &self.id
        }
        async fn wait(&self) -> Result<ExitStatus, SandboxError> {
            Ok(ExitStatus {
                code: Some(0),
                signaled: false,
            })
        }
        async fn poll(&self) -> Result<Option<ExitStatus>, SandboxError> {
            Ok(Some(ExitStatus {
                code: Some(0),
                signaled: false,
            }))
        }
        async fn signal(&self, _signal: Signal) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    struct FakeSandbox {
        id: String,
        renews: Arc<AtomicU32>,
    }

    #[async_trait]
    impl Sandbox for FakeSandbox {
        fn id(&self) -> &str {
            &self.id
        }
        fn handle(&self) -> SandboxHandle {
            SandboxHandle::new("fake", &self.id)
        }
        async fn spawn(&self, _command: Command) -> Result<Box<dyn ProcessHandle>, SandboxError> {
            Ok(Box::new(FakeProcess {
                id: "proc-1".into(),
            }))
        }
        async fn attach(&self, _req: MountRequirement) -> Result<RealizedMount, SandboxError> {
            Err(SandboxError::new("fake has no mounts"))
        }
        async fn artifacts(&self) -> Result<Vec<Artifact>, SandboxError> {
            Ok(Vec::new())
        }
        async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, SandboxError> {
            Ok(Vec::new())
        }
        fn realized(&self) -> &[RealizedMount] {
            &[]
        }
        async fn process(&self, process_id: &str) -> Result<Box<dyn ProcessHandle>, SandboxError> {
            Ok(Box::new(FakeProcess {
                id: process_id.into(),
            }))
        }
        async fn status(&self) -> Result<SandboxStatus, SandboxError> {
            Ok(SandboxStatus::Ready)
        }
        async fn renew_lease(&self) -> Result<(), SandboxError> {
            self.renews.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn dispose(&self) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    struct FakeProvider {
        renews: Arc<AtomicU32>,
    }

    #[async_trait]
    impl SandboxProvider for FakeProvider {
        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                isolation: IsolationClass::Workdir,
                tool_transparent: false,
                path_fidelity: false,
                enforced_readonly: false,
                network_isolation: false,
                enforced_network_allowlist: false,
                secret_egress_substitution: false,
                resource_limits: false,
                custom_rootfs: false,
                package_provisioning: false,
            }
        }
        async fn create(&self, spec: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError> {
            Ok(Box::new(FakeSandbox {
                id: spec.scope.clone(),
                renews: self.renews.clone(),
            }))
        }
        async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError> {
            Ok(Box::new(FakeSandbox {
                id: handle.sandbox_id.clone(),
                renews: self.renews.clone(),
            }))
        }
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            scope: "thread-1".into(),
            isolation: IsolationClass::Workdir,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: crate::FilesystemContinuity::Retained,
            lease_ttl_secs: Some(60),
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
        }
    }

    fn caps(isolation: IsolationClass, network_isolation: bool) -> SandboxCapabilities {
        SandboxCapabilities {
            isolation,
            tool_transparent: true,
            path_fidelity: true,
            enforced_readonly: true,
            network_isolation,
            enforced_network_allowlist: network_isolation,
            secret_egress_substitution: true,
            resource_limits: true,
            custom_rootfs: false,
            package_provisioning: false,
        }
    }

    #[test]
    fn sandbox_requirement_derivation_and_admission_decision_table() {
        // Cause/effect graph:
        // C1=opaque child; C2=requested isolation; C3=read-only mount;
        // C4=restricted/allowlisted network; C5=limits; C6=custom rootfs;
        // C7=packages. Effects: E1=minimum monotonic requirement vector;
        // E2=one capability predicate accepts every axis; E3=missing any required
        // axis rejects. Constraints: opaque raises isolation to Namespace and
        // requires transparent paths; Allowlist implies network isolation.
        //
        // Decision table:
        // R1 !C1&&!C2..C7 -> Workdir requirement, basic provider accepts.
        // R2 C1 -> Namespace+transparent+path-fidelity.
        // R3 C2..C7 -> every declared enforcement bit is required.
        // R4 R3 and one missing capability -> reject; full vector -> accept.
        let bare = spec();
        let r1 = SandboxRequirements::from_spec(&bare, false);
        assert_eq!(r1, SandboxRequirements::default(), "R1");

        let r2 = SandboxRequirements::from_spec(&bare, true);
        assert_eq!(r2.isolation, IsolationClass::Namespace, "R2 isolation");
        assert!(r2.tool_transparent && r2.path_fidelity, "R2 paths");

        let mut demanding = bare;
        demanding.isolation = IsolationClass::Container;
        demanding.mounts.push(crate::vocab::MountRequirement {
            mount_id: "input".into(),
            source: crate::vocab::MountSource::File {
                file_id: "file".into(),
                content_hash: None,
            },
            mount_path: "/workspace/input".into(),
            access: crate::vocab::MountAccess::ReadOnly,
            lifetime: crate::vocab::MountLifetime::Session,
            required: true,
        });
        demanding.network = NetworkPolicy::Allowlist {
            hosts: vec!["api.example.test".into()],
        };
        demanding.limits.memory_bytes = Some(64 * 1024 * 1024);
        demanding
            .packages
            .managers
            .insert("npm".into(), vec!["tsx@4".into()]);
        demanding.environment = Some(crate::EnvironmentKind::Image {
            reference: "image@sha256:1".into(),
        });
        let r3 = SandboxRequirements::from_spec(&demanding, true);
        assert_eq!(r3.isolation, IsolationClass::Container, "R3 isolation");
        assert!(
            r3.tool_transparent
                && r3.path_fidelity
                && r3.enforced_readonly
                && r3.network_isolation
                && r3.enforced_network_allowlist
                && r3.resource_limits
                && r3.custom_rootfs
                && r3.package_provisioning,
            "R3 vector: {r3:?}"
        );

        let mut full = caps(IsolationClass::Container, true);
        full.custom_rootfs = true;
        full.package_provisioning = true;
        assert!(full.satisfies_requirements(&r3), "R4 full");
        for missing in [
            "tool_transparent",
            "path_fidelity",
            "enforced_readonly",
            "network_isolation",
            "enforced_network_allowlist",
            "resource_limits",
            "custom_rootfs",
            "package_provisioning",
        ] {
            let mut weak = full.clone();
            match missing {
                "tool_transparent" => weak.tool_transparent = false,
                "path_fidelity" => weak.path_fidelity = false,
                "enforced_readonly" => weak.enforced_readonly = false,
                "network_isolation" => weak.network_isolation = false,
                "enforced_network_allowlist" => weak.enforced_network_allowlist = false,
                "resource_limits" => weak.resource_limits = false,
                "custom_rootfs" => weak.custom_rootfs = false,
                "package_provisioning" => weak.package_provisioning = false,
                _ => unreachable!(),
            }
            assert!(!weak.satisfies_requirements(&r3), "R4 missing {missing}");
        }
    }

    #[test]
    fn satisfies_requires_meeting_or_exceeding_isolation() {
        let mut s = spec();
        s.isolation = IsolationClass::Namespace;
        // exact and stronger classes satisfy
        assert!(caps(IsolationClass::Namespace, false).satisfies(&s));
        assert!(caps(IsolationClass::Container, false).satisfies(&s));
        // weaker fails closed
        assert!(!caps(IsolationClass::Workdir, false).satisfies(&s));
    }

    #[test]
    fn satisfies_requires_network_isolation_for_restricted_egress() {
        let mut s = spec();
        s.network = NetworkPolicy::None;
        assert!(!caps(IsolationClass::Workdir, false).satisfies(&s));
        assert!(caps(IsolationClass::Workdir, true).satisfies(&s));
        // unrestricted egress needs no network isolation
        s.network = NetworkPolicy::Unrestricted;
        assert!(caps(IsolationClass::Workdir, false).satisfies(&s));
    }

    #[test]
    fn satisfies_requires_resource_limit_enforcement_when_limits_are_set() {
        let mut s = spec();
        s.limits.memory_bytes = Some(256 * 1024 * 1024);
        // A tier that can't cgroup fails closed; one that can passes.
        let mut weak = caps(IsolationClass::Workdir, false);
        weak.resource_limits = false;
        assert!(!weak.satisfies(&s));
        let mut strong = caps(IsolationClass::Workdir, false);
        strong.resource_limits = true;
        assert!(strong.satisfies(&s));
        // Unset limits don't require enforcement.
        s.limits = Default::default();
        assert!(weak.satisfies(&s));
    }

    #[test]
    fn satisfies_requires_network_isolation_for_an_allowlist_too() {
        // An Allowlist is restricted (rank 1), so it needs network isolation just like
        // `None` — the middle egress class the other satisfies tests skipped.
        let mut s = spec();
        s.network = NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        };
        assert!(!caps(IsolationClass::Workdir, false).satisfies(&s));
        assert!(caps(IsolationClass::Workdir, true).satisfies(&s));
    }

    #[test]
    fn satisfies_rejects_isolation_without_enforced_allowlist() {
        let mut s = spec();
        s.network = NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        };
        let mut isolated = caps(IsolationClass::Container, true);
        isolated.enforced_network_allowlist = false;
        assert!(!isolated.satisfies(&s));
        isolated.enforced_network_allowlist = true;
        assert!(isolated.satisfies(&s));
    }

    /// A provider whose readiness probe can be toggled, to exercise `select_provider`.
    struct ProbeProvider {
        caps: SandboxCapabilities,
        ready: bool,
    }
    #[async_trait]
    impl SandboxProvider for ProbeProvider {
        fn capabilities(&self) -> SandboxCapabilities {
            self.caps.clone()
        }
        async fn probe_ready(&self) -> Result<(), SandboxError> {
            if self.ready {
                Ok(())
            } else {
                Err(SandboxError::new("backend not ready"))
            }
        }
        async fn create(&self, spec: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError> {
            Ok(Box::new(FakeSandbox {
                id: spec.scope.clone(),
                renews: Arc::new(AtomicU32::new(0)),
            }))
        }
        async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError> {
            Ok(Box::new(FakeSandbox {
                id: handle.sandbox_id.clone(),
                renews: Arc::new(AtomicU32::new(0)),
            }))
        }
    }

    #[tokio::test]
    async fn select_provider_picks_the_first_capable_and_ready_backend() {
        let mut s = spec();
        s.isolation = IsolationClass::Namespace;
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![
            // capable but not ready → skipped
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Container, true),
                ready: false,
            }),
            // capable AND ready → chosen
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Namespace, true),
                ready: true,
            }),
        ];
        let chosen = select_provider(&candidates, &s).await.unwrap();
        assert_eq!(chosen.capabilities().isolation, IsolationClass::Namespace);
    }

    #[tokio::test]
    async fn select_provider_fails_closed_rather_than_downgrading() {
        let mut s = spec();
        s.isolation = IsolationClass::Container;
        // Only an under-isolating and a not-ready backend are offered.
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Namespace, true),
                ready: true,
            }),
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Container, true),
                ready: false,
            }),
        ];
        let result = select_provider(&candidates, &s).await;
        assert!(matches!(result, Err(SelectionError::NoCapableBackend)));
    }

    #[tokio::test]
    async fn select_provider_with_no_candidates_fails_closed() {
        // Boundary: an empty candidate list never downgrades to an unisolated run.
        let candidates: Vec<Box<dyn SandboxProvider>> = Vec::new();
        let result = select_provider(&candidates, &spec()).await;
        assert!(matches!(result, Err(SelectionError::NoCapableBackend)));
    }

    // --- IsolationPolicy (ADR-0056 §5): the floor is a policy input. Decision table
    // over (floor met? × on_unmet × candidates), preserving never-downgrade for
    // FailClosed and making DegradeWithConsent a recorded, non-silent placement.

    fn policy(
        require: IsolationClass,
        prefer: IsolationClass,
        on_unmet: OnUnmet,
    ) -> IsolationPolicy {
        IsolationPolicy {
            require,
            prefer,
            on_unmet,
        }
    }

    #[tokio::test]
    async fn policy_places_at_the_floor_and_prefers_the_preferred_tier() {
        // require=Namespace floor is met by both Namespace and Container; prefer=Namespace
        // picks the exact preferred tier (not the strongest), and it is not degraded.
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Container, true),
                ready: true,
            }),
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Namespace, true),
                ready: true,
            }),
        ];
        let sel = select_provider_with_policy(
            &candidates,
            &spec(),
            &policy(
                IsolationClass::Namespace,
                IsolationClass::Namespace,
                OnUnmet::FailClosed,
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            sel.provider.capabilities().isolation,
            IsolationClass::Namespace
        );
        assert_eq!(
            sel.degraded_to, None,
            "a floor-meeting placement is not a degrade"
        );
    }

    #[tokio::test]
    async fn policy_fail_closed_refuses_when_the_floor_is_unmet() {
        // require=Container, only Namespace available, FailClosed → never downgrade.
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![Box::new(ProbeProvider {
            caps: caps(IsolationClass::Namespace, true),
            ready: true,
        })];
        let result = select_provider_with_policy(
            &candidates,
            &spec(),
            &policy(
                IsolationClass::Container,
                IsolationClass::Container,
                OnUnmet::FailClosed,
            ),
        )
        .await;
        assert!(matches!(result, Err(SelectionError::NoCapableBackend)));
    }

    #[tokio::test]
    async fn policy_degrade_with_consent_places_below_the_floor_and_records_it() {
        // require=Container, only Namespace + Workdir available, DegradeWithConsent →
        // the STRONGEST below-floor tier (Namespace) is placed, and degraded_to reports
        // it so the caller emits the audit/metric/marker.
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Workdir, true),
                ready: true,
            }),
            Box::new(ProbeProvider {
                caps: caps(IsolationClass::Namespace, true),
                ready: true,
            }),
        ];
        let sel = select_provider_with_policy(
            &candidates,
            &spec(),
            &policy(
                IsolationClass::Container,
                IsolationClass::Container,
                OnUnmet::DegradeWithConsent,
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            sel.provider.capabilities().isolation,
            IsolationClass::Namespace
        );
        assert_eq!(
            sel.degraded_to,
            Some(IsolationClass::Namespace),
            "a consented degrade is recorded, never silent"
        );
    }

    #[tokio::test]
    async fn policy_degrade_with_consent_still_fails_closed_with_no_backend_at_all() {
        // Even DegradeWithConsent cannot place a run with zero ready backends.
        let candidates: Vec<Box<dyn SandboxProvider>> = Vec::new();
        let result = select_provider_with_policy(
            &candidates,
            &spec(),
            &policy(
                IsolationClass::Container,
                IsolationClass::Container,
                OnUnmet::DegradeWithConsent,
            ),
        )
        .await;
        assert!(matches!(result, Err(SelectionError::NoCapableBackend)));
    }

    #[tokio::test]
    async fn policy_excludes_a_backend_that_cannot_enforce_the_specs_limits() {
        // A spec asking for cgroup limits must not be placed on a tier that cannot
        // enforce them — even under a strong isolation class, `non_isolation_ok` bars it.
        let mut s = spec();
        s.limits.memory_bytes = Some(512 * 1024 * 1024);
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![Box::new(ProbeProvider {
            caps: SandboxCapabilities {
                resource_limits: false, // strong isolation but cannot enforce limits
                ..caps(IsolationClass::Container, true)
            },
            ready: true,
        })];
        let result = select_provider_with_policy(
            &candidates,
            &s,
            &policy(
                IsolationClass::Workdir,
                IsolationClass::Workdir,
                OnUnmet::DegradeWithConsent,
            ),
        )
        .await;
        assert!(
            matches!(result, Err(SelectionError::NoCapableBackend)),
            "a limits-incapable backend is excluded even from the degrade set"
        );
    }

    #[tokio::test]
    async fn policy_excludes_a_backend_that_cannot_meet_the_specs_network() {
        // A restricted-egress spec needs network isolation; a non-isolating backend is
        // excluded from the floor set even if its isolation class qualifies.
        let mut s = spec();
        s.network = NetworkPolicy::None;
        let candidates: Vec<Box<dyn SandboxProvider>> = vec![Box::new(ProbeProvider {
            caps: caps(IsolationClass::Container, false), // no network isolation
            ready: true,
        })];
        let result = select_provider_with_policy(
            &candidates,
            &s,
            &policy(
                IsolationClass::Workdir,
                IsolationClass::Workdir,
                OnUnmet::FailClosed,
            ),
        )
        .await;
        assert!(matches!(result, Err(SelectionError::NoCapableBackend)));
    }

    #[test]
    fn every_sandbox_status_variant_round_trips_on_the_wire() {
        // Status crosses the reconnect boundary (queried idempotently after a takeover),
        // so its wire tags are load-bearing. `Provisioning`/`Terminated` were never
        // exercised (fakes always return `Ready`).
        for (s, tag) in [
            (SandboxStatus::Provisioning, "provisioning"),
            (SandboxStatus::Ready, "ready"),
            (SandboxStatus::Terminated, "terminated"),
        ] {
            let wire = serde_json::to_string(&s).unwrap();
            assert_eq!(wire, format!("\"{tag}\""));
            assert_eq!(serde_json::from_str::<SandboxStatus>(&wire).unwrap(), s);
        }
    }

    /// Cause-effect graph for mediated secret custody:
    ///
    /// C1 provider substitutes the secret at egress
    /// C2 provider enforces a no-bypass target allowlist
    /// E1 real material may remain outside the workload iff C1 AND C2.
    ///
    /// | Rule | C1 substitution | C2 no-bypass | E1 custody evidence |
    /// |---|---|---|---|
    /// | E1 | F | F | F |
    /// | E2 | T | F | F |
    /// | E3 | F | T | F |
    /// | E4 | T | T | T |
    #[test]
    fn secret_egress_custody_requires_substitution_and_no_bypass() {
        for (substitution, no_bypass, expected) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, true),
        ] {
            let mut capabilities = caps(IsolationClass::Container, true);
            capabilities.secret_egress_substitution = substitution;
            capabilities.enforced_network_allowlist = no_bypass;
            assert_eq!(
                capabilities.supports_secret_egress_without_bypass(),
                expected,
                "substitution={substitution}, no_bypass={no_bypass}"
            );
        }
    }

    #[test]
    fn a_signal_killed_exit_status_round_trips() {
        // A process reaped by a signal has no exit code and `signaled = true` — the
        // shape every fake elided by always returning `code: Some(0)`.
        let killed = ExitStatus {
            code: None,
            signaled: true,
        };
        let wire = serde_json::to_string(&killed).unwrap();
        assert_eq!(serde_json::from_str::<ExitStatus>(&wire).unwrap(), killed);
        assert!(killed.code.is_none() && killed.signaled);
    }

    #[test]
    fn every_signal_variant_round_trips_on_the_wire() {
        // Only `Term` was ever delivered in a test; pin all three wire tags.
        for (sig, tag) in [
            (Signal::Term, "term"),
            (Signal::Kill, "kill"),
            (Signal::Int, "int"),
        ] {
            let wire = serde_json::to_string(&sig).unwrap();
            assert_eq!(wire, format!("\"{tag}\""));
            assert_eq!(serde_json::from_str::<Signal>(&wire).unwrap(), sig);
        }
    }

    #[tokio::test]
    async fn default_probe_ready_is_ok() {
        let provider = FakeProvider {
            renews: Arc::new(AtomicU32::new(0)),
        };
        assert!(provider.probe_ready().await.is_ok());
    }

    #[tokio::test]
    async fn create_persist_adopt_poll_lease_lifecycle() {
        let renews = Arc::new(AtomicU32::new(0));
        let provider: Box<dyn SandboxProvider> = Box::new(FakeProvider {
            renews: renews.clone(),
        });

        // Create, then persist the durable handle (as a host would to its store).
        let sandbox = provider.create(&spec()).await.unwrap();
        let handle = sandbox.handle();
        let wire = serde_json::to_string(&handle).unwrap(); // handle is serializable
        drop(sandbox); // simulate the owning host process going away

        // Recovery: reconnect from the persisted handle alone.
        let recovered: SandboxHandle = serde_json::from_str(&wire).unwrap();
        let sandbox = provider.adopt(&recovered).await.unwrap();
        assert_eq!(sandbox.id(), "thread-1");
        assert!(matches!(
            sandbox.status().await.unwrap(),
            SandboxStatus::Ready
        ));

        // Launch an opaque process (e.g. Claude Code), then resolve its outcome
        // idempotently via poll — the reconnect-safe path.
        let proc = sandbox
            .spawn(Command::new(["claude", "--acp"]))
            .await
            .unwrap();
        let reattached = sandbox.process(proc.id()).await.unwrap();
        assert!(matches!(
            reattached.poll().await.unwrap(),
            Some(ExitStatus { code: Some(0), .. })
        ));

        sandbox.renew_lease().await.unwrap();
        assert_eq!(renews.load(Ordering::SeqCst), 1);
        sandbox.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn fixtures_conform_to_the_ports() {
        // The selection/lifecycle tests above don't drive every port method; assert
        // the fakes are well-formed contract impls (a valid `Sandbox`/`ProcessHandle`/
        // `SandboxProvider`) so the parts they *do* rely on rest on a sound fixture.
        let sb = FakeSandbox {
            id: "s".into(),
            renews: Arc::new(AtomicU32::new(0)),
        };
        assert_eq!(sb.id(), "s");
        assert!(sb.attach(a_mount()).await.is_err());
        assert!(sb.artifacts().await.unwrap().is_empty());
        assert!(sb.read_artifact("x").await.unwrap().is_empty());
        assert!(sb.realized().is_empty());

        let proc = FakeProcess { id: "p".into() };
        assert_eq!(proc.id(), "p");
        assert_eq!(proc.wait().await.unwrap().code, Some(0));
        proc.signal(Signal::Term).await.unwrap();

        // The Workdir fake provider (used by `default_probe_ready_is_ok`) and the
        // toggleable ProbeProvider both realize the same ports.
        let fp = FakeProvider {
            renews: Arc::new(AtomicU32::new(0)),
        };
        assert_eq!(fp.capabilities().isolation, IsolationClass::Workdir);

        let pp = ProbeProvider {
            caps: caps(IsolationClass::Container, true),
            ready: true,
        };
        assert!(pp.create(&spec()).await.is_ok());
        assert_eq!(
            pp.adopt(&SandboxHandle::new("k", "id")).await.unwrap().id(),
            "id"
        );
    }

    fn a_mount() -> MountRequirement {
        MountRequirement {
            mount_id: "m".into(),
            source: crate::vocab::MountSource::File {
                file_id: "f".into(),
                content_hash: None,
            },
            mount_path: "/workspace/x".into(),
            access: crate::vocab::MountAccess::ReadOnly,
            lifetime: crate::vocab::MountLifetime::PerRun,
            required: false,
        }
    }
}
