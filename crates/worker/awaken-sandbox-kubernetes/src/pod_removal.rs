// @formal surface obligation=resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal reason: Exact Pod replacement and removal under the UID/resourceVersion fence and the
// @formal reason: retained-claim gate; delegates claim deletion to the continuation owner.
//! Exact Sandbox Pod replacement and removal: delete only the observed
//! incarnation, await its absence, then release the retained claim and the
//! original projected objects.

use super::{
    ContinuationObservationDisposition, K8sRuntime, SANDBOX_ATTEMPT_ANNOTATION, api_not_found,
    backend, continuation, continuation_claim_for_pod, lifecycle, observed_pod_incarnation,
    participants,
};
use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxObservationPurpose;
use awaken_sandbox_container::engine::ExistingRealization;

pub(super) async fn replace_exact_realization(
    runtime: &K8sRuntime,
    observed: &ExistingRealization,
) -> Result<(), RuntimeError> {
    let pod = match runtime.pods().get(&observed.locator).await {
        Ok(pod) => pod,
        Err(error) if api_not_found(&error) => return Ok(()),
        Err(error) => return Err(backend(error)),
    };
    if pod.metadata.finalizers.as_ref().is_some_and(|values| {
        values
            .iter()
            .any(|value| value == awaken_container_contract::ORIGINAL_GUARD_FINALIZER)
    }) {
        return Err(backend(
            "brokered Pod replacement requires the original terminal cleanup owner",
        ));
    }
    let resource_version = observed
        .incarnation
        .version
        .as_deref()
        .ok_or_else(|| backend("Kubernetes Sandbox observation has no resourceVersion"))?;
    lifecycle::delete_exact_pod(
        runtime,
        &pod,
        &observed.incarnation.identity,
        resource_version,
        observed.attempt_id.as_deref(),
    )
    .await
}

