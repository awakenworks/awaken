// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Pure stamping and reading of the provider evidence on Kubernetes objects (effect
// @formal reason: fence, attempt and realization annotations, exact Pod owner); every create, adopt and
// @formal reason: delete path compares against this one codec. No API effect here.
//! The provider evidence a Sandbox Pod and its projected objects carry:
//! effect-fence, attempt and realization annotations and the exact Pod owner.

use awaken_sandbox_container::engine::{
    ExistingRealization, ExistingRealizationPhase, ExistingRealizationRecovery,
    PhysicalIncarnation, container_effect_fence_from_values,
};
use awaken_sandbox_container::{ContainerEffectFence, RuntimeError};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use super::backend;
use super::realization::{PodReadiness, pod_readiness};

pub(crate) const SANDBOX_REALIZATION_ANNOTATION: &str = "awaken.dev/sandbox-realization";
pub(crate) const SANDBOX_ADOPTION_ANNOTATION: &str = "awaken.dev/sandbox-adoption";
pub(crate) const SANDBOX_EFFECT_ANNOTATION: &str = "awaken.dev/sandbox-effect";
pub(crate) const SANDBOX_EFFECT_OWNER_ANNOTATION: &str = "awaken.dev/sandbox-effect-owner";
pub(crate) const SANDBOX_EFFECT_RUNTIME_ANNOTATION: &str = "awaken.dev/sandbox-effect-runtime";
pub(crate) const SANDBOX_EFFECT_EPOCH_ANNOTATION: &str = "awaken.dev/sandbox-effect-epoch";
pub(crate) const SANDBOX_EFFECT_EXPIRY_ANNOTATION: &str = "awaken.dev/sandbox-effect-expiry";
pub(crate) const SANDBOX_ATTEMPT_ANNOTATION: &str = "awaken.dev/sandbox-attempt";

pub(crate) fn k8s_attempt_label(attempt_id: &str) -> String {
    let digest = blake3::hash(attempt_id.as_bytes()).to_hex();
    format!("a-{}", &digest.as_str()[..32])
}

pub(crate) fn stamp_k8s_attempt<K>(object: &mut K, attempt_id: &str)
where
    K: kube::Resource<DynamicType = ()>,
{
    object
        .meta_mut()
        .labels
        .get_or_insert_with(Default::default)
        .insert(
            awaken_sandbox_container::engine::SANDBOX_ATTEMPT_LABEL.to_owned(),
            k8s_attempt_label(attempt_id),
        );
}

pub(crate) fn observed_pod_phase(pod: &Pod) -> ExistingRealizationPhase {
    if pod.metadata.deletion_timestamp.is_some() {
        return ExistingRealizationPhase::Indeterminate;
    }
    match pod_readiness(pod) {
        PodReadiness::Ready => ExistingRealizationPhase::Ready,
        PodReadiness::Waiting(_) => ExistingRealizationPhase::Creating,
        PodReadiness::Failed(_) => ExistingRealizationPhase::Terminal,
    }
}

pub(crate) fn observed_pod_incarnation(pod: &Pod) -> Result<PhysicalIncarnation, RuntimeError> {
    Ok(PhysicalIncarnation {
        identity: pod
            .metadata
            .uid
            .clone()
            .ok_or_else(|| backend("Kubernetes Sandbox Pod has no UID"))?,
        version: Some(
            pod.metadata
                .resource_version
                .clone()
                .ok_or_else(|| backend("Kubernetes Sandbox Pod has no resourceVersion"))?,
        ),
    })
}

