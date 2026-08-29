use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::{Job, JobStatus};
use k8s_openapi::api::core::v1::{
    ConfigMap, Container, ContainerState, ContainerStateRunning, ContainerStateTerminated,
    ContainerStatus, Pod, PodSecurityContext, PodStatus, Toleration,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};

use super::*;
use crate::k8s::stamp_realization;
use crate::k8s_package_image::K8sPackageImageProvisioner;

const NAMESPACE: &str = "awaken-system";
const SESSION_ID: &str = "session-proof";
const REGISTRY: &str = "registry.local:5000/environments";

struct Fixture {
    config_maps: Vec<ConfigMap>,
    jobs: Vec<Job>,
    pods: Vec<Pod>,
    image: String,
}

impl Fixture {
    fn verify(&self) -> Result<K8sPackageRealizationProof, K8sPackageRealizationError> {
        verify_k8s_package_realization(&K8sPackageRealizationEvidence {
            namespace: NAMESPACE,
            session_id: SESSION_ID,
            package_registry: REGISTRY,
            config_maps: &self.config_maps,
            jobs: &self.jobs,
            pods: &self.pods,
        })
    }

    fn job_index(&self, kind: &str) -> usize {
        self.jobs
            .iter()
            .position(|job| {
                annotation_value(&job.metadata, PACKAGE_JOB_KIND_ANNOTATION) == Some(kind)
            })
            .expect("fixture Job kind")
    }

    fn job_pod_index(&self, kind: &str) -> usize {
        self.pods
            .iter()
            .position(|pod| {
                annotation_value(&pod.metadata, PACKAGE_JOB_KIND_ANNOTATION) == Some(kind)
            })
            .expect("fixture Job Pod kind")
    }

    fn sandbox_index(&self) -> usize {
        self.pods
            .iter()
            .position(|pod| {
                annotation_value(&pod.metadata, SANDBOX_SCOPE_ANNOTATION) == Some(SESSION_ID)
            })
            .expect("fixture Sandbox")
    }
}

fn test_builder() -> K8sPackageImageProvisioner {
    test_builder_with_pull_secrets(vec!["registry-auth".into()])
}

fn test_builder_with_pull_secrets(image_pull_secrets: Vec<String>) -> K8sPackageImageProvisioner {
    crate::k8s::install_rustls_crypto_provider();
    let config = kube::Config::new("http://127.0.0.1:1/".parse().unwrap());
    let client = kube::Client::try_from(config).unwrap();
    K8sPackageImageProvisioner::new(client, NAMESPACE, REGISTRY, image_pull_secrets, true)
        .unwrap()
        .with_forward_proxy(crate::ForwardProxy {
            url: "http://proxy.internal:8080".into(),
        })
        .unwrap()
}

fn package_requirements() -> awaken_provisioning_contract::PackageRequirements {
    awaken_provisioning_contract::PackageRequirements {
        managers: [("npm".into(), vec!["@playwright/mcp@latest".into()])]
            .into_iter()
            .collect(),
        resolution_id: Some("env-browser:3".into()),
    }
}

fn succeed_job(job: &mut Job, uid: &str) {
    job.metadata.uid = Some(uid.into());
    job.status = Some(JobStatus {
        active: Some(0),
        failed: Some(0),
        succeeded: Some(1),
        ..Default::default()
    });
}

fn default_no_execute_toleration(key: &str) -> Toleration {
    Toleration {
        effect: Some("NoExecute".into()),
        key: Some(key.into()),
        operator: Some("Exists".into()),
        toleration_seconds: Some(300),
        value: None,
    }
}

fn apply_api_job_template_defaults(pod: &mut k8s_openapi::api::core::v1::PodSpec) {
    for container in &mut pod.containers {
        container.resources = Some(Default::default());
        container.termination_message_path = Some("/dev/termination-log".into());
        container.termination_message_policy = Some("File".into());
    }
    if let Some(volumes) = pod.volumes.as_mut() {
        for volume in volumes {
            if let Some(config) = volume.config_map.as_mut() {
                config.default_mode = Some(0o644);
            }
            if let Some(secret) = volume.secret.as_mut() {
                secret.default_mode = Some(0o644);
            }
        }
    }
    pod.dns_policy = Some("ClusterFirst".into());
    pod.enable_service_links = Some(true);
    pod.scheduler_name = Some("default-scheduler".into());
    pod.service_account = Some("default".into());
    pod.service_account_name = Some("default".into());
    pod.termination_grace_period_seconds = Some(30);
    pod.preemption_policy = Some("PreemptLowerPriority".into());
    pod.security_context = Some(PodSecurityContext::default());
}

