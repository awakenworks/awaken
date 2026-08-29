//! Canonical Kubernetes package-image realization evidence.
//!
//! The Kubernetes package builder and every external observer share this one
//! owner for contract keys, deterministic names, immutable spec projections,
//! and proof cardinality. The contract is deliberately read-only and
//! secret-free: it returns identities and UIDs, never recipes, proxy values,
//! Registry credentials, or Kubernetes Secret names. It is structural evidence
//! only. A composing deployment must separately prove that only the pinned Open
//! Worker identity and the Kubernetes Job controller can write these objects.

use std::collections::BTreeMap;
use std::io::{self, Write};

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{ConfigMap, Container, Pod, PodSpec, Toleration, Volume};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use serde::{Deserialize, Serialize};

use crate::{RuntimeError, runtime::RuntimeError::Backend};

pub const K8S_PACKAGE_REALIZATION_CONTRACT_VERSION: &str = "v1";
pub const PACKAGE_REALIZATION_CONTRACT_ANNOTATION: &str = "awaken.dev/package-realization-contract";
pub const PACKAGE_RECIPE_FINGERPRINT_ANNOTATION: &str = "awaken.dev/package-recipe-fingerprint";
pub const PACKAGE_IMAGE_DESTINATION_ANNOTATION: &str = "awaken.dev/package-image-destination";
pub const PACKAGE_CONFIG_MAP_UID_ANNOTATION: &str = "awaken.dev/package-configmap-uid";
pub const PACKAGE_JOB_KIND_ANNOTATION: &str = "awaken.dev/package-job-kind";
pub const PACKAGE_PROJECTION_DIGEST_ANNOTATION: &str = "awaken.dev/package-projection-digest";
pub const SANDBOX_SCOPE_ANNOTATION: &str = "awaken.dev/sandbox-scope";
pub const RESOLVED_IMAGE_ANNOTATION: &str = "awaken.dev/resolved-image";

pub(crate) const PACKAGE_BUILD_JOB_KIND: &str = "build";
pub(crate) const PACKAGE_IMAGE_CHECK_JOB_KIND: &str = "image-check";
const REALIZATION_DIGEST_ANNOTATION: &str = "awaken.dev/realization-digest";
const PACKAGE_BUILD_NAME_PREFIX: &str = "awaken-package-";
const PACKAGE_IMAGE_CHECK_NAME_PREFIX: &str = "awaken-image-check-";
const RELEASE_ANNOTATION_VALUE_BUDGET: usize = 4 * 1024;
const MAX_EVIDENCE_OBJECTS_PER_KIND: usize = 256;
const MAX_EVIDENCE_OBJECT_BYTES: usize = 1024 * 1024;
const MAX_EVIDENCE_TEXT_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct K8sPackageRealizationProof {
    pub contract_version: String,
    pub recipe_fingerprint: String,
    pub destination: String,
    pub image: String,
    pub config_map_uid: String,
    pub build_job_uid: Option<String>,
    pub build_pod_uid: Option<String>,
    pub image_check_job_uid: String,
    pub image_check_pod_uid: String,
    pub sandbox_pod_uid: String,
}

fn job_succeeded(job: &Job) -> bool {
    job.status.as_ref().is_some_and(|status| {
        status.succeeded == Some(1)
            && status.failed.unwrap_or_default() == 0
            && status.active.unwrap_or_default() == 0
    })
}

fn exact_controller_owner(owner: &OwnerReference, job: &JobFact) -> bool {
    owner.api_version == "batch/v1"
        && owner.kind == "Job"
        && owner.name == job.name
        && owner.uid == job.uid
        && owner.controller == Some(true)
        && owner.block_owner_deletion == Some(true)
}

fn has_no_owner_references(metadata: &ObjectMeta) -> bool {
    metadata.owner_references.as_ref().is_none_or(Vec::is_empty)
}

