// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: Static owner: checkpoint participant persistence in the source Pod metadata under its
// @formal reason: existing UID and resourceVersion CAS. Dynamic path: recheck current grant and exact
// @formal reason: Pod/PVC binding, persist pending or complete evidence, then validate the returned
// @formal reason: participant; replay reobserves physical truth and conflict or response loss returns
// @formal reason: error. Kubernetes IO, concurrency and eventual reconciliation remain outside a direct
// @formal reason: formal-proof claim.
//! The source Pod's existing metadata/CAS owns its checkpoint participant.
//! No Worker cache, additional object, finalizer or deletion authority.

use super::{
    K8sRuntime, backend, continuation, continuation_claim_for_pod, observed_pod, pod_effect_fence,
    stamp_effect_fence,
};
use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerObservationExpectation;
use awaken_sandbox_container::engine::sandbox_observation;
use k8s_openapi::api::core::v1::Pod;
use kube::api::PostParams;
use participant::ContainerCheckpointParticipant;

pub(super) mod participant;
use awaken_provisioning_contract::{SandboxCheckpointAccess, SandboxCheckpointMutation};

const CHECKPOINT_ANNOTATION: &str = "awaken.dev/checkpoint-upload";

fn participant(pod: &Pod) -> Result<Option<ContainerCheckpointParticipant>, RuntimeError> {
    pod.metadata
        .annotations
        .as_ref()
        .and_then(|values| values.get(CHECKPOINT_ANNOTATION))
        .map(|value| serde_json::from_str(value).map_err(backend))
        .transpose()
}

fn admit_pod(
    pod: &Pod,
    expectation: ContainerObservationExpectation<'_>,
    access: SandboxCheckpointAccess<'_>,
) -> Result<(), RuntimeError> {
    access.admit().map_err(backend)?;
    if expectation
        .runtime_handle
        .is_some_and(awaken_sandbox_container::ContainerContinuationHandle::terminal_only)
    {
        return Err(backend("joined-empty evidence cannot authorize checkpoint"));
    }
    let Some((pod_uid, Some(claim_uid))) = expectation
        .runtime_handle
        .and_then(awaken_sandbox_container::ContainerContinuationHandle::kubernetes_incarnation)
    else {
        return Err(backend(
            "Kubernetes checkpoint requires exact Pod and retained PVC UIDs",
        ));
    };
    if expectation.effect_fence != Some(access.effect_fence)
        || pod.metadata.name.as_deref() != Some(expectation.container_id)
        || pod.metadata.deletion_timestamp.is_some()
        || continuation::observed_pod_cleanup_authorization(pod)?.is_some()
        || continuation::bound_claim_uid(pod) != Some(claim_uid)
        || continuation::bound_claim_name(pod)?
            != Some(continuation_claim_for_pod(expectation.container_id)?.as_str())
    {
        return Err(backend(
            "Kubernetes checkpoint source is deleting, gated or differs from its exact binding",
        ));
    }
    let observed = observed_pod(pod)?;
    if !matches!(
        sandbox_observation(
            Some(pod_uid),
            expectation.adoption_fingerprint,
            expectation.realization_fingerprint,
            expectation.effect_fence,
            &[observed],
            expectation.retained_attempt,
        )?,
        pc::SandboxObservation::Ready | pc::SandboxObservation::Terminal { .. }
    ) {
        return Err(backend(
            "Kubernetes checkpoint source is not an exact available realization",
        ));
    }
    Ok(())
}

pub(super) async fn persist_checkpoint_upload(
    runtime: &K8sRuntime,
    expectation: ContainerObservationExpectation<'_>,
    access: SandboxCheckpointAccess<'_>,
    mutation: SandboxCheckpointMutation<'_>,
) -> Result<Option<pc::SandboxCheckpointUpload>, RuntimeError> {
    access.admit().map_err(backend)?;
    let pods = runtime.pods();
    let observed = pods.get(expectation.container_id).await.map_err(backend)?;
    admit_pod(&observed, expectation, access)?;
    let claim_name = continuation_claim_for_pod(expectation.container_id)?;
    let claim = runtime
        .persistent_volume_claims()
        .get(&claim_name)
        .await
        .map_err(backend)?;
    access.admit().map_err(backend)?;
    let expected_claim =
        continuation::bound_claim_uid(&observed).expect("Pod admission proved the exact claim UID");
    continuation::rebuild_claim_admission(&claim, expected_claim)?;
    let previous = participant(&observed)?;
    let next = ContainerCheckpointParticipant::apply(previous.as_ref(), access, mutation)
        .map_err(backend)?;
    // Exact replay needs no write, but still consumed fresh Pod/PVC and
    // current-authority evidence. A copied receipt alone is never a grant.
    if previous.as_ref() == Some(&next)
        && pod_effect_fence(observed.metadata.annotations.as_ref())?.as_ref()
            == Some(access.effect_fence)
    {
        return Ok(next.upload());
    }
    let mut replacement = observed.clone();
    stamp_effect_fence(&mut replacement, access.effect_fence);
    replacement
        .metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert(
            CHECKPOINT_ANNOTATION.into(),
            serde_json::to_string(&next).map_err(backend)?,
        );
    access.admit().map_err(backend)?;
    // Preserve the observed UID and resourceVersion in the one Pod replace.
    // Conflict or response loss returns error; aggregate retry observes the
    // accepted prefix rather than silently overwriting a competing write.
    let written = pods
        .replace(
            expectation.container_id,
            &PostParams::default(),
            &replacement,
        )
        .await
        .map_err(backend)?;
    admit_pod(&written, expectation, access)?;
    if participant(&written)?.as_ref() != Some(&next)
        || pod_effect_fence(written.metadata.annotations.as_ref())?.as_ref()
            != Some(access.effect_fence)
    {
        return Err(backend(
            "Kubernetes checkpoint CAS response changed its exact participant",
        ));
    }
    Ok(next.upload())
}
