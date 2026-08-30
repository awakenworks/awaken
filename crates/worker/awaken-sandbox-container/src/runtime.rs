//! Dependency-inverted container runtime port and its shared value types.

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_provisioning_contract as pc;
use awaken_sandbox_control::SandboxControlServiceKind;

use crate::ContainerPlan;
use crate::egress::EgressRealizationIdentity;
use crate::podman_plan::RootfsPlan;
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
use crate::provider_contract::ContainerEffectFence;
use crate::resident_hand::ResidentHandConfig;

/// Stable label every managed Sandbox container/pod carries. The wire value is
/// retained for compatibility with existing resources; it is discovery evidence,
/// never disposal authorization.
pub(crate) const MANAGED_SANDBOX_LABEL: &str = "awaken.sandbox";
/// Stable, non-secret logical scope selector used to discover a provider effect
/// across Worker-process owner changes.
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) const SANDBOX_SCOPE_LABEL: &str = "awaken.sandbox.scope";
/// Exact immutable provider realization carried by every current container.
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) const SANDBOX_REALIZATION_LABEL: &str = "awaken.sandbox.realization";
/// Pure provider-effective adoption identity. It is checked before package,
/// Resource, or workspace effects; the realization label extends it with facts
/// resolved during creation.
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) const SANDBOX_ADOPTION_LABEL: &str = "awaken.sandbox.adoption";
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) const SANDBOX_EFFECT_LABEL: &str = "awaken.sandbox.effect";
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) const SANDBOX_EFFECT_OWNER_LABEL: &str = "awaken.sandbox.effect-owner";
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) const SANDBOX_EFFECT_RUNTIME_LABEL: &str = "awaken.sandbox.effect-runtime";
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) const SANDBOX_EFFECT_EPOCH_LABEL: &str = "awaken.sandbox.effect-epoch";
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) const SANDBOX_EFFECT_EXPIRY_LABEL: &str = "awaken.sandbox.effect-expiry";
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) const SANDBOX_ATTEMPT_LABEL: &str = "awaken.sandbox.attempt";

#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) fn container_effect_label_values(
    fence: Option<&ContainerEffectFence>,
) -> Vec<(&'static str, String)> {
    fence.map_or_else(Vec::new, |fence| {
        vec![
            (SANDBOX_EFFECT_LABEL, fence.operation_id.clone()),
            (SANDBOX_EFFECT_OWNER_LABEL, fence.owner.clone()),
            (
                SANDBOX_EFFECT_RUNTIME_LABEL,
                fence.runtime_incarnation.clone(),
            ),
            (SANDBOX_EFFECT_EPOCH_LABEL, fence.epoch.to_string()),
            (
                SANDBOX_EFFECT_EXPIRY_LABEL,
                fence.expires_at_unix_ms.to_string(),
            ),
        ]
    })
}

#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) fn container_effect_fence_from_values(
    operation_id: Option<&str>,
    owner: Option<&str>,
    runtime_incarnation: Option<&str>,
    epoch: Option<&str>,
    expires_at_unix_ms: Option<&str>,
) -> Result<Option<ContainerEffectFence>, RuntimeError> {
    match (
        operation_id,
        owner,
        runtime_incarnation,
        epoch,
        expires_at_unix_ms,
    ) {
        (None, None, None, None, None) => Ok(None),
        (
            Some(operation_id),
            Some(owner),
            Some(runtime_incarnation),
            Some(epoch),
            Some(expires_at_unix_ms),
        ) => ContainerEffectFence::new(
            operation_id,
            owner,
            runtime_incarnation,
            epoch
                .parse::<u64>()
                .map_err(|_| RuntimeError::Backend("invalid Sandbox effect epoch".into()))?,
            expires_at_unix_ms
                .parse::<u64>()
                .map_err(|_| RuntimeError::Backend("invalid Sandbox effect expiry".into()))?,
        )
        .map(Some)
        .map_err(|error| RuntimeError::Backend(error.to_string())),
        _ => Err(RuntimeError::Backend(
            "incomplete Sandbox effect fence labels".into(),
        )),
    }
}

pub(crate) fn container_runtime_unix_now_ms() -> Result<u64, pc::SandboxError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| pc::SandboxError::new(format!("read Sandbox effect time: {error}")))?
        .as_millis()
        .try_into()
        .map_err(|_| pc::SandboxError::new("Sandbox effect time exceeds u64"))
}
/// Identifies the runtime incarnation that currently owns a container. It fences
/// realization/adoption only and cannot authorize garbage collection.
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) const RUNTIME_OWNER_LABEL: &str = "awaken.sandbox.owner";

pub(crate) fn runtime_owner_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{epoch}-{sequence}", std::process::id())
}

#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) fn sandbox_scope_identity(
    realization_namespace: &str,
    scope: &str,
) -> Result<String, RuntimeError> {
    if scope.is_empty() {
        return Err(RuntimeError::Backend(
            "sandbox scope cannot be empty".into(),
        ));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"awaken-container-scope/v1\0");
    hasher.update(&(realization_namespace.len() as u64).to_be_bytes());
    hasher.update(realization_namespace.as_bytes());
    hasher.update(&(scope.len() as u64).to_be_bytes());
    hasher.update(scope.as_bytes());
    Ok(hasher.finalize().to_hex().to_string())
}

#[derive(serde::Serialize)]
struct ContainerAdoptionProjection<'a> {
    schema: u8,
    command: &'a [String],
    rootfs: &'a RootfsPlan,
    egress: &'a EgressRealizationIdentity,
    resident_hand: Option<&'a ResidentHandConfig>,
    runtime: &'a std::collections::BTreeMap<String, String>,
}

#[derive(serde::Serialize)]
struct ContainerRealizationProjection<'a> {
    schema: u8,
    adoption: &'a pc::SandboxRealizationFingerprint,
    resolved_image: &'a str,
}

/// Bind the complete effective request to every stable, non-secret provider
/// configuration fact needed to reopen it. This value is pure and can therefore
/// be recomputed before adoption or package/resource I/O.
pub(crate) fn container_adoption_fingerprint(
    spec: &pc::SandboxSpec,
    plan: &ContainerPlan,
    resident_hand: Option<&ResidentHandConfig>,
    runtime: &std::collections::BTreeMap<String, String>,
) -> pc::SandboxRealizationFingerprint {
    pc::SandboxRealizationFingerprint::from_provider_projection(
        spec,
        &ContainerAdoptionProjection {
            schema: 1,
            command: &plan.command,
            rootfs: &plan.rootfs,
            egress: &plan.egress_identity,
            resident_hand,
            runtime,
        },
    )
}

/// Extend the pure adoption identity with the one immutable fact resolved during
/// creation: the exact package/base image. Backend labels/annotations carry this
/// physical fingerprint, while the durable handle carries both layers.
pub(crate) fn container_realization_fingerprint(
    spec: &pc::SandboxSpec,
    adoption: &pc::SandboxRealizationFingerprint,
    resolved_image: &str,
) -> pc::SandboxRealizationFingerprint {
    pc::SandboxRealizationFingerprint::from_provider_projection(
        spec,
        &ContainerRealizationProjection {
            schema: 1,
            adoption,
            resolved_image,
        },
    )
}

/// A daemon-global container name. Session/thread ids are only unique inside one
/// stable Worker/deployment namespace, while Docker and Podman names may share a
/// daemon across hosts and CI processes. The process incarnation is deliberately
/// absent so a replacement process can address the exact pre-receipt effect.
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) fn runtime_container_name(
    realization_namespace: &str,
    scope: &str,
) -> Result<String, RuntimeError> {
    // The complete length-delimited namespace+scope identity is the sole
    // daemon-global uniqueness authority. A truncated readable scope prefix
    // would make two long Session ids with the same prefix collide even though
    // their discovery labels are distinct.
    Ok(format!(
        "awaken-{}",
        sandbox_scope_identity(realization_namespace, scope)?
    ))
}