fn pod_claims_job(pod: &Pod, job: &JobFact) -> bool {
    let annotations = pod.metadata.annotations.as_ref();
    let correlated = annotations.is_some_and(|annotations| {
        annotations
            .get(PACKAGE_REALIZATION_CONTRACT_ANNOTATION)
            .is_some_and(|value| value == K8S_PACKAGE_REALIZATION_CONTRACT_VERSION)
            && annotations
                .get(PACKAGE_RECIPE_FINGERPRINT_ANNOTATION)
                .is_some_and(|value| value == &job.fingerprint)
            && annotations
                .get(PACKAGE_IMAGE_DESTINATION_ANNOTATION)
                .is_some_and(|value| value == &job.destination)
            && annotations
                .get(PACKAGE_CONFIG_MAP_UID_ANNOTATION)
                .is_some_and(|value| value == &job.config_uid)
            && annotations
                .get(PACKAGE_JOB_KIND_ANNOTATION)
                .is_some_and(|value| value == &job.kind)
    });
    let named = pod
        .metadata
        .name
        .as_deref()
        .is_some_and(|name| name.starts_with(&format!("{}-", job.name)));
    let labelled = pod.metadata.labels.as_ref().is_some_and(|labels| {
        labels
            .get("job-name")
            .or_else(|| labels.get("batch.kubernetes.io/job-name"))
            .is_some_and(|name| name == &job.name)
    });
    correlated || named || labelled
}

fn normalized_template(
    job: &Job,
) -> Result<(Option<BTreeMap<String, String>>, PodSpec), K8sPackageRealizationError> {
    let mut template = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.clone())
        .ok_or_else(|| K8sPackageRealizationError::invalid("package Job has no Pod template"))?;
    normalize_pod_spec(&mut template, false);
    let mut labels = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.metadata.as_ref())
        .and_then(|metadata| metadata.labels.clone());
    normalize_controller_labels(&mut labels);
    Ok((labels, template))
}

fn verify_job_pod(
    job_object: &Job,
    job: &JobFact,
    pod: &Pod,
    namespace: &str,
) -> Result<String, K8sPackageRealizationError> {
    if !object_namespace(&pod.metadata, namespace) || pod.metadata.deletion_timestamp.is_some() {
        return Err(K8sPackageRealizationError::invalid(
            "package Job Pod is foreign or terminating",
        ));
    }
    let owners = pod.metadata.owner_references.as_deref().unwrap_or_default();
    if owners.len() != 1 || !exact_controller_owner(&owners[0], job) {
        return Err(K8sPackageRealizationError::invalid(
            "package Job Pod does not have exactly one matching Job UID controller owner",
        ));
    }
    for key in [
        PACKAGE_REALIZATION_CONTRACT_ANNOTATION,
        PACKAGE_RECIPE_FINGERPRINT_ANNOTATION,
        PACKAGE_IMAGE_DESTINATION_ANNOTATION,
        PACKAGE_CONFIG_MAP_UID_ANNOTATION,
        PACKAGE_JOB_KIND_ANNOTATION,
    ] {
        if annotation(&pod.metadata, key)? != annotation(&job_object.metadata, key)? {
            return Err(K8sPackageRealizationError::invalid(
                "package Job Pod correlation differs from its exact Job",
            ));
        }
    }
    let (expected_labels, expected_spec) = normalized_template(job_object)?;
    let mut actual_labels = pod.metadata.labels.clone();
    normalize_controller_labels(&mut actual_labels);
    let mut actual_spec = pod
        .spec
        .clone()
        .ok_or_else(|| K8sPackageRealizationError::invalid("package Job Pod has no spec"))?;
    normalize_pod_spec(&mut actual_spec, true);
    if expected_labels != actual_labels || expected_spec != actual_spec {
        return Err(K8sPackageRealizationError::invalid(
            "package Job Pod differs from the exact Job template",
        ));
    }
    metadata_uid(&pod.metadata, "package Job Pod")
}

fn one_claimed_pod<'a>(
    job_object: &Job,
    job: &JobFact,
    pods: &'a [Pod],
    namespace: &str,
) -> Result<(&'a Pod, String), K8sPackageRealizationError> {
    let claimed = pods
        .iter()
        .filter(|pod| pod_claims_job(pod, job))
        .collect::<Vec<_>>();
    if claimed.len() != 1 {
        return Err(K8sPackageRealizationError::invalid(format!(
            "package Job `{}` has {} claiming Pod snapshots instead of exactly one",
            job.name,
            claimed.len()
        )));
    }
    let uid = verify_job_pod(job_object, job, claimed[0], namespace)?;
    Ok((claimed[0], uid))
}

