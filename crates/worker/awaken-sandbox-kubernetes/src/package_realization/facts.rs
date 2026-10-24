// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: Pure exact package-image evidence code moved by responsibility;
// @formal reason: no wire, snapshot, hashing or authorization rules change.
//! Read-only bounded package facts.

use super::identity::{
    annotation, destination_repository, has_no_owner_references, metadata_uid, object_namespace,
    valid_fingerprint,
};
use super::projection::{
    config_map_projection, digest_json, job_projection, package_build_name,
    package_image_check_name, projection_digest,
};
use super::{
    K8S_PACKAGE_REALIZATION_CONTRACT_VERSION, K8sPackageRealizationError,
    MAX_EVIDENCE_OBJECT_BYTES, MAX_EVIDENCE_OBJECTS_PER_KIND, PACKAGE_BUILD_JOB_KIND,
    PACKAGE_CONFIG_MAP_UID_ANNOTATION, PACKAGE_IMAGE_CHECK_JOB_KIND,
    PACKAGE_IMAGE_DESTINATION_ANNOTATION, PACKAGE_JOB_KIND_ANNOTATION,
    PACKAGE_REALIZATION_CONTRACT_ANNOTATION, PACKAGE_RECIPE_FINGERPRINT_ANNOTATION,
    REALIZATION_DIGEST_ANNOTATION,
};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde::Serialize;
use std::io;
use std::io::Write;

pub(super) struct BoundedWriter {
    written: usize,
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .written
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("serialized Kubernetes object size overflow"))?;
        if next > MAX_EVIDENCE_OBJECT_BYTES {
            return Err(io::Error::other(
                "serialized Kubernetes object exceeds proof bound",
            ));
        }
        self.written = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn require_bounded_objects<T: Serialize>(
    kind: &str,
    values: &[T],
) -> Result<(), K8sPackageRealizationError> {
    if values.len() > MAX_EVIDENCE_OBJECTS_PER_KIND {
        return Err(K8sPackageRealizationError::invalid(format!(
            "{kind} evidence exceeds the object-count bound"
        )));
    }
    for value in values {
        let mut writer = BoundedWriter { written: 0 };
        serde_json::to_writer(&mut writer, value).map_err(|_| {
            K8sPackageRealizationError::invalid(format!(
                "{kind} evidence exceeds the serialized-object bound"
            ))
        })?;
    }
    Ok(())
}

pub(super) fn require_contract(metadata: &ObjectMeta) -> Result<(), K8sPackageRealizationError> {
    if annotation(metadata, PACKAGE_REALIZATION_CONTRACT_ANNOTATION)?
        != K8S_PACKAGE_REALIZATION_CONTRACT_VERSION
    {
        return Err(K8sPackageRealizationError::invalid(
            "object names another package-realization contract version",
        ));
    }
    Ok(())
}

pub(super) fn self_verify_config_projection(
    config: &ConfigMap,
) -> Result<(), K8sPackageRealizationError> {
    let actual = digest_json(&config_map_projection(config))
        .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?;
    if projection_digest(&config.metadata)
        .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?
        != actual
    {
        return Err(K8sPackageRealizationError::invalid(
            "ConfigMap projection digest does not match its exact data",
        ));
    }
    Ok(())
}

