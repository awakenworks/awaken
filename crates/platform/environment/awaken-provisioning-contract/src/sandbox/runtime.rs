// @formal entrypoint sandbox.admission_never_weakens_isolation
// @formal entrypoint sandbox.admission_requires_requested_capabilities
// @formal entrypoint sandbox.fail_closed_policy_never_downgrades
// @formal entrypoint sandbox.workspace_path_projection_is_one_logical_root_and_never_discloses_provider_host_paths
//! Durable sandbox handles, provider selection, and live execution ports.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::spec::{Command, SandboxSpec};
use crate::vocab::{Artifact, MountAccess, MountRequirement, RealizedMount};

use std::sync::Arc;

use super::fence::SandboxEffectFence;
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::foundation::*;
use super::handle::SandboxHandle;
use super::restore_contract::{SandboxRestoreRequest, SandboxRestoreResult, SandboxRestoreTarget};
use super::{SandboxAdoption, SandboxAdoptionLayout, SandboxEffect, SandboxRealizationIntent};

mod disposal;
mod memory_reconciliation;
mod provisioning_deadline;

use crate::with_current_sandbox_effect;
pub use disposal::{SandboxDisposalAuthorization, SandboxDisposalPreparation};
pub use memory_reconciliation::MemoryReconciliationAck;
use memory_reconciliation::default_memory_reconciliation_ack;
pub use provisioning_deadline::ProvisioningDeadline;

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

/// Typed observation of one exact durable Sandbox handle before adoption or
/// fenced physical cleanup. Provider errors are deliberately kept outside this
/// enum: an error is indeterminate and can only be retried, never interpreted
/// as proof that a live substrate may be replaced or disposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxObservation {
    /// The exact physical realization is still converging. `deadline` is the
    /// provider's bound on that convergence, read at this observation; only a
    /// provider whose backend progresses provisioning on its own and records
    /// when it began can give one. Without it the wait has no owner, so a
    /// consumer must treat it as a failed attempt rather than wait.
    Provisioning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadline: Option<ProvisioningDeadline>,
    },
    /// The exact physical realization is ready to be adopted.
    Ready,
    /// The provider proved that the exact durable realization is gone. A
    /// daemon-backed provider includes the immutable incarnation it searched
    /// for; a deterministic filesystem provider may use its marker as the
    /// complete absence proof and therefore carry no separate value.
    DefinitivelyUnavailable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        physical_incarnation: Option<String>,
    },
    /// The exact immutable physical realization still exists in a terminal
    /// state and must be disposed under the same aggregate effect fence.
    /// Keeping this distinct from absence prevents a terminal Pod/container
    /// from being acknowledged without deletion.
    Terminal { physical_incarnation: String },
    /// The provider positively observed the exact original data source retained
    /// independently of its retiring Worker. This read-only result is for
    /// WorkerRetirement only: it says nothing about physical termination and
    /// grants no adoption, source preparation, disposal or completion authority.
    RetainedOriginal { physical_incarnation: String },
    /// The exact physical realization has crossed its provider-owned durable
    /// cleanup gate. Live output, credential, checkpoint, and Memory reads are
    /// no longer permitted; only replay of already-durable receipts and exact
    /// physical disposal may continue under the same aggregate fence.
    /// This is an observation of backend evidence, not a second cleanup state
    /// machine or mutation authority.
    Disposing { physical_incarnation: String },
    /// The provider reached a stable physical fact, but it cannot represent
    /// the durable handle being observed (for example a foreign incarnation,
    /// missing immutable fingerprint, or duplicate realization). This is a
    /// non-destructive permanent rejection, not transient unavailability and
    /// never replacement authority.
    Incompatible { reason: String },
}

/// What one [`SandboxProvider::observe`] call is for. The two cases are
/// named so a caller cannot degrade an effect revalidation into an unfenced
/// inspection by omitting a value.
#[derive(Debug, Clone, Copy)]
pub enum SandboxObservationBasis<'a> {
    /// Inspect a handle that no aggregate effect owns: a Session whose
    /// Environment is unowned, or a retiring Candidate that was never
    /// published. The recorded handle evidence is the whole expectation, and
    /// the result never authorizes adoption or disposal of an owned Sandbox.
    Unattached,
    /// Revalidate at an aggregate-authorized physical-effect boundary: the
    /// effective spec must match the handle's immutable realization evidence
    /// and the recorded fence must not be newer than `effect_fence`.
    Effect {
        spec: &'a SandboxSpec,
        effect_fence: &'a SandboxEffectFence,
    },
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
    #[serde(default, skip_serializing_if = "crate::spec::is_false")]
    pub process_tree_supervision: bool,
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
            process_tree_supervision: spec.process_tree_supervision,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    #[serde(default, skip_serializing_if = "crate::spec::is_false")]
    pub process_tree_supervision: bool,
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
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub control_services:
        std::collections::BTreeSet<awaken_sandbox_control::SandboxControlServiceKind>,
}