fn successful_container_status<'a>(
    pod: &'a Pod,
    name: &str,
) -> Result<&'a k8s_openapi::api::core::v1::ContainerStatus, K8sPackageRealizationError> {
    let statuses = pod
        .status
        .as_ref()
        .and_then(|status| status.container_statuses.as_ref())
        .ok_or_else(|| K8sPackageRealizationError::invalid("package Job Pod has no status"))?;
    if statuses.len() != 1 {
        return Err(K8sPackageRealizationError::invalid(
            "package Job Pod has ambiguous container status cardinality",
        ));
    }
    let status = &statuses[0];
    let state = status
        .state
        .as_ref()
        .ok_or_else(|| K8sPackageRealizationError::invalid("package Job container has no state"))?;
    if status.name != name
        || state.running.is_some()
        || state.waiting.is_some()
        || state
            .terminated
            .as_ref()
            .is_none_or(|terminated| terminated.exit_code != 0)
    {
        return Err(K8sPackageRealizationError::invalid(
            "package Job container is not one exact successful termination",
        ));
    }
    Ok(status)
}

fn build_result(
    job_object: &Job,
    job: &JobFact,
    pods: &[Pod],
    namespace: &str,
) -> Result<(String, String), K8sPackageRealizationError> {
    if !job_succeeded(job_object) {
        return Err(K8sPackageRealizationError::invalid(
            "Build Job is not exactly successful",
        ));
    }
    let (pod, pod_uid) = one_claimed_pod(job_object, job, pods, namespace)?;
    let status = successful_container_status(pod, "buildkit")?;
    let message = status
        .state
        .as_ref()
        .and_then(|state| state.terminated.as_ref())
        .and_then(|terminated| terminated.message.as_deref())
        .ok_or_else(|| {
            K8sPackageRealizationError::invalid("BuildKit returned no immutable image")
        })?;
    let (repository, _) = immutable_image(message)?;
    if repository != destination_repository(&job.destination, &job.fingerprint)? {
        return Err(K8sPackageRealizationError::invalid(
            "BuildKit result repository differs from the exact destination",
        ));
    }
    Ok((pod_uid, message.into()))
}

fn image_check_result(
    job_object: &Job,
    job: &JobFact,
    pods: &[Pod],
    namespace: &str,
) -> Result<(String, String), K8sPackageRealizationError> {
    if !job_succeeded(job_object) {
        return Err(K8sPackageRealizationError::invalid(
            "image-check Job is not exactly successful",
        ));
    }
    let (pod, pod_uid) = one_claimed_pod(job_object, job, pods, namespace)?;
    let status = successful_container_status(pod, "verify")?;
    if status.image != job.destination {
        return Err(K8sPackageRealizationError::invalid(
            "image-check status names a different mutable destination",
        ));
    }
    let resolved = image_id(&status.image_id)?;
    let (repository, _) = immutable_image(resolved)?;
    if repository != destination_repository(&job.destination, &job.fingerprint)? {
        return Err(K8sPackageRealizationError::invalid(
            "image-check digest repository differs from the exact destination",
        ));
    }
    Ok((pod_uid, resolved.into()))
}

/// Extract a successful BuildKit result with the same UID, ownerRef, Pod-spec,
/// status, and cardinality rules used by the retained verifier. The builder may
/// persist the returned immutable image; this function owns no second receipt.
pub(crate) fn verified_package_build_result(
    job: &Job,
    pods: &[Pod],
    namespace: &str,
) -> Result<String, RuntimeError> {
    let fact = job_fact(job, namespace).map_err(|error| runtime_error(error.to_string()))?;
    if fact.kind != PACKAGE_BUILD_JOB_KIND {
        return Err(runtime_error("package result names a non-build Job"));
    }
    build_result(job, &fact, pods, namespace)
        .map(|(_, image)| image)
        .map_err(|error| runtime_error(error.to_string()))
}

/// Extract a successful package image-check result without introducing another
/// image-id parser or ownerRef policy beside the retained verifier.
pub(crate) fn verified_package_image_check_result(
    job: &Job,
    pods: &[Pod],
    namespace: &str,
) -> Result<String, RuntimeError> {
    let fact = job_fact(job, namespace).map_err(|error| runtime_error(error.to_string()))?;
    if fact.kind != PACKAGE_IMAGE_CHECK_JOB_KIND {
        return Err(runtime_error("package result names a non-image-check Job"));
    }
    image_check_result(job, &fact, pods, namespace)
        .map(|(_, image)| image)
        .map_err(|error| runtime_error(error.to_string()))
}

