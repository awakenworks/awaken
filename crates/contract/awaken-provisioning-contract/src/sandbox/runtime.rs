use super::*;


#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSandboxHandleV1 {
    pub outputs_path: String,
    pub base_env: Vec<crate::EnvVar>,
    pub continuation_excluded_paths: Vec<String>,
    /// Workdir egress posture is part of the durable projection. Older handles
    /// default to the historical permissive posture; a restored handle is also
    /// fenced by its complete SandboxSpec fingerprint.
    #[serde(default)]
    pub deny_tool_egress: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceSandboxHandleV1 {
    pub outputs_path: String,
    pub base_env: Vec<crate::EnvVar>,
    pub network: crate::NetworkPolicy,
    /// Exact provider control topology realized when this handle was created.
    /// It is recovery evidence, not a replacement demand authority.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub control_services:
        std::collections::BTreeSet<awaken_sandbox_control::SandboxControlServiceKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceProviderKind {
    Bubblewrap,
    Seatbelt,
}

impl NamespaceProviderKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bubblewrap => "bwrap",
            Self::Seatbelt => "seatbelt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerSandboxHandleV1 {
    pub container_id: String,
    pub outputs_path: String,
    pub base_env: Vec<crate::EnvVar>,
    pub live_input_projection: bool,
    /// Sandbox-absolute paths excluded from a later mutable-layer checkpoint.
    /// The creating provider freezes this evidence so adoption never has to
    /// rediscover independently governed mounts from ambient infrastructure.
    #[serde(default)]
    pub continuation_excluded_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_handle: Option<ContainerContinuationHandle>,
    /// Exact provider runtime incarnation which owned any published Sandbox
    /// control service. Older handles omit it; adoption of a newly demanded
    /// service then fails closed instead of trusting an ambient same-name
    /// runtime object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_control_incarnation: Option<SandboxControlIncarnation>,
    /// Exact provider control topology realized alongside the incarnation.
    /// Empty legacy handles remain ordinary; adoption never infers a demand.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub control_services:
        std::collections::BTreeSet<awaken_sandbox_control::SandboxControlServiceKind>,
}

/// Runtime-owned incarnation evidence carried through Worker adoption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContainerContinuationHandle {
    KubernetesContinuation { claim_uid: String },
    HostBindRestoration(HostBindRestorationHandle),
}

impl ContainerContinuationHandle {
    #[must_use]
    pub const fn is_host_bind_restoration(&self) -> bool {
        matches!(self, Self::HostBindRestoration(_))
    }
}

impl SandboxHandle {
    /// Construct a deliberately non-resumable handle for ephemeral providers and
    /// test doubles. Durable built-in providers use one of the typed constructors.
    pub fn new(provider_kind: impl Into<String>, sandbox_id: impl Into<String>) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            restoration: None,
            payload: SandboxHandlePayload::Unmanaged {
                provider_kind: provider_kind.into(),
            },
        }
    }

    #[must_use]
    pub fn local(sandbox_id: impl Into<String>, payload: LocalSandboxHandleV1) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            restoration: None,
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
            sandbox_id: sandbox_id.into(),
            restoration: None,
            payload: match provider {
                NamespaceProviderKind::Bubblewrap => SandboxHandlePayload::BubblewrapV1(payload),
                NamespaceProviderKind::Seatbelt => SandboxHandlePayload::SeatbeltV1(payload),
            },
        }
    }

    #[must_use]
    pub fn container(sandbox_id: impl Into<String>, payload: ContainerSandboxHandleV1) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            restoration: None,
            payload: SandboxHandlePayload::ContainerV1(payload),
        }
    }

    #[must_use]
    pub const fn restoration(&self) -> Option<&SandboxRestorationEvidence> {
        self.restoration.as_ref()
    }

    pub fn local_payload(&self) -> Result<&LocalSandboxHandleV1, SandboxError> {
        match &self.payload {
            SandboxHandlePayload::LocalV1(payload) => Ok(payload),
            _ => Err(self.payload_mismatch("local")),
        }
    }

    pub fn namespace_payload(
        &self,
        expected_provider: NamespaceProviderKind,
    ) -> Result<&NamespaceSandboxHandleV1, SandboxError> {
        match (expected_provider, &self.payload) {
            (NamespaceProviderKind::Bubblewrap, SandboxHandlePayload::BubblewrapV1(payload))
            | (NamespaceProviderKind::Seatbelt, SandboxHandlePayload::SeatbeltV1(payload)) => {
                Ok(payload)
            }
            _ => Err(self.payload_mismatch(expected_provider.as_str())),
        }
    }

    pub fn container_payload(&self) -> Result<&ContainerSandboxHandleV1, SandboxError> {
        match &self.payload {
            SandboxHandlePayload::ContainerV1(payload) => Ok(payload),
            _ => Err(self.payload_mismatch("container")),
        }
    }

    #[must_use]
    pub fn provider_kind(&self) -> &str {
        match &self.payload {
            SandboxHandlePayload::Unmanaged { provider_kind } => provider_kind,
            SandboxHandlePayload::LocalV1(_) => "local",
            SandboxHandlePayload::BubblewrapV1(_) => "bwrap",
            SandboxHandlePayload::SeatbeltV1(_) => "seatbelt",
            SandboxHandlePayload::ContainerV1(_) => "container",
        }
    }

    fn payload_mismatch(&self, expected_provider: &str) -> SandboxError {
        SandboxError::new(format!(
            "{expected_provider} provider cannot adopt {:?} handle payload",
            self.provider_kind()
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
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub control_services:
        std::collections::BTreeSet<awaken_sandbox_control::SandboxControlServiceKind>,
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
            control_services: spec.control_services.clone(),
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
    /// Closed control services this concrete provider can actually publish.
    /// The empty default keeps older Workers fail-closed for new demands.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub control_services:
        std::collections::BTreeSet<awaken_sandbox_control::SandboxControlServiceKind>,
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
            && required.control_services.is_subset(&self.control_services)
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

    /// Acquire the one physical target for a restore effect without reading or
    /// interpreting checkpoint bytes. This is an explicit composition seam,
    /// not a capability advertisement: bare providers never call it from
    /// ordinary create/adopt paths.
    async fn acquire_restore(
        &self,
        _spec: &SandboxSpec,
        _request: &SandboxRestoreRequest,
    ) -> Result<SandboxRestoreTarget<Box<dyn Sandbox>>, SandboxError> {
        Err(SandboxError::new(
            "sandbox provider does not implement exact restore target acquisition",
        ))
    }

    /// Create a distinct environment from one verified filesystem checkpoint.
    /// The default fails closed so out-of-tree providers cannot accidentally
    /// advertise continuation without implementing it.
    async fn restore(
        &self,
        _spec: &SandboxSpec,
        _request: &SandboxRestoreRequest,
        _store: &dyn SandboxCheckpointStore,
    ) -> Result<SandboxRestoreResult<Box<dyn Sandbox>>, SandboxError> {
        Err(SandboxError::new(
            "sandbox provider does not implement checkpoint restore",
        ))
    }

    /// Remove only the unpublished physical target named by one durable
    /// `Restoring` tuple. Terminal cleanup calls this before deleting the source
    /// checkpoint. Absence is an idempotent success for implementing providers;
    /// the default fails closed because it cannot prove the target set.
    async fn dispose_restored(
        &self,
        _spec: &SandboxSpec,
        _request: &SandboxRestoreRequest,
    ) -> Result<(), SandboxError> {
        Err(SandboxError::new(
            "sandbox provider does not implement exact restored-target disposal",
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