#[cfg(all(test, any(feature = "docker", feature = "podman")))]
mod runtime_identity_tests {
    use super::*;

    #[test]
    fn physical_name_uses_the_complete_scope_identity() {
        // Cause/effect table: C1 namespace equal/different; C2 scopes equal or
        // share an arbitrarily long readable prefix but differ at the tail; C3
        // scope is non-empty/empty. R1 only exact namespace+scope equality yields
        // the same name; R2 every other non-empty row differs while remaining
        // within daemon name limits; R3 empty scope rejects before adapter I/O.
        let prefix = "x".repeat(256);
        let left = runtime_container_name("worker-a", &format!("{prefix}-left")).unwrap();
        let replay = runtime_container_name("worker-a", &format!("{prefix}-left")).unwrap();
        let right = runtime_container_name("worker-a", &format!("{prefix}-right")).unwrap();
        let foreign = runtime_container_name("worker-b", &format!("{prefix}-left")).unwrap();
        assert_eq!(left, replay, "R1");
        assert_ne!(left, right, "R2 full scope");
        assert_ne!(left, foreign, "R2 namespace");
        assert!(left.len() < 128, "R2 bounded name");
        assert!(runtime_container_name("worker-a", "").is_err(), "R3");
    }
}

/// A no-bypass claim is publishable only when both independent deployment
/// evidence sources exist: runtime isolation and a capability issuer.
pub(crate) const fn allowlist_capability_advertised(
    runtime_attested: bool,
    issuer_installed: bool,
) -> bool {
    runtime_attested && issuer_installed
}

/// Capabilities common to one concrete container runtime. Network denial is
/// runtime evidence rather than an isolation-class assumption.
pub(crate) fn container_capabilities(
    network_isolation: bool,
    enforced_network_allowlist: bool,
    package_provisioning: bool,
    control_services: std::collections::BTreeSet<SandboxControlServiceKind>,
) -> pc::SandboxCapabilities {
    pc::SandboxCapabilities {
        isolation: pc::IsolationClass::Container,
        tool_transparent: true,
        path_fidelity: true,
        enforced_readonly: true,
        network_isolation,
        enforced_network_allowlist,
        secret_egress_substitution: false,
        resource_limits: true,
        custom_rootfs: true,
        package_provisioning,
        control_services,
    }
}

#[cfg(test)]
mod capability_tests {
    use super::allowlist_capability_advertised;

    #[test]
    fn allowlist_claim_requires_runtime_and_issuer() {
        assert!(!allowlist_capability_advertised(false, false));
        assert!(!allowlist_capability_advertised(false, true));
        assert!(!allowlist_capability_advertised(true, false));
        assert!(allowlist_capability_advertised(true, true));
    }
}

#[cfg(kani)]
#[kani::proof]
fn allowlist_claim_never_exceeds_its_evidence() {
    let runtime_attested: bool = kani::any();
    let issuer_installed: bool = kani::any();
    let advertised = allowlist_capability_advertised(runtime_attested, issuer_installed);
    assert!(!advertised || (runtime_attested && issuer_installed));
}

/// A memory-store mount carried into a remote container runtime. The canonical
/// MemoryMounter seeds and harvests these bytes; the Pod receives no Resource
/// authority credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryMount {
    pub store_id: String,
    pub mount_path: String,
    pub access: pc::MountAccess,
    pub snapshot_tar: Vec<u8>,
}

/// Deployment-owned policy for a Kubernetes Session's retained active volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct K8sContinuationVolume {
    pub storage_class_name: Option<String>,
    pub size: String,
}

/// A container/pod runtime failure.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("container {0:?} not found")]
    NotFound(String),
    #[error("container runtime failed: {0}")]
    Backend(String),
    /// The adapter crossed a backend mutation boundary and could not prove
    /// whether that exact effect converged. Callers must retain every staged
    /// participant that the physical object may still reference; treating this
    /// as an ordinary backend rejection would turn response loss into dangling
    /// host binds or prematurely torn-down Memory mounts.
    #[error("container runtime effect may have committed: {0}")]
    MayHaveCommitted(String),
}

impl RuntimeError {
    /// Classify an error observed after this invocation crossed its first
    /// backend mutation boundary. The wrapper is idempotent so nested runtime
    /// adapters cannot erase the stronger outcome.
    pub(crate) fn after_mutation(self) -> Self {
        match self {
            Self::MayHaveCommitted(_) => self,
            error => Self::MayHaveCommitted(error.to_string()),
        }
    }
}

/// Whether a container is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Provisioning,
    Running,
    Gone,
}

/// One runtime-level physical target selected by an exact restore identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRestoreTarget {
    pub container_id: String,
    pub disposition: pc::SandboxRestoreTargetDisposition,
}

/// Whether a provider is binding a newly realized runtime object or proving an
/// adopted object against durable incarnation evidence.
#[derive(Debug, Clone, Copy)]
pub enum SandboxControlBindingRequest<'a> {
    New {
        required: &'a std::collections::BTreeSet<SandboxControlServiceKind>,
    },
    Adopt {
        required: &'a std::collections::BTreeSet<SandboxControlServiceKind>,
        expected: Option<&'a pc::SandboxControlIncarnation>,
    },
}

impl SandboxControlBindingRequest<'_> {
    #[must_use]
    pub fn required(&self) -> &std::collections::BTreeSet<SandboxControlServiceKind> {
        match self {
            Self::New { required } | Self::Adopt { required, .. } => required,
        }
    }
}

/// Provider-neutral phase observed for one physical container realization.
#[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExistingRealizationPhase {
    /// The object exists but has not reached its runnable state.
    Creating,
    /// The object is runnable and can be returned after response loss.
    Ready,
    /// The exact object terminated and may be replaced under its immutable
    /// fingerprint.
    Terminal,
    /// Deleting/restarting/otherwise ambiguous state; never mutated by retry.
    Indeterminate,
}

/// Adapter observation after selecting the one stable Session-scope identity.
#[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExistingRealization {
    pub locator: String,
    /// Immutable backend incarnation used by the adapter's compare-and-delete
    /// effect. Docker and Podman use the container id. Kubernetes uses the Pod
    /// UID plus resourceVersion; a human-readable name is never a deletion
    /// fence by itself.
    pub incarnation: PhysicalIncarnation,
    /// Raw adapter evidence; the decision owner compares it byte-for-byte with
    /// the typed expected fingerprint and never constructs trusted evidence from
    /// an arbitrary backend label.
    pub adoption_fingerprint: Option<String>,
    pub fingerprint: Option<String>,
    pub fence: Option<crate::ContainerEffectFence>,
    pub attempt_id: Option<String>,
    /// Whether a new provider process can reconstruct every lifecycle
    /// dependency of this object, or only the in-process attempt that created
    /// the host binds may reuse it.
    pub recovery: ExistingRealizationRecovery,
    pub phase: ExistingRealizationPhase,
}

#[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhysicalIncarnation {
    pub identity: String,
    pub version: Option<String>,
}

/// Whether the provider can reconstruct the lifecycle evidence attached to an
/// existing exact physical object. This is an observed capability fact, not an
/// adapter-specific retry policy.
#[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExistingRealizationRecovery {
    Reconstructible,
    CurrentAttemptOnly,
}