fn apply_api_pod_defaults(pod: &mut k8s_openapi::api::core::v1::PodSpec) {
    apply_api_job_template_defaults(pod);
    pod.priority = Some(0);
    pod.host_users = Some(true);
    pod.set_hostname_as_fqdn = Some(false);
    pod.share_process_namespace = Some(false);
    pod.host_network = Some(false);
    pod.host_pid = Some(false);
    pod.host_ipc = Some(false);
    pod.node_name = Some("worker-a".into());
    pod.tolerations = Some(vec![
        default_no_execute_toleration("node.kubernetes.io/not-ready"),
        default_no_execute_toleration("node.kubernetes.io/unreachable"),
    ]);
}

fn successful_job_pod(
    job: &Job,
    uid: &str,
    image_id: &str,
    termination_message: Option<String>,
) -> Pod {
    let job_name = job.metadata.name.as_deref().expect("Job name");
    let job_uid = job.metadata.uid.as_deref().expect("Job UID");
    let template = job.spec.as_ref().expect("Job spec").template.clone();
    let mut metadata = template.metadata.unwrap_or_default();
    metadata.name = Some(format!("{job_name}-proof"));
    metadata.namespace = Some(NAMESPACE.into());
    metadata.uid = Some(uid.into());
    metadata.owner_references = Some(vec![OwnerReference {
        api_version: "batch/v1".into(),
        block_owner_deletion: Some(true),
        controller: Some(true),
        kind: "Job".into(),
        name: job_name.into(),
        uid: job_uid.into(),
    }]);
    let labels = metadata.labels.get_or_insert_with(Default::default);
    for key in ["job-name", "batch.kubernetes.io/job-name"] {
        labels.insert(key.into(), job_name.into());
    }
    for key in ["controller-uid", "batch.kubernetes.io/controller-uid"] {
        labels.insert(key.into(), job_uid.into());
    }
    let mut spec = template.spec.expect("Job Pod spec");
    apply_api_pod_defaults(&mut spec);
    let container = spec.containers.first().expect("one Job container");
    let status = ContainerStatus {
        image: container.image.clone().expect("Job container image"),
        image_id: image_id.into(),
        name: container.name.clone(),
        ready: false,
        restart_count: 0,
        state: Some(ContainerState {
            terminated: Some(ContainerStateTerminated {
                exit_code: 0,
                message: termination_message,
                reason: Some("Completed".into()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    Pod {
        metadata,
        spec: Some(spec),
        status: Some(PodStatus {
            container_statuses: Some(vec![status]),
            phase: Some("Succeeded".into()),
            ..Default::default()
        }),
    }
}

fn running_sandbox(image: &str) -> Pod {
    let mut pod = Pod {
        metadata: ObjectMeta {
            name: Some(crate::k8s::pod_name(
                &crate::k8s::k8s_runtime_id(SESSION_ID).unwrap(),
            )),
            namespace: Some(NAMESPACE.into()),
            labels: Some(BTreeMap::from([(
                crate::MANAGED_SANDBOX_LABEL.into(),
                "1".into(),
            )])),
            ..Default::default()
        },
        spec: Some(k8s_openapi::api::core::v1::PodSpec {
            automount_service_account_token: Some(false),
            containers: vec![Container {
                name: "agent".into(),
                image: Some(image.into()),
                ..Default::default()
            }],
            restart_policy: Some("Never".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(stamp_sandbox_release_annotations(
        &mut pod, SESSION_ID, image
    ));
    stamp_realization(&mut pod).unwrap();
    pod.metadata.uid = Some("sandbox-pod-uid".into());
    pod.status = Some(PodStatus {
        container_statuses: Some(vec![ContainerStatus {
            image: image.into(),
            image_id: format!("containerd://{image}"),
            name: "agent".into(),
            ready: true,
            restart_count: 0,
            state: Some(ContainerState {
                running: Some(ContainerStateRunning::default()),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        phase: Some("Running".into()),
        ..Default::default()
    });
    pod
}

fn canonical_fixture() -> Fixture {
    let builder = test_builder();
    let (mut config, mut build_job, destination) = builder
        .build_objects("registry.local/base@sha256:exact", &package_requirements())
        .unwrap();
    config.metadata.uid = Some("config-map-uid".into());
    bind_package_job_to_config(&mut build_job, &config, PACKAGE_BUILD_JOB_KIND).unwrap();
    stamp_realization(&mut build_job).unwrap();
    succeed_job(&mut build_job, "build-job-uid");

    let (_, mut check_job) = builder
        .image_check_job(&destination, Some(&config))
        .unwrap();
    succeed_job(&mut check_job, "check-job-uid");

    let repository = destination.rsplit_once(':').expect("tagged destination").0;
    let image = format!("{repository}@sha256:{}", "a".repeat(64));
    let build_pod = successful_job_pod(
        &build_job,
        "build-pod-uid",
        "containerd://moby/buildkit@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        Some(image.clone()),
    );
    let check_pod = successful_job_pod(
        &check_job,
        "check-pod-uid",
        &format!("containerd://{image}"),
        None,
    );
    let sandbox = running_sandbox(&image);
    Fixture {
        config_maps: vec![config],
        jobs: vec![build_job, check_job],
        pods: vec![build_pod, check_pod, sandbox],
        image,
    }
}

fn assert_rejected(fixture: Fixture, rule: &str) {
    assert!(fixture.verify().is_err(), "{rule} must fail closed");
}

#[tokio::test]
async fn fresh_and_recovered_realizations_return_one_secret_free_typed_proof() {
    /* Cause/effect graph and decision table:
     * C1 exact immutable ConfigMap UID; C2 exact successful Build Job/Pod pair;
     * C3 exact successful image-check Job/Pod pair; C4 one Running+Ready cold
     * Sandbox on the same digest; C5 Registry recovery has C1+C3+C4 but no C2.
     * Effects: E1 return every observed UID and the immutable image; E2 omit
     * both Build UIDs only for recovery; E3 serialize no recipe, proxy, Secret,
     * base image, or workload status. Rules R1=C1+C2+C3+C4=>E1+E3 and
     * R2=C1+C3+C4+C5=>E1+E2+E3. The proof is a projection, never a new receipt.
     */
    let fixture = canonical_fixture();
    let proof = fixture.verify().expect("R1 fresh realization");
    assert_eq!(proof.config_map_uid, "config-map-uid", "R1/C1");
    assert_eq!(
        proof.build_job_uid.as_deref(),
        Some("build-job-uid"),
        "R1/C2"
    );
    assert_eq!(
        proof.build_pod_uid.as_deref(),
        Some("build-pod-uid"),
        "R1/C2"
    );
    assert_eq!(proof.image_check_job_uid, "check-job-uid", "R1/C3");
    assert_eq!(proof.image_check_pod_uid, "check-pod-uid", "R1/C3");
    assert_eq!(proof.sandbox_pod_uid, "sandbox-pod-uid", "R1/C4");
    assert_eq!(proof.image, fixture.image, "R1 same immutable digest");
    let encoded = serde_json::to_string(&proof).unwrap();
    for secret in [
        "Dockerfile",
        "@playwright/mcp",
        "proxy.internal",
        "registry-auth",
        "registry.local/base",
    ] {
        assert!(!encoded.contains(secret), "R1/E3 `{secret}`");
    }

    let mut recovered = canonical_fixture();
    recovered.jobs.retain(|job| {
        annotation_value(&job.metadata, PACKAGE_JOB_KIND_ANNOTATION) != Some(PACKAGE_BUILD_JOB_KIND)
    });
    recovered.pods.retain(|pod| {
        annotation_value(&pod.metadata, PACKAGE_JOB_KIND_ANNOTATION) != Some(PACKAGE_BUILD_JOB_KIND)
    });
    let proof = recovered.verify().expect("R2 recovered realization");
    assert_eq!(proof.build_job_uid, None, "R2/E2");
    assert_eq!(proof.build_pod_uid, None, "R2/E2");
}

#[tokio::test]
async fn exact_reuse_accepts_only_api_defaults_and_rejects_copied_digests() {
    /* 409 decision table:
     * C1 same realization+projection with only UID/resourceVersion/controller
     * defaults, including the version-gated Job pod replacement default,
     * empty resource/security values, and an omitted empty pull-secret list;
     * C2 same name and copied annotations but changed ConfigMap data;
     * C3 copied annotations but changed Job executable spec; C4 changed generic
     * realization digest; C5 an ownerRef changes direct-object lifecycle; C6 a
     * non-default Job replacement policy changes execution semantics.
     * Effects: E1 reuse the exact API object; E2 reject the 409 without
     * deleting/replacing it. R1=C1=>E1; R2=C2|C3|C4|C5|C6=>E2.
     */
    let builder = test_builder_with_pull_secrets(Vec::new());
    let (desired_config, mut desired_job, destination) = builder
        .build_objects("registry.local/base@sha256:exact", &package_requirements())
        .unwrap();
    let mut observed_config = desired_config.clone();
    observed_config.metadata.uid = Some("config-map-uid".into());
    observed_config.metadata.resource_version = Some("7".into());
    verify_exact_package_config_map(&desired_config, &observed_config).expect("R1 ConfigMap");

    let mut wrong_data = observed_config.clone();
    wrong_data
        .data
        .get_or_insert_with(Default::default)
        .insert("Dockerfile".into(), "FROM attacker".into());
    assert!(
        verify_exact_package_config_map(&desired_config, &wrong_data).is_err(),
        "R2/C2"
    );

    let fake_owner = OwnerReference {
        api_version: "v1".into(),
        block_owner_deletion: Some(true),
        controller: Some(false),
        kind: "Secret".into(),
        name: "fake-owner".into(),
        uid: "fake-owner-uid".into(),
    };
    let mut wrong_owner_config = observed_config.clone();
    wrong_owner_config.metadata.owner_references = Some(vec![fake_owner.clone()]);
    assert!(
        verify_exact_package_config_map(&desired_config, &wrong_owner_config).is_err(),
        "R2/C5 ConfigMap"
    );

    bind_package_job_to_config(&mut desired_job, &observed_config, PACKAGE_BUILD_JOB_KIND).unwrap();
    stamp_realization(&mut desired_job).unwrap();
    let mut observed_job = desired_job.clone();
    observed_job.metadata.uid = Some("build-job-uid".into());
    observed_job.metadata.resource_version = Some("9".into());
    let job_spec = observed_job.spec.as_mut().unwrap();
    job_spec.completions = Some(1);
    job_spec.parallelism = Some(1);
    job_spec.completion_mode = Some("NonIndexed".into());
    job_spec.manual_selector = Some(false);
    job_spec.suspend = Some(false);
    job_spec.pod_replacement_policy = Some("TerminatingOrFailed".into());
    job_spec.template.spec.as_mut().unwrap().image_pull_secrets = None;
    let selector_labels = BTreeMap::from([("controller-uid".into(), "build-job-uid".into())]);
    job_spec.selector = Some(LabelSelector {
        match_labels: Some(selector_labels.clone()),
        ..Default::default()
    });
    job_spec
        .template
        .metadata
        .get_or_insert_with(Default::default)
        .labels
        .get_or_insert_with(Default::default)
        .extend(selector_labels);
    apply_api_job_template_defaults(job_spec.template.spec.as_mut().unwrap());
    assert_eq!(
        serde_json::to_value(job_projection(&desired_job).unwrap()).unwrap(),
        serde_json::to_value(job_projection(&observed_job).unwrap()).unwrap(),
        "R1 normalized Job projection"
    );
    verify_exact_package_job(&desired_job, &observed_job).expect("R1 Job");

    let mut wrong_spec = observed_job.clone();
    wrong_spec
        .spec
        .as_mut()
        .unwrap()
        .template
        .spec
        .as_mut()
        .unwrap()
        .containers[0]
        .image = Some(destination);
    assert!(
        verify_exact_package_job(&desired_job, &wrong_spec).is_err(),
        "R2/C3"
    );

    let mut wrong_replacement_policy = observed_job.clone();
    wrong_replacement_policy
        .spec
        .as_mut()
        .unwrap()
        .pod_replacement_policy = Some("Failed".into());
    assert!(
        verify_exact_package_job(&desired_job, &wrong_replacement_policy).is_err(),
        "R2/C6"
    );

    let mut wrong_owner_job = observed_job.clone();
    wrong_owner_job.metadata.owner_references = Some(vec![fake_owner]);
    assert!(
        verify_exact_package_job(&desired_job, &wrong_owner_job).is_err(),
        "R2/C5 Job"
    );

    let mut wrong_realization = observed_job;
    wrong_realization
        .metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert(REALIZATION_DIGEST_ANNOTATION.into(), "b".repeat(64));
    assert!(
        verify_exact_package_job(&desired_job, &wrong_realization).is_err(),
        "R2/C4"
    );
}

#[tokio::test]
async fn uid_owner_spec_status_and_digest_tampering_all_fail_closed() {
    /* Chain integrity decision table:
     * C1 Job names a different ConfigMap UID; C2 Job Pod has a wrong/extra
     * controller ownerRef; C3 Pod correlation annotations differ; C4 Pod spec
     * differs from the exact Job template; C5 Job or container status is not
     * exact success; C6 Build/check/Sandbox digests differ. Effect E1 for every
     * rule is no proof. R1=C1=>E1; R2=C2=>E1; R3=C3=>E1; R4=C4=>E1;
     * R5=C5=>E1; R6=C6=>E1. No annotation alone can repair a broken UID edge.
     */
    let mut fixture = canonical_fixture();
    let check = fixture.job_index(PACKAGE_IMAGE_CHECK_JOB_KIND);
    let fingerprint = annotation_value(
        &fixture.jobs[check].metadata,
        PACKAGE_RECIPE_FINGERPRINT_ANNOTATION,
    )
    .unwrap()
    .to_owned();
    let destination = annotation_value(
        &fixture.jobs[check].metadata,
        PACKAGE_IMAGE_DESTINATION_ANNOTATION,
    )
    .unwrap()
    .to_owned();
    bind_package_job(
        &mut fixture.jobs[check],
        &fingerprint,
        &destination,
        "wrong-config-uid",
        PACKAGE_IMAGE_CHECK_JOB_KIND,
    )
    .unwrap();
    stamp_realization(&mut fixture.jobs[check]).unwrap();
    let annotations = fixture.jobs[check]
        .spec
        .as_ref()
        .unwrap()
        .template
        .metadata
        .as_ref()
        .unwrap()
        .annotations
        .clone();
    let check_pod = fixture.job_pod_index(PACKAGE_IMAGE_CHECK_JOB_KIND);
    fixture.pods[check_pod].metadata.annotations = annotations;
    assert_rejected(fixture, "R1 wrong ConfigMap UID");

    let mut fixture = canonical_fixture();
    let check_pod = fixture.job_pod_index(PACKAGE_IMAGE_CHECK_JOB_KIND);
    fixture.pods[check_pod]
        .metadata
        .owner_references
        .as_mut()
        .unwrap()[0]
        .uid = "wrong-job-uid".into();
    assert_rejected(fixture, "R2 wrong owner UID");

    let mut fixture = canonical_fixture();
    let check_pod = fixture.job_pod_index(PACKAGE_IMAGE_CHECK_JOB_KIND);
    fixture.pods[check_pod]
        .metadata
        .owner_references
        .as_mut()
        .unwrap()
        .push(OwnerReference {
            api_version: "v1".into(),
            block_owner_deletion: None,
            controller: Some(false),
            kind: "ConfigMap".into(),
            name: "fake".into(),
            uid: "fake".into(),
        });
    assert_rejected(fixture, "R2 extra ownerRef");

    let mut fixture = canonical_fixture();
    let check_pod = fixture.job_pod_index(PACKAGE_IMAGE_CHECK_JOB_KIND);
    fixture.pods[check_pod]
        .metadata
        .owner_references
        .as_mut()
        .unwrap()[0]
        .block_owner_deletion = Some(false);
    assert_rejected(fixture, "R2 non-controller-lifecycle ownerRef");

    let mut fixture = canonical_fixture();
    let check_pod = fixture.job_pod_index(PACKAGE_IMAGE_CHECK_JOB_KIND);
    fixture.pods[check_pod]
        .metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert(PACKAGE_CONFIG_MAP_UID_ANNOTATION.into(), "fake".into());
    assert_rejected(fixture, "R3 fake Pod annotation");

    let mut fixture = canonical_fixture();
    let sandbox = fixture.sandbox_index();
    fixture.pods[sandbox].metadata.name = Some("fake-sandbox".into());
    assert_rejected(
        fixture,
        "R3 fake Sandbox annotation on a non-canonical name",
    );

    let mut fixture = canonical_fixture();
    let sandbox = fixture.sandbox_index();
    fixture.pods[sandbox].metadata.owner_references = Some(vec![OwnerReference {
        api_version: "v1".into(),
        block_owner_deletion: Some(true),
        controller: Some(false),
        kind: "Secret".into(),
        name: "fake-owner".into(),
        uid: "fake-owner-uid".into(),
    }]);
    assert_rejected(fixture, "R2 fake Sandbox ownerRef");

    let mut fixture = canonical_fixture();
    let check_pod = fixture.job_pod_index(PACKAGE_IMAGE_CHECK_JOB_KIND);
    fixture.pods[check_pod].spec.as_mut().unwrap().containers[0].command =
        Some(vec!["/bin/false".into()]);
    assert_rejected(fixture, "R4 Pod spec drift");

    let mut fixture = canonical_fixture();
    let check = fixture.job_index(PACKAGE_IMAGE_CHECK_JOB_KIND);
    fixture.jobs[check].status.as_mut().unwrap().succeeded = Some(2);
    assert_rejected(fixture, "R5 ambiguous Job success");

    let mut fixture = canonical_fixture();
    let check_pod = fixture.job_pod_index(PACKAGE_IMAGE_CHECK_JOB_KIND);
    let duplicate_status = fixture.pods[check_pod]
        .status
        .as_ref()
        .unwrap()
        .container_statuses
        .as_ref()
        .unwrap()[0]
        .clone();
    fixture.pods[check_pod]
        .status
        .as_mut()
        .unwrap()
        .container_statuses
        .as_mut()
        .unwrap()
        .push(duplicate_status);
    assert_rejected(fixture, "R5 ambiguous container status");

    let mut fixture = canonical_fixture();
    let build_pod = fixture.job_pod_index(PACKAGE_BUILD_JOB_KIND);
    fixture.pods[build_pod]
        .status
        .as_mut()
        .unwrap()
        .container_statuses
        .as_mut()
        .unwrap()[0]
        .state
        .as_mut()
        .unwrap()
        .terminated
        .as_mut()
        .unwrap()
        .message = Some(format!(
        "{REGISTRY}/awaken-packages@sha256:{}",
        "b".repeat(64)
    ));
    assert_rejected(fixture, "R6 Build digest drift");

    let mut fixture = canonical_fixture();
    let check_pod = fixture.job_pod_index(PACKAGE_IMAGE_CHECK_JOB_KIND);
    fixture.pods[check_pod]
        .status
        .as_mut()
        .unwrap()
        .container_statuses
        .as_mut()
        .unwrap()[0]
        .image_id = format!(
        "containerd://{REGISTRY}/awaken-packages@sha256:{}",
        "b".repeat(64)
    );
    assert_rejected(fixture, "R6 image-check digest drift");

    let mut fixture = canonical_fixture();
    let sandbox = fixture.sandbox_index();
    fixture.pods[sandbox]
        .status
        .as_mut()
        .unwrap()
        .container_statuses
        .as_mut()
        .unwrap()[0]
        .image_id = format!(
        "containerd://{REGISTRY}/awaken-packages@sha256:{}",
        "b".repeat(64)
    );
    assert_rejected(fixture, "R6 Sandbox digest drift");
}

#[tokio::test]
async fn duplicate_and_conflicting_snapshots_never_select_a_winner() {
    /* Cardinality decision table:
     * C1 two snapshots share one immutable UID; C2 two ConfigMap incarnations
     * claim the recipe; C3 two Job incarnations claim the deterministic name;
     * C4 two Sandbox Pods claim the Session or its canonical name; C5 recovery
     * contains a Build Pod without its exact Build Job.
     * Effect E1 is fail closed for every cause, with no latest/first winner.
     * Rules R1=C1=>E1; R2=C2=>E1; R3=C3=>E1; R4=C4=>E1; R5=C5=>E1.
     */
    let mut fixture = canonical_fixture();
    let check_pod = fixture.job_pod_index(PACKAGE_IMAGE_CHECK_JOB_KIND);
    fixture.pods.push(fixture.pods[check_pod].clone());
    assert_rejected(fixture, "R1 duplicate UID snapshot");

    let mut fixture = canonical_fixture();
    let mut config = fixture.config_maps[0].clone();
    config.metadata.uid = Some("conflicting-config-uid".into());
    fixture.config_maps.push(config);
    assert_rejected(fixture, "R2 competing ConfigMap incarnation");

    let mut fixture = canonical_fixture();
    let check = fixture.job_index(PACKAGE_IMAGE_CHECK_JOB_KIND);
    let mut job = fixture.jobs[check].clone();
    job.metadata.uid = Some("conflicting-check-job-uid".into());
    fixture.jobs.push(job);
    assert_rejected(fixture, "R3 competing Job incarnation");

    let mut fixture = canonical_fixture();
    let sandbox = fixture.sandbox_index();
    let mut pod = fixture.pods[sandbox].clone();
    pod.metadata.uid = Some("conflicting-sandbox-uid".into());
    fixture.pods.push(pod);
    assert_rejected(fixture, "R4 competing Sandbox");

    let mut fixture = canonical_fixture();
    let sandbox = fixture.sandbox_index();
    let mut pod = fixture.pods[sandbox].clone();
    pod.metadata.uid = Some("canonical-name-conflict-uid".into());
    pod.metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert(SANDBOX_SCOPE_ANNOTATION.into(), "another-session".into());
    fixture.pods.push(pod);
    assert_rejected(fixture, "R4 canonical Sandbox name conflict");

    let mut fixture = canonical_fixture();
    fixture.jobs.retain(|job| {
        annotation_value(&job.metadata, PACKAGE_JOB_KIND_ANNOTATION) != Some(PACKAGE_BUILD_JOB_KIND)
    });
    assert_rejected(fixture, "R5 orphan Build Pod in recovery evidence");
}

#[tokio::test]
async fn evidence_bounds_stop_before_semantic_selection() {
    /* Resource-bound decision table:
     * C1 more than 256 objects of one kind; C2 one serialized object exceeds
     * 1 MiB; C3 a selector string is empty, controlled, or over 4 KiB.
     * Effect E1 reject before graph selection or allocation proportional to
     * attacker-controlled cardinality. R1=C1=>E1; R2=C2=>E1; R3=C3=>E1.
     */
    let too_many = vec![ConfigMap::default(); MAX_EVIDENCE_OBJECTS_PER_KIND + 1];
    assert!(
        verify_k8s_package_realization(&K8sPackageRealizationEvidence {
            namespace: NAMESPACE,
            session_id: SESSION_ID,
            package_registry: REGISTRY,
            config_maps: &too_many,
            jobs: &[],
            pods: &[],
        })
        .is_err(),
        "R1"
    );

    let oversized = ConfigMap {
        data: Some(BTreeMap::from([(
            "Dockerfile".into(),
            "x".repeat(MAX_EVIDENCE_OBJECT_BYTES + 1),
        )])),
        ..Default::default()
    };
    assert!(
        verify_k8s_package_realization(&K8sPackageRealizationEvidence {
            namespace: NAMESPACE,
            session_id: SESSION_ID,
            package_registry: REGISTRY,
            config_maps: &[oversized],
            jobs: &[],
            pods: &[],
        })
        .is_err(),
        "R2"
    );

    let fixture = canonical_fixture();
    assert!(
        verify_k8s_package_realization(&K8sPackageRealizationEvidence {
            namespace: "bad\nnamespace",
            session_id: SESSION_ID,
            package_registry: REGISTRY,
            config_maps: &fixture.config_maps,
            jobs: &fixture.jobs,
            pods: &fixture.pods,
        })
        .is_err(),
        "R3"
    );
}
