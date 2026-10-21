// @formal surface obligation=sandbox_continuation.ephemeral_never_allocates_or_references_a_claim
// @formal reason: Observation of the objects that already exist for one scope and the create or
// @formal reason: rebuild decision over them; claim admission follows the one continuation selector.
//! What already exists for one Sandbox scope, and the create, rebuild and
//! continuation decisions taken over that observation.

use super::{
    ContinuationObservationDisposition, ContinuationObservationEvidence, K8sRuntime,
    SANDBOX_ADOPTION_ANNOTATION, SANDBOX_ATTEMPT_ANNOTATION, SANDBOX_REALIZATION_ANNOTATION,
    admit_network, api_not_found, backend, continuation, continuation_claim_for_pod,
    continuation_claim_observation_required, continuation_observation_disposition,
    create_or_verify, observed_pod_incarnation, observed_pod_phase, pod_effect_fence, pod_name,
    projection_verification, rebuild_claim_admission, rebuild_continuation_expectation,
    unenforceable_k8s_limit,
};
use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::{SandboxEffectFence, SandboxObservationPurpose};
use awaken_sandbox_container::engine::{
    ExistingRealization, ExistingRealizationDecision, ExistingRealizationPhase,
    ExistingRealizationRecovery, RebuildContinuityEvidence, existing_realization_decision,
};
use awaken_sandbox_container::{ContainerPlan, ContainerRealizationContext, ContainerRuntime as _};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};

pub(super) fn create_claim_admission(
    desired: &PersistentVolumeClaim,
    observed: Option<&PersistentVolumeClaim>,
    expected_fence: &SandboxEffectFence,
) -> Result<continuation::CreateClaimAdmission, RuntimeError> {
    let Some(observed) = observed else {
        return continuation::create_claim_admission(expected_fence, None);
    };
    let observed_fence = pod_effect_fence(observed.metadata.annotations.as_ref())?;
    let admission = continuation::create_claim_admission(
        expected_fence,
        Some(continuation::CreateClaimObservation {
            effect_fence: observed_fence.as_ref(),
            uid_present: observed.metadata.uid.is_some(),
            terminating: observed.metadata.deletion_timestamp.is_some(),
        }),
    )?;
    // First Create, 409 replay and handle-free terminal reconstruction
    // share one complete immutable PVC projection verifier.
    projection_verification::verify_claim_projection(desired, observed, None)?;
    Ok(admission)
}

pub(super) async fn preflight_create_claim(
    runtime: &K8sRuntime,
    context: &ContainerRealizationContext<'_>,
    plan: &ContainerPlan,
) -> Result<(), RuntimeError> {
    let Some(desired) = runtime.desired_create_claim(context, plan)? else {
        return Ok(());
    };
    let expected_fence = context
        .effect_fence
        .expect("desired retained claim requires an effect fence");
    let name = desired
        .metadata
        .name
        .as_deref()
        .expect("canonical continuation claim has a name");
    let observed = crate::get_optional(&runtime.persistent_volume_claims(), name).await?;
    create_claim_admission(&desired, observed.as_ref(), expected_fence).map(drop)
}

pub(super) async fn create_or_recover_claim(
    runtime: &K8sRuntime,
    context: &ContainerRealizationContext<'_>,
    plan: &ContainerPlan,
) -> Result<Option<PersistentVolumeClaim>, RuntimeError> {
    let Some(desired) = runtime.desired_create_claim(context, plan)? else {
        return Ok(None);
    };
    let expected_fence = context
        .effect_fence
        .expect("desired retained claim requires an effect fence");
    let claims = runtime.persistent_volume_claims();
    let observed = create_or_verify(&claims, &desired).await?;
    match create_claim_admission(&desired, Some(&observed), expected_fence)? {
        continuation::CreateClaimAdmission::Reuse => Ok(Some(observed)),
        continuation::CreateClaimAdmission::Create => {
            unreachable!("an API-created or conflict-observed continuation claim is present")
        }
    }
}

