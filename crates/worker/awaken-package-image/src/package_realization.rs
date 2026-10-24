// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: The one read-only structural verifier binds the exact immutable package ConfigMap, Job,
// @formal reason: Pod, image digest, and Sandbox evidence without owning durable build state or secret
// @formal reason: material; this is product-boundary classification, not a direct proof claim.
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

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, PodSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use serde::{Deserialize, Serialize};

use awaken_container_contract::RuntimeError;
use awaken_worker_fleet_kubernetes::api_defaults::normalize_pod_spec;
use awaken_worker_fleet_kubernetes::managed_names::{k8s_runtime_id, pod_name};

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
    normalize_job_pod_labels(&mut labels, false);
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
    normalize_job_pod_labels(&mut actual_labels, true);
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

/// Count only Pods that belong to this exact Job incarnation. A same-name Pod
/// from a deleted/recreated Job is not evidence for the currently observed
/// Job, and an empty result is an incomplete Kubernetes observation rather
/// than malformed realization evidence.
pub(crate) fn exact_package_job_pods(
    job_object: &Job,
    pods: &[Pod],
    namespace: &str,
) -> Result<Vec<Pod>, K8sPackageRealizationError> {
    let job = job_fact(job_object, namespace)?;
    Ok(pods
        .iter()
        .filter(|pod| {
            object_namespace(&pod.metadata, namespace)
                && pod.metadata.deletion_timestamp.is_none()
                && pod_claims_job(pod, &job)
                && pod
                    .metadata
                    .owner_references
                    .as_deref()
                    .is_some_and(|owners| {
                        owners.len() == 1 && exact_controller_owner(&owners[0], &job)
                    })
        })
        .cloned()
        .collect())
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
    let runtime_id = k8s_runtime_id(session_id)
        .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?;
    let expected_name = pod_name(&runtime_id);
    if !object_namespace(&pod.metadata, namespace)
        || pod.metadata.deletion_timestamp.is_some()
        || !has_no_owner_references(&pod.metadata)
        || pod.metadata.name.as_deref() != Some(expected_name.as_str())
        || pod
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(awaken_container_contract::MANAGED_SANDBOX_LABEL))
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

    let runtime_id = k8s_runtime_id(evidence.session_id)
        .map_err(|error| K8sPackageRealizationError::invalid(error.to_string()))?;
    let expected_sandbox_name = pod_name(&runtime_id);
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

mod facts;
mod identity;
mod projection;

use facts::*;
pub use identity::immutable_image;
use identity::{
    annotation, bounded_text, destination_repository, has_no_owner_references, image_id,
    metadata_uid, object_namespace, runtime_error, valid_fingerprint,
};
pub use projection::stamp_sandbox_release_annotations;
use projection::*;
pub(crate) use projection::{
    bind_package_job_to_config, package_build_name, package_config_annotations,
    package_image_check_name, stamp_package_config_map, verify_exact_package_config_map,
    verify_exact_package_job,
};

#[cfg(test)]
use projection::bind_package_job;

#[cfg(test)]
mod tests;
