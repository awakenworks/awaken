// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Pure exact comparison of an observed Pod or PersistentVolumeClaim with the immutable
// @formal reason: projection the provider writes; creation, original-object cleanup and restore share
// @formal reason: this one verifier. Kubernetes defaulting stays in api_defaults; no API effect here.
//! Exact verification of an observed Pod or claim against its immutable
//! projection, shared by creation, projected-object cleanup and restore.

use k8s_openapi::api::core::v1::{Container, PersistentVolumeClaim, Pod, VolumeMount};

use super::{api_defaults, backend, hardened_security_context, pod_projection, realization};
use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract as pc;
use awaken_worker_fleet_kubernetes::has_forbidden_sandbox_namespace_shape;

/// Decode only the historical directory initializer into the expected immutable
/// projection. New creation never emits it. Full spec and original digest still
/// have to match: an arbitrary init, command, image or mount is not normalized away.
fn expected_historical_directory_projection(
    expected: &Pod,
    observed: &Pod,
) -> Result<Pod, RuntimeError> {
    let mut expected = expected.clone();
    let has_legacy = observed
        .spec
        .as_ref()
        .and_then(|spec| spec.init_containers.as_ref())
        .is_some_and(|items| items.iter().any(|item| item.name == "continuation-init"));
    if !has_legacy {
        return Ok(expected);
    }
    let spec = expected
        .spec
        .as_mut()
        .ok_or_else(|| backend("desired Pod has no specification"))?;
    if spec
        .init_containers
        .as_ref()
        .is_some_and(|items| items.iter().any(|item| item.name == "continuation-init"))
    {
        return Ok(expected);
    }
    let agent = spec
        .containers
        .iter()
        .find(|item| item.name == "agent")
        .ok_or_else(|| backend("desired Pod has no agent"))?;
    let directories = agent
        .volume_mounts
        .iter()
        .flatten()
        .filter(|mount| mount.name == pod_projection::CONTINUATION_VOLUME)
        .filter_map(|mount| mount.sub_path.as_ref())
        .map(|path| format!("/state/{path}"))
        .collect::<Vec<_>>();
    if directories.is_empty() {
        return Ok(expected);
    }
    let initializer = Container {
        name: "continuation-init".into(),
        image: agent.image.clone(),
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("mkdir -p {}", directories.join(" ")),
        ]),
        volume_mounts: Some(vec![VolumeMount {
            name: pod_projection::CONTINUATION_VOLUME.into(),
            mount_path: "/state".into(),
            ..Default::default()
        }]),
        security_context: Some(hardened_security_context()),
        ..Default::default()
    };
    spec.init_containers
        .get_or_insert_with(Vec::new)
        .insert(0, initializer);
    realization::stamp_pod_realization(&mut expected)?;
    Ok(expected)
}

pub(super) fn verify_pod_projection(
    expected: &Pod,
    observed: &Pod,
    disposal: Option<&pc::SandboxDisposalAuthorization>,
) -> Result<(), RuntimeError> {
    let expected = expected_historical_directory_projection(expected, observed)?;
    let mut expected_spec = expected
        .spec
        .clone()
        .ok_or_else(|| backend("desired Kubernetes restore Pod has no specification"))?;
    let mut observed_spec = observed
        .spec
        .clone()
        .ok_or_else(|| backend("observed Kubernetes restore Pod has no specification"))?;
    api_defaults::normalize_pod_pair(&mut expected_spec, &mut observed_spec);
    let expected_labels = expected.metadata.labels.as_ref();
    let observed_labels = observed.metadata.labels.as_ref();
    let exact_labels = expected_labels.is_some_and(|expected| {
        expected.iter().all(|(key, value)| {
            key == awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL
                || observed_labels.and_then(|labels| labels.get(key)) == Some(value)
        })
    });
    if expected.metadata.name != observed.metadata.name
        || !exact_labels
        || expected_spec != observed_spec
        || has_forbidden_sandbox_namespace_shape(&observed_spec)
    {
        // Report schema field names only. Projection values can contain
        // execution inputs, so diagnostics must never print either full spec.
        let expected_fields = serde_json::to_value(&expected_spec).map_err(backend)?;
        let observed_fields = serde_json::to_value(&observed_spec).map_err(backend)?;
        let fields = expected_fields
            .as_object()
            .into_iter()
            .flat_map(|object| object.keys())
            .chain(
                observed_fields
                    .as_object()
                    .into_iter()
                    .flat_map(|object| object.keys()),
            )
            .filter(|key| expected_fields.get(*key) != observed_fields.get(*key))
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        return Err(backend(format!(
            "Kubernetes restore Pod differs from the complete immutable security projection (spec fields: {})",
            fields.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    realization::verify_realization(&expected, observed, disposal)
}

pub(super) fn verify_claim_projection(
    expected: &PersistentVolumeClaim,
    observed: &PersistentVolumeClaim,
    disposal: Option<&pc::SandboxDisposalAuthorization>,
) -> Result<(), RuntimeError> {
    verify_claim_projection_shape(expected, observed)?;
    realization::verify_realization(expected, observed, disposal)
}

/// The terminal guard already excludes any new Pod binding. A claim whose
/// exact UID has deletionTimestamp may be a lost response to this cleanup;
/// preserve the same immutable spec/digest check while allowing its deletion
/// to converge. The caller must separately verify guard policy, UID and the
/// predecessor effect fence before using this narrower terminal projection.
pub(super) fn verify_guarded_claim_projection(
    expected: &PersistentVolumeClaim,
    observed: &PersistentVolumeClaim,
) -> Result<(), RuntimeError> {
    verify_claim_projection_shape(expected, observed)?;
    if realization::realization_digest_matches(expected, observed)? {
        Ok(())
    } else {
        Err(backend(
            "guarded Kubernetes continuation PVC differs from its original realization",
        ))
    }
}

fn verify_claim_projection_shape(
    expected: &PersistentVolumeClaim,
    observed: &PersistentVolumeClaim,
) -> Result<(), RuntimeError> {
    let expected_spec = expected
        .spec
        .as_ref()
        .ok_or_else(|| backend("desired Kubernetes continuation PVC has no specification"))?;
    let observed_spec = observed
        .spec
        .as_ref()
        .ok_or_else(|| backend("observed Kubernetes continuation PVC has no specification"))?;
    // Omitted StorageClass delegates selection to Kubernetes. Admission may
    // assign the cluster default before returning CREATE (or later), so it is
    // not an explicitly pinned class. An explicit empty class instead disables
    // defaulting and must not be substituted for this delegated policy.
    // volumeName is the controller's binding output. Preserve every other spec
    // field exactly, including selectors and clone/data sources; a copied
    // realization annotation cannot authorize an extra storage input.
    let mut projected = expected_spec.clone();
    if projected.storage_class_name.is_none()
        && claim_storage_class_matches(None, observed_spec.storage_class_name.as_deref())
    {
        projected.storage_class_name = observed_spec.storage_class_name.clone();
    }
    projected.volume_name = observed_spec.volume_name.clone();
    if expected.metadata.name != observed.metadata.name || projected != *observed_spec {
        return Err(backend(
            "Kubernetes continuation PVC differs from the exact immutable projection",
        ));
    }
    Ok(())
}

/// An omitted desired storage class delegates exactly one choice to the
/// apiserver's defaulting admission. A named class remains byte-exact; an empty
/// admitted class is never a valid default projection.
fn claim_storage_class_matches(expected: Option<&str>, observed: Option<&str>) -> bool {
    match expected {
        Some(expected) => observed == Some(expected),
        None => observed.is_none_or(|observed| !observed.trim().is_empty()),
    }
}