fn sandbox_result(
    pod: &Pod,
    namespace: &str,
    session_id: &str,
) -> Result<(String, String), K8sPackageRealizationError> {
    require_contract(&pod.metadata)?;
    let runtime_id = crate::k8s::k8s_runtime_id(session_id)
        .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?;
    let expected_name = crate::k8s::pod_name(&runtime_id);
    if !object_namespace(&pod.metadata, namespace)
        || pod.metadata.deletion_timestamp.is_some()
        || !has_no_owner_references(&pod.metadata)
        || pod.metadata.name.as_deref() != Some(expected_name.as_str())
        || pod
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(crate::MANAGED_SANDBOX_LABEL))
            .map(String::as_str)
            != Some("1")
        || annotation(&pod.metadata, SANDBOX_SCOPE_ANNOTATION)? != session_id
        || !valid_fingerprint(annotation(&pod.metadata, REALIZATION_DIGEST_ANNOTATION)?)
    {
        return Err(K8sPackageRealizationError::invalid(
            "mission Sandbox lacks exact live Open realization identity",
        ));
    }
    let resolved = annotation(&pod.metadata, RESOLVED_IMAGE_ANNOTATION)?;
    immutable_image(resolved)?;
    let spec = pod
        .spec
        .as_ref()
        .ok_or_else(|| K8sPackageRealizationError::invalid("mission Sandbox has no Pod spec"))?;
    let agents = spec
        .containers
        .iter()
        .filter(|container| container.name == "agent")
        .collect::<Vec<_>>();
    if agents.len() != 1 || agents[0].image.as_deref() != Some(resolved) {
        return Err(K8sPackageRealizationError::invalid(
            "mission Sandbox does not run the exact resolved image in one agent container",
        ));
    }
    let status = pod
        .status
        .as_ref()
        .ok_or_else(|| K8sPackageRealizationError::invalid("mission Sandbox has no status"))?;
    if status.phase.as_deref() != Some("Running") {
        return Err(K8sPackageRealizationError::invalid(
            "mission Sandbox is not Running",
        ));
    }
    let agent_statuses = status
        .container_statuses
        .as_ref()
        .into_iter()
        .flatten()
        .filter(|status| status.name == "agent")
        .collect::<Vec<_>>();
    if agent_statuses.len() != 1
        || !agent_statuses[0].ready
        || agent_statuses[0]
            .state
            .as_ref()
            .is_none_or(|state| state.running.is_none())
        || image_id(&agent_statuses[0].image_id)? != resolved
    {
        return Err(K8sPackageRealizationError::invalid(
            "mission Sandbox agent is not exactly Ready on the resolved digest",
        ));
    }
    Ok((
        metadata_uid(&pod.metadata, "mission Sandbox")?,
        resolved.into(),
    ))
}

fn annotation_value<'a>(metadata: &'a ObjectMeta, key: &str) -> Option<&'a str> {
    metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(key))
        .map(String::as_str)
}

fn unique_snapshot_uids<T>(
    kind: &str,
    values: &[T],
    metadata: impl Fn(&T) -> &ObjectMeta,
) -> Result<(), K8sPackageRealizationError> {
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        if let Some(uid) = metadata(value).uid.as_deref()
            && !seen.insert(uid)
        {
            return Err(K8sPackageRealizationError::invalid(format!(
                "{kind} evidence contains multiple snapshots for one UID"
            )));
        }
    }
    Ok(())
}