/// The sole create/retry decision shared by Docker, Podman, and Kubernetes.
#[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExistingRealizationDecision {
    Create,
    ValidateExisting(ExistingRealization),
    ConvergeCreating(ExistingRealization),
    ReuseReady(ExistingRealization),
    ReplaceExact(ExistingRealization),
}

/// Adapter-collected evidence for rebuilding when the primary runtime object is
/// absent. Only an exact durable continuation participant (currently a
/// Kubernetes PVC UID) can preserve a retained filesystem in that row.
#[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RebuildContinuityEvidence {
    Unavailable,
    ExactContinuation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExactRemovalDecision {
    Remove,
    AlreadyAbsent,
}

fn exact_removal_decision(
    observation: pc::SandboxObservation,
) -> Result<ExactRemovalDecision, RuntimeError> {
    match observation {
        pc::SandboxObservation::Provisioning
        | pc::SandboxObservation::Ready
        | pc::SandboxObservation::Terminal { .. }
        | pc::SandboxObservation::Disposing { .. } => Ok(ExactRemovalDecision::Remove),
        pc::SandboxObservation::DefinitivelyUnavailable { .. } => {
            Ok(ExactRemovalDecision::AlreadyAbsent)
        }
        pc::SandboxObservation::Incompatible { reason } => Err(RuntimeError::Backend(reason)),
    }
}