pub(super) fn self_verify_job_projection(job: &Job) -> Result<(), K8sPackageRealizationError> {
    let actual = digest_json(
        &job_projection(job)
            .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?,
    )
    .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?;
    if projection_digest(&job.metadata)
        .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?
        != actual
    {
        return Err(K8sPackageRealizationError::invalid(
            "Job projection digest does not match its exact immutable spec",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ConfigFact {
    pub(super) fingerprint: String,
    pub(super) destination: String,
    pub(super) name: String,
    pub(super) uid: String,
}

pub(super) fn config_fact(
    config: &ConfigMap,
    namespace: &str,
) -> Result<ConfigFact, K8sPackageRealizationError> {
    require_contract(&config.metadata)?;
    if !object_namespace(&config.metadata, namespace)
        || config.metadata.deletion_timestamp.is_some()
        || !has_no_owner_references(&config.metadata)
        || config.immutable != Some(true)
        || config
            .binary_data
            .as_ref()
            .is_some_and(|data| !data.is_empty())
    {
        return Err(K8sPackageRealizationError::invalid(
            "package ConfigMap is mutable, terminating, foreign, or carries binary data",
        ));
    }
    let fingerprint = annotation(&config.metadata, PACKAGE_RECIPE_FINGERPRINT_ANNOTATION)?;
    if !valid_fingerprint(fingerprint) {
        return Err(K8sPackageRealizationError::invalid(
            "package ConfigMap fingerprint is not canonical lowercase hex",
        ));
    }
    let destination = annotation(&config.metadata, PACKAGE_IMAGE_DESTINATION_ANNOTATION)?;
    destination_repository(destination, fingerprint)?;
    let name = config
        .metadata
        .name
        .as_deref()
        .ok_or_else(|| K8sPackageRealizationError::invalid("package ConfigMap has no name"))?;
    if name
        != package_build_name(fingerprint)
            .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?
    {
        return Err(K8sPackageRealizationError::invalid(
            "package ConfigMap name is not the canonical recipe name",
        ));
    }
    let data = config.data.as_ref().ok_or_else(|| {
        K8sPackageRealizationError::invalid("package ConfigMap has no Dockerfile data")
    })?;
    if !data.contains_key("Dockerfile")
        || data
            .keys()
            .any(|key| !matches!(key.as_str(), "Dockerfile" | "buildkitd.toml"))
    {
        return Err(K8sPackageRealizationError::invalid(
            "package ConfigMap does not contain the canonical bounded input set",
        ));
    }
    self_verify_config_projection(config)?;
    if !valid_fingerprint(annotation(&config.metadata, REALIZATION_DIGEST_ANNOTATION)?) {
        return Err(K8sPackageRealizationError::invalid(
            "package ConfigMap has no canonical realization digest",
        ));
    }
    Ok(ConfigFact {
        fingerprint: fingerprint.into(),
        destination: destination.into(),
        name: name.into(),
        uid: metadata_uid(&config.metadata, "package ConfigMap")?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct JobFact {
    pub(super) fingerprint: String,
    pub(super) destination: String,
    pub(super) config_uid: String,
    pub(super) name: String,
    pub(super) uid: String,
    pub(super) kind: String,
}

pub(super) fn job_fact(job: &Job, namespace: &str) -> Result<JobFact, K8sPackageRealizationError> {
    require_contract(&job.metadata)?;
    if !object_namespace(&job.metadata, namespace)
        || job.metadata.deletion_timestamp.is_some()
        || !has_no_owner_references(&job.metadata)
    {
        return Err(K8sPackageRealizationError::invalid(
            "package Job is foreign or terminating",
        ));
    }
    let fingerprint = annotation(&job.metadata, PACKAGE_RECIPE_FINGERPRINT_ANNOTATION)?;
    if !valid_fingerprint(fingerprint) {
        return Err(K8sPackageRealizationError::invalid(
            "package Job fingerprint is not canonical lowercase hex",
        ));
    }
    let destination = annotation(&job.metadata, PACKAGE_IMAGE_DESTINATION_ANNOTATION)?;
    destination_repository(destination, fingerprint)?;
    let config_uid = annotation(&job.metadata, PACKAGE_CONFIG_MAP_UID_ANNOTATION)?;
    let kind = annotation(&job.metadata, PACKAGE_JOB_KIND_ANNOTATION)?;
    let name = job
        .metadata
        .name
        .as_deref()
        .ok_or_else(|| K8sPackageRealizationError::invalid("package Job has no name"))?;
    let expected_name = match kind {
        PACKAGE_BUILD_JOB_KIND => package_build_name(fingerprint),
        PACKAGE_IMAGE_CHECK_JOB_KIND => package_image_check_name(destination),
        _ => {
            return Err(K8sPackageRealizationError::invalid(
                "package Job kind is unknown",
            ));
        }
    }
    .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?;
    if name != expected_name {
        return Err(K8sPackageRealizationError::invalid(
            "package Job name is not canonical for its exact input",
        ));
    }
    let spec = job
        .spec
        .as_ref()
        .ok_or_else(|| K8sPackageRealizationError::invalid("package Job has no immutable spec"))?;
    if spec.backoff_limit != Some(0)
        || spec.active_deadline_seconds.is_none_or(|value| value <= 0)
        || spec
            .ttl_seconds_after_finished
            .is_none_or(|value| value <= 0)
    {
        return Err(K8sPackageRealizationError::invalid(
            "package Job is not no-retry, deadline-bounded, and TTL-bounded",
        ));
    }
    let template = &spec.template;
    let template_metadata = template.metadata.as_ref().ok_or_else(|| {
        K8sPackageRealizationError::invalid("package Job has no Pod-template metadata")
    })?;
    for key in [
        PACKAGE_REALIZATION_CONTRACT_ANNOTATION,
        PACKAGE_RECIPE_FINGERPRINT_ANNOTATION,
        PACKAGE_IMAGE_DESTINATION_ANNOTATION,
        PACKAGE_CONFIG_MAP_UID_ANNOTATION,
        PACKAGE_JOB_KIND_ANNOTATION,
    ] {
        if annotation(template_metadata, key)? != annotation(&job.metadata, key)? {
            return Err(K8sPackageRealizationError::invalid(
                "package Job and Pod template carry conflicting correlation",
            ));
        }
    }
    let pod = template
        .spec
        .as_ref()
        .ok_or_else(|| K8sPackageRealizationError::invalid("package Job has no Pod template"))?;
    if pod.automount_service_account_token != Some(false)
        || pod.restart_policy.as_deref() != Some("Never")
        || awaken_worker_fleet_kubernetes::has_forbidden_sandbox_namespace_shape(pod)
        || pod.containers.len() != 1
        || pod
            .init_containers
            .as_ref()
            .is_some_and(|values| !values.is_empty())
    {
        return Err(K8sPackageRealizationError::invalid(
            "package Job Pod template violates the canonical tokenless one-container shape",
        ));
    }
    let container = &pod.containers[0];
    match kind {
        PACKAGE_BUILD_JOB_KIND if container.name == "buildkit" => {
            let config_name = package_build_name(fingerprint)
                .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?;
            let config_volumes = pod
                .volumes
                .as_ref()
                .into_iter()
                .flatten()
                .filter(|volume| {
                    volume.name == "build-input"
                        && volume
                            .config_map
                            .as_ref()
                            .is_some_and(|config| config.name == config_name)
                })
                .count();
            if config_volumes != 1 {
                return Err(K8sPackageRealizationError::invalid(
                    "Build Job does not mount its one canonical recipe ConfigMap",
                ));
            }
        }
        PACKAGE_IMAGE_CHECK_JOB_KIND
            if container.name == "verify"
                && container.image.as_deref() == Some(destination)
                && container.image_pull_policy.as_deref() == Some("Always") => {}
        _ => {
            return Err(K8sPackageRealizationError::invalid(
                "package Job container does not match its declared kind",
            ));
        }
    }
    self_verify_job_projection(job)?;
    if !valid_fingerprint(annotation(&job.metadata, REALIZATION_DIGEST_ANNOTATION)?) {
        return Err(K8sPackageRealizationError::invalid(
            "package Job has no canonical realization digest",
        ));
    }
    Ok(JobFact {
        fingerprint: fingerprint.into(),
        destination: destination.into(),
        config_uid: config_uid.into(),
        name: name.into(),
        uid: metadata_uid(&job.metadata, "package Job")?,
        kind: kind.into(),
    })
}
