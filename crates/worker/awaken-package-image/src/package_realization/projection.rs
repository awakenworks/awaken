// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: Pure exact package-image evidence code moved by responsibility;
// @formal reason: no wire, snapshot, hashing or authorization rules change.
//! Immutable package object projections and stamps.

use super::identity::{
    bounded_text, destination_repository, has_no_owner_references, runtime_error, valid_fingerprint,
};
use super::{
    K8S_PACKAGE_REALIZATION_CONTRACT_VERSION, PACKAGE_BUILD_JOB_KIND, PACKAGE_BUILD_NAME_PREFIX,
    PACKAGE_CONFIG_MAP_UID_ANNOTATION, PACKAGE_IMAGE_CHECK_JOB_KIND,
    PACKAGE_IMAGE_CHECK_NAME_PREFIX, PACKAGE_IMAGE_DESTINATION_ANNOTATION,
    PACKAGE_JOB_KIND_ANNOTATION, PACKAGE_PROJECTION_DIGEST_ANNOTATION,
    PACKAGE_REALIZATION_CONTRACT_ANNOTATION, PACKAGE_RECIPE_FINGERPRINT_ANNOTATION,
    REALIZATION_DIGEST_ANNOTATION, RELEASE_ANNOTATION_VALUE_BUDGET, RESOLVED_IMAGE_ANNOTATION,
    SANDBOX_SCOPE_ANNOTATION,
};
use awaken_container_contract::RuntimeError;
use awaken_worker_fleet_kubernetes::api_defaults::normalize_pod_spec;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::batch::v1::JobSpec;
use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde::Serialize;
use std::collections::BTreeMap;

pub(crate) fn package_build_name(fingerprint: &str) -> Result<String, RuntimeError> {
    if !valid_fingerprint(fingerprint) {
        return Err(runtime_error(
            "package recipe fingerprint is not canonical lowercase hex",
        ));
    }
    Ok(format!("{PACKAGE_BUILD_NAME_PREFIX}{}", &fingerprint[..24]))
}

pub(crate) fn package_image_check_name(image: &str) -> Result<String, RuntimeError> {
    if !bounded_text(image) {
        return Err(runtime_error(
            "package image-check identity is empty or unbounded",
        ));
    }
    let fingerprint = blake3::hash(image.as_bytes()).to_hex().to_string();
    Ok(format!(
        "{PACKAGE_IMAGE_CHECK_NAME_PREFIX}{}",
        &fingerprint[..20]
    ))
}

pub(crate) fn package_config_annotations(
    fingerprint: &str,
    destination: &str,
) -> Result<BTreeMap<String, String>, RuntimeError> {
    package_build_name(fingerprint)?;
    destination_repository(destination, fingerprint)
        .map_err(|error| runtime_error(error.to_string()))?;
    Ok(BTreeMap::from([
        (
            PACKAGE_REALIZATION_CONTRACT_ANNOTATION.into(),
            K8S_PACKAGE_REALIZATION_CONTRACT_VERSION.into(),
        ),
        (
            PACKAGE_RECIPE_FINGERPRINT_ANNOTATION.into(),
            fingerprint.into(),
        ),
        (
            PACKAGE_IMAGE_DESTINATION_ANNOTATION.into(),
            destination.into(),
        ),
    ]))
}

pub(super) fn correlation_annotations(
    fingerprint: &str,
    destination: &str,
    config_uid: &str,
    kind: &str,
) -> Result<BTreeMap<String, String>, RuntimeError> {
    if !bounded_text(config_uid) {
        return Err(runtime_error("package ConfigMap has no bounded UID"));
    }
    if !matches!(kind, PACKAGE_BUILD_JOB_KIND | PACKAGE_IMAGE_CHECK_JOB_KIND) {
        return Err(runtime_error("unknown package Job kind"));
    }
    let mut annotations = package_config_annotations(fingerprint, destination)?;
    annotations.insert(PACKAGE_CONFIG_MAP_UID_ANNOTATION.into(), config_uid.into());
    annotations.insert(PACKAGE_JOB_KIND_ANNOTATION.into(), kind.into());
    Ok(annotations)
}

pub(super) fn digest_json<T: Serialize>(value: &T) -> Result<String, RuntimeError> {
    let encoded = serde_json::to_vec(value).map_err(|error| runtime_error(error.to_string()))?;
    Ok(blake3::hash(&encoded).to_hex().to_string())
}

#[derive(Serialize)]
pub(super) struct ConfigMapProjection<'a> {
    name: Option<&'a str>,
    namespace: Option<&'a str>,
    data: &'a Option<BTreeMap<String, String>>,
    binary_data: &'a Option<BTreeMap<String, k8s_openapi::ByteString>>,
    immutable: Option<bool>,
    correlation: BTreeMap<String, String>,
}