/// Verify one exact ConfigMap -> optional fresh Build Job/Pod -> mandatory
/// image-check Job/Pod -> immutable digest -> one cold mission Sandbox chain.
///
/// Absence of the Build pair is the one recovery case: an existing Registry
/// digest can be re-observed by the canonical image-check path. Every present
/// link has exact-one cardinality. A caller must first collapse watch updates to
/// one final snapshot per immutable UID; duplicates are rejected here.
pub fn verify_k8s_package_realization(
    evidence: &K8sPackageRealizationEvidence<'_>,
) -> Result<K8sPackageRealizationProof, K8sPackageRealizationError> {
    for (label, value) in [
        ("namespace", evidence.namespace),
        ("Session id", evidence.session_id),
        ("package Registry", evidence.package_registry),
    ] {
        if !bounded_text(value) {
            return Err(K8sPackageRealizationError::invalid(format!(
                "{label} is empty, unbounded, or contains controls"
            )));
        }
    }
    require_bounded_objects("ConfigMap", evidence.config_maps)?;
    require_bounded_objects("Job", evidence.jobs)?;
    require_bounded_objects("Pod", evidence.pods)?;
    unique_snapshot_uids("ConfigMap", evidence.config_maps, |value| &value.metadata)?;
    unique_snapshot_uids("Job", evidence.jobs, |value| &value.metadata)?;
    unique_snapshot_uids("Pod", evidence.pods, |value| &value.metadata)?;

    let runtime_id = crate::k8s::k8s_runtime_id(evidence.session_id)
        .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?;
    let expected_sandbox_name = crate::k8s::pod_name(&runtime_id);
    let sandbox_candidates = evidence
        .pods
        .iter()
        .filter(|pod| {
            annotation_value(&pod.metadata, SANDBOX_SCOPE_ANNOTATION) == Some(evidence.session_id)
                || pod.metadata.name.as_deref() == Some(expected_sandbox_name.as_str())
        })
        .map(|pod| {
            sandbox_result(pod, evidence.namespace, evidence.session_id).map(|result| (pod, result))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if sandbox_candidates.len() != 1 {
        return Err(K8sPackageRealizationError::invalid(format!(
            "mission has {} matching Sandbox Pods instead of exactly one",
            sandbox_candidates.len()
        )));
    }
    let (sandbox_pod_uid, sandbox_image) = &sandbox_candidates[0].1;
    let (sandbox_repository, _) = immutable_image(sandbox_image)?;
    let registry_prefix = evidence.package_registry.trim_end_matches('/');
    let expected_repository = format!("{registry_prefix}/awaken-packages");
    if sandbox_repository != expected_repository {
        return Err(K8sPackageRealizationError::invalid(
            "mission Sandbox image is outside the exact package Registry repository",
        ));
    }

    let mut check_candidates = Vec::new();
    for job_object in evidence.jobs.iter().filter(|job| {
        annotation_value(&job.metadata, PACKAGE_REALIZATION_CONTRACT_ANNOTATION)
            == Some(K8S_PACKAGE_REALIZATION_CONTRACT_VERSION)
            && annotation_value(&job.metadata, PACKAGE_JOB_KIND_ANNOTATION)
                == Some(PACKAGE_IMAGE_CHECK_JOB_KIND)
            && job_succeeded(job)
    }) {
        let job = job_fact(job_object, evidence.namespace)?;
        if destination_repository(&job.destination, &job.fingerprint)? != sandbox_repository {
            continue;
        }
        let (pod_uid, image) =
            image_check_result(job_object, &job, evidence.pods, evidence.namespace)?;
        if image == *sandbox_image {
            check_candidates.push((job_object, job, pod_uid, image));
        }
    }
    if check_candidates.len() != 1 {
        return Err(K8sPackageRealizationError::invalid(format!(
            "mission has {} exact successful image-check chains instead of one",
            check_candidates.len()
        )));
    }
    let (check_object, check, check_pod_uid, _) = &check_candidates[0];
    let related_checks = evidence
        .jobs
        .iter()
        .filter(|job| {
            job.metadata.name.as_deref() == Some(check.name.as_str())
                || (annotation_value(&job.metadata, PACKAGE_JOB_KIND_ANNOTATION)
                    == Some(PACKAGE_IMAGE_CHECK_JOB_KIND)
                    && (annotation_value(&job.metadata, PACKAGE_RECIPE_FINGERPRINT_ANNOTATION)
                        == Some(check.fingerprint.as_str())
                        || annotation_value(&job.metadata, PACKAGE_IMAGE_DESTINATION_ANNOTATION)
                            == Some(check.destination.as_str())
                        || annotation_value(&job.metadata, PACKAGE_CONFIG_MAP_UID_ANNOTATION)
                            == Some(check.config_uid.as_str())))
        })
        .collect::<Vec<_>>();
    if related_checks.len() != 1 || related_checks[0].metadata.uid != check_object.metadata.uid {
        return Err(K8sPackageRealizationError::invalid(
            "package proof contains multiple or conflicting image-check Job incarnations",
        ));
    }

    let config_name = package_build_name(&check.fingerprint)
        .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?;

    let related_configs = evidence
        .config_maps
        .iter()
        .filter(|config| {
            config.metadata.name.as_deref() == Some(config_name.as_str())
                || annotation_value(&config.metadata, PACKAGE_RECIPE_FINGERPRINT_ANNOTATION)
                    == Some(check.fingerprint.as_str())
                || annotation_value(&config.metadata, PACKAGE_IMAGE_DESTINATION_ANNOTATION)
                    == Some(check.destination.as_str())
                || config.metadata.uid.as_deref() == Some(check.config_uid.as_str())
        })
        .map(|config| config_fact(config, evidence.namespace))
        .collect::<Result<Vec<_>, _>>()?;
    if related_configs.len() != 1
        || related_configs[0].fingerprint != check.fingerprint
        || related_configs[0].destination != check.destination
        || related_configs[0].uid != check.config_uid
    {
        return Err(K8sPackageRealizationError::invalid(
            "image-check does not bind exactly one canonical recipe ConfigMap UID",
        ));
    }
    let config = &related_configs[0];

    let related_builds = evidence
        .jobs
        .iter()
        .filter(|job| {
            job.metadata.name.as_deref() == Some(config.name.as_str())
                || (annotation_value(&job.metadata, PACKAGE_JOB_KIND_ANNOTATION)
                    == Some(PACKAGE_BUILD_JOB_KIND)
                    && (annotation_value(&job.metadata, PACKAGE_RECIPE_FINGERPRINT_ANNOTATION)
                        == Some(check.fingerprint.as_str())
                        || annotation_value(&job.metadata, PACKAGE_IMAGE_DESTINATION_ANNOTATION)
                            == Some(check.destination.as_str())
                        || annotation_value(&job.metadata, PACKAGE_CONFIG_MAP_UID_ANNOTATION)
                            == Some(check.config_uid.as_str())))
        })
        .collect::<Vec<_>>();
    if related_builds.len() > 1 {
        return Err(K8sPackageRealizationError::invalid(
            "package proof contains multiple Build Job incarnations",
        ));
    }
    let (build_job_uid, build_pod_uid) = if let Some(job_object) = related_builds.first() {
        let build = job_fact(job_object, evidence.namespace)?;
        if build.fingerprint != check.fingerprint
            || build.destination != check.destination
            || build.config_uid != config.uid
        {
            return Err(K8sPackageRealizationError::invalid(
                "Build Job conflicts with the exact ConfigMap/image-check chain",
            ));
        }
        let (pod_uid, image) = build_result(job_object, &build, evidence.pods, evidence.namespace)?;
        if image != *sandbox_image {
            return Err(K8sPackageRealizationError::invalid(
                "BuildKit result differs from image-check and Sandbox digest",
            ));
        }
        (Some(build.uid), Some(pod_uid))
    } else {
        let absent_build = JobFact {
            fingerprint: check.fingerprint.clone(),
            destination: check.destination.clone(),
            config_uid: check.config_uid.clone(),
            name: config.name.clone(),
            uid: String::new(),
            kind: PACKAGE_BUILD_JOB_KIND.into(),
        };
        if evidence
            .pods
            .iter()
            .any(|pod| pod_claims_job(pod, &absent_build))
        {
            return Err(K8sPackageRealizationError::invalid(
                "recovered package proof contains a Build Pod without its exact Job",
            ));
        }
        (None, None)
    };

    Ok(K8sPackageRealizationProof {
        contract_version: K8S_PACKAGE_REALIZATION_CONTRACT_VERSION.into(),
        recipe_fingerprint: check.fingerprint.clone(),
        destination: check.destination.clone(),
        image: sandbox_image.clone(),
        config_map_uid: config.uid.clone(),
        build_job_uid,
        build_pod_uid,
        image_check_job_uid: check.uid.clone(),
        image_check_pod_uid: check_pod_uid.clone(),
        sandbox_pod_uid: sandbox_pod_uid.clone(),
    })
}

fn destination_repository<'a>(
    destination: &'a str,
    fingerprint: &str,
) -> Result<&'a str, K8sPackageRealizationError> {
    let suffix = format!(":{fingerprint}");
    let repository = destination
        .strip_suffix(&suffix)
        .filter(|repository| bounded_text(repository) && !repository.contains('@'))
        .ok_or_else(|| {
            K8sPackageRealizationError::invalid(
                "package destination does not bind the exact recipe fingerprint",
            )
        })?;
    Ok(repository)
}

