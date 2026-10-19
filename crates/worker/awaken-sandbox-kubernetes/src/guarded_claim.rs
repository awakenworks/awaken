// @formal surface obligation=resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal reason: The terminal-only guarded claim: its physical state, observation and exact removal under
// @formal reason: the reservation's UID and live isolation policy; claim deletion stays with continuation.
//! The terminal-only guarded claim a joined Worker Create left behind: its
//! physical state, its observation, and its exact removal.

use super::K8sRuntime;
use super::backend;
use super::continuation;
use super::continuation_claim_for_pod;
use super::create_ledger;
use super::pod_effect_fence;
use super::pod_name;
use super::projection_verification;
use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxEffectFence;
use awaken_provisioning_contract::SandboxObservationPurpose;
use awaken_sandbox_container::ContainerObservationExpectation;
use awaken_sandbox_container::ContainerPlan;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;

pub(super) fn verify_joined_original_claim(
    runtime: &K8sRuntime,
    scope: &str,
    plan: &ContainerPlan,
    predecessor: &SandboxEffectFence,
    ticket_uid: &str,
    observed: &PersistentVolumeClaim,
) -> Result<(), RuntimeError> {
    if observed.metadata.uid.as_deref() != Some(ticket_uid)
        || observed.metadata.deletion_timestamp.is_some()
    {
        return Err(backend(
            "original continuation claim UID or live state changed",
        ));
    }
    let desired = runtime
        .desired_create_claim_for_effect(scope, plan, predecessor)?
        .ok_or_else(|| backend("original continuation claim has no frozen retained plan"))?;
    super::existing_realization::create_claim_admission(&desired, Some(observed), predecessor)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GuardedClaimPhysical {
    Incompatible,
    Absent,
    Live,
    Deleting,
}

pub(crate) async fn guarded_claim_physical_state(
    runtime: &K8sRuntime,
    container_id: &str,
    guarded: &awaken_sandbox_container::KubernetesGuardedClaimHandle,
) -> Result<GuardedClaimPhysical, RuntimeError> {
    let Some(guard_uid) = guarded.guard_uid() else {
        return Ok(GuardedClaimPhysical::Incompatible);
    };
    let runtime_id = runtime.realization_runtime_id(guarded.scope())?;
    let canonical_pod = pod_name(&runtime_id);
    let canonical_claim = continuation_claim_for_pod(&canonical_pod)?;
    if container_id != canonical_pod
        || guarded.guard().request.resource != "pods"
        || guarded.guard().request.name != canonical_pod
        || guarded.guard().request.namespace != runtime.namespace
        || guarded.guard().isolation_pending
        || guarded.claim_uid().trim().is_empty()
    {
        return Ok(GuardedClaimPhysical::Incompatible);
    }
    if !create_ledger::verify_pod_isolation(&runtime.clients.control, guarded.guard(), guard_uid)
        .await?
    {
        return Ok(GuardedClaimPhysical::Incompatible);
    }
    let Some(claim) =
        crate::get_optional(&runtime.persistent_volume_claims(), &canonical_claim).await?
    else {
        return Ok(GuardedClaimPhysical::Absent);
    };
    let desired = runtime.desired_claim_for_effect(guarded.scope(), guarded.predecessor())?;
    let claim_fence = pod_effect_fence(claim.metadata.annotations.as_ref())?;
    if claim.metadata.uid.as_deref() != Some(guarded.claim_uid())
        || claim.metadata.resource_version.is_none()
        || !claim_fence.as_ref().is_some_and(|fence| {
            continuation::original_claim_effect_matches(guarded.predecessor(), fence)
        })
        || projection_verification::verify_guarded_claim_projection(&desired, &claim).is_err()
    {
        return Ok(GuardedClaimPhysical::Incompatible);
    }
    Ok(if claim.metadata.deletion_timestamp.is_some() {
        GuardedClaimPhysical::Deleting
    } else {
        GuardedClaimPhysical::Live
    })
}

pub(super) async fn observe_guarded_claim(
    runtime: &K8sRuntime,
    expectation: ContainerObservationExpectation<'_>,
    guarded: &awaken_sandbox_container::KubernetesGuardedClaimHandle,
) -> Result<pc::SandboxObservation, RuntimeError> {
    let incompatible = || pc::SandboxObservation::Incompatible {
        reason: "guarded Kubernetes claim differs from its terminal-only physical evidence".into(),
    };
    if expectation.purpose != SandboxObservationPurpose::TerminalCleanup
        || expectation.effect_fence.is_none()
        || expectation.adoption_fingerprint.is_none()
        || expectation.realization_fingerprint.is_none()
    {
        return Ok(incompatible());
    }
    let Some(guard_uid) = guarded.guard_uid() else {
        return Ok(incompatible());
    };
    Ok(
        match guarded_claim_physical_state(runtime, expectation.container_id, guarded).await? {
            GuardedClaimPhysical::Incompatible => incompatible(),
            GuardedClaimPhysical::Absent => pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: Some(guard_uid.to_owned()),
            },
            GuardedClaimPhysical::Live => pc::SandboxObservation::Terminal {
                physical_incarnation: guard_uid.to_owned(),
            },
            GuardedClaimPhysical::Deleting => pc::SandboxObservation::Disposing {
                physical_incarnation: guard_uid.to_owned(),
            },
        },
    )
}

pub(crate) async fn remove_guarded_claim(
    runtime: &K8sRuntime,
    container_id: &str,
    guarded: &awaken_sandbox_container::KubernetesGuardedClaimHandle,
    authorization: &pc::SandboxDisposalAuthorization,
    authority: &dyn pc::CurrentExecutionAuthority,
) -> Result<(), RuntimeError> {
    authorization.validate().map_err(backend)?;
    let admit = || {
        pc::with_current_sandbox_effect(authorization.effect_fence(), authority, |_| Ok(()))
            .map_err(backend)
    };
    admit()?;
    match guarded_claim_physical_state(runtime, container_id, guarded).await? {
        GuardedClaimPhysical::Incompatible => {
            return Err(backend(
                "guarded continuation claim changed before exact disposal",
            ));
        }
        GuardedClaimPhysical::Absent => return admit(),
        GuardedClaimPhysical::Live | GuardedClaimPhysical::Deleting => {}
    }
    admit()?;
    let claims = runtime.persistent_volume_claims();
    if let Some(pending) =
        continuation::initiate_claim_deletion(&claims, container_id, guarded.claim_uid(), &admit)
            .await?
    {
        continuation::await_claim_deletion(&claims, &pending, &admit).await?;
    }
    admit()?;
    if guarded_claim_physical_state(runtime, container_id, guarded).await?
        != GuardedClaimPhysical::Absent
    {
        return Err(backend(
            "guarded continuation claim or its Pod name did not close after deletion",
        ));
    }
    admit()
}