pub(crate) fn pod_effect_fence(
    annotations: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<Option<ContainerEffectFence>, RuntimeError> {
    container_effect_fence_from_values(
        annotations
            .and_then(|values| values.get(SANDBOX_EFFECT_ANNOTATION))
            .map(String::as_str),
        annotations
            .and_then(|values| values.get(SANDBOX_EFFECT_OWNER_ANNOTATION))
            .map(String::as_str),
        annotations
            .and_then(|values| values.get(SANDBOX_EFFECT_RUNTIME_ANNOTATION))
            .map(String::as_str),
        annotations
            .and_then(|values| values.get(SANDBOX_EFFECT_EPOCH_ANNOTATION))
            .map(String::as_str),
        annotations
            .and_then(|values| values.get(SANDBOX_EFFECT_EXPIRY_ANNOTATION))
            .map(String::as_str),
    )
}

pub(crate) fn stamp_effect_fence<K>(object: &mut K, effect_fence: &ContainerEffectFence)
where
    K: kube::Resource<DynamicType = ()>,
{
    let annotations = object
        .meta_mut()
        .annotations
        .get_or_insert_with(Default::default);
    annotations.insert(
        SANDBOX_EFFECT_ANNOTATION.to_owned(),
        effect_fence.operation_id().to_owned(),
    );
    annotations.insert(
        SANDBOX_EFFECT_OWNER_ANNOTATION.to_owned(),
        effect_fence.owner().to_owned(),
    );
    annotations.insert(
        SANDBOX_EFFECT_RUNTIME_ANNOTATION.to_owned(),
        effect_fence.runtime_incarnation().to_owned(),
    );
    annotations.insert(
        SANDBOX_EFFECT_EPOCH_ANNOTATION.to_owned(),
        effect_fence.epoch().to_string(),
    );
    annotations.insert(
        SANDBOX_EFFECT_EXPIRY_ANNOTATION.to_owned(),
        effect_fence.expires_at_unix_ms().to_string(),
    );
}

pub(crate) fn observed_pod(pod: &Pod) -> Result<ExistingRealization, RuntimeError> {
    let locator = pod
        .metadata
        .name
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no name"))?;
    let annotations = pod.metadata.annotations.as_ref();
    Ok(ExistingRealization {
        locator,
        incarnation: observed_pod_incarnation(pod)?,
        adoption_fingerprint: annotations
            .and_then(|annotations| annotations.get(SANDBOX_ADOPTION_ANNOTATION))
            .cloned(),
        fingerprint: annotations
            .and_then(|annotations| annotations.get(SANDBOX_REALIZATION_ANNOTATION))
            .cloned(),
        fence: pod_effect_fence(annotations)?,
        attempt_id: annotations
            .and_then(|annotations| annotations.get(SANDBOX_ATTEMPT_ANNOTATION))
            .cloned(),
        recovery: ExistingRealizationRecovery::Reconstructible,
        phase: observed_pod_phase(pod),
    })
}

pub(crate) fn pod_owner_reference(
    pod: &Pod,
    expected_uid: &str,
) -> Result<OwnerReference, RuntimeError> {
    let name = pod
        .metadata
        .name
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no name"))?;
    let uid = pod
        .metadata
        .uid
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no UID"))?;
    if uid != expected_uid {
        return Err(backend(
            "Kubernetes Sandbox Pod incarnation changed before projection",
        ));
    }
    if pod.metadata.deletion_timestamp.is_some() {
        return Err(backend(
            "Kubernetes Sandbox Pod is terminating before projection",
        ));
    }
    Ok(OwnerReference {
        api_version: "v1".into(),
        kind: "Pod".into(),
        name,
        uid,
        controller: Some(true),
        block_owner_deletion: Some(false),
    })
}

pub(crate) fn has_exact_pod_owner(metadata: &ObjectMeta, pod_name: &str, pod_uid: &str) -> bool {
    metadata.owner_references.as_deref().is_some_and(|owners| {
        let [owner] = owners else {
            return false;
        };
        owner.api_version == "v1"
            && owner.kind == "Pod"
            && owner.name == pod_name
            && owner.uid == pod_uid
            && owner.controller == Some(true)
    })
}

pub(crate) fn has_exact_attempt(metadata: &ObjectMeta, attempt_id: &str) -> bool {
    metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(SANDBOX_ATTEMPT_ANNOTATION))
        .is_some_and(|attempt| attempt == attempt_id)
        && metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(awaken_sandbox_container::engine::SANDBOX_ATTEMPT_LABEL))
            .is_some_and(|attempt| attempt == &k8s_attempt_label(attempt_id))
}

pub(crate) fn verify_projected_content(
    metadata: &ObjectMeta,
    pod_name: &str,
    pod_uid: &str,
    attempt_id: &str,
) -> Result<(), RuntimeError> {
    if has_exact_attempt(metadata, attempt_id) && has_exact_pod_owner(metadata, pod_name, pod_uid) {
        Ok(())
    } else {
        Err(backend(
            "Kubernetes projected content differs from its exact Pod owner or attempt",
        ))
    }
}

pub(crate) fn projected_content_delete_preconditions(
    metadata: &ObjectMeta,
    pod_name: &str,
    pod_uid: &str,
    attempt_id: &str,
) -> Result<Option<kube::api::Preconditions>, RuntimeError> {
    if !has_exact_attempt(metadata, attempt_id) {
        return Ok(None);
    }
    let exact_owner = has_exact_pod_owner(metadata, pod_name, pod_uid);
    if !exact_owner {
        return Ok(None);
    }
    let uid = metadata
        .uid
        .clone()
        .ok_or_else(|| backend("Kubernetes projected content has no UID"))?;
    let resource_version = metadata
        .resource_version
        .clone()
        .ok_or_else(|| backend("Kubernetes projected content has no resourceVersion"))?;
    Ok(Some(kube::api::Preconditions {
        uid: Some(uid),
        resource_version: Some(resource_version),
    }))
}