pub(super) async fn remove_bound(
    runtime: &K8sRuntime,
    container_id: &str,
    persisted_pod_uid: Option<&str>,
    persisted_claim_uid: Option<&str>,
    projected: Option<&awaken_sandbox_container::KubernetesProjectedParticipants>,
    release_original_guard: bool,
    authorization: Option<(
        &pc::SandboxDisposalAuthorization,
        &dyn pc::CurrentExecutionAuthority,
    )>,
) -> Result<(), RuntimeError> {
    // One borrowed admission callback threads the existing operation-bound
    // authority through all cleanup writes. It owns no lease or retry state.
    // Unfenced legacy removal retains its existing UID/continuation gates;
    // aggregate cleanup cannot supply provenance without current authority.
    let admit = || {
        if let Some((authorization, authority)) = authorization {
            pc::with_current_sandbox_effect(authorization.effect_fence(), authority, |_| Ok(()))
                .map_err(backend)?;
        }
        Ok(())
    };
    admit()?;
    participants::validate_bound(
        runtime,
        projected,
        persisted_pod_uid,
        None,
        authorization.is_some(),
    )?;
    let cleanup_authorization = authorization.map(|(authorization, _)| {
        continuation::PodCleanupAuthorization::Aggregate(authorization.clone())
    });
    let authorization = cleanup_authorization.as_ref();
    let effect_fenced = authorization.is_some();
    let pods = runtime.pods();
    let observed_pod = match pods.get(container_id).await {
        Ok(pod) => Some(pod),
        Err(error) if api_not_found(&error) => None,
        Err(error) => return Err(backend(error)),
    };
    admit()?;
    participants::validate_bound(
        runtime,
        projected,
        persisted_pod_uid,
        observed_pod.as_ref(),
        effect_fenced,
    )?;
    let guarded_original = awaken_worker_fleet_kubernetes::original_guard_release_required(
        observed_pod.as_ref(),
        release_original_guard,
    )?;
    let pod_claim_uid = continuation::validated_bound_pod_claim_uid(
        observed_pod.as_ref(),
        persisted_pod_uid,
        persisted_claim_uid,
    )?;
    let expected_claim_uid = persisted_claim_uid.or(pod_claim_uid.as_deref());
    if runtime.continuation_volume.is_some() && expected_claim_uid.is_none() {
        let claim_name = continuation_claim_for_pod(container_id)?;
        match runtime.persistent_volume_claims().get(&claim_name).await {
            Err(error) if api_not_found(&error) => {}
            Err(error) => return Err(backend(error)),
            Ok(_) => {
                return Err(backend(
                    "continuation PVC exists without persisted incarnation evidence",
                ));
            }
        }
    }

    let continuation = super::existing_realization::observed_continuation_disposition_for_claim(
        runtime,
        container_id,
        observed_pod.as_ref(),
        expected_claim_uid,
        SandboxObservationPurpose::TerminalCleanup,
        effect_fenced,
        effect_fenced && persisted_claim_uid.is_some(),
    )
    .await?;
    admit()?;
    if continuation == ContinuationObservationDisposition::Incompatible {
        return Err(backend(
            "Kubernetes Sandbox continuation state differs from its durable handle",
        ));
    }

    let Some(observed_pod) = observed_pod else {
        return continuation::finish_after_absent_pod(
            runtime,
            container_id,
            expected_claim_uid,
            projected,
            &admit,
        )
        .await;
    };

    if continuation == ContinuationObservationDisposition::Disposing
        && observed_pod.metadata.deletion_timestamp.is_some()
        && continuation::observed_pod_cleanup_authorization(&observed_pod)?.is_none()
    {
        // Cause/effect row KCO1/K3: PVC termination plus exact deleting Pod
        // without our finalizer can only be reached after this cleanup
        // owner released the gate (the replace response may have been
        // lost). No mutation remains authorized: await exact A, then
        // re-read and await exact P. A foreign replacement UID ends only
        // the A wait and still cannot redirect the claim fence.
        let pod_uid = persisted_pod_uid.expect("disposing Pod has persisted UID");
        return continuation::finish_released_bound_disposal(
            runtime,
            container_id,
            pod_uid,
            expected_claim_uid,
            projected,
            guarded_original,
            &admit,
        )
        .await;
    }

    // Cause/effect order for a retained source is fixed: exact Pod A first
    // acquires the durable cleanup finalizer by UID/resourceVersion CAS;
    // only that winner may establish P's deletion intent. Rebuild DELETE
    // winning the CAS leaves P untouched. Once P is terminating, begin A's
    // exact deletion, remove only our finalizer, then await both objects.
    let (pending_pod_cleanup, pending_claim_deletion) = continuation::begin_bound_disposal(
        runtime,
        container_id,
        &observed_pod,
        persisted_pod_uid,
        expected_claim_uid,
        authorization,
        &admit,
    )
    .await?;
    let pod = pending_pod_cleanup
        .as_ref()
        .map_or(&observed_pod, |pending| &pending.pod);
    let incarnation = observed_pod_incarnation(pod)?;
    let resource_version = incarnation
        .version
        .as_deref()
        .expect("Kubernetes Pod observations always include resourceVersion");
    let attempt_id = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(SANDBOX_ATTEMPT_ANNOTATION))
        .map(String::as_str);
    if let Some(projected) = projected {
        participants::finish_projected(runtime, projected, container_id, true, &admit).await?;
    }
    lifecycle::begin_exact_pod_delete(
        runtime,
        pod,
        &incarnation.identity,
        resource_version,
        if projected.is_some() {
            None
        } else {
            attempt_id
        },
        &admit,
    )
    .await?;
    continuation::release_bound_guards(
        &pods,
        container_id,
        &incarnation.identity,
        pending_pod_cleanup.as_ref(),
        guarded_original,
        &admit,
    )
    .await?;
    lifecycle::await_exact_pod_deleted(runtime, container_id, &incarnation.identity, &admit)
        .await?;
    if let Some(pending) = pending_claim_deletion.as_ref() {
        admit()?;
        continuation::await_claim_deletion(&runtime.persistent_volume_claims(), pending, &admit)
            .await?;
    }
    participants::finish_cleanup(runtime, projected, container_id, &admit).await
}
