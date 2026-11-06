// @formal surface obligation=sandbox.fail_closed_policy_never_downgrades
// @formal reason: The existing Pod handoff composes original projection/effect/participant readers;
// @formal reason: the existing finalizer module alone owns GET/replace/409 and callback ordering.
//! Exact Pod metadata handoff over the original participant and realization evidence.

use awaken_container_contract::RuntimeError;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;

use super::super::{backend, realization};

/// Transfer only the process-local runtime-owner lease on one already
/// realization-fenced Pod. Kubernetes resourceVersion supplies the CAS fence;
/// the expected UID prevents a same-name replacement from being adopted.
pub(crate) async fn transfer_runtime_owner(
    api: &Api<Pod>,
    pod: Pod,
    expected_uid: &str,
    next_owner: &str,
) -> Result<Pod, RuntimeError> {
    if pod.metadata.uid.as_deref() != Some(expected_uid) {
        return Err(backend(
            "Kubernetes Sandbox Pod changed before runtime-owner transfer",
        ));
    }
    let has_owner = |observed: &Pod| {
        observed
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL))
            .map(String::as_str)
            == Some(next_owner)
    };
    if has_owner(&pod) {
        return Ok(pod);
    }
    let name = pod
        .metadata
        .name
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no name"))?;
    let verify_identity = |observed: &Pod| {
        if observed.metadata.uid.as_deref() != Some(expected_uid) {
            return Err(backend(
                "Kubernetes Sandbox Pod changed during runtime-owner transfer",
            ));
        }
        verify_pod_metadata_identity(&pod, observed)?;
        if super::retained(&pod)? != super::retained(observed)? {
            return Err(backend(
                "Kubernetes Sandbox Pod changed its original projected results",
            ));
        }
        Ok(())
    };
    let replacement = |observed: &Pod| {
        verify_identity(observed)?;
        if has_owner(observed) {
            return Ok(None);
        }
        if observed
            .metadata
            .resource_version
            .as_deref()
            .is_none_or(|rv| rv.trim().is_empty())
        {
            return Err(backend(
                "Kubernetes Sandbox Pod has no resourceVersion for runtime-owner transfer",
            ));
        }
        let mut replacement = observed.clone();
        replacement
            .metadata
            .labels
            .get_or_insert_with(Default::default)
            .insert(
                awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL.to_string(),
                next_owner.to_owned(),
            );
        Ok(Some(replacement))
    };
    let verify = |written: &Pod| {
        verify_identity(written)?;
        if !has_owner(written) {
            return Err(backend(
                "Kubernetes Sandbox Pod runtime-owner transfer was not retained",
            ));
        }
        Ok(())
    };
    // This original four-argument API has no effect fence. Preserve that
    // boundary for prepare/restore/control; do not invent expiry authority.
    awaken_worker_fleet_kubernetes::pod_metadata::update_pod_metadata(
        api,
        &name,
        Some(pod.clone()),
        true,
        &awaken_worker_fleet_kubernetes::pod_metadata::unfenced_checkpoint,
        &replacement,
        &verify,
    )
    .await?
    .ok_or_else(|| backend("Kubernetes Sandbox Pod disappeared during runtime-owner transfer"))
}

/// Reuse the existing complete projection and typed effect/restore readers for
/// both Pod-only metadata mutations. Status, resourceVersion and unrelated
/// metadata may advance; immutable execution and original intent may not.
pub(super) fn verify_pod_metadata_identity(
    expected: &Pod,
    observed: &Pod,
) -> Result<(), RuntimeError> {
    super::super::projection_verification::verify_pod_projection(expected, observed, None)?;
    let value = |pod: &Pod, key: &str| {
        pod.metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(key))
            .cloned()
    };
    if [
        super::super::SANDBOX_ATTEMPT_ANNOTATION,
        super::super::SANDBOX_ADOPTION_ANNOTATION,
        super::super::SANDBOX_REALIZATION_ANNOTATION,
        super::DECLARATION,
        awaken_container_contract::ISOLATION_ID_ANNOTATION,
        awaken_container_contract::REQUEST_DIGEST_ANNOTATION,
    ]
    .into_iter()
    .any(|key| value(expected, key) != value(observed, key))
        || expected.metadata.namespace != observed.metadata.namespace
        || super::super::pod_effect_fence(expected.metadata.annotations.as_ref())?
            != super::super::pod_effect_fence(observed.metadata.annotations.as_ref())?
        || realization::restoration_evidence(expected)?
            != realization::restoration_evidence(observed)?
        || realization::restoration_plan_fingerprint(expected)
            != realization::restoration_plan_fingerprint(observed)
    {
        return Err(backend(
            "original projected-object CAS changed its effect or attempt",
        ));
    }
    Ok(())
}