impl SandboxCapabilities {
    /// One monotonic compatibility predicate used by local provider selection
    /// and remote Worker admission. Ranking policy runs only after this succeeds.
    #[must_use]
    pub fn satisfies_requirements(&self, required: &SandboxRequirements) -> bool {
        (!required.process_tree_supervision || self.process_tree_supervision)
            && capability_requirements_satisfied(
                self.isolation,
                required.isolation,
                self.network_isolation,
                required.network_isolation,
                self.resource_limits,
                required.resource_limits,
            )
            && (!required.tool_transparent || self.tool_transparent)
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
pub(crate) const fn degradation_is_authorized(
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
/// [`probe_ready`](SandboxReadiness::probe_ready) check passes. It never downgrades to
/// a weaker tier — if nothing qualifies it returns [`SelectionError::NoCapableBackend`]
/// (the host maps this to a `Gated` outcome), so a spec is never silently placed on an
/// under-isolating or unavailable backend. This is the deliberate divergence from a
/// "degrade to a portable scope" policy: in a managed/multi-tenant plane a silent
/// isolation downgrade is a security regression, not a convenience.
pub async fn select_provider<'a, P: SandboxReadiness + ?Sized>(
    candidates: &'a [Box<P>],
    spec: &SandboxSpec,
) -> Result<&'a P, SelectionError> {
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
pub struct PolicySelection<'a, P: ?Sized> {
    pub provider: &'a P,
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
///
/// `put` is one exact-operation, atomic idempotency boundary. Its logical key is
/// `(workspace_id, session_id, generation_id, suspend_effect_id)`. The first
/// successful call durably binds that key to the complete metadata and bytes.
/// An exact retry, including one after response loss or process restart, must
/// return the same [`StoredCheckpointObject`] without writing a second object.
/// A concurrent or later call with the same key but any different metadata or
/// bytes must fail without overwriting either version. Implementations must
/// preserve this behavior across processes; an in-memory deduplication cache is
/// not sufficient. `delete` is idempotent for an already-absent exact object.
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

/// Exact, provider-neutral input for capturing and persisting one filesystem
/// checkpoint, including its format, byte bound and immutable object metadata.
///
/// Session lifecycle authorization deliberately does not cross this value.
/// The Runtime adapter projects the committed operation/generation into these
/// facts; the provider separately requires an exact effect fence and current
/// execution authority before capture, persistence and receipt replay. The
/// adapter later wraps the artifact in a Session-owned receipt. Terminal takeover
/// reuses this input to recover the original capture before deleting its object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxCheckpointCaptureRequest {
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

// Only isolation can be explicitly degraded. All other guarantees pass through
// the same closed requirement projection and compatibility predicate as direct
// provider selection and Worker placement; do not keep a second partial list.
fn non_isolation_ok(caps: &SandboxCapabilities, spec: &SandboxSpec) -> bool {
    let mut required = SandboxRequirements::from_spec(spec, false);
    required.isolation = IsolationClass::Workdir;
    caps.satisfies_requirements(&required)
}

/// Fail-closed provider selection with an explicit **policy floor** (ADR-0056 §5). It
/// first places on the strongest ready backend that meets `policy.require` (the exact
/// `prefer` class winning a tie). If none meets the floor, `on_unmet` decides: `FailClosed`
/// returns [`SelectionError::NoCapableBackend`] (the never-downgrade guarantee);
/// `DegradeWithConsent` places on the strongest ready backend *below* the floor and
/// reports `degraded_to` so the caller records the degradation. A spec is never silently
/// placed below its floor.
pub async fn select_provider_with_policy<'a, P: SandboxReadiness + ?Sized>(
    candidates: &'a [Box<P>],
    spec: &SandboxSpec,
    policy: &IsolationPolicy,
) -> Result<PolicySelection<'a, P>, SelectionError> {
    // Ready backends meeting the floor (isolation >= require) and the spec's network.
    let mut at_or_above: Vec<&P> = Vec::new();
    // Ready backends below the floor but network-sound — the degrade candidates.
    let mut below: Vec<&P> = Vec::new();
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

#[cfg(test)]
mod exhaustive {
    use super::*;

    const EVERY_ISOLATION: [IsolationClass; 3] = [
        IsolationClass::Workdir,
        IsolationClass::Namespace,
        IsolationClass::Container,
    ];

    const fn listed_isolation(class: IsolationClass) -> usize {
        match class {
            IsolationClass::Workdir => 0,
            IsolationClass::Namespace => 1,
            IsolationClass::Container => 2,
        }
    }

    #[test]
    fn every_isolation_class_is_listed_once() {
        for (index, class) in EVERY_ISOLATION.into_iter().enumerate() {
            assert_eq!(listed_isolation(class), index);
        }
    }