fn immutable_image(value: &str) -> Result<(&str, &str), K8sPackageRealizationError> {
    if !bounded_text(value) {
        return Err(K8sPackageRealizationError::invalid(
            "immutable image is empty or unbounded",
        ));
    }
    let (repository, digest) = value.rsplit_once("@sha256:").ok_or_else(|| {
        K8sPackageRealizationError::invalid("image is not an immutable sha256 reference")
    })?;
    if repository.is_empty()
        || repository.contains('@')
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(K8sPackageRealizationError::invalid(
            "image is not one canonical immutable sha256 reference",
        ));
    }
    Ok((repository, digest))
}

fn image_id(value: &str) -> Result<&str, K8sPackageRealizationError> {
    let image = value.split_once("://").map_or(value, |(_, image)| image);
    immutable_image(image)?;
    Ok(image)
}

fn annotation<'a>(
    metadata: &'a ObjectMeta,
    key: &str,
) -> Result<&'a str, K8sPackageRealizationError> {
    metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(key))
        .map(String::as_str)
        .filter(|value| bounded_text(value))
        .ok_or_else(|| {
            K8sPackageRealizationError::invalid(format!("object has no bounded `{key}` annotation"))
        })
}

fn metadata_uid(metadata: &ObjectMeta, kind: &str) -> Result<String, K8sPackageRealizationError> {
    metadata
        .uid
        .as_deref()
        .filter(|uid| bounded_text(uid))
        .map(ToOwned::to_owned)
        .ok_or_else(|| K8sPackageRealizationError::invalid(format!("{kind} has no bounded UID")))
}