pub(super) fn owned_annotations(metadata: &ObjectMeta, keys: &[&str]) -> BTreeMap<String, String> {
    metadata
        .annotations
        .as_ref()
        .into_iter()
        .flat_map(|annotations| annotations.iter())
        .filter(|(key, _)| keys.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub(super) fn config_map_projection(config: &ConfigMap) -> ConfigMapProjection<'_> {
    ConfigMapProjection {
        name: config.metadata.name.as_deref(),
        namespace: config.metadata.namespace.as_deref(),
        data: &config.data,
        binary_data: &config.binary_data,
        immutable: config.immutable,
        correlation: owned_annotations(
            &config.metadata,
            &[
                PACKAGE_REALIZATION_CONTRACT_ANNOTATION,
                PACKAGE_RECIPE_FINGERPRINT_ANNOTATION,
                PACKAGE_IMAGE_DESTINATION_ANNOTATION,
            ],
        ),
    }
}

pub(crate) fn stamp_package_config_map(config: &mut ConfigMap) -> Result<(), RuntimeError> {
    config.immutable = Some(true);
    let digest = digest_json(&config_map_projection(config))?;
    config
        .metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert(PACKAGE_PROJECTION_DIGEST_ANNOTATION.into(), digest);
    Ok(())
}

pub(super) fn normalize_job_pod_labels(
    labels: &mut Option<BTreeMap<String, String>>,
    observed_pod: bool,
) {
    if let Some(labels) = labels {
        for key in [
            "batch.kubernetes.io/controller-uid",
            "batch.kubernetes.io/job-name",
            "controller-uid",
            "job-name",
        ] {
            labels.remove(key);
        }
        if observed_pod {
            // GKE admission projects the selected node's standard topology
            // coordinates onto the created Pod, not its immutable Job template.
            // Only these scheduling observations are normalized; every other
            // injected label remains part of the exact evidence comparison.
            labels.remove("topology.kubernetes.io/region");
            labels.remove("topology.kubernetes.io/zone");
        }
    }
}

pub(super) fn normalized_job_spec(job: &Job) -> Result<JobSpec, RuntimeError> {
    let mut spec = job
        .spec
        .clone()
        .ok_or_else(|| runtime_error("package Job has no spec"))?;
    spec.selector = None;
    if spec.completions == Some(1) {
        spec.completions = None;
    }
    if spec.parallelism == Some(1) {
        spec.parallelism = None;
    }
    if spec.completion_mode.as_deref() == Some("NonIndexed") {
        spec.completion_mode = None;
    }
    if spec.suspend == Some(false) {
        spec.suspend = None;
    }
    if spec.manual_selector == Some(false) {
        spec.manual_selector = None;
    }
    // Kubernetes 1.31+ defaults Jobs without a podFailurePolicy to
    // TerminatingOrFailed. It is API-owned serialization noise, just like the
    // controller selector and NonIndexed defaults above, and must not make an
    // otherwise identical deterministic package Job fail its 409 reuse fence.
    if spec.pod_replacement_policy.as_deref() == Some("TerminatingOrFailed")
        && spec.pod_failure_policy.is_none()
    {
        spec.pod_replacement_policy = None;
    }
    normalize_job_pod_labels(
        &mut spec
            .template
            .metadata
            .get_or_insert_with(Default::default)
            .labels,
        false,
    );
    if let Some(pod) = spec.template.spec.as_mut() {
        normalize_pod_spec(pod, false);
    }
    Ok(spec)
}

#[derive(Serialize)]
pub(super) struct JobProjection {
    name: Option<String>,
    namespace: Option<String>,
    labels: Option<BTreeMap<String, String>>,
    correlation: BTreeMap<String, String>,
    spec: JobSpec,
}

pub(super) fn job_projection(job: &Job) -> Result<JobProjection, RuntimeError> {
    Ok(JobProjection {
        name: job.metadata.name.clone(),
        namespace: job.metadata.namespace.clone(),
        labels: job.metadata.labels.clone(),
        correlation: owned_annotations(
            &job.metadata,
            &[
                PACKAGE_REALIZATION_CONTRACT_ANNOTATION,
                PACKAGE_RECIPE_FINGERPRINT_ANNOTATION,
                PACKAGE_IMAGE_DESTINATION_ANNOTATION,
                PACKAGE_CONFIG_MAP_UID_ANNOTATION,
                PACKAGE_JOB_KIND_ANNOTATION,
            ],
        ),
        spec: normalized_job_spec(job)?,
    })
}

pub(crate) fn bind_package_job(
    job: &mut Job,
    fingerprint: &str,
    destination: &str,
    config_uid: &str,
    kind: &str,
) -> Result<(), RuntimeError> {
    let annotations = correlation_annotations(fingerprint, destination, config_uid, kind)?;
    job.metadata.annotations = Some(annotations.clone());
    let template = job
        .spec
        .as_mut()
        .ok_or_else(|| runtime_error("package Job has no spec"))?
        .template
        .metadata
        .get_or_insert_with(Default::default);
    template.annotations = Some(annotations);
    let digest = digest_json(&job_projection(job)?)?;
    job.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert(PACKAGE_PROJECTION_DIGEST_ANNOTATION.into(), digest);
    Ok(())
}

/// Bind a package Job to the API-observed immutable ConfigMap incarnation. The
/// ConfigMap UID does not exist before create/exact-reuse, so callers cannot
/// manufacture this edge from the deterministic name alone.
pub(crate) fn bind_package_job_to_config(
    job: &mut Job,
    config: &ConfigMap,
    kind: &str,
) -> Result<(), RuntimeError> {
    let annotations = config
        .metadata
        .annotations
        .as_ref()
        .ok_or_else(|| runtime_error("package ConfigMap has no correlation annotations"))?;
    let fingerprint = annotations
        .get(PACKAGE_RECIPE_FINGERPRINT_ANNOTATION)
        .ok_or_else(|| runtime_error("package ConfigMap has no recipe fingerprint"))?;
    let destination = annotations
        .get(PACKAGE_IMAGE_DESTINATION_ANNOTATION)
        .ok_or_else(|| runtime_error("package ConfigMap has no image destination"))?;
    let uid = config
        .metadata
        .uid
        .as_deref()
        .filter(|uid| bounded_text(uid))
        .ok_or_else(|| runtime_error("API-observed package ConfigMap has no bounded UID"))?;
    bind_package_job(job, fingerprint, destination, uid, kind)
}

pub(super) fn projection_digest(metadata: &ObjectMeta) -> Result<&str, RuntimeError> {
    metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(PACKAGE_PROJECTION_DIGEST_ANNOTATION))
        .map(String::as_str)
        .filter(|digest| valid_fingerprint(digest))
        .ok_or_else(|| runtime_error("package object has no canonical projection digest"))
}