pub(super) async fn existing_realizations(
    runtime: &K8sRuntime,
    scope: &str,
    plan: &ContainerPlan,
) -> Result<Vec<ExistingRealization>, RuntimeError> {
    let runtime_id = runtime.realization_runtime_id(scope)?;
    let name = pod_name(&runtime_id);
    let Some(pod) = crate::get_optional(&runtime.pods(), &name).await? else {
        return Ok(Vec::new());
    };
    if pod
        .metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get(awaken_sandbox_container::engine::MANAGED_SANDBOX_LABEL))
        .map(String::as_str)
        != Some("1")
    {
        return Err(backend(
            "Kubernetes stable Sandbox name is occupied by a non-Awaken Pod",
        ));
    }
    let annotations = pod.metadata.annotations.as_ref();
    let fingerprint = annotations
        .and_then(|values| values.get(SANDBOX_REALIZATION_ANNOTATION))
        .cloned();
    let fence = pod_effect_fence(annotations)?;
    let attempt_id = annotations
        .and_then(|values| values.get(SANDBOX_ATTEMPT_ANNOTATION))
        .cloned();
    let mut phase = observed_pod_phase(&pod);

    let expected_claim =
        continuation::claim_name(&runtime_id, plan, runtime.continuation_volume.is_some());
    let pod_claim = continuation::bound_claim_name(&pod)?;
    match (expected_claim.as_deref(), pod_claim) {
        (None, None) => {}
        (Some(expected), Some(actual)) if expected == actual => {
            let live_claim_uid = match runtime.persistent_volume_claims().get(expected).await {
                Ok(claim) => Some(continuation::claim_uid(&claim)?),
                Err(error) if api_not_found(&error) => None,
                Err(error) => return Err(backend(error)),
            };
            if live_claim_uid.as_deref() != continuation::bound_claim_uid(&pod) {
                return Err(backend(
                    "Kubernetes continuation PVC differs from the Pod's immutable binding evidence",
                ));
            }
        }
        _ => phase = ExistingRealizationPhase::Indeterminate,
    }

    let incarnation = observed_pod_incarnation(&pod)?;
    let recovery = if plan.egress_identity.ephemeral_capability || attempt_id.is_none() {
        ExistingRealizationRecovery::CurrentAttemptOnly
    } else {
        ExistingRealizationRecovery::Reconstructible
    };
    Ok(vec![ExistingRealization {
        locator: name,
        incarnation,
        adoption_fingerprint: annotations
            .and_then(|values| values.get(SANDBOX_ADOPTION_ANNOTATION))
            .cloned(),
        fingerprint,
        fence,
        attempt_id,
        recovery,
        phase,
    }])
}

pub(super) async fn create_decision(
    runtime: &K8sRuntime,
    context: &ContainerRealizationContext<'_>,
    plan: &ContainerPlan,
    realization_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
) -> Result<ExistingRealizationDecision, RuntimeError> {
    preflight_create_claim(runtime, context, plan).await?;
    let rebuild_continuity = rebuild_continuity_evidence(runtime, context, plan).await?;
    existing_realization_decision(
        context,
        realization_fingerprint,
        rebuild_continuity,
        &existing_realizations(runtime, context.scope, plan).await?,
    )
}

/// Revalidate the aggregate-authorized source continuation immediately
/// before the shared create/replace decision. A stable PVC name is not
/// continuity evidence: only the exact persisted UID may preserve a
/// retained workspace after the source Pod is absent or terminal.
pub(super) async fn rebuild_continuity_evidence(
    runtime: &K8sRuntime,
    context: &ContainerRealizationContext<'_>,
    plan: &ContainerPlan,
) -> Result<RebuildContinuityEvidence, RuntimeError> {
    let runtime_id = runtime.realization_runtime_id(context.scope)?;
    let expected = rebuild_continuation_expectation(
        context.intent,
        continuation::claim_name(&runtime_id, plan, runtime.continuation_volume.is_some()),
    )?;
    let Some(expected) = expected else {
        return Ok(RebuildContinuityEvidence::Unavailable);
    };
    let claim = match runtime
        .persistent_volume_claims()
        .get(&expected.claim_name)
        .await
    {
        Ok(claim) => claim,
        Err(error) if api_not_found(&error) => {
            return Err(backend(
                "Kubernetes rebuild source continuation PVC is absent",
            ));
        }
        Err(error) => return Err(backend(error)),
    };
    rebuild_claim_admission(&claim, &expected.claim_uid)?;
    Ok(RebuildContinuityEvidence::ExactContinuation)
}