fn object_namespace(metadata: &ObjectMeta, namespace: &str) -> bool {
    metadata.namespace.as_deref() == Some(namespace)
}

struct BoundedWriter {
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

fn require_bounded_objects<T: Serialize>(
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

fn require_contract(metadata: &ObjectMeta) -> Result<(), K8sPackageRealizationError> {
    if annotation(metadata, PACKAGE_REALIZATION_CONTRACT_ANNOTATION)?
        != K8S_PACKAGE_REALIZATION_CONTRACT_VERSION
    {
        return Err(K8sPackageRealizationError::invalid(
            "object names another package-realization contract version",
        ));
    }
    Ok(())
}

fn self_verify_config_projection(config: &ConfigMap) -> Result<(), K8sPackageRealizationError> {
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

fn self_verify_job_projection(job: &Job) -> Result<(), K8sPackageRealizationError> {
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
struct ConfigFact {
    fingerprint: String,
    destination: String,
    name: String,
    uid: String,
}

fn config_fact(
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
struct JobFact {
    fingerprint: String,
    destination: String,
    config_uid: String,
    name: String,
    uid: String,
    kind: String,
}

fn job_fact(job: &Job, namespace: &str) -> Result<JobFact, K8sPackageRealizationError> {
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
        || crate::k8s::has_forbidden_sandbox_namespace_shape(pod)
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

/// Already-decoded Kubernetes snapshots captured by a bounded caller. The
/// slices must contain the complete ConfigMap/Job/Pod correlation closure for
/// the mission; this pure verifier cannot prove that its caller omitted no
/// conflicting object. It applies its own count, string, and serialized-object
/// ceilings as a second fence, never performs cluster I/O, and owns no retained
/// state.
pub struct K8sPackageRealizationEvidence<'a> {
    pub namespace: &'a str,
    pub session_id: &'a str,
    pub package_registry: &'a str,
    pub config_maps: &'a [ConfigMap],
    pub jobs: &'a [Job],
    pub pods: &'a [Pod],
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid Kubernetes package realization evidence: {0}")]
pub struct K8sPackageRealizationError(String);

impl K8sPackageRealizationError {
    fn invalid(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

fn runtime_error(message: impl Into<String>) -> RuntimeError {
    Backend(message.into())
}

fn valid_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn bounded_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_EVIDENCE_TEXT_BYTES
        && !value.chars().any(char::is_control)
}

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

fn correlation_annotations(
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

fn digest_json<T: Serialize>(value: &T) -> Result<String, RuntimeError> {
    let encoded = serde_json::to_vec(value).map_err(|error| runtime_error(error.to_string()))?;
    Ok(blake3::hash(&encoded).to_hex().to_string())
}

#[derive(Serialize)]
struct ConfigMapProjection<'a> {
    name: Option<&'a str>,
    namespace: Option<&'a str>,
    data: &'a Option<BTreeMap<String, String>>,
    binary_data: &'a Option<BTreeMap<String, k8s_openapi::ByteString>>,
    immutable: Option<bool>,
    correlation: BTreeMap<String, String>,
}

fn owned_annotations(metadata: &ObjectMeta, keys: &[&str]) -> BTreeMap<String, String> {
    metadata
        .annotations
        .as_ref()
        .into_iter()
        .flat_map(|annotations| annotations.iter())
        .filter(|(key, _)| keys.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn config_map_projection(config: &ConfigMap) -> ConfigMapProjection<'_> {
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

fn normalize_volume(volume: &mut Volume) {
    if let Some(config) = volume.config_map.as_mut()
        && config.default_mode == Some(0o644)
    {
        config.default_mode = None;
    }
    if let Some(secret) = volume.secret.as_mut()
        && secret.default_mode == Some(0o644)
    {
        secret.default_mode = None;
    }
}

fn normalize_container(container: &mut Container) {
    if container.termination_message_path.as_deref() == Some("/dev/termination-log") {
        container.termination_message_path = None;
    }
    if container.termination_message_policy.as_deref() == Some("File") {
        container.termination_message_policy = None;
    }
}

fn normalize_pod_spec(spec: &mut PodSpec, observed_pod: bool) {
    for container in &mut spec.containers {
        normalize_container(container);
    }
    if let Some(containers) = spec.init_containers.as_mut() {
        for container in containers {
            normalize_container(container);
        }
    }
    if let Some(volumes) = spec.volumes.as_mut() {
        for volume in volumes {
            normalize_volume(volume);
        }
    }
    if spec.dns_policy.as_deref() == Some("ClusterFirst") {
        spec.dns_policy = None;
    }
    if spec.enable_service_links == Some(true) {
        spec.enable_service_links = None;
    }
    if spec.scheduler_name.as_deref() == Some("default-scheduler") {
        spec.scheduler_name = None;
    }
    if spec.service_account.as_deref() == Some("default") {
        spec.service_account = None;
    }
    if spec.service_account_name.as_deref() == Some("default") {
        spec.service_account_name = None;
    }
    if spec.termination_grace_period_seconds == Some(30) {
        spec.termination_grace_period_seconds = None;
    }
    if spec.preemption_policy.as_deref() == Some("PreemptLowerPriority") {
        spec.preemption_policy = None;
    }
    if observed_pod {
        spec.node_name = None;
        if spec.priority == Some(0) {
            spec.priority = None;
        }
        if spec.host_users == Some(true) {
            spec.host_users = None;
        }
        if spec.set_hostname_as_fqdn == Some(false) {
            spec.set_hostname_as_fqdn = None;
        }
        if spec.share_process_namespace == Some(false) {
            spec.share_process_namespace = None;
        }
        if spec.host_network == Some(false) {
            spec.host_network = None;
        }
        if spec.host_pid == Some(false) {
            spec.host_pid = None;
        }
        if spec.host_ipc == Some(false) {
            spec.host_ipc = None;
        }
        if spec
            .security_context
            .as_ref()
            .is_some_and(|context| context == &Default::default())
        {
            spec.security_context = None;
        }
        if let Some(tolerations) = spec.tolerations.as_mut() {
            tolerations.retain(|toleration| !default_no_execute_toleration(toleration));
            if tolerations.is_empty() {
                spec.tolerations = None;
            }
        }
    }
}

fn default_no_execute_toleration(toleration: &Toleration) -> bool {
    matches!(
        toleration.key.as_deref(),
        Some("node.kubernetes.io/not-ready" | "node.kubernetes.io/unreachable")
    ) && toleration.operator.as_deref() == Some("Exists")
        && toleration.effect.as_deref() == Some("NoExecute")
        && toleration.toleration_seconds == Some(300)
        && toleration.value.is_none()
}

fn normalize_controller_labels(labels: &mut Option<BTreeMap<String, String>>) {
    if let Some(labels) = labels {
        for key in [
            "batch.kubernetes.io/controller-uid",
            "batch.kubernetes.io/job-name",
            "controller-uid",
            "job-name",
        ] {
            labels.remove(key);
        }
    }
}

fn normalized_job_spec(job: &Job) -> Result<JobSpec, RuntimeError> {
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
    normalize_controller_labels(
        &mut spec
            .template
            .metadata
            .get_or_insert_with(Default::default)
            .labels,
    );
    if let Some(pod) = spec.template.spec.as_mut() {
        normalize_pod_spec(pod, false);
    }
    Ok(spec)
}

#[derive(Serialize)]
struct JobProjection {
    name: Option<String>,
    namespace: Option<String>,
    labels: Option<BTreeMap<String, String>>,
    correlation: BTreeMap<String, String>,
    spec: JobSpec,
}

fn job_projection(job: &Job) -> Result<JobProjection, RuntimeError> {
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

fn projection_digest(metadata: &ObjectMeta) -> Result<&str, RuntimeError> {
    metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(PACKAGE_PROJECTION_DIGEST_ANNOTATION))
        .map(String::as_str)
        .filter(|digest| valid_fingerprint(digest))
        .ok_or_else(|| runtime_error("package object has no canonical projection digest"))
}

fn realization_digest(metadata: &ObjectMeta) -> Result<&str, RuntimeError> {
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

#[cfg(test)]
#[path = "k8s_package_realization/tests.rs"]
mod tests;

pub(crate) fn stamp_sandbox_release_annotations(
    pod: &mut Pod,
    scope: &str,
    resolved_image: &str,
) -> bool {
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