pub(super) fn realization_digest(metadata: &ObjectMeta) -> Result<&str, RuntimeError> {
    metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(REALIZATION_DIGEST_ANNOTATION))
        .map(String::as_str)
        .filter(|digest| valid_fingerprint(digest))
        .ok_or_else(|| runtime_error("package object has no canonical realization digest"))
}

pub(crate) fn verify_exact_package_config_map(
    desired: &ConfigMap,
    observed: &ConfigMap,
) -> Result<(), RuntimeError> {
    let expected = digest_json(&config_map_projection(desired))?;
    let actual = digest_json(&config_map_projection(observed))?;
    if expected != actual
        || projection_digest(&desired.metadata)? != expected
        || projection_digest(&observed.metadata)? != actual
        || realization_digest(&desired.metadata)? != realization_digest(&observed.metadata)?
        || !has_no_owner_references(&desired.metadata)
        || !has_no_owner_references(&observed.metadata)
        || observed.metadata.deletion_timestamp.is_some()
    {
        return Err(runtime_error(
            "existing package ConfigMap has a different immutable projection",
        ));
    }
    Ok(())
}

pub(crate) fn verify_exact_package_job(desired: &Job, observed: &Job) -> Result<(), RuntimeError> {
    let expected = digest_json(&job_projection(desired)?)?;
    let actual = digest_json(&job_projection(observed)?)?;
    if expected != actual
        || projection_digest(&desired.metadata)? != expected
        || projection_digest(&observed.metadata)? != actual
        || realization_digest(&desired.metadata)? != realization_digest(&observed.metadata)?
        || !has_no_owner_references(&desired.metadata)
        || !has_no_owner_references(&observed.metadata)
        || observed.metadata.deletion_timestamp.is_some()
    {
        return Err(runtime_error(
            "existing package Job has a different immutable projection",
        ));
    }
    Ok(())
}

pub fn stamp_sandbox_release_annotations(pod: &mut Pod, scope: &str, resolved_image: &str) -> bool {
    if scope
        .len()
        .checked_add(resolved_image.len())
        .is_none_or(|size| size > RELEASE_ANNOTATION_VALUE_BUDGET)
    {
        return false;
    }
    let annotations = pod
        .metadata
        .annotations
        .get_or_insert_with(Default::default);
    annotations.insert(
        PACKAGE_REALIZATION_CONTRACT_ANNOTATION.into(),
        K8S_PACKAGE_REALIZATION_CONTRACT_VERSION.into(),
    );
    annotations.insert(SANDBOX_SCOPE_ANNOTATION.into(), scope.into());
    annotations.insert(RESOLVED_IMAGE_ANNOTATION.into(), resolved_image.into());
    true
}
