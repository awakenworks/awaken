//! The source Pod's existing metadata/CAS owns its checkpoint participant.
//! No Worker cache, additional object, finalizer or deletion authority.

use super::*;
use crate::checkpoint::ContainerCheckpointParticipant;
use crate::{ContainerCheckpointAccess, ContainerCheckpointMutation};

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
    access: ContainerCheckpointAccess<'_>,
) -> Result<(), RuntimeError> {
    access.admit().map_err(backend)?;
    let Some(crate::ContainerContinuationHandle::KubernetesContinuationV2 {
        pod_uid,
        claim_uid: Some(claim_uid),
    }) = expectation.runtime_handle
    else {
        return Err(backend(
            "Kubernetes checkpoint requires exact Pod and retained PVC UIDs",
        ));
    };
    if expectation.effect_fence != Some(access.effect_fence)
        || pod.metadata.name.as_deref() != Some(expectation.container_id)
        || pod.metadata.deletion_timestamp.is_some()
        || continuation::observed_pod_cleanup_authorization(pod)?.is_some()
        || continuation::bound_claim_uid(pod) != Some(claim_uid.as_str())
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

impl K8sRuntime {
    pub(super) async fn persist_checkpoint_upload(
        &self,
        expectation: ContainerObservationExpectation<'_>,
        access: ContainerCheckpointAccess<'_>,
        mutation: ContainerCheckpointMutation<'_>,
    ) -> Result<Option<pc::SandboxCheckpointUpload>, RuntimeError> {
        access.admit().map_err(backend)?;
        let pods = self.pods();
        let observed = pods.get(expectation.container_id).await.map_err(backend)?;
        admit_pod(&observed, expectation, access)?;
        let claim_name = continuation_claim_for_pod(expectation.container_id)?;
        let claim = self
            .persistent_volume_claims()
            .get(&claim_name)
            .await
            .map_err(backend)?;
        access.admit().map_err(backend)?;
        let expected_claim = continuation::bound_claim_uid(&observed)
            .expect("Pod admission proved the exact claim UID");
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
}