/// Decide how one exact immutable realization may be recovered.
///
/// Adapters own discovery and effects only. They must pass every object selected
/// by the stable Session-scope identity; this function owns the 0/1/>1,
/// fingerprint, and phase policy so no backend grows a different retry rule.
#[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) fn existing_realization_decision(
    context: &crate::ContainerRealizationContext<'_>,
    expected: Option<&pc::SandboxRealizationFingerprint>,
    rebuild_continuity: RebuildContinuityEvidence,
    observed: &[ExistingRealization],
) -> Result<ExistingRealizationDecision, RuntimeError> {
    let [observed] = observed else {
        return if observed.is_empty() {
            match context.intent {
                crate::ContainerRealizationIntent::Create => {
                    Ok(ExistingRealizationDecision::Create)
                }
                crate::ContainerRealizationIntent::Rebuild { .. }
                    if rebuild_continuity == RebuildContinuityEvidence::ExactContinuation =>
                {
                    Ok(ExistingRealizationDecision::Create)
                }
                crate::ContainerRealizationIntent::Rebuild { .. } => Err(RuntimeError::Backend(
                    "absent Sandbox rebuild has no exact retained continuation evidence".into(),
                )),
            }
        } else {
            Err(RuntimeError::Backend(format!(
                "stable Sandbox scope resolved to {} physical realizations",
                observed.len()
            )))
        };
    };
    let Some(expected_fence) = context.effect_fence else {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` cannot be recovered without a durable effect fence",
            observed.locator
        )));
    };
    let Some(observed_fence) = observed.fence.as_ref() else {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` has no durable effect fence",
            observed.locator
        )));
    };
    if let crate::ContainerRealizationIntent::Rebuild {
        source_incarnation, ..
    } = context.intent
        && (rebuild_continuity != RebuildContinuityEvidence::ExactContinuation
            || observed.recovery != ExistingRealizationRecovery::Reconstructible
            || observed.phase != ExistingRealizationPhase::Terminal
            || observed.incarnation.identity != *source_incarnation)
    {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` is not the exact reconstructible terminal rebuild source",
            observed.locator
        )));
    }
    let current_attempt_owned = observed.recovery
        == ExistingRealizationRecovery::CurrentAttemptOnly
        && observed.attempt_id.as_deref() == Some(context.attempt.as_str());
    if observed.recovery == ExistingRealizationRecovery::CurrentAttemptOnly
        && !current_attempt_owned
    {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` depends on an unrecoverable prior process-local attempt",
            observed.locator
        )));
    }
    if !observed_fence.authorizes_successor(expected_fence)
        || (observed_fence.epoch == expected_fence.epoch
            && matches!(context.intent, crate::ContainerRealizationIntent::Create)
            && !observed_fence.same_effect_identity(expected_fence))
    {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` is owned by a newer or conflicting effect",
            observed.locator
        )));
    }
    if observed_fence.epoch < expected_fence.epoch {
        if observed.phase == ExistingRealizationPhase::Indeterminate {
            return Err(RuntimeError::Backend(format!(
                "existing Sandbox realization `{}` cannot be replaced by this effect",
                observed.locator
            )));
        }
        if observed.incarnation.identity.trim().is_empty() {
            return Err(RuntimeError::Backend(format!(
                "existing Sandbox realization `{}` has no immutable incarnation evidence",
                observed.locator
            )));
        }
        return Ok(ExistingRealizationDecision::ReplaceExact(observed.clone()));
    }
    if observed.adoption_fingerprint.as_deref() != Some(context.adoption_fingerprint.as_str()) {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` has missing or different adoption evidence",
            observed.locator
        )));
    }
    let Some(expected) = expected else {
        if observed.phase == ExistingRealizationPhase::Indeterminate {
            return Err(RuntimeError::Backend(format!(
                "existing Sandbox realization `{}` is in an indeterminate phase",
                observed.locator
            )));
        }
        return Ok(ExistingRealizationDecision::ValidateExisting(
            observed.clone(),
        ));
    };
    if observed.fingerprint.as_deref() != Some(expected.as_str()) {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` has missing or different immutable evidence",
            observed.locator
        )));
    }
    if observed.incarnation.identity.trim().is_empty() {
        return Err(RuntimeError::Backend(format!(
            "existing Sandbox realization `{}` has no immutable incarnation evidence",
            observed.locator
        )));
    }
    let reusable =
        observed.recovery == ExistingRealizationRecovery::Reconstructible || current_attempt_owned;
    Ok(match observed.phase {
        ExistingRealizationPhase::Creating if reusable => {
            ExistingRealizationDecision::ConvergeCreating(observed.clone())
        }
        ExistingRealizationPhase::Ready if reusable => {
            ExistingRealizationDecision::ReuseReady(observed.clone())
        }
        ExistingRealizationPhase::Creating
        | ExistingRealizationPhase::Ready
        | ExistingRealizationPhase::Terminal => {
            ExistingRealizationDecision::ReplaceExact(observed.clone())
        }
        ExistingRealizationPhase::Indeterminate => {
            return Err(RuntimeError::Backend(format!(
                "existing Sandbox realization `{}` is in an indeterminate phase",
                observed.locator
            )));
        }
    })
}

/// Project one adapter observation into the provider-neutral adoption result.
///
/// Discovery remains adapter-owned, but absence, immutable-incarnation
/// matching, and phase semantics are shared here so Docker, Podman, and
/// Kubernetes cannot independently decide that a transport error or an
/// ambiguous physical object is safe to replace.
#[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) fn sandbox_observation(
    expected_incarnation: Option<&str>,
    expected_adoption: Option<&pc::SandboxRealizationFingerprint>,
    expected_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    expected_fence: Option<&pc::SandboxEffectFence>,
    observed: &[ExistingRealization],
) -> Result<pc::SandboxObservation, RuntimeError> {
    if expected_incarnation.is_some() != expected_fingerprint.is_some()
        || expected_incarnation.is_some() != expected_adoption.is_some()
    {
        return Ok(pc::SandboxObservation::Incompatible {
            reason:
                "current Sandbox observation requires incarnation, adoption, and realization fingerprints"
                    .into(),
        });
    }
    if expected_incarnation.is_some_and(|value| value.trim().is_empty()) {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: "Sandbox observation requires immutable incarnation evidence".into(),
        });
    }
    let [observed] = observed else {
        return if observed.is_empty() && expected_incarnation.is_some() {
            Ok(pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: expected_incarnation.map(str::to_owned),
            })
        } else {
            Ok(pc::SandboxObservation::Incompatible {
                reason: format!(
                    "durable Sandbox observation resolved to {} physical realizations without exact current evidence",
                    observed.len()
                ),
            })
        };
    };
    if expected_incarnation.is_some()
        && let Some(expected_fence) = expected_fence
    {
        let Some(observed_fence) = observed.fence.as_ref() else {
            return Ok(pc::SandboxObservation::Incompatible {
                reason: format!(
                    "durable Sandbox incarnation `{}` has no effect-fence evidence",
                    observed.incarnation.identity
                ),
            });
        };
        if !observed_fence.authorizes_successor(expected_fence) {
            return Ok(pc::SandboxObservation::Incompatible {
                reason: format!(
                    "durable Sandbox incarnation `{}` is fenced by a newer or foreign realization lease",
                    observed.incarnation.identity
                ),
            });
        }
    }
    if let Some(expected_adoption) = expected_adoption
        && observed.adoption_fingerprint.as_deref() != Some(expected_adoption.as_str())
    {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: format!(
                "durable Sandbox incarnation `{}` has missing or different adoption fingerprint",
                observed.incarnation.identity
            ),
        });
    }
    if let Some(expected_fingerprint) = expected_fingerprint
        && observed.fingerprint.as_deref() != Some(expected_fingerprint.as_str())
    {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: format!(
                "durable Sandbox incarnation `{}` has missing or different realization fingerprint",
                observed.incarnation.identity
            ),
        });
    }
    if expected_incarnation.is_some_and(|expected| observed.incarnation.identity != expected) {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: format!(
                "durable Sandbox incarnation `{}` resolved to foreign physical incarnation `{}`",
                expected_incarnation.unwrap_or_default(),
                observed.incarnation.identity
            ),
        });
    }
    if expected_incarnation.is_some()
        && observed.recovery == ExistingRealizationRecovery::CurrentAttemptOnly
        && matches!(
            observed.phase,
            ExistingRealizationPhase::Creating | ExistingRealizationPhase::Ready
        )
    {
        return Ok(pc::SandboxObservation::Incompatible {
            reason: format!(
                "durable Sandbox incarnation `{}` depends on a prior process-local realization attempt",
                observed.incarnation.identity
            ),
        });
    }
    match observed.phase {
        ExistingRealizationPhase::Creating => Ok(pc::SandboxObservation::Provisioning),
        ExistingRealizationPhase::Ready => Ok(pc::SandboxObservation::Ready),
        ExistingRealizationPhase::Terminal if expected_incarnation.is_some() => {
            Ok(pc::SandboxObservation::Terminal {
                physical_incarnation: observed.incarnation.identity.clone(),
            })
        }
        ExistingRealizationPhase::Terminal => Ok(pc::SandboxObservation::Incompatible {
            reason: "legacy Sandbox observation cannot prove terminal incarnation ownership".into(),
        }),
        ExistingRealizationPhase::Indeterminate => Err(RuntimeError::Backend(format!(
            "Sandbox observation for `{}` is in an indeterminate phase",
            expected_incarnation.unwrap_or("legacy-handle")
        ))),
    }
}

/// Non-authoritative liveness projection used by the legacy Sandbox status
/// surface. Unlike `sandbox_observation`, Gone here is never deletion evidence;
/// destructive recovery must use the exact fingerprinted observation above.
#[cfg(feature = "podman")]
pub(crate) fn container_state_observation(
    expected_incarnation: &str,
    observed: &[ExistingRealization],
) -> Result<ContainerState, RuntimeError> {
    let [observed] = observed else {
        return if observed.is_empty() {
            Ok(ContainerState::Gone)
        } else {
            Err(RuntimeError::Backend(format!(
                "container status resolved to {} physical realizations",
                observed.len()
            )))
        };
    };
    if observed.incarnation.identity != expected_incarnation {
        return Err(RuntimeError::Backend(format!(
            "container status for `{expected_incarnation}` resolved to foreign incarnation `{}`",
            observed.incarnation.identity
        )));
    }
    match observed.phase {
        ExistingRealizationPhase::Creating => Ok(ContainerState::Provisioning),
        ExistingRealizationPhase::Ready => Ok(ContainerState::Running),
        ExistingRealizationPhase::Terminal => Ok(ContainerState::Gone),
        ExistingRealizationPhase::Indeterminate => Err(RuntimeError::Backend(format!(
            "container `{expected_incarnation}` has indeterminate liveness"
        ))),
    }
}

/// Compatibility-only identity for the public unfenced `ContainerRuntime`
/// create seam. It lets legacy callers create an absent object through the
/// canonical adapter implementation while the shared decision kernel rejects
/// every existing object because no durable effect fence exists. It is never
/// persisted as Session realization truth.
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) fn legacy_unfenced_fingerprint(scope: &str) -> pc::SandboxRealizationFingerprint {
    pc::SandboxRealizationFingerprint::from_spec(&pc::SandboxSpec {
        scope: scope.to_owned(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Ephemeral,
        control_services: Default::default(),
        lease_ttl_secs: None,
    })
}

#[cfg(test)]
mod realization_tests {
    use super::*;

    fn fingerprint(scope: &str) -> pc::SandboxRealizationFingerprint {
        pc::SandboxRealizationFingerprint::from_spec(&pc::SandboxSpec {
            scope: scope.into(),
            isolation: pc::IsolationClass::Container,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::Unrestricted,
            outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            control_services: Default::default(),
            lease_ttl_secs: None,
        })
    }

    fn observed(
        fingerprint: Option<pc::SandboxRealizationFingerprint>,
        phase: ExistingRealizationPhase,
    ) -> ExistingRealization {
        ExistingRealization {
            locator: "physical-1".into(),
            incarnation: PhysicalIncarnation {
                identity: "incarnation-1".into(),
                version: Some("revision-1".into()),
            },
            adoption_fingerprint: fingerprint.as_ref().map(ToString::to_string),
            fingerprint: fingerprint.map(|value| value.to_string()),
            fence: Some(
                crate::ContainerEffectFence::new(
                    "operation-1",
                    "owner-1",
                    "runtime-1",
                    1,
                    u64::MAX,
                )
                .unwrap(),
            ),
            attempt_id: Some("attempt-1".into()),
            recovery: ExistingRealizationRecovery::CurrentAttemptOnly,
            phase,
        }
    }

    fn realization_decision_for_test(
        adoption: Option<&pc::SandboxRealizationFingerprint>,
        realization: Option<&pc::SandboxRealizationFingerprint>,
        effect_fence: Option<&crate::ContainerEffectFence>,
        intent: &crate::ContainerRealizationIntent,
        attempt: &crate::ContainerCreateAttempt,
        rebuild_continuity: RebuildContinuityEvidence,
        observed: &[ExistingRealization],
    ) -> Result<ExistingRealizationDecision, RuntimeError> {
        let adoption = adoption.ok_or_else(|| {
            RuntimeError::Backend(
                "test realization context requires an adoption fingerprint".into(),
            )
        })?;
        let context = crate::ContainerRealizationContext::new(
            "test-scope",
            adoption,
            effect_fence,
            intent,
            attempt,
        );
        existing_realization_decision(&context, realization, rebuild_continuity, observed)
    }

    #[test]
    fn exact_realization_recovery_decision_table_is_total() {
        // Cause/effect table: C1 selected object count 0/1/>1, C2 exact/missing/
        // different fingerprint, C3 Creating/Ready/Terminal/Indeterminate, C4
        // lifecycle evidence reusable/recreate-only; C5 the observed fence has
        // an older/equal/newer expiry than the incoming same-lease fence. R1 0=>create; R2
        // 1+exact+Creating+reusable=>converge; R3 1+exact+Ready+reusable=>reuse;
        // R4 exact Terminal from this attempt=>replace only the
        // API-observed immutable incarnation; R5 >1, !exact, missing incarnation,
        // Indeterminate, or a process-local prior attempt=>fail with no adapter
        // effect; R6 an equal/newer incoming expiry preserves the immutable
        // operation/owner/runtime/epoch identity and may reuse; R7 an incoming
        // expiry regression is rejected before adapter mutation. Both R6/R7
        // are owned by SandboxEffectFence::authorizes_successor.
        let exact = fingerprint("exact");
        let different = fingerprint("different");
        let fence =
            crate::ContainerEffectFence::new("operation-1", "owner-1", "runtime-1", 1, u64::MAX)
                .unwrap();
        let attempt = crate::ContainerCreateAttempt("attempt-1".into());
        let replacement_attempt = crate::ContainerCreateAttempt("attempt-2".into());
        assert_eq!(
            realization_decision_for_test(
                Some(&exact),
                Some(&exact),
                Some(&fence),
                &crate::ContainerRealizationIntent::Create,
                &attempt,
                RebuildContinuityEvidence::Unavailable,
                &[],
            )
            .unwrap(),
            ExistingRealizationDecision::Create,
            "R1"
        );
        for (phase, expected) in [
            (
                ExistingRealizationPhase::Creating,
                ExistingRealizationDecision::ConvergeCreating(observed(
                    Some(exact.clone()),
                    ExistingRealizationPhase::Creating,
                )),
            ),
            (
                ExistingRealizationPhase::Ready,
                ExistingRealizationDecision::ReuseReady(observed(
                    Some(exact.clone()),
                    ExistingRealizationPhase::Ready,
                )),
            ),
            (
                ExistingRealizationPhase::Terminal,
                ExistingRealizationDecision::ReplaceExact(observed(
                    Some(exact.clone()),
                    ExistingRealizationPhase::Terminal,
                )),
            ),
        ] {
            assert_eq!(
                realization_decision_for_test(
                    Some(&exact),
                    Some(&exact),
                    Some(&fence),
                    &crate::ContainerRealizationIntent::Create,
                    &attempt,
                    RebuildContinuityEvidence::Unavailable,
                    &[observed(Some(exact.clone()), phase)]
                )
                .unwrap(),
                expected
            );
        }
        let mut renewed_observed = observed(Some(exact.clone()), ExistingRealizationPhase::Ready);
        renewed_observed
            .fence
            .as_mut()
            .expect("test realization fence")
            .expires_at_unix_ms = 123;
        assert!(
            matches!(
                realization_decision_for_test(
                    Some(&exact),
                    Some(&exact),
                    Some(&fence),
                    &crate::ContainerRealizationIntent::Create,
                    &attempt,
                    RebuildContinuityEvidence::Unavailable,
                    &[renewed_observed],
                )
                .unwrap(),
                ExistingRealizationDecision::ReuseReady(_)
            ),
            "R6 renewed expiry",
        );
        let incoming_with_shorter_expiry =
            crate::ContainerEffectFence::new("operation-1", "owner-1", "runtime-1", 1, 122)
                .unwrap();
        let mut observed_with_newer_expiry =
            observed(Some(exact.clone()), ExistingRealizationPhase::Ready);
        observed_with_newer_expiry
            .fence
            .as_mut()
            .expect("test realization fence")
            .expires_at_unix_ms = 123;
        assert!(
            realization_decision_for_test(
                Some(&exact),
                Some(&exact),
                Some(&incoming_with_shorter_expiry),
                &crate::ContainerRealizationIntent::Create,
                &attempt,
                RebuildContinuityEvidence::Unavailable,
                &[observed_with_newer_expiry],
            )
            .is_err(),
            "R7 incoming expiry regression"
        );
        for phase in [
            ExistingRealizationPhase::Creating,
            ExistingRealizationPhase::Ready,
        ] {
            let exact_observed = observed(Some(exact.clone()), phase);
            assert!(
                realization_decision_for_test(
                    Some(&exact),
                    Some(&exact),
                    Some(&fence),
                    &crate::ContainerRealizationIntent::Create,
                    &replacement_attempt,
                    RebuildContinuityEvidence::Unavailable,
                    std::slice::from_ref(&exact_observed),
                )
                .is_err(),
                "R5 process-local participant evidence cannot be recreated"
            );
        }
        let mut older_process_local =
            observed(Some(exact.clone()), ExistingRealizationPhase::Terminal);
        older_process_local
            .fence
            .as_mut()
            .expect("test realization fence")
            .epoch = 0;
        assert!(
            realization_decision_for_test(
                Some(&exact),
                Some(&exact),
                Some(&fence),
                &crate::ContainerRealizationIntent::Create,
                &replacement_attempt,
                RebuildContinuityEvidence::Unavailable,
                &[older_process_local],
            )
            .is_err(),
            "R5 a newer lease cannot delete unrecoverable prior-attempt participants"
        );
        let mut missing_adoption = observed(Some(exact.clone()), ExistingRealizationPhase::Ready);
        missing_adoption.adoption_fingerprint = None;
        let mut different_adoption = observed(Some(exact.clone()), ExistingRealizationPhase::Ready);
        different_adoption.adoption_fingerprint = Some(different.to_string());
        let mut missing_physical = observed(Some(exact.clone()), ExistingRealizationPhase::Ready);
        missing_physical.fingerprint = None;
        let mut different_physical = observed(Some(exact.clone()), ExistingRealizationPhase::Ready);
        different_physical.fingerprint = Some(different.to_string());
        for rejected in [
            missing_adoption,
            different_adoption,
            missing_physical,
            different_physical,
            observed(Some(exact.clone()), ExistingRealizationPhase::Indeterminate),
        ] {
            assert!(
                realization_decision_for_test(
                    Some(&exact),
                    Some(&exact),
                    Some(&fence),
                    &crate::ContainerRealizationIntent::Create,
                    &attempt,
                    RebuildContinuityEvidence::Unavailable,
                    &[rejected],
                )
                .is_err(),
                "R5"
            );
        }
        let mut missing_incarnation =
            observed(Some(exact.clone()), ExistingRealizationPhase::Ready);
        missing_incarnation.incarnation.identity.clear();
        assert!(
            realization_decision_for_test(
                Some(&exact),
                Some(&exact),
                Some(&fence),
                &crate::ContainerRealizationIntent::Create,
                &attempt,
                RebuildContinuityEvidence::Unavailable,
                &[missing_incarnation],
            )
            .is_err(),
            "R5"
        );
        let duplicate = observed(Some(exact.clone()), ExistingRealizationPhase::Ready);
        assert!(
            realization_decision_for_test(
                Some(&exact),
                Some(&exact),
                Some(&fence),
                &crate::ContainerRealizationIntent::Create,
                &attempt,
                RebuildContinuityEvidence::Unavailable,
                &[duplicate.clone(), duplicate],
            )
            .is_err(),
            "R5"
        );
    }

    #[test]
    fn rebuild_continuity_and_operation_decision_table_is_total() {
        /* Rebuild cause/effect table. Causes: C1 primary object absent/exact
         * terminal/other phase; C2 exact retained continuation present/absent;
         * C3 source incarnation exact/foreign; C4 current lease exact/foreign;
         * C5 operation is original Create/new authorized Rebuild/unrelated
         * Create. Effects: E1 absent Create creates; E2 absent Rebuild creates
         * only around exact continuation; E3 exact reconstructible terminal
         * source is replaced; E4 every other row is rejected before adapter
         * mutation. Rules: B1 Create+absent=>E1; B2 Rebuild+absent+continuation
         * =>E2; B3 Rebuild+terminal+exact(C2-C4)=>E3 even when C5 changes from
         * Create to Rebuild; B4 missing/foreign/nonterminal or unrelated
         * same-lease Create=>E4. */
        let exact = fingerprint("exact-rebuild");
        let create_fence = crate::ContainerEffectFence::new(
            "create-operation",
            "owner-1",
            "runtime-1",
            7,
            u64::MAX,
        )
        .unwrap();
        let rebuild_fence = crate::ContainerEffectFence::new(
            "rebuild-operation",
            "owner-1",
            "runtime-1",
            7,
            u64::MAX,
        )
        .unwrap();
        let attempt = crate::ContainerCreateAttempt("rebuild-attempt".into());
        let intent = crate::ContainerRealizationIntent::Rebuild {
            source_incarnation: "incarnation-1".into(),
            source_runtime_handle: Some(
                pc::ContainerContinuationHandle::KubernetesContinuationV2 {
                    pod_uid: "incarnation-1".into(),
                    claim_uid: Some("claim-1".into()),
                },
            ),
        };

        assert!(
            realization_decision_for_test(
                Some(&exact),
                Some(&exact),
                Some(&rebuild_fence),
                &intent,
                &attempt,
                RebuildContinuityEvidence::Unavailable,
                &[],
            )
            .is_err(),
            "B4 absent continuity"
        );
        assert_eq!(
            realization_decision_for_test(
                Some(&exact),
                Some(&exact),
                Some(&rebuild_fence),
                &intent,
                &attempt,
                RebuildContinuityEvidence::ExactContinuation,
                &[],
            )
            .unwrap(),
            ExistingRealizationDecision::Create,
            "B2"
        );

        let mut terminal = observed(Some(exact.clone()), ExistingRealizationPhase::Terminal);
        terminal.recovery = ExistingRealizationRecovery::Reconstructible;
        terminal.fence = Some(create_fence.clone());
        assert_eq!(
            realization_decision_for_test(
                Some(&exact),
                Some(&exact),
                Some(&rebuild_fence),
                &intent,
                &attempt,
                RebuildContinuityEvidence::ExactContinuation,
                std::slice::from_ref(&terminal),
            )
            .unwrap(),
            ExistingRealizationDecision::ReplaceExact(terminal),
            "B3 same lease, authorized new Rebuild operation"
        );

        let unrelated_create = crate::ContainerRealizationIntent::Create;
        let existing = {
            let mut value = observed(Some(exact.clone()), ExistingRealizationPhase::Ready);
            value.recovery = ExistingRealizationRecovery::Reconstructible;
            value.fence = Some(create_fence);
            value
        };
        assert!(
            realization_decision_for_test(
                Some(&exact),
                Some(&exact),
                Some(&rebuild_fence),
                &unrelated_create,
                &attempt,
                RebuildContinuityEvidence::Unavailable,
                &[existing],
            )
            .is_err(),
            "B4 unrelated same-lease Create operation"
        );
    }

    #[test]
    fn exact_adoption_observation_decision_table_is_total() {
        // Cause/effect table: C1 exact query yields 0/1/>1 objects; C2 the one
        // object's immutable incarnation is exact/foreign; C3 phase is
        // Creating/Ready/Terminal/Indeterminate; C4 an optional physical-effect
        // lease is older/equal/newer/foreign/missing; C5 the live substrate is
        // reconstructible/current-attempt-only. R1 absence is unavailable while
        // exact Terminal requires authorization;
        // R2 exact reconstructible Creating and Ready remain adoptable; R3 duplicate,
        // foreign, indeterminate, process-local, missing evidence, a
        // newer/foreign lease, or a same-lease expiry regression fails closed
        // and cannot authorize rebuild. The same canonical successor predicate
        // owns this observation edge and realization admission above.
        let exact = fingerprint("exact");
        let reconstructible = |phase| {
            let mut value = observed(Some(exact.clone()), phase);
            value.recovery = ExistingRealizationRecovery::Reconstructible;
            value
        };
        let creating = reconstructible(ExistingRealizationPhase::Creating);
        let ready = reconstructible(ExistingRealizationPhase::Ready);
        let terminal = observed(Some(exact.clone()), ExistingRealizationPhase::Terminal);
        let indeterminate = observed(Some(exact.clone()), ExistingRealizationPhase::Indeterminate);

        assert_eq!(
            sandbox_observation(Some("incarnation-1"), Some(&exact), Some(&exact), None, &[],)
                .unwrap(),
            pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: Some("incarnation-1".into()),
            },
            "R1 absence"
        );
        assert_eq!(
            sandbox_observation(
                Some("incarnation-1"),
                Some(&exact),
                Some(&exact),
                None,
                &[creating],
            )
            .unwrap(),
            pc::SandboxObservation::Provisioning,
            "R2 creating"
        );
        assert_eq!(
            sandbox_observation(
                Some("incarnation-1"),
                Some(&exact),
                Some(&exact),
                None,
                std::slice::from_ref(&ready),
            )
            .unwrap(),
            pc::SandboxObservation::Ready,
            "R2 ready"
        );
        assert_eq!(
            sandbox_observation(
                Some("incarnation-1"),
                Some(&exact),
                Some(&exact),
                None,
                std::slice::from_ref(&terminal),
            )
            .unwrap(),
            pc::SandboxObservation::Terminal {
                physical_incarnation: "incarnation-1".into(),
            },
            "R1 terminal requires authorization"
        );

        let mut foreign = ready.clone();
        foreign.incarnation.identity = "incarnation-2".into();
        assert!(
            sandbox_observation(
                Some("incarnation-1"),
                Some(&exact),
                Some(&exact),
                None,
                &[indeterminate],
            )
            .is_err(),
            "R3 indeterminate"
        );
        for incompatible in [
            sandbox_observation(
                Some("incarnation-1"),
                Some(&exact),
                Some(&exact),
                None,
                &[foreign],
            ),
            sandbox_observation(
                Some("incarnation-1"),
                Some(&exact),
                Some(&exact),
                None,
                &[ready.clone(), ready],
            ),
            sandbox_observation(Some(""), Some(&exact), Some(&exact), None, &[]),
            sandbox_observation(None, None, None, None, &[]),
            sandbox_observation(None, None, None, None, &[terminal]),
            sandbox_observation(
                Some("incarnation-1"),
                Some(&exact),
                Some(&exact),
                None,
                &[observed(
                    Some(exact.clone()),
                    ExistingRealizationPhase::Ready,
                )],
            ),
        ] {
            assert!(
                matches!(
                    incompatible.unwrap(),
                    pc::SandboxObservation::Incompatible { .. }
                ),
                "R3 deterministic conflict"
            );
        }
        assert_eq!(
            sandbox_observation(
                None,
                None,
                None,
                None,
                &[observed(None, ExistingRealizationPhase::Ready)],
            )
            .unwrap(),
            pc::SandboxObservation::Ready,
            "R2 legacy ready"
        );
        let wrong_fingerprint = fingerprint("wrong");
        let mut wrong_adoption = observed(Some(exact.clone()), ExistingRealizationPhase::Ready);
        wrong_adoption.recovery = ExistingRealizationRecovery::Reconstructible;
        wrong_adoption.adoption_fingerprint = Some(wrong_fingerprint.to_string());
        assert!(
            matches!(
                sandbox_observation(
                    Some("incarnation-1"),
                    Some(&exact),
                    Some(&exact),
                    None,
                    &[wrong_adoption],
                )
                .unwrap(),
                pc::SandboxObservation::Incompatible { .. }
            ),
            "R3 wrong adoption fingerprint"
        );
        assert!(
            matches!(
                sandbox_observation(
                    Some("incarnation-1"),
                    Some(&exact),
                    Some(&wrong_fingerprint),
                    None,
                    &[reconstructible(ExistingRealizationPhase::Ready)],
                )
                .unwrap(),
                pc::SandboxObservation::Incompatible { .. }
            ),
            "R3 wrong fingerprint"
        );
        let mut missing_physical = observed(Some(exact.clone()), ExistingRealizationPhase::Ready);
        missing_physical.recovery = ExistingRealizationRecovery::Reconstructible;
        missing_physical.fingerprint = None;
        assert!(
            matches!(
                sandbox_observation(
                    Some("incarnation-1"),
                    Some(&exact),
                    Some(&exact),
                    None,
                    &[missing_physical],
                )
                .unwrap(),
                pc::SandboxObservation::Incompatible { .. }
            ),
            "R3 missing fingerprint"
        );

        let current_fence = crate::ContainerEffectFence::new(
            "terminal-operation",
            "owner-1",
            "runtime-1",
            2,
            u64::MAX,
        )
        .unwrap();
        let mut same_lease_different_operation = reconstructible(ExistingRealizationPhase::Ready);
        same_lease_different_operation.fence = Some(
            crate::ContainerEffectFence::new(
                "create-operation",
                "owner-1",
                "runtime-1",
                2,
                u64::MAX,
            )
            .unwrap(),
        );
        for admitted in [
            reconstructible(ExistingRealizationPhase::Ready),
            same_lease_different_operation,
        ] {
            assert_eq!(
                sandbox_observation(
                    Some("incarnation-1"),
                    Some(&exact),
                    Some(&exact),
                    Some(&current_fence),
                    &[admitted],
                )
                .unwrap(),
                pc::SandboxObservation::Ready,
                "R2 older lease and same-lease response loss remain observable",
            );
        }
        let mut missing_fence = reconstructible(ExistingRealizationPhase::Ready);
        missing_fence.fence = None;
        let mut newer_fence = reconstructible(ExistingRealizationPhase::Ready);
        newer_fence.fence = Some(
            crate::ContainerEffectFence::new("newer", "owner-2", "runtime-2", 3, u64::MAX).unwrap(),
        );
        let mut foreign_same_epoch = reconstructible(ExistingRealizationPhase::Ready);
        foreign_same_epoch.fence = Some(
            crate::ContainerEffectFence::new("foreign", "owner-2", "runtime-2", 2, u64::MAX)
                .unwrap(),
        );
        let shorter_current_fence =
            crate::ContainerEffectFence::new("terminal-operation", "owner-1", "runtime-1", 2, 100)
                .unwrap();
        let mut same_lease_newer_expiry = reconstructible(ExistingRealizationPhase::Ready);
        same_lease_newer_expiry.fence = Some(
            crate::ContainerEffectFence::new("create-operation", "owner-1", "runtime-1", 2, 101)
                .unwrap(),
        );
        for (current, rejected) in [
            (&current_fence, missing_fence),
            (&current_fence, newer_fence),
            (&current_fence, foreign_same_epoch),
            (&shorter_current_fence, same_lease_newer_expiry),
        ] {
            assert!(
                matches!(
                    sandbox_observation(
                        Some("incarnation-1"),
                        Some(&exact),
                        Some(&exact),
                        Some(current),
                        &[rejected],
                    )
                    .unwrap(),
                    pc::SandboxObservation::Incompatible { .. }
                ),
                "R3 fenced observation rejects missing/newer/foreign/regressed evidence",
            );
        }
    }

    #[test]
    fn exact_terminal_removal_decision_table_preserves_uncertainty() {
        // Cause/effect table: C1 exact observation is Provisioning/Ready,
        // Terminal, DefinitivelyUnavailable, Incompatible, or adapter error (the latter
        // never reaches this pure kernel). R1 live phases request one immutable
        // remove; R2 exact absence requests auxiliary-only convergence; R3 a
        // deterministic incompatibility rejects with no destructive effect.
        for observation in [
            pc::SandboxObservation::Provisioning,
            pc::SandboxObservation::Ready,
            pc::SandboxObservation::Terminal {
                physical_incarnation: "incarnation-1".into(),
            },
        ] {
            assert_eq!(
                exact_removal_decision(observation).unwrap(),
                ExactRemovalDecision::Remove,
                "R1",
            );
        }
        assert_eq!(
            exact_removal_decision(pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: Some("incarnation-1".into()),
            })
            .unwrap(),
            ExactRemovalDecision::AlreadyAbsent,
            "R2",
        );
        assert!(
            exact_removal_decision(pc::SandboxObservation::Incompatible {
                reason: "foreign".into(),
            })
            .is_err(),
            "R3",
        );
    }
}

/// One opaque agent process started inside an already-running container.
pub struct RuntimeAgentProcess {
    pub process: Box<dyn pc::ProcessHandle>,
    pub channel: Box<dyn AgentChannel>,
}

/// Independent package-image build/publish port.
#[async_trait]
pub trait PackageImageProvisioner: Send + Sync {
    async fn package_base_image_identity(&self, reference: &str) -> Result<String, RuntimeError> {
        Ok(reference.to_owned())
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError>;

    async fn package_image_available(
        &self,
        _base_image: &str,
        _packages: &pc::PackageRequirements,
        _network: &pc::NetworkPolicy,
        _image: &str,
    ) -> Result<bool, RuntimeError> {
        Ok(false)
    }
}

/// Runtime seam driven by the provider and implemented by Docker, Podman, K8s,
/// or the test fake. It owns no Session or application policy.
#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    /// Stable non-secret deployment facts that change the immutable runtime
    /// object but are not represented by the neutral SandboxSpec.
    fn realization_configuration(
        &self,
    ) -> Result<std::collections::BTreeMap<String, String>, RuntimeError> {
        Ok(std::collections::BTreeMap::from([(
            "adapter_type".into(),
            std::any::type_name::<Self>().into(),
        )]))
    }

    fn enforces_network_none(&self) -> bool {
        false
    }

    /// Live evidence that arbitrary workload traffic can reach the public
    /// network only through the configured allowlist proxy.
    fn enforces_network_allowlist(&self) -> bool {
        false
    }

    /// Revalidate backend availability and mutable external enforcement.
    /// Every adapter must name its evidence source; readiness has no permissive
    /// default that could accidentally advertise stale authority.
    async fn probe_ready(&self) -> Result<(), RuntimeError>;

    fn supports_package_provisioning(&self) -> bool {
        false
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        _network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        if packages.is_empty() {
            Ok(base_image.to_string())
        } else {
            Err(RuntimeError::Backend(
                "container runtime cannot provision package requirements".into(),
            ))
        }
    }

    fn has_native_memory_mounts(&self) -> bool {
        false
    }

    fn uses_persistent_volume_claims(&self) -> bool {
        false
    }

    /// Whether byte-backed mounts must be materialized as host files before
    /// runtime creation. Docker and Podman bind those files directly. Native
    /// runtimes such as Kubernetes carry the same resolved bytes in their API
    /// projection and must not also write a redundant plaintext host copy.
    fn uses_host_bind_materialization(&self) -> bool {
        true
    }

    fn supports_secret_writeback(&self) -> bool {
        true
    }

    fn sandbox_control_services(&self) -> std::collections::BTreeSet<SandboxControlServiceKind> {
        std::collections::BTreeSet::new()
    }

    async fn sandbox_control_binding(
        &self,
        _container_id: &str,
        request: SandboxControlBindingRequest<'_>,
    ) -> Result<Option<pc::SandboxControlIncarnation>, RuntimeError> {
        if request.required().is_empty() {
            Ok(None)
        } else {
            Err(RuntimeError::Backend(
                "container runtime cannot bind a Sandbox control service incarnation".into(),
            ))
        }
    }

    async fn open_sandbox_control_channel(
        &self,
        _container_id: &str,
        _binding: &pc::SandboxControlIncarnation,
        _kind: SandboxControlServiceKind,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not publish the requested Sandbox control service".into(),
        ))
    }

    fn uses_host_live_input_bind(&self) -> bool {
        false
    }

    fn supports_live_input_projection(&self) -> bool {
        self.uses_host_live_input_bind()
    }

    async fn project_live_input(
        &self,
        _container_id: &str,
        _path: &str,
        _bytes: &[u8],
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not support live input projection".into(),
        ))
    }

    async fn remove_live_input(
        &self,
        _container_id: &str,
        _path: &str,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not support live input projection".into(),
        ))
    }

    async fn read_live_file(
        &self,
        _container_id: &str,
        _path: &str,
    ) -> Result<Option<Vec<u8>>, RuntimeError> {
        Ok(None)
    }

    /// Read-only admission over the same stable-scope observation and pure
    /// decision kernel used immediately before the create/replace effect.
    async fn preflight_create_for_effect(
        &self,
        context: &crate::ContainerRealizationContext<'_>,
        _plan: &ContainerPlan,
        _realization_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    ) -> Result<(), RuntimeError> {
        if context.effect_fence.is_some() {
            Err(RuntimeError::Backend(
                "container runtime does not implement fenced realization admission".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// Legacy unfenced runtime seam retained for source compatibility. Managed
    /// Session providers use `create_for_effect`; downstream adapters continue
    /// to compile and may serve explicitly ephemeral callers through this port.
    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError>;

    async fn create_for_effect(
        &self,
        context: &crate::ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
        _realization_fingerprint: &pc::SandboxRealizationFingerprint,
    ) -> Result<String, RuntimeError> {
        if context.effect_fence.is_some() {
            return Err(RuntimeError::Backend(
                "container runtime does not implement fenced realization effects".into(),
            ));
        }
        self.create(context.scope, plan).await
    }

    async fn recover_restore_target(
        &self,
        _id: &str,
        _plan: &ContainerPlan,
        _plan_fingerprint: &str,
        _evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<Option<RuntimeRestoreTarget>, RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not implement exact restore target observation".into(),
        ))
    }

    async fn restore_or_adopt(
        &self,
        _id: &str,
        _plan: &ContainerPlan,
        _plan_fingerprint: &str,
        _evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<RuntimeRestoreTarget, RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not implement exact restore target acquisition".into(),
        ))
    }

    async fn restoration_evidence(
        &self,
        _container_id: &str,
    ) -> Result<Option<pc::SandboxRestorationEvidence>, RuntimeError> {
        Ok(None)
    }

    async fn restoration_plan_fingerprint(
        &self,
        _container_id: &str,
    ) -> Result<Option<String>, RuntimeError> {
        Ok(None)
    }

    async fn dispose_restore_target(
        &self,
        _id: &str,
        _plan: &ContainerPlan,
        _plan_fingerprint: &str,
        _evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not implement exact restored-target disposal".into(),
        ))
    }

    /// Runtime-owned, non-secret incarnation evidence persisted inside the
    /// canonical SandboxHandle. Most runtimes need none; Kubernetes uses it to
    /// fence retained-volume deletion across Worker replacement.
    async fn handle_extra(
        &self,
        _container_id: &str,
    ) -> Result<Option<pc::ContainerContinuationHandle>, RuntimeError> {
        Ok(None)
    }

    /// Observe the exact immutable physical incarnation named by a durable
    /// handle. Not-found/terminal is a typed result; backend, authorization,
    /// timeout, and parse failures remain errors and can never authorize rebuild.
    async fn observe(
        &self,
        _expectation: crate::ContainerObservationExpectation<'_>,
    ) -> Result<pc::SandboxObservation, RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not implement effect-free observation".into(),
        ))
    }

    async fn spawn(
        &self,
        _container_id: &str,
        _command: pc::MaterializedCommand,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not implement exec".into(),
        ))
    }

    async fn spawn_agent(
        &self,
        _container_id: &str,
        _command: pc::MaterializedCommand,
    ) -> Result<RuntimeAgentProcess, RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not implement attached exec".into(),
        ))
    }

    async fn process(
        &self,
        _container_id: &str,
        _process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime cannot reconnect to exec process".into(),
        ))
    }

    async fn open_channel(&self, container_id: &str)
    -> Result<Box<dyn AgentChannel>, RuntimeError>;
    async fn inspect(&self, container_id: &str) -> Result<ContainerState, RuntimeError>;
    async fn wait(&self, container_id: &str) -> Result<pc::ExitStatus, RuntimeError>;
    async fn poll(&self, container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError>;
    async fn signal(&self, container_id: &str, signal: pc::Signal) -> Result<(), RuntimeError>;
    async fn artifacts(&self, container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError>;
    async fn read_artifact(
        &self,
        container_id: &str,
        artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError>;
    async fn touch_lease(&self, container_id: &str) -> Result<(), RuntimeError>;
    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError>;

    async fn remove_with_handle(
        &self,
        container_id: &str,
        runtime_handle: Option<&pc::ContainerContinuationHandle>,
    ) -> Result<(), RuntimeError> {
        if runtime_handle.is_some() {
            return Err(RuntimeError::Backend(
                "container runtime cannot remove an unrecognized continuation handle".into(),
            ));
        }
        self.remove(container_id).await
    }

    /// Revalidate and remove one exact durable incarnation under an
    /// aggregate-owned effect fence. The default is intentionally fail-closed:
    /// a legacy `remove` implementation may target a mutable name rather than
    /// the immutable ID/UID carried by the handle.
    async fn dispose_authorized(
        &self,
        expectation: crate::ContainerObservationExpectation<'_>,
        authorization: &pc::SandboxDisposalAuthorization,
    ) -> Result<(), RuntimeError> {
        let effect_fence = authorization.effect_fence();
        if expectation.effect_fence != Some(effect_fence) {
            return Err(RuntimeError::Backend(
                "physical disposal observation differs from its typed successor fence".into(),
            ));
        }
        effect_fence
            .validate_live_at(
                container_runtime_unix_now_ms()
                    .map_err(|error| RuntimeError::Backend(error.to_string()))?,
            )
            .map_err(|error| RuntimeError::Backend(error.to_string()))?;
        match exact_removal_decision(self.observe(expectation).await?)? {
            // Adapter observation may report absence only after every typed
            // physical participant is gone. Kubernetes keeps a live/terminating
            // retained claim out of this row, so response-loss replay has no
            // auxiliary mutation left to perform.
            ExactRemovalDecision::AlreadyAbsent => Ok(()),
            ExactRemovalDecision::Remove => {
                self.remove_exact_incarnation(
                    expectation.container_id,
                    expectation.runtime_handle,
                    authorization,
                )
                .await
            }
        }
    }

    /// Adapter-only compare-and-delete effect reached after the shared exact
    /// observation policy. Implementations must use an immutable container ID
    /// or Pod UID precondition; a mutable name is never sufficient.
    async fn remove_exact_incarnation(
        &self,
        _container_id: &str,
        _runtime_handle: Option<&pc::ContainerContinuationHandle>,
        _authorization: &pc::SandboxDisposalAuthorization,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Backend(
            "container runtime does not implement immutable-incarnation removal".into(),
        ))
    }
}