    /// @exhaustive IsolationClass(3) x IsolationClass(3) x bool(2) x bool(2) x bool(2) x bool(2) = 144
    /// @obligation sandbox.admission_never_weakens_isolation
    #[test]
    fn sandbox_admission_never_weakens_the_isolation_floor() {
        for actual in EVERY_ISOLATION {
            for required in EVERY_ISOLATION {
                for has_network in [false, true] {
                    for needs_network in [false, true] {
                        for has_limits in [false, true] {
                            for needs_limits in [false, true] {
                                let admitted = capability_requirements_satisfied(
                                    actual,
                                    required,
                                    has_network,
                                    needs_network,
                                    has_limits,
                                    needs_limits,
                                );
                                if admitted {
                                    assert!(isolation_rank(actual) >= isolation_rank(required));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// @exhaustive IsolationClass(3) x bool(2) x bool(2) x bool(2) x bool(2) = 48
    /// @obligation sandbox.admission_requires_requested_capabilities
    #[test]
    fn sandbox_admission_requires_every_requested_capability() {
        for actual in EVERY_ISOLATION {
            for has_network in [false, true] {
                for needs_network in [false, true] {
                    for has_limits in [false, true] {
                        for needs_limits in [false, true] {
                            let admitted = capability_requirements_satisfied(
                                actual,
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
                    }
                }
            }
        }
    }

    /// @exhaustive IsolationClass(3) x IsolationClass(3) = 9
    /// @obligation sandbox.fail_closed_policy_never_downgrades
    #[test]
    fn fail_closed_sandbox_policy_never_authorizes_a_downgrade() {
        for actual in EVERY_ISOLATION {
            for required in EVERY_ISOLATION {
                if degradation_is_authorized(actual, required, OnUnmet::FailClosed) {
                    assert!(isolation_rank(actual) >= isolation_rank(required));
                }
            }
        }
    }

    /// @exhaustive bool(2) x bool(2) = 4
    /// @obligation sandbox.workspace_path_projection_is_one_logical_root_and_never_discloses_provider_host_paths
    #[test]
    fn workspace_path_projection_requires_exact_fidelity_admission() {
        for provider_has_path_fidelity in [false, true] {
            for workload_requires_path_fidelity in [false, true] {
                let capabilities = SandboxCapabilities {
                    path_fidelity: provider_has_path_fidelity,
                    ..SandboxCapabilities::default()
                };
                let requirements = SandboxRequirements {
                    path_fidelity: workload_requires_path_fidelity,
                    ..SandboxRequirements::default()
                };

                // Admission cause/effect table (all four rows are enumerated):
                // A1 !required + !provided -> admit; A2 !required + provided -> admit;
                // A3 required + !provided -> reject; A4 required + provided -> admit.
                // Effect E1 is exact fail-closed selection before any provider bind or
                // workload launch can expose a physical path. Constraint: every other
                // requirement/capability axis is the same default value, isolating the
                // complete path-fidelity decision rather than bypassing another cause.
                assert_eq!(
                    capabilities.satisfies_requirements(&requirements),
                    !workload_requires_path_fidelity || provider_has_path_fidelity
                );
            }
        }
    }
}

/// What a backend can enforce and whether it is usable right now: the facts
/// provider selection, Worker readiness and the Worker manifest read. Split
/// from [`SandboxProvider`] because those callers never realize a Sandbox and
/// must not depend on which live Sandbox type a provider hands out.
#[async_trait]
pub trait SandboxReadiness: Send + Sync {
    /// What this backend can enforce (probed at startup for selection).
    fn capabilities(&self) -> SandboxCapabilities;

    /// Portable filesystem checkpoint formats this concrete provider fully
    /// implements. Worker composition projects these into the existing
    /// `WorkerManifest.checkpoint_formats` authority; no second capability field
    /// is maintained on `SandboxCapabilities`.
    fn checkpoint_formats(&self) -> Vec<String>;

    /// A cheap liveness probe run at selection time: `Ok` iff this backend is
    /// actually usable *right now* — bwrap/unprivileged-userns available, a
    /// container daemon reachable, a provider base qualified. Required, so a
    /// provider states its own readiness and `select_provider` fails closed
    /// (never a silent unisolated run) rather than deferring the failure to
    /// creation.
    async fn probe_ready(&self) -> Result<(), SandboxError>;
}

/// Process-startup installation of the shared services of one physical provider.
/// This is the same installation role consumed by Session adaptation; it does
/// not require the provider's associated live Sandbox type.
///
/// Installation type decision table: SI1 a caller has only this role, with no
/// `Environment` type; installing either service compiles. This compile-only
/// case grants no realization or readiness authority and executes no effect.
///
/// ```no_run
/// use std::sync::Arc;
/// use awaken_provisioning_contract::{MemoryMounter, SandboxInstallation, SecretBroker};
/// fn install_on_one_owner(
///     provider: &dyn SandboxInstallation,
///     mounter: Arc<dyn MemoryMounter>,
///     broker: Arc<dyn SecretBroker>,
/// ) {
///     provider.install_memory_mounter(mounter);
///     provider.install_secret_broker(broker);
/// }
/// ```
pub trait SandboxInstallation: Send + Sync {
    /// Install the governed MemoryStore mount adapter after composition; every
    /// Sandbox realized afterwards mounts Memory through it.
    fn install_memory_mounter(&self, mounter: Arc<dyn MemoryMounter>);

    /// Install the credential-file broker after composition; every Sandbox
    /// realized afterwards resolves and writes back Secrets through it.
    fn install_secret_broker(&self, broker: Arc<dyn SecretBroker>);
}

/// The one provider abstraction: realizes a [`SandboxSpec`] into a live
/// Sandbox, reopens and observes the durable handles it minted, reconstructs
/// a terminal Sandbox for fenced disposal, and owns any restore targets.
///
/// `Environment` is the live Sandbox type this provider hands out: a
/// container backend hands out shared [`SessionSandbox`](super::SessionSandbox)s
/// (`Environment = dyn SessionSandbox`), while filesystem providers expose
/// their shared base `Sandbox` role. Every provider-owned durable handle payload stays
/// opaque to callers: [`Self::adoption_layout`] and
/// [`Self::validate_closed_observation`] are the only readers.
#[async_trait]
pub trait SandboxProvider: SandboxReadiness + SandboxInstallation {
    /// The live Sandbox this provider realizes.
    type Environment: ?Sized + Sandbox + 'static;

    /// Prepare only immutable images from canonical mount-less specs. No
    /// Session filesystem is allocated and success acknowledges demand, not
    /// readiness. A provider without images has nothing to prepare and says so
    /// by succeeding.
    async fn prewarm_images(&self, specs: &[SandboxSpec]) -> Result<(), SandboxError>;

    /// The layout this provider reopens from `handle`. The handle payload is
    /// opaque outside the provider's own codec, so a Session owner asks here
    /// before any adoption I/O instead of decoding it.
    fn adoption_layout(
        &self,
        handle: &SandboxHandle,
    ) -> Result<SandboxAdoptionLayout, SandboxError>;

    /// Validate the physical evidence one closed observation (absence,
    /// `Terminal`, `Disposing`) carries against the exact incarnation recorded
    /// in `handle`. Any other observation is not closed evidence and is
    /// refused.
    fn validate_closed_observation(
        &self,
        handle: &SandboxHandle,
        observation: &SandboxObservation,
    ) -> Result<(), SandboxError>;

    /// Realize one Sandbox for `effect`. A fenced effect is validated against
    /// the original captured current authority, which is re-entered after
    /// asynchronous preparation and before publishing the result; a durable
    /// fence or cached receipt alone is not a live grant. A `Rebuild` intent
    /// requires a fenced effect ([`SandboxRealizationIntent::admit_under`]).
    ///
    /// This is the provider's creation obligation. It carries no default: a
    /// provider that cannot fence creation must fail to compile rather than
    /// inherit a body.
    ///
    /// It is **synchronous** and returns a [`ClaimedEffect`](crate::ClaimedEffect),
    /// the second half of the same discipline. The fence says the effect is
    /// authorized; the claim says who is on record for it if the caller goes
    /// away mid-flight. Returning the claim before the first poll is what makes
    /// "sandbox on the host, no responsibility record" unrepresentable rather
    /// than merely discouraged: a provider cannot mint the claim afterwards,
    /// because `ClaimedEffect::new` consumes it. Callers still write
    /// `.claim_for_effect(..).await` — [`std::future::IntoFuture`] makes the
    /// call site read like an `async fn` — but a caller dropped mid-await
    /// leaves the claim's retention behind instead of an orphan.
    fn claim_for_effect<'a>(
        &'a self,
        spec: &'a SandboxSpec,
        effect: SandboxEffect<'a>,
        intent: SandboxRealizationIntent,
    ) -> crate::ClaimedEffect<'a, Arc<Self::Environment>>;

    /// Observe one exact durable handle without renewing, mounting, deleting,
    /// or otherwise adopting it. `basis` says what the observation is for:
    /// [`SandboxObservationBasis::Effect`] revalidates the complete fence and
    /// the effective spec (which must be `adoption.spec`) against immutable
    /// realization evidence at an aggregate-authorized boundary;
    /// [`SandboxObservationBasis::Unattached`] inspects a handle no aggregate
    /// effect owns and never becomes mutation authority. Providers keep
    /// backend `NotFound`/terminal evidence separate from transport,
    /// authorization, and timeout errors.
    async fn observe(
        &self,
        adoption: SandboxAdoption<'_>,
        basis: SandboxObservationBasis<'_>,
    ) -> Result<SandboxObservation, SandboxError>;

    /// The provider's only adoption edge: reconnect to an already realized
    /// Sandbox from a persisted handle after a host restart or across a
    /// takeover. A fenced effect revalidates the exact aggregate-owned fence,
    /// re-observes the handle without treating an indeterminate backend failure
    /// as Gone, and re-enters the current authority after waits and before
    /// success; the durable fence alone is not a process-local clock or
    /// admission proof. Carries no default, so a provider that cannot fence
    /// adoption fails to compile.
    async fn adopt_for_effect(
        &self,
        adoption: SandboxAdoption<'_>,
        effect: SandboxEffect<'_>,
    ) -> Result<Arc<Self::Environment>, SandboxError>;

    /// Reconstruct the same Sandbox lifecycle owner solely to finish an exact
    /// terminal physical realization. Unlike ordinary adoption this must not
    /// renew a lease, launch a resident process, or reinterpret absence as a
    /// live Sandbox. A persisted `handle` proves an already-published physical
    /// incarnation. An in-flight restore has no aggregate binding yet, so its
    /// exact prior effect fence is instead supplied as immutable evidence while
    /// `terminal_effect_fence` identifies the terminal effect; `authority`
    /// supplies its current authorization. Re-enter that authority after every
    /// reconstruction wait and before marker/mount/control effects or success,
    /// including exact absence. Never treat the copied fence as a live grant.
    /// Returning `None` is permitted only for provider-proved exact absence or
    /// a durable Removed tombstone.
    async fn prepare_terminal_for_effect(
        &self,
        spec: &SandboxSpec,
        handle: Option<&SandboxHandle>,
        expected_effect_fence: Option<&SandboxEffectFence>,
        terminal_effect_fence: &SandboxEffectFence,
        authority: &dyn crate::CurrentExecutionAuthority,
    ) -> Result<Option<Arc<Self::Environment>>, SandboxError>;

    /// The restore rung this provider owns (ADR-0081 capability ladder). A
    /// caller that needs one refuses at selection time instead of discovering
    /// an `Unsupported` error at the physical effect.
    fn restoration(&self) -> SandboxRestoration<'_, Arc<Self::Environment>>;
}

/// The restore rung one provider owns. Checkpoint restoration always brings
/// custody of its restore targets, so a provider that restores bytes without
/// owning the target they land in is unrepresentable.
pub enum SandboxRestoration<'a, Env: Send + 'static> {
    /// The provider owns no restore target.
    Unsupported,
    /// The provider acquires and disposes exact restore targets; checkpoint
    /// bytes are restored by the composition's own codec.
    Targets(&'a dyn RestoreTargetProvider<Env>),
    /// The provider restores checkpoint bytes into its own restore target.
    Restorable(&'a dyn RestorableProvider<Env>),
}

impl<'a, Env: Send + 'static> SandboxRestoration<'a, Env> {
    /// The restore-target custody this rung carries, or the refusal a caller
    /// returns before any physical effect.
    pub fn targets(self) -> Result<&'a dyn RestoreTargetProvider<Env>, SandboxError> {
        match self {
            Self::Unsupported => Err(SandboxError::unsupported(
                "this Sandbox provider does not own exact restore targets",
            )),
            Self::Targets(targets) => Ok(targets),
            Self::Restorable(restorable) => Ok(restorable),
        }
    }

    /// The checkpoint restoration this rung carries, or the refusal a caller
    /// returns before any physical effect.
    pub fn restorable(self) -> Result<&'a dyn RestorableProvider<Env>, SandboxError> {
        match self {
            Self::Restorable(restorable) => Ok(restorable),
            Self::Unsupported | Self::Targets(_) => Err(SandboxError::unsupported(
                "this Sandbox provider does not restore checkpoints",
            )),
        }
    }
}

/// A provider that owns the physical target of an exact restore: it can
/// acquire the one target an aggregate-projected request names and dispose an
/// unpublished target under disposal authorization. Checkpoint bytes are not
/// read here; a composition may restore them through its own codec.
///
/// `Env` is the shared live Sandbox a provider hands out
/// (`Arc<SandboxProvider::Environment>`).
#[async_trait]
pub trait RestoreTargetProvider<Env: Send + 'static>: Send + Sync {
    /// Acquire the one physical target for an aggregate-projected exact restore
    /// without reading or interpreting checkpoint bytes.
    async fn acquire_restore(
        &self,
        spec: &SandboxSpec,
        request: &SandboxRestoreRequest,
    ) -> Result<SandboxRestoreTarget<Env>, SandboxError>;

    /// Remove only the unpublished physical target named by one durable
    /// restoring tuple using the aggregate preparation/successor authorization.
    /// The tuple identifies the target; it does not authorize physical deletion.
    /// The current operation-bound authority is mandatory after target-lock and
    /// backend waits and before the next effect or success. Provider-proved
    /// absence is an idempotent success only under that same current authority.
    async fn dispose_restored(
        &self,
        spec: &SandboxSpec,
        request: &SandboxRestoreRequest,
        authorization: &SandboxDisposalAuthorization,
        authority: &dyn crate::CurrentExecutionAuthority,
    ) -> Result<(), SandboxError>;
}

/// A provider that restores an exact filesystem checkpoint into its own
/// restore target.
#[async_trait]
pub trait RestorableProvider<Env: Send + 'static>: RestoreTargetProvider<Env> {
    /// Complete one exact restore and return canonical evidence only after the
    /// checkpoint bytes have been materialized and verified.
    async fn restore_environment(
        &self,
        spec: &SandboxSpec,
        request: &SandboxRestoreRequest,
        store: &dyn SandboxCheckpointStore,
    ) -> Result<SandboxRestoreResult<Env>, SandboxError>;
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

    /// The filesystem-checkpoint capability, when this Sandbox has it. A
    /// Sandbox without it does not implement [`CheckpointingSandbox`].
    fn checkpointing(&self) -> Option<&dyn CheckpointingSandbox> {
        None
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

    /// Confirm that the Host completed every copy-backed Memory obligation for
    /// this aggregate effect and supplied the provider's exact complete ordered
    /// materialization evidence. Implementations retire Copy guards without
    /// teardown; FUSE guards remain owned by ordinary disposal. This is a
    /// process-local lifecycle acknowledgement, never a durable Memory receipt.
    /// After any asynchronous guard acquisition, implementations enter the
    /// supplied effect-bound current authority before validating/draining or
    /// admitting a cached/empty acknowledgement. No await may occur inside
    /// that callback. The shared [`MemoryReconciliationAck`] owns this kernel.
    async fn acknowledge_memory_reconciliation(
        &self,
        effect_fence: &SandboxEffectFence,
        complete_materializations: &[MemoryMaterializationEvidence],
        authority: &dyn crate::CurrentExecutionAuthority,
    ) -> Result<(), SandboxError> {
        with_current_sandbox_effect(effect_fence, authority, |_| {
            default_memory_reconciliation_ack(complete_materializations)
        })
    }

    /// Complete every source-dependent disposal participant under one aggregate
    /// effect without deleting the physical realization. Implementations must
    /// return only after credential write-back and provider-local reconciliation
    /// gates are durable/replayable. The returned fence is the exact durable
    /// provider predecessor, which may predate the caller's fence after a
    /// same-generation response-loss replay. The aggregate persists that
    /// preparation fact before it may invoke [`Self::dispose_for_effect`].
    /// The current operation-bound authority must guard synchronous preparation
    /// and be re-entered after asynchronous waits, including empty Memory and
    /// cached success. It does not certify a remote backend's in-flight commit.
    async fn prepare_disposal_for_effect(
        &self,
        effect_fence: &SandboxEffectFence,
        authority: &dyn crate::CurrentExecutionAuthority,
    ) -> Result<SandboxEffectFence, SandboxError>;

    /// Reclaim an environment that no aggregate effect owns: a capability
    /// probe, warm-pool capacity, an unowned Session, or a candidate that never
    /// reached publication. Required, so every implementation states how it is
    /// reclaimed, and named so the call site declares which case it means.
    /// Anything a committed Session owns must use [`Self::dispose_for_effect`].
    async fn dispose_unattached(&self) -> Result<(), SandboxError>;

    /// Physically tear down the exact realization authorized by one aggregate
    /// effect fence after the aggregate durably accepted disposal preparation.
    /// This edge must not repeat source-dependent Artifact, Memory, credential,
    /// or checkpoint I/O. Implementations revalidate immutable incarnation
    /// evidence at the destructive edge and retain retryable physical lifecycle
    /// guards when removal or dependency release fails.
    /// Current operation-bound authority is required even for cached success.
    /// Re-enter after waits and hold the guard through synchronous destructive
    /// effects; the preparation receipt is not a grant across later I/O.
    /// Backend commit fencing remains the resource adapter's responsibility.
    /// This is the Sandbox's destructive obligation and carries no default, so
    /// an environment that cannot fence teardown fails to compile.
    async fn dispose_for_effect(
        &self,
        authorization: &SandboxDisposalAuthorization,
        authority: &dyn crate::CurrentExecutionAuthority,
    ) -> Result<(), SandboxError>;
}

/// A Sandbox that persists its complete mutable filesystem as a portable
/// checkpoint. There is no unfenced checkpoint: durable Session continuation
/// is the only owner of a checkpoint, so every checkpoint write is an
/// aggregate effect.
#[async_trait]
pub trait CheckpointingSandbox: Sandbox {
    /// Persist the complete mutable filesystem while the aggregate-owned
    /// realization fence still authorizes this exact physical Sandbox.
    /// Implementations omit independently governed mounts and credential
    /// material, enforce `max_bytes`, and return only after the object adapter
    /// reports durability. `effect_fence` retains Control coordinates;
    /// `authority` owns current liveness. Re-enter it before WAL and store
    /// effects, after awaited responses and on cached receipt replay.
    async fn checkpoint_for_effect(
        &self,
        request: &SandboxCheckpointCaptureRequest,
        store: &dyn SandboxCheckpointStore,
        effect_fence: &SandboxEffectFence,
        authority: &dyn crate::CurrentExecutionAuthority,
    ) -> Result<SandboxCheckpointRef, SandboxError>;

    /// Finish and delete one checkpoint upload whose root receipt was lost to
    /// a terminal takeover. `expected_effect_fence` identifies the immutable
    /// suspend participant and may already be expired; it never authorizes a
    /// mutation. Every write-ahead update, exact `put` replay, and idempotent
    /// object deletion consumes the current operation-bound `authority`, not
    /// the copied terminal expiry. Re-enter after participant/store waits and
    /// before returning success, including cached-receipt replay. This is
    /// the checkpoint port's terminal edge, not a second cleanup state machine.
    async fn cleanup_checkpoint_for_terminal(
        &self,
        request: &SandboxCheckpointCaptureRequest,
        store: &dyn SandboxCheckpointStore,
        expected_effect_fence: &SandboxEffectFence,
        terminal_effect_fence: &SandboxEffectFence,
        authority: &dyn crate::CurrentExecutionAuthority,
    ) -> Result<(), SandboxError>;
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
    /// [`ProcessHandle::poll`] rather than assuming failure. Cancelling this
    /// observation must preserve the provider's completion handle; an outstanding
    /// wait must not block another caller's status query or signal delivery.
    /// Exit is the launched
    /// process's status, not evidence that descendants stopped or that its
    /// working directory is safe to reuse or dispose.
    async fn wait(&self) -> Result<ExitStatus, SandboxError>;

    /// Non-blocking, idempotent status: `None` while still running, `Some(status)`
    /// once exited. Safe to call repeatedly from any host after a reconnect — this
    /// is how you resolve an indeterminate outcome without re-running the process.
    async fn poll(&self) -> Result<Option<ExitStatus>, SandboxError>;

    /// Deliver a signal (terminate/kill/interrupt). This operation is idempotent:
    /// if the owned process exits before or during delivery, implementations return
    /// success after confirming that exit. A successful delivery is not a
    /// whole-process-tree quiescence receipt. Descendant termination and owner-crash
    /// containment depend on the provider's OS execution boundary; callers must
    /// not infer either from this process-only contract.
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
mod terminal_memory_reconciliation_contract_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn disposal_authorization_requires_a_distinct_authorized_successor() {
        // Cause/effect graph: C1 a provider participant was prepared under an
        // exact fence; C2 the physical caller presents the same operation, a
        // same-lease successor, a higher-epoch successor, an older epoch, or a
        // foreign realization; C3 the aggregate preparation fingerprint is
        // present/empty. Effects: E1 the raw preparation cannot authorize
        // deletion; E2 same-lease or monotonic-epoch successors produce one
        // typed predecessor→successor projection; E3 stale/foreign/empty facts
        // fail before provider mutation.
        //
        // | Rule | successor | preparation fingerprint | Effect |
        // | D1 | same operation | exact | reject / E1 |
        // | D2 | distinct, same lease | exact | admit / E2 |
        // | D3 | distinct, higher epoch | exact | admit / E2 |
        // | D4 | older or foreign | exact | reject / E3 |
        // | D5 | authorized | empty | reject / E3 |
        let prepared =
            SandboxEffectFence::new("prepare", "owner-a", "runtime-a", 4, 40_000).unwrap();
        let same = prepared.clone();
        let preparation = SandboxDisposalPreparation::new(prepared.clone(), "prepared").unwrap();
        let disposal_id = preparation.operation_id().unwrap();
        let same_lease =
            SandboxEffectFence::new(disposal_id.clone(), "owner-a", "runtime-a", 4, 50_000)
                .unwrap();
        let higher =
            SandboxEffectFence::new(disposal_id.clone(), "owner-b", "runtime-b", 5, 60_000)
                .unwrap();
        let older = SandboxEffectFence::new(disposal_id.clone(), "owner-a", "runtime-a", 3, 60_000)
            .unwrap();
        let foreign =
            SandboxEffectFence::new(disposal_id, "owner-b", "runtime-b", 4, 60_000).unwrap();

        assert!(preparation.authorize(same).is_err(), "D1/E1");
        for (rule, successor) in [("D2", same_lease.clone()), ("D3", higher.clone())] {
            let authorization = preparation.authorize(successor.clone()).expect(rule);
            assert_eq!(
                authorization.prepared_effect_fence(),
                &prepared,
                "{rule}/E2"
            );
            assert_eq!(authorization.effect_fence(), &successor, "{rule}/E2");
        }
        for (rule, successor) in [("D4 older", older), ("D4 foreign", foreign)] {
            assert!(preparation.authorize(successor).is_err(), "{rule}/E3");
        }
        assert!(
            SandboxDisposalPreparation::new(prepared, " ").is_err(),
            "D5/E3"
        );
    }

    #[test]
    fn disposal_authorization_wire_is_canonically_validated() {
        // Wire cause/effect table D6. Causes: C1 the serialized authorization
        // carries exact A/fingerprint/B, a B operation not derived from A+fp,
        // a same-epoch foreign B, an empty fingerprint, or an unknown field.
        // Effects: E1 only the exact value reconstructs through the canonical
        // value-object owner; E2 every malformed combination fails during
        // deserialization, before any provider can observe or mutate a Sandbox.
        //
        // | Rule | A/fingerprint/B relation | shape | Effect |
        // |---|---|---|---|
        // | D6a | exact | closed | E1 round-trip |
        // | D6b | B operation unbound | closed | E2 reject |
        // | D6c | B foreign at same epoch | closed | E2 reject |
        // | D6d | empty fingerprint | closed | E2 reject |
        // | D6e | exact | unknown field | E2 reject |
        let prepared =
            SandboxEffectFence::new("prepare", "owner-a", "runtime-a", 4, 40_000).unwrap();
        let fingerprint = "prepared";
        let preparation = SandboxDisposalPreparation::new(prepared.clone(), fingerprint).unwrap();
        let disposal_id = preparation.operation_id().unwrap();
        let successor =
            SandboxEffectFence::new(disposal_id, "owner-a", "runtime-a", 4, 50_000).unwrap();
        let authorization = preparation.authorize(successor).unwrap();
        let exact = serde_json::to_value(&authorization).unwrap();
        assert_eq!(
            serde_json::from_value::<SandboxDisposalAuthorization>(exact.clone()).unwrap(),
            authorization,
            "D6a/E1"
        );

        let mut unbound = exact.clone();
        unbound["effect_fence"]["operation_id"] = serde_json::json!("foreign-operation");
        assert!(
            serde_json::from_value::<SandboxDisposalAuthorization>(unbound).is_err(),
            "D6b/E2"
        );

        let mut foreign = exact.clone();
        foreign["effect_fence"]["owner"] = serde_json::json!("owner-b");
        assert!(
            serde_json::from_value::<SandboxDisposalAuthorization>(foreign).is_err(),
            "D6c/E2"
        );

        let mut empty_fingerprint = exact.clone();
        empty_fingerprint["preparation_fingerprint"] = serde_json::json!(" ");
        assert!(
            serde_json::from_value::<SandboxDisposalAuthorization>(empty_fingerprint).is_err(),
            "D6d/E2"
        );

        let mut unknown = exact;
        unknown["parallel_authority"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<SandboxDisposalAuthorization>(unknown).is_err(),
            "D6e/E2"
        );
    }

    #[test]
    fn default_acknowledgement_fails_closed_when_copy_evidence_exists() {
        // Provider-default cause/effect table: C1 the caller supplies no copy
        // materialization evidence / one or more exact copy materializations.
        // R1 no evidence is a provider-neutral no-op because FUSE writes through;
        // R2 any non-empty evidence fails closed so an out-of-tree provider cannot
        // silently claim that Host-owned terminal CAS retired its live copy guard.
        assert!(default_memory_reconciliation_ack(&[]).is_ok(), "R1");
        let evidence = MemoryMaterializationEvidence::new(
            "store",
            "/memory",
            vec![MemoryMaterializationHead {
                path: "value".into(),
                id: "head".into(),
                version: 1,
                content_sha256: "digest".into(),
            }],
        )
        .unwrap();
        assert!(
            default_memory_reconciliation_ack(&[evidence]).is_err(),
            "R2"
        );
    }

    #[test]
    fn shared_ack_kernel_is_exact_effect_scoped_and_response_loss_safe() {
        // Shared-kernel decision table: C1 expected/supplied evidence is exact
        // including canonical order; C2 effect-bound local proof is live; C3 state is empty / same
        // effect / same realization lease with another operation / foreign lease.
        // K1 !C1 or !C2 rejects before the provider closure; K2 exact+live+empty
        // runs it once and records the complete effect identity; K3 a same-effect
        // replay rechecks current authority without rerunning drain; K4 a same-lease successor effect
        // reruns provider authorization/drain and rebinds the acknowledgement;
        // K5 a different or higher lease rejects. The disposal gate admits only
        // the latest exact K2/K4 effect. Empty evidence validates but records no
        // claim because it says nothing about FUSE teardown.
        let first = MemoryMaterializationEvidence::new(
            "store-a",
            "/a",
            vec![MemoryMaterializationHead {
                path: "value".into(),
                id: "head-a".into(),
                version: 1,
                content_sha256: "digest-a".into(),
            }],
        )
        .unwrap();
        let second = MemoryMaterializationEvidence::new(
            "store-b",
            "/b",
            vec![MemoryMaterializationHead {
                path: "value".into(),
                id: "head-b".into(),
                version: 1,
                content_sha256: "digest-b".into(),
            }],
        )
        .unwrap();
        let expected = vec![first.clone(), second.clone()];
        let reversed = vec![second, first];
        let live = SandboxEffectFence::new("effect", "owner", "runtime", 2, u64::MAX).unwrap();
        let expired = SandboxEffectFence::new("effect", "owner", "runtime", 2, 0).unwrap();
        let takeover =
            SandboxEffectFence::new("terminal", "owner", "runtime", 2, u64::MAX).unwrap();
        let foreign =
            SandboxEffectFence::new("foreign", "other-owner", "runtime", 2, u64::MAX).unwrap();
        let higher = SandboxEffectFence::new("higher", "owner", "runtime", 3, u64::MAX).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let ack = MemoryReconciliationAck::default();

        for (rule, fence, supplied) in [
            ("K1 order", &live, reversed.as_slice()),
            ("K1 expired", &expired, expected.as_slice()),
        ] {
            let authority = if rule == "K1 expired" {
                crate::test_support::EffectAuthority::denied()
            } else {
                crate::test_support::EffectAuthority::new(fence)
            };
            let calls = calls.clone();
            assert!(
                ack.acknowledge(fence, &authority, &expected, supplied, |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .is_err(),
                "{rule}"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "K1");

        ack.acknowledge(
            &live,
            &crate::test_support::EffectAuthority::new(&live),
            &expected,
            &expected,
            {
                let calls = calls.clone();
                move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .expect("K2");
        ack.acknowledge(
            &live,
            &crate::test_support::EffectAuthority::new(&live),
            &expected,
            &expected,
            {
                let calls = calls.clone();
                move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .expect("K3");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "K2/K3");

        ack.acknowledge(
            &takeover,
            &crate::test_support::EffectAuthority::new(&takeover),
            &expected,
            &expected,
            {
                let calls = calls.clone();
                move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .expect("K4");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "K4 reauthorizes");
        assert!(
            ack.require_for_disposal(
                &expected,
                &live,
                &crate::test_support::EffectAuthority::new(&live),
                |_| Ok(())
            )
            .is_err(),
            "K4 supersedes the old operation"
        );
        ack.require_for_disposal(
            &expected,
            &takeover,
            &crate::test_support::EffectAuthority::new(&takeover),
            |_| Ok(()),
        )
        .expect("K4 latest gate");

        assert!(
            ack.acknowledge(
                &foreign,
                &crate::test_support::EffectAuthority::new(&foreign),
                &expected,
                &expected,
                |_| Ok(())
            )
            .is_err(),
            "K5 foreign owner"
        );
        assert!(
            ack.acknowledge(
                &higher,
                &crate::test_support::EffectAuthority::new(&higher),
                &expected,
                &expected,
                |_| Ok(())
            )
            .is_err(),
            "K5 higher epoch"
        );
        assert!(
            ack.require_for_disposal(
                &expected,
                &foreign,
                &crate::test_support::EffectAuthority::new(&foreign),
                |_| Ok(())
            )
            .is_err(),
            "K5 gate"
        );

        let empty = MemoryReconciliationAck::default();
        empty
            .acknowledge(
                &live,
                &crate::test_support::EffectAuthority::new(&live),
                &[],
                &[],
                |_| panic!("empty evidence has no Copy drain"),
            )
            .expect("empty evidence");
        empty
            .require_for_disposal(
                &[],
                &foreign,
                &crate::test_support::EffectAuthority::new(&foreign),
                |_| Ok(()),
            )
            .expect("empty evidence leaves FUSE to ordinary disposal");
    }
}