pub(super) async fn admit_plan(
    runtime: &K8sRuntime,
    plan: &ContainerPlan,
) -> Result<(), RuntimeError> {
    if let Some(limit) = unenforceable_k8s_limit(&plan.limits) {
        return Err(RuntimeError::Backend(format!(
            "k8s cannot enforce a per-Pod `{limit}` limit (it is a node/kubelet \
             setting, not a Pod-spec field); refusing to place a `{limit}`-limited \
             spec on the k8s tier rather than silently dropping the cap"
        )));
    }
    if !matches!(plan.network, awaken_sandbox_container::NetworkMode::Open) {
        runtime.probe_ready().await?;
    }
    admit_network(
        &plan.network,
        runtime.network_policy_attestation.current(),
        runtime.network_policy_attestation.allowlist_current(),
    )
}

pub(super) async fn observed_continuation_disposition(
    runtime: &K8sRuntime,
    pod_name: &str,
    pod: Option<&Pod>,
    runtime_handle: Option<&awaken_sandbox_container::ContainerContinuationHandle>,
    purpose: SandboxObservationPurpose,
    effect_fenced: bool,
    effect_fenced_total_absence: bool,
) -> Result<ContinuationObservationDisposition, RuntimeError> {
    let expected_claim_uid = match runtime_handle {
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuation {
            claim_uid,
        }) => Some(claim_uid.as_str()),
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV2 {
            claim_uid,
            ..
        })
        | Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
            claim_uid,
            ..
        }) => claim_uid.as_deref(),
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesGuardedClaim(_)) => {
            return Err(backend(
                "guarded Kubernetes claim has no ordinary Pod continuation",
            ));
        }
        Some(awaken_sandbox_container::ContainerContinuationHandle::HostBindRestoration(_)) => {
            return Err(backend(
                "Kubernetes observation received a host-bind continuation handle",
            ));
        }
        None => None,
    };
    observed_continuation_disposition_for_claim(
        runtime,
        pod_name,
        pod,
        expected_claim_uid,
        purpose,
        effect_fenced,
        effect_fenced_total_absence,
    )
    .await
}

pub(super) async fn observed_continuation_disposition_for_claim(
    runtime: &K8sRuntime,
    pod_name: &str,
    pod: Option<&Pod>,
    expected_claim_uid: Option<&str>,
    purpose: SandboxObservationPurpose,
    effect_fenced: bool,
    effect_fenced_total_absence: bool,
) -> Result<ContinuationObservationDisposition, RuntimeError> {
    let claim_name = continuation_claim_for_pod(pod_name)?;
    if let Some(pod) = pod {
        let bound_claim = continuation::bound_claim_name(pod)?;
        let bound_claim_uid = continuation::bound_claim_uid(pod);
        let binding_matches = match expected_claim_uid {
            None => bound_claim.is_none() && bound_claim_uid.is_none(),
            Some(expected_uid) => {
                bound_claim == Some(claim_name.as_str()) && bound_claim_uid == Some(expected_uid)
            }
        };
        if !binding_matches {
            return Ok(ContinuationObservationDisposition::Incompatible);
        }
    }
    if !continuation_claim_observation_required(
        runtime.continuation_volume.is_some(),
        expected_claim_uid.is_some(),
    ) {
        return Ok(ContinuationObservationDisposition::Live);
    }
    let observed_claim = match runtime.persistent_volume_claims().get(&claim_name).await {
        Ok(claim) => Some(claim),
        Err(error) if api_not_found(&error) => None,
        Err(error) => return Err(backend(error)),
    };
    let cleanup_authorization = pod
        .map(continuation::observed_pod_cleanup_authorization)
        .transpose()?
        .flatten();
    let cleanup_gate_held = cleanup_authorization.is_some();
    continuation_observation_disposition(ContinuationObservationEvidence {
        purpose,
        pod_present: pod.is_some(),
        pod_deleting: pod.is_some_and(|pod| pod.metadata.deletion_timestamp.is_some()),
        cleanup_gate_held,
        original_guard_retained: pod
            .is_some_and(|pod| continuation::original_guard_retained(pod, purpose)),
        gc_independent: pod.is_some_and(|pod| {
            pod.metadata
                .owner_references
                .as_ref()
                .is_none_or(Vec::is_empty)
        }) && observed_claim.as_ref().is_some_and(|claim| {
            claim
                .metadata
                .owner_references
                .as_ref()
                .is_none_or(Vec::is_empty)
        }),
        expected_uid: expected_claim_uid,
        observed_uid: observed_claim
            .as_ref()
            .map(continuation::claim_uid)
            .transpose()?
            .as_deref(),
        observed_terminating: observed_claim
            .as_ref()
            .is_some_and(|claim| claim.metadata.deletion_timestamp.is_some()),
        effect_fenced,
        effect_fenced_total_absence,
    })
}
