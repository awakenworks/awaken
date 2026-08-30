//! Behavior tests for the Kubernetes runtime adapter.
//!
//! Kept as a child of `k8s` so private adapter evidence and cause/effect
//! decision tables remain adjacent to their production owner.

use crate::ForwardProxy;
use awaken_provisioning_contract::ProcessHandle;
use std::sync::Arc;

use super::*;

#[tokio::test]
async fn continuation_handle_kind_is_rejected_before_kubernetes_io() {
    /* Provider-kind admission table KH1. Causes: C1 current Kubernetes V2,
     * legacy Kubernetes, or foreign host-bind continuation evidence reaches
     * observation/removal; C2 an apiserver is unavailable. Effects: E1 V2
     * yields its exact Pod/PVC fences; E2 legacy deletion fails for lack of a
     * Pod UID; E3 host-bind fails locally before any apiserver observation or
     * deletion. Rules: KH1a V2=>E1; KH1b legacy=>E2; KH1c host-bind+C2=>E3.
     * This is provider-kind admission only; the Session aggregate remains the
     * lifecycle authority and the existing K8s removal path remains the sole
     * physical effect owner. */
    let current = pc::ContainerContinuationHandle::KubernetesContinuationV2 {
        pod_uid: "pod-a".into(),
        claim_uid: Some("claim-a".into()),
    };
    assert_eq!(
        runtime_contract::deletion_incarnation(Some(&current)).unwrap(),
        (Some("pod-a"), Some("claim-a")),
        "KH1a/E1",
    );
    let legacy = pc::ContainerContinuationHandle::KubernetesContinuation {
        claim_uid: "claim-a".into(),
    };
    assert!(
        runtime_contract::deletion_incarnation(Some(&legacy)).is_err(),
        "KH1b/E2",
    );

    let host_bind = pc::ContainerContinuationHandle::HostBindRestoration(
        pc::HostBindRestorationHandle::for_restore("/tmp/awaken-acp-stage-k8s-reject").unwrap(),
    );
    assert!(
        runtime_contract::deletion_incarnation(Some(&host_bind))
            .unwrap_err()
            .to_string()
            .contains("host-bind"),
        "KH1c/E3 removal admission",
    );
    let runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
    let error = ContainerRuntime::observe(
        &runtime,
        ContainerObservationExpectation {
            container_id: "never-read",
            adoption_fingerprint: None,
            realization_fingerprint: None,
            runtime_handle: Some(&host_bind),
            effect_fence: None,
        },
    )
    .await
    .expect_err("KH1c host-bind must fail before apiserver I/O");
    assert!(error.to_string().contains("host-bind"), "KH1c/E3");
}

#[test]
fn continuation_observation_has_one_disposing_authority() {
    /* Pod/PVC observation table KCO1. Causes: C1 exact source Pod A is
     * present/absent; C2 A is live/deleting; C3 Awaken's cleanup finalizer is
     * held; C4 durable V2 expects claim P and observes P live/terminating/
     * absent/foreign; C5 the aggregate terminal fence is present; C6 the
     * total-absence proof carries exact fenced V2 claim evidence. Effects:
     * E1 ordinary A+P remains Live; E2 own finalizer proves every required
     * source-durability effect preceded physical cleanup and projects Disposing, including the PVC
     * DELETE response-loss cut before A DELETE; E3 deleting A without the
     * finalizer projects Disposing only when exact P is already terminating
     * (finalizer-release response loss); E4 absent A+terminating P projects
     * Disposing so cleanup only awaits P; E5 absent A+live P, a foreign P,
     * deleting-without-proof, or missing fence fails closed; E6 exact A/P total
     * absence is Live here and becomes DefinitivelyUnavailable only in the
     * shared physical-observation owner.
     *
     * | Rule | A | deleting | own F | P | fence | total proof | Effect |
     * | K1 | present | no  | no  | exact live | yes/no | no | E1 Live |
     * | K2 | present | no/yes | yes | exact live/term | yes | no | E2 Disposing |
     * | K3 | present | yes | no | exact term | yes | no | E3 Disposing |
     * | K4 | absent | n/a | no | exact term | yes | no | E4 Disposing |
     * | K5 | absent | n/a | no | exact live | yes | no | E5 Incompatible |
     * | K6 | present | yes | no | exact live | yes | no | E5 Indeterminate |
     * | K7 | any | any | any | foreign | yes | any | E5 Incompatible |
     * | K8 | absent | n/a | no | absent | yes | yes | E6 Live |
     * | K9 | absent | n/a | no | absent | no | no | E5 Incompatible | */
    use ContinuationObservationDisposition::{Disposing, Incompatible, Live};

    let classify = |pod_present,
                    pod_deleting,
                    cleanup_gate,
                    observed_claim_uid,
                    claim_terminating,
                    effect_fenced,
                    fenced_total_absence| {
        continuation_observation_disposition(ContinuationObservationEvidence {
            pod_present,
            pod_deleting,
            cleanup_gate_held: cleanup_gate,
            expected_uid: Some("p"),
            observed_uid: observed_claim_uid,
            observed_terminating: claim_terminating,
            effect_fenced,
            effect_fenced_total_absence: fenced_total_absence,
        })
    };

    assert_eq!(
        classify(true, false, false, Some("p"), false, false, false).unwrap(),
        Live,
        "K1"
    );
    for (deleting, terminating) in [(false, false), (true, false), (true, true)] {
        assert_eq!(
            classify(true, deleting, true, Some("p"), terminating, true, false).unwrap(),
            Disposing,
            "K2 deleting={deleting} terminating={terminating}"
        );
    }
    assert_eq!(
        classify(true, true, false, Some("p"), true, true, false).unwrap(),
        Disposing,
        "K3"
    );
    assert_eq!(
        classify(false, false, false, Some("p"), true, true, false).unwrap(),
        Disposing,
        "K4"
    );
    assert_eq!(
        classify(false, false, false, Some("p"), false, true, false).unwrap(),
        Incompatible,
        "K5"
    );
    assert!(
        classify(true, true, false, Some("p"), false, true, false).is_err(),
        "K6"
    );
    assert_eq!(
        classify(true, false, false, Some("q"), false, true, false).unwrap(),
        Incompatible,
        "K7"
    );
    assert_eq!(
        classify(false, false, false, None, false, true, true).unwrap(),
        Live,
        "K8"
    );
    assert_eq!(
        classify(false, false, false, None, false, false, false).unwrap(),
        Incompatible,
        "K9"
    );
}

#[test]
fn pod_effect_fence_projection_decision_table_is_total() {
    /* Effect-evidence cause/effect table. Causes: C1 none/all/partial of
     * operation, owner, runtime, epoch, and expiry annotations; C2 numeric
     * fields valid/invalid. Effects: E1 none remains legacy evidence; E2
     * all valid fields reconstruct the exact neutral fence used by create,
     * observe, and terminal removal; E3 partial or malformed evidence fails
     * closed before adoption or deletion. */
    assert_eq!(pod_effect_fence(None).unwrap(), None, "E1");

    let fence =
        pc::SandboxEffectFence::new("operation-1", "owner-1", "runtime-1", 7, 9_999).unwrap();
    let complete = std::collections::BTreeMap::from([
        (SANDBOX_EFFECT_ANNOTATION.into(), fence.operation_id.clone()),
        (SANDBOX_EFFECT_OWNER_ANNOTATION.into(), fence.owner.clone()),
        (
            SANDBOX_EFFECT_RUNTIME_ANNOTATION.into(),
            fence.runtime_incarnation.clone(),
        ),
        (
            SANDBOX_EFFECT_EPOCH_ANNOTATION.into(),
            fence.epoch.to_string(),
        ),
        (
            SANDBOX_EFFECT_EXPIRY_ANNOTATION.into(),
            fence.expires_at_unix_ms.to_string(),
        ),
    ]);
    assert_eq!(
        pod_effect_fence(Some(&complete)).unwrap(),
        Some(fence),
        "E2"
    );

    let mut partial = complete.clone();
    partial.remove(SANDBOX_EFFECT_OWNER_ANNOTATION);
    assert!(pod_effect_fence(Some(&partial)).is_err(), "E3 partial");
    let mut malformed = complete;
    malformed.insert(SANDBOX_EFFECT_EPOCH_ANNOTATION.into(), "invalid".into());
    assert!(pod_effect_fence(Some(&malformed)).is_err(), "E3 malformed");
}

#[tokio::test]
async fn realization_configuration_covers_every_immutable_kubernetes_input() {
    /* Provider-configuration cause/effect table. Causes: C1 namespace and
     * stable realization namespace; C2 direct/rendezvous/resident transport;
     * C3 image-pull Secret list; C4 retained-volume policy; C5 GC owner.
     * Effect E1 exact replay yields identical adoption identity; E2 changing
     * any C1-C5 input changes the pure projection before apiserver I/O. The
     * short-lived network capability is deliberately absent and represented
     * by EgressRealizationIdentity's non-secret issuer revision instead. */
    let base = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
    let replay = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
    assert_eq!(
        base.realization_configuration().unwrap(),
        replay.realization_configuration().unwrap(),
        "E1"
    );

    let changed = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap())
        .with_rendezvous("127.0.0.1:7000".parse().unwrap())
        .with_image_pull_secrets(["registry-auth".into()])
        .with_continuation_volume(crate::K8sContinuationVolume {
            storage_class_name: Some("fast".into()),
            size: "20Gi".into(),
        })
        .with_owner(OwnerReference {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            name: "sandbox-owner".into(),
            uid: "owner-uid".into(),
            controller: Some(true),
            block_owner_deletion: Some(false),
        });
    assert_ne!(
        base.realization_configuration().unwrap(),
        changed.realization_configuration().unwrap(),
        "E2"
    );
}

#[test]
fn projected_cleanup_requires_attempt_owner_and_object_cas() {
    /* Projected-cleanup cause/effect decision table.
     * Causes: C0 K8s is compiled without Docker/Podman; C1 exact/different
     * raw+label attempt; C2 exact/different Pod
     * owner name+UID; C3 projected object UID/resourceVersion present/missing.
     * Effects: E0 the shared attempt label remains available to K8s; E1
     * authorize one UID+RV-preconditioned delete; E2 ignore an
     * object outside this exact attempt/owner; E3 fail closed before DELETE.
     * Rules: P0 C0=>E0 (this crate-level label reference is compile coverage);
     * P1 exact(C1)+exact(C2)+complete(C3)=>E1; P2 different(C1)=>E2;
     * P3 different(C2)=>E2; P4 exact(C1+C2)+missing(C3)=>E3. Pod absence is
     * intentionally outside this kernel and delegates exclusively to GC.
     */
    let metadata = |attempt: &str, owner_uid: &str, uid: Option<&str>| ObjectMeta {
        name: Some("awaken-session-1-cfg-0".into()),
        uid: uid.map(str::to_owned),
        resource_version: Some("rv-1".into()),
        annotations: Some(std::collections::BTreeMap::from([(
            SANDBOX_ATTEMPT_ANNOTATION.into(),
            attempt.into(),
        )])),
        labels: Some(std::collections::BTreeMap::from([(
            crate::SANDBOX_ATTEMPT_LABEL.into(),
            k8s_attempt_label(attempt),
        )])),
        owner_references: Some(vec![OwnerReference {
            api_version: "v1".into(),
            kind: "Pod".into(),
            name: "awaken-session-1".into(),
            uid: owner_uid.into(),
            controller: Some(true),
            block_owner_deletion: Some(false),
        }]),
        ..Default::default()
    };
    let exact = metadata("attempt-1", "pod-uid-1", Some("content-uid-1"));
    assert_eq!(
        projected_content_delete_preconditions(
            &exact,
            "awaken-session-1",
            "pod-uid-1",
            "attempt-1",
        )
        .unwrap(),
        Some(kube::api::Preconditions {
            uid: Some("content-uid-1".into()),
            resource_version: Some("rv-1".into()),
        }),
        "P1"
    );
    assert!(
        projected_content_delete_preconditions(
            &exact,
            "awaken-session-1",
            "pod-uid-1",
            "attempt-2",
        )
        .unwrap()
        .is_none(),
        "P2"
    );
    assert!(
        projected_content_delete_preconditions(
            &exact,
            "awaken-session-1",
            "pod-uid-2",
            "attempt-1",
        )
        .unwrap()
        .is_none(),
        "P3"
    );
    assert!(
        projected_content_delete_preconditions(
            &metadata("attempt-1", "pod-uid-1", None),
            "awaken-session-1",
            "pod-uid-1",
            "attempt-1",
        )
        .is_err(),
        "P4"
    );
}

#[test]
fn pod_observation_projects_only_api_facts_for_the_shared_decision() {
    /* K8s observation-projection cause/effect table.
     * Causes: C1 Pod UID/resourceVersion present/missing; C2 immutable
     * fingerprint present/missing; C3 Pod creating/terminal/terminating.
     * Effects: E1 emit exact incarnation+fingerprint facts and shared phase;
     * E2 fail closed on incomplete API identity; E3 mark deletion-in-progress
     * indeterminate, never unavailable. Rules: O1 complete(C1)+C2+C3=>E1;
     * O2 missing(C1)=>E2; O3 terminating(C3)=>E3. The provider-neutral
     * `sandbox_observation` tests own the later Ready/Unavailable decision.
     */
    let mut pod = Pod {
        metadata: ObjectMeta {
            name: Some("awaken-session-1".into()),
            uid: Some("pod-uid-1".into()),
            resource_version: Some("rv-1".into()),
            annotations: Some(std::collections::BTreeMap::from([
                (SANDBOX_ADOPTION_ANNOTATION.into(), "adoption-1".into()),
                (
                    SANDBOX_REALIZATION_ANNOTATION.into(),
                    "fingerprint-1".into(),
                ),
            ])),
            ..Default::default()
        },
        status: Some(k8s_openapi::api::core::v1::PodStatus {
            phase: Some("Failed".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let terminal = observed_pod(&pod).expect("O1");
    assert_eq!(terminal.incarnation.identity, "pod-uid-1", "O1");
    assert_eq!(terminal.incarnation.version.as_deref(), Some("rv-1"), "O1");
    assert_eq!(
        terminal.adoption_fingerprint.as_deref(),
        Some("adoption-1"),
        "O1"
    );
    assert_eq!(terminal.fingerprint.as_deref(), Some("fingerprint-1"), "O1");
    assert_eq!(terminal.phase, ExistingRealizationPhase::Terminal, "O1");

    pod.metadata.uid = None;
    assert!(observed_pod(&pod).is_err(), "O2");
    pod.metadata.uid = Some("pod-uid-1".into());
    pod.metadata.deletion_timestamp =
        serde_json::from_str("\"2026-08-29T00:00:00Z\"").expect("valid Kubernetes timestamp");
    assert_eq!(
        observed_pod(&pod).unwrap().phase,
        ExistingRealizationPhase::Indeterminate,
        "O3"
    );
}

#[tokio::test]
async fn labels_without_live_policy_evidence_never_create_a_capability() {
    // Cause/effect rule: a test runtime has no live apiserver attestation;
    // posture labels alone therefore leave network isolation false. The
    // positive and additive-widening rules live beside the sole attestor in
    // `network_policy::tests`.
    let absent = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
    assert!(!absent.enforces_network_none());
}

#[tokio::test]
async fn kube_client_accepts_an_http_proxy_from_deployment_environment() {
    // A host-level HTTP(S)_PROXY is consumed by kube::Config discovery. K8s
    // workers must keep accepting that standard deployment posture instead of
    // panicking while the Session runtime is being composed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut config = kube::Config::new("https://127.0.0.1:1/".parse().unwrap());
    config.proxy_url = Some("http://127.0.0.1:18082/".parse().unwrap());
    Client::try_from(config).expect("the k8s client is compiled with HTTP proxy support");
}

#[tokio::test]
async fn empty_scope_fails_before_the_first_k8s_write() {
    /* Boundary rule extending the table above: an empty opaque scope has no
     * runtime identity and fails before the lazy test client can reach its
     * deliberately unavailable API server. Long valid scopes are covered by
     * names::tests and map to a bounded content identity. */
    let runtime = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
    let plan = plan_with_memory(Vec::new());
    let error = runtime.create("", &plan).await.unwrap_err();
    assert!(error.to_string().contains("sandbox scope"), "N5: {error}");
}

#[tokio::test]
async fn package_builder_job_is_rootless_bounded_and_registry_backed() {
    // Cause/effect decision table: R1 exact base/packages + shared Registry
    // produce one deterministic ConfigMap/Job destination; R2 insecure local
    // Registry emits an explicit BuildKit host policy; R3 the Job is rootless,
    // tokenless, no-retry, bounded, admits RootlessKit's subordinate mount
    // namespace through unconfined seccomp and AppArmor, and gives rootless
    // BuildKit a COS-compatible state volume; R4 output uses the termination
    // digest contract; R5 unsafe Registry prefixes fail before entering
    // BuildKit configuration; R6 ring+aws-lc feature unification => select
    // ring before constructing the lazy kube client.
    install_rustls_crypto_provider();
    let config = kube::Config::new("http://127.0.0.1:1/".parse().unwrap());
    let client = Client::try_from(config).unwrap();
    let builder = K8sPackageImageProvisioner::new(
        client.clone(),
        "awaken-system",
        "registry.local:5000/environments",
        vec!["registry-auth".into()],
        true,
    )
    .unwrap()
    .with_buildkit_image("registry.local:5000/system/buildkit:v0.30.0-rootless")
    .unwrap()
    .with_forward_proxy(ForwardProxy {
        url: "http://proxy.internal:8080".into(),
    })
    .unwrap();
    let packages = pc::PackageRequirements {
        managers: [("npm".into(), vec!["@playwright/mcp@latest".into()])]
            .into_iter()
            .collect(),
        resolution_id: Some("env-browser:3".into()),
    };
    let (config, job, destination) = builder
        .build_objects("registry.local/base@sha256:exact", &packages)
        .unwrap();
    assert!(
        destination.starts_with("registry.local:5000/environments/awaken-packages:"),
        "R1"
    );
    let data = config.data.unwrap();
    assert!(data["Dockerfile"].contains("@playwright/mcp@latest"), "R1");
    assert!(data["buildkitd.toml"].contains("http = true"), "R2");
    let spec = job.spec.unwrap();
    assert_eq!(spec.backoff_limit, Some(0), "R3");
    assert!(
        (30 * 60..60 * 60).contains(&spec.active_deadline_seconds.unwrap()),
        "R3 cold package builds stay bounded below the Coordinator run ceiling"
    );
    let pod = spec.template.spec.unwrap();
    assert_eq!(pod.automount_service_account_token, Some(false), "R3");
    assert_eq!(
        pod.image_pull_secrets.as_ref().unwrap()[0].name,
        "registry-auth",
        "R3"
    );
    assert!(
        pod.volumes
            .as_ref()
            .unwrap()
            .iter()
            .any(|volume| volume.name == "registry-auth"),
        "R3 private Registry auth"
    );
    let buildkit = &pod.containers[0];
    assert!(
        pod.volumes
            .as_ref()
            .unwrap()
            .iter()
            .any(|volume| { volume.name == "buildkit-state" && volume.empty_dir.is_some() }),
        "R3 rootless BuildKit state must use an emptyDir on Google COS"
    );
    assert!(
        buildkit
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .any(|mount| {
                mount.name == "buildkit-state"
                    && mount.mount_path == "/home/user/.local/share/buildkit"
            }),
        "R3 rootless BuildKit state must use the official writable path"
    );
    let security_context = buildkit.security_context.as_ref().unwrap();
    assert_eq!(
        buildkit.image.as_deref(),
        Some("registry.local:5000/system/buildkit:v0.30.0-rootless"),
        "R3 the operator-selected mirror must be the only BuildKit pull reference"
    );
    assert_eq!(security_context.run_as_user, Some(1000), "R3");
    assert_eq!(
        security_context.allow_privilege_escalation,
        Some(true),
        "R3 rootless newuidmap/newgidmap helpers require setuid execution"
    );
    assert_eq!(
        security_context
            .app_armor_profile
            .as_ref()
            .map(|profile| profile.type_.as_str()),
        Some("Unconfined"),
        "R3 GKE AppArmor must not reject RootlessKit mount propagation"
    );
    assert!(
        buildkit.args.as_ref().unwrap()[0].contains("/dev/termination-log"),
        "R4"
    );
    let environment = buildkit.env.as_ref().unwrap();
    for name in [
        "FORWARD_PROXY",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
    ] {
        assert!(
            environment.iter().any(|variable| {
                variable.name == name
                    && variable.value.as_deref() == Some("http://proxy.internal:8080")
            }),
            "R6 BuildKit and package-manager egress must inherit {name}"
        );
    }
    assert!(
        environment.iter().any(|variable| {
            variable.name == "NO_PROXY"
                && variable
                    .value
                    .as_deref()
                    .is_some_and(|value| value.contains("registry.local:5000"))
        }),
        "R6 the package Registry must bypass the external proxy"
    );
    assert!(
        buildkit.args.as_ref().unwrap()[0].contains("build-arg:HTTP_PROXY"),
        "R6 predefined proxy args must reach package-manager RUN steps"
    );
    assert!(
        buildkit.args.as_ref().unwrap()[0]
            .contains("mkdir -p /tmp/workspace\ncp /input/Dockerfile /tmp/workspace/Dockerfile"),
        "R6 rootless BuildKit must use a writable workspace"
    );
    assert!(
        K8sPackageImageProvisioner::new(
            client,
            "awaken-system",
            "registry.local/environments\n[registry.\"attacker\"]",
            Vec::new(),
            true,
        )
        .is_err(),
        "R5"
    );
    assert!(
        K8sPackageImageProvisioner::new(
            Client::try_from(kube::Config::new("http://127.0.0.1:1/".parse().unwrap())).unwrap(),
            "awaken-system",
            "registry.local/environments",
            Vec::new(),
            true,
        )
        .unwrap()
        .with_buildkit_image("registry.local/bad image")
        .is_err(),
        "R7 an invalid mirrored builder reference must fail before a Kubernetes write"
    );
    assert!(
        K8sPackageImageProvisioner::new(
            Client::try_from(kube::Config::new("http://127.0.0.1:1/".parse().unwrap())).unwrap(),
            "awaken-system",
            "registry.local/environments",
            Vec::new(),
            true,
        )
        .unwrap()
        .with_forward_proxy(ForwardProxy {
            url: "file:///tmp/not-a-proxy".into(),
        })
        .is_err(),
        "R9 an invalid package-build proxy must fail before a Kubernetes write"
    );
}

struct FixedBroker;

#[async_trait]
impl pc::SecretBroker for FixedBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(b"k8s-secret".to_vec())
    }

    async fn materialize_process(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.materialize(reference).await
    }

    async fn write_back(&self, _reference: &str, _bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::new("not supported"))
    }
}

async fn materialized(command: pc::Command) -> pc::MaterializedCommand {
    pc::materialize_process_command(&[], command, None)
        .await
        .unwrap()
}

fn exec_process(completion: Option<tokio::task::JoinHandle<Option<Status>>>) -> K8sExecProcess {
    let rt = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
    K8sExecProcess {
        id: "k8s-exec-test".into(),
        pod: "pod-test".into(),
        pid_file: "/tmp/pid".into(),
        pods: rt.pods(),
        state: tokio::sync::Mutex::new(K8sExecState {
            completion,
            status: None,
        }),
    }
}

fn success_status(code: i32) -> Status {
    Status {
        status: Some(if code == 0 { "Success" } else { "Failure" }.into()),
        details: Some(
            k8s_openapi::apimachinery::pkg::apis::meta::v1::StatusDetails {
                causes: Some(vec![
                    k8s_openapi::apimachinery::pkg::apis::meta::v1::StatusCause {
                        reason: Some("ExitCode".into()),
                        message: Some(code.to_string()),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
        ),
        ..Default::default()
    }
}

#[test]
fn signal_completion_uses_one_authoritative_success_rule() {
    // Cause/effect graph: C1 signal command exits zero/non-zero; C2 target
    // process is unfinished/finished. Effect E1 accepts the signal effect
    // when C1 is zero OR C2 is finished; E2 rejects it only when neither is
    // true. Decision table: R1 zero+unfinished=>E1, R2 zero+finished=>E1,
    // R3 non-zero+finished=>E1, R4 non-zero+unfinished=>E2. FMECA: parallel
    // success branches can drift and turn a harmless exit race into a false
    // failure (S4/O3/D4); this predicate is the sole owner of the rule.
    let success = pc::ExitStatus {
        code: Some(0),
        signaled: false,
    };
    let failure = pc::ExitStatus {
        code: Some(1),
        signaled: false,
    };
    let finished = pc::ExitStatus {
        code: Some(143),
        signaled: true,
    };

    assert!(signal_effect_is_complete(&success, None), "R1/E1");
    assert!(
        signal_effect_is_complete(&success, Some(&finished)),
        "R2/E1"
    );
    assert!(
        signal_effect_is_complete(&failure, Some(&finished)),
        "R3/E1"
    );
    assert!(!signal_effect_is_complete(&failure, None), "R4/E2");
}

#[tokio::test]
async fn exec_wait_and_poll_cache_the_remote_completion_status() {
    let process = exec_process(Some(tokio::spawn(async { Some(success_status(7)) })));
    assert_eq!(process.id(), "k8s-exec-test");
    assert_eq!(process.wait().await.unwrap().code, Some(7));
    assert_eq!(process.wait().await.unwrap().code, Some(7));
    assert_eq!(process.poll().await.unwrap().unwrap().code, Some(7));
}

#[tokio::test]
async fn exec_poll_distinguishes_running_missing_and_finished_status() {
    let process = exec_process(Some(tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        Some(success_status(0))
    })));
    assert_eq!(process.poll().await.unwrap(), None);
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(process.poll().await.unwrap().unwrap().code, Some(0));

    let missing = exec_process(None);
    assert!(missing.poll().await.is_err());
    assert!(missing.wait().await.is_err());
    assert_eq!(k8s_exit_status(None).code, Some(1));
}

#[tokio::test]
async fn exec_completion_join_failures_and_signal_transport_fail_closed() {
    let aborted_wait = tokio::spawn(std::future::pending::<Option<Status>>());
    aborted_wait.abort();
    let process = exec_process(Some(aborted_wait));
    assert!(process.wait().await.is_err());

    let aborted_poll = tokio::spawn(std::future::pending::<Option<Status>>());
    aborted_poll.abort();
    tokio::task::yield_now().await;
    let process = exec_process(Some(aborted_poll));
    assert!(process.poll().await.is_err());

    let process = exec_process(None);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        process.signal(pc::Signal::Kill),
    )
    .await
    .expect("unreachable test API must fail promptly");
    assert!(result.is_err());
}

#[test]
fn live_file_harvest_requires_a_successful_remote_exit_status() {
    // Credential-harvest FMECA decision table. Causes: C1 remote `cat`
    // exits 0; C2 it exits nonzero (missing/permission denied); C3 the API
    // stream closes without a Status frame; C4 returned bytes are empty.
    // Effects: E1 exact bytes are eligible for broker write-back; E2 fail
    // closed so stale/empty material cannot replace authority. Rules: H1
    // C1=>E1; H2 C1+C4=>E1 (empty is data, validation belongs upstream);
    // H3 C2|C3=>E2.
    assert_eq!(
        k8s_live_file_result(Some(success_status(0)), b"rotated".to_vec()).unwrap(),
        Some(b"rotated".to_vec()),
        "H1"
    );
    assert_eq!(
        k8s_live_file_result(Some(success_status(0)), Vec::new()).unwrap(),
        Some(Vec::new()),
        "H2"
    );
    assert!(
        k8s_live_file_result(Some(success_status(1)), Vec::new()).is_err(),
        "H3 nonzero"
    );
    assert!(
        k8s_live_file_result(None, Vec::new()).is_err(),
        "H3 missing status"
    );
}

#[tokio::test]
async fn exec_admission_and_argv_materialization_cover_all_command_boundaries() {
    assert!(k8s_exec_argv("empty", pc::MaterializedCommand::new(Vec::<String>::new())).is_err());

    let mut secret = pc::Command::new(["echo", "value"]);
    secret.env.push(pc::EnvVar {
        name: "TOKEN".into(),
        value: pc::EnvValue::Secret {
            reference: "credential://test".into(),
        },
        visibility: pc::EnvVisibility::Process,
    });
    let broker: Arc<dyn pc::SecretBroker> = Arc::new(FixedBroker);
    let secret = pc::materialize_process_command(&[], secret, Some(&broker))
        .await
        .unwrap();
    let secret = k8s_exec_argv("secret", secret).unwrap();
    assert_eq!(secret.secret_stdin.len(), 1);
    assert!(
        secret
            .argv
            .iter()
            .all(|value| !value.contains("container-process-secret")),
        "the secret prelude must never enter Kubernetes exec argv"
    );
    assert!(secret.argv.iter().any(|value| value == "TOKEN"));

    let mut inline = pc::Command::new(["echo", "value"]);
    inline.cwd = "/workspace".into();
    inline.env.push(pc::EnvVar {
        name: "MODE".into(),
        value: pc::EnvValue::Inline {
            value: "test".into(),
        },
        visibility: pc::EnvVisibility::Process,
    });
    let inline = materialized(inline).await;
    let inline = k8s_exec_argv("inline", inline).unwrap();
    assert_eq!(inline.pid_file, "/tmp/inline.pid");
    assert!(inline.argv.iter().any(|value| value == "MODE=test"));
    assert!(inline.argv.iter().any(|value| value == "/workspace"));
    assert!(inline.secret_stdin.is_empty());

    let rt = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
    let mut piped = pc::MaterializedCommand::new(["echo", "value"]);
    piped.stdio = pc::Stdio::Piped;
    assert!(rt.spawn("pod", piped).await.is_err());
    assert!(
        rt.spawn_agent("pod", pc::MaterializedCommand::new(Vec::<String>::new()))
            .await
            .is_err()
    );
}

#[test]
fn secret_stdin_prelude_becomes_process_env_and_preserves_agent_protocol_input() {
    use std::io::Write as _;

    let mut command = pc::MaterializedCommand::new([
        "sh",
        "-c",
        "printf '%s|' \"$TOKEN\"; IFS= read -r line; printf '%s' \"$line\"",
    ]);
    command.env.push(pc::MaterializedEnvVar {
        name: "TOKEN".into(),
        value: pc::MaterializedEnvValue::Secret(awaken_runtime_contract::RedactedString::new(
            "test-secret",
        )),
    });
    let execution = k8s_exec_argv("secret-prelude-test", command).unwrap();
    assert!(
        execution
            .argv
            .iter()
            .all(|part| !part.contains("test-secret"))
    );

    let mut child = std::process::Command::new(&execution.argv[0])
        .args(&execution.argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    for secret in execution.secret_stdin {
        stdin.write_all(secret.expose().as_bytes()).unwrap();
    }
    stdin.write_all(b"protocol-message\n").unwrap();
    let output = child.wait_with_output().unwrap();
    let _ = std::fs::remove_file(execution.pid_file);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"test-secret|protocol-message");
}

#[tokio::test]
async fn builder_methods_set_every_field_and_pod_delegates_to_build_pod() {
    let rt = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap())
        .with_owner(OwnerReference::default())
        .with_rendezvous("127.0.0.1:7000".parse().unwrap())
        .with_image_pull_secrets(["registry-pull".into()]);
    assert!(rt.owner.is_some());
    assert_eq!(rt.rendezvous, Some("127.0.0.1:7000".parse().unwrap()));
    let labels = rt
        .pod("s1", &plan_with_memory(vec![]))
        .metadata
        .labels
        .unwrap();
    assert_eq!(
        labels.get(crate::MANAGED_SANDBOX_LABEL).map(String::as_str),
        Some("1")
    );
    assert_eq!(
        labels.get(crate::RUNTIME_OWNER_LABEL).map(String::as_str),
        Some(rt.owner_id.as_str())
    );
    assert_eq!(
        rt.pod("s1", &plan_with_memory(vec![]))
            .spec
            .unwrap()
            .image_pull_secrets
            .unwrap()[0]
            .name,
        "registry-pull"
    );
    // pod() threads the builder state into build_pod and the Api handle builds.
    let pod = rt.pod("s1", &plan_with_memory(vec![]));
    assert!(pod.metadata.name.is_some());
    assert!(pod.spec.is_some());
    let _ = rt.pods();
}

#[tokio::test]
async fn artifacts_are_out_of_band_and_touch_lease_is_a_noop() {
    let rt = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
    assert!(rt.artifacts("pod").await.unwrap().is_empty());
    assert!(rt.touch_lease("pod").await.is_ok());
    assert!(rt.read_artifact("pod", "artifact").await.is_err());
}

#[test]
fn pod_resources_maps_requests_and_limits_independently() {
    // Cause/effect graph: C1 requests set, C2 limits set, C3 pids set.
    // R1 C1+C2+C3 => E1 Kubernetes requests and limits carry cpu/memory/disk,
    // E2 pids is absent because Kubernetes has no Pod resource key. R2 neither
    // C1 nor an expressible C2 => no ResourceRequirements (next test).
    let r = pod_resources(
        &pc::ResourceRequests {
            cpu_millis: Some(750),
            memory_bytes: Some(536_870_912),
            disk_bytes: Some(1024),
        },
        &pc::ResourceLimits {
            cpu_millis: Some(1500),
            memory_bytes: Some(1_073_741_824),
            pids: Some(256),
            disk_bytes: Some(2048),
        },
    )
    .expect("requests and limits are set");
    let requests = r.requests.expect("requests map present");
    assert_eq!(requests.get("cpu").unwrap().0, "750m");
    assert_eq!(requests.get("memory").unwrap().0, "536870912");
    assert_eq!(requests.get("ephemeral-storage").unwrap().0, "1024");
    let limits = r.limits.expect("limits map present");
    assert_eq!(limits.get("cpu").unwrap().0, "1500m");
    assert_eq!(limits.get("memory").unwrap().0, "1073741824");
    assert_eq!(limits.get("ephemeral-storage").unwrap().0, "2048");
    // pids has no standard pod-level key.
    assert!(!limits.contains_key("pids"));
}

#[test]
fn pod_resources_is_none_without_expressible_caps() {
    assert!(
        pod_resources(
            &pc::ResourceRequests::default(),
            &pc::ResourceLimits::default()
        )
        .is_none()
    );
    // pids-only → nothing k8s expresses as a pod limit.
    assert!(
        pod_resources(
            &pc::ResourceRequests::default(),
            &pc::ResourceLimits {
                pids: Some(9),
                ..Default::default()
            }
        )
        .is_none()
    );
}

#[test]
fn a_pids_limit_is_flagged_unenforceable_on_k8s_so_create_fails_closed() {
    // pids is the one limit k8s cannot express at the Pod spec — flagged so `create`
    // refuses it rather than silently dropping the cap (C6: no fail-open on a
    // fork-bomb guard). CPU/memory/disk are enforceable, so they are NOT flagged.
    assert_eq!(
        unenforceable_k8s_limit(&pc::ResourceLimits {
            pids: Some(64),
            ..Default::default()
        }),
        Some("pids")
    );
    assert_eq!(
        unenforceable_k8s_limit(&pc::ResourceLimits {
            cpu_millis: Some(1000),
            memory_bytes: Some(1 << 30),
            disk_bytes: Some(1 << 20),
            ..Default::default()
        }),
        None
    );
    assert_eq!(
        unenforceable_k8s_limit(&pc::ResourceLimits::default()),
        None
    );
}

fn plan_with_memory(mounts: Vec<crate::MemoryMount>) -> ContainerPlan {
    ContainerPlan {
        image: "agent:1".into(),
        command: vec!["claude".into(), "--acp".into()],
        env: vec![("TZ".into(), "UTC".into())],
        control_services: Default::default(),
        packages: Default::default(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: crate::NetworkMode::Open,
        egress_identity: Default::default(),
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits {
            memory_bytes: Some(1 << 30),
            ..Default::default()
        },
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        memory_mounts: mounts,
        rootfs: crate::RootfsPlan::HostUserland,
    }
}

fn empty_memory_tar() -> Vec<u8> {
    tar::Builder::new(Vec::new()).into_inner().unwrap()
}

#[test]
fn build_pod_realizes_memory_mounts_from_one_authoritative_snapshot_path() {
    /* Memory projection cause/effect decision table — KM1:
     * C1 the canonical MemoryMounter produced a bounded snapshot; C2 access is
     * read-only or read-write; C3 the Pod has no Resource-authority network
     * credential; C4 projection is delayed, completed with the same effect, or
     * carries foreign effect evidence. C1+C2+C3 => E1 one emptyDir per mount +
     * one runtime-only projector, E2 Agent access matches C2, E3 no memoryd,
     * second database, ConfigMap size ceiling, or network client exists in the
     * Pod. C4 delayed => E4 Agent waits without observing the empty volume;
     * same-effect completion => E5 replay does not clear/project twice; foreign
     * evidence => E6 fail closed before Memory mutation. Snapshot/harvest
     * failures are covered by the provider lifecycle table and fail before
     * success is published.
     */
    let plan = plan_with_memory(vec![
        crate::MemoryMount {
            store_id: "s1".into(),
            mount_path: "/workspace/.mnt/a".into(),
            access: pc::MountAccess::ReadOnly,
            snapshot_tar: empty_memory_tar(),
        },
        crate::MemoryMount {
            store_id: "s2".into(),
            mount_path: "/workspace/.mnt/b".into(),
            access: pc::MountAccess::ReadWrite,
            snapshot_tar: empty_memory_tar(),
        },
    ]);
    let effect_fence =
        ContainerEffectFence::new("memory-projection-1", "owner-1", "runtime-1", 1, u64::MAX)
            .unwrap();
    let pod = build_pod_with_continuation(
        "run-1",
        &plan,
        &None,
        None,
        &[],
        None,
        Some(&effect_fence),
        None,
    );
    let spec = pod.spec.unwrap();

    // Agent + Memory projector + isolated input projector; no second Memory implementation.
    assert_eq!(spec.containers.len(), 3);
    assert_eq!(spec.containers[0].name, "agent");
    assert_eq!(spec.containers[1].name, memory::PROJECTOR);
    assert!(spec.init_containers.is_none());
    // One emptyDir per store, the three writable-rootfs dirs, the live
    // read-only input tree, and the projection-completion rendezvous. Large
    // snapshots are streamed, not ConfigMaps.
    let volumes = spec.volumes.as_ref().unwrap();
    assert_eq!(volumes.len(), 2 + 3 + 1 + 1);
    assert!(volumes.iter().all(|volume| volume.empty_dir.is_some()));
    // The agent mounts both memory volumes + writable dirs + live inputs +
    // the read-only projection-completion rendezvous.
    let agent = &spec.containers[0];
    assert_eq!(agent.volume_mounts.as_ref().unwrap().len(), 2 + 3 + 1 + 1);
    assert_eq!(
        agent.command.as_ref().unwrap(),
        &memory::gated_agent_command(&plan.command, &effect_fence),
        "KM1/E4"
    );
    let mounts = agent.volume_mounts.as_ref().unwrap();
    assert_eq!(
        mounts
            .iter()
            .find(|m| m.mount_path.ends_with("/a"))
            .unwrap()
            .read_only,
        Some(true)
    );
    assert_eq!(
        mounts
            .iter()
            .find(|m| m.mount_path.ends_with("/b"))
            .unwrap()
            .read_only,
        Some(false)
    );
    assert!(agent.resources.is_some());
    assert!(
        spec.containers
            .iter()
            .all(|container| !container.name.starts_with("memoryd-"))
    );
    assert_eq!(
        spec.containers[1].volume_mounts.as_ref().unwrap().len(),
        3,
        "one projector owns the writable Memory sides and completion marker"
    );
    assert_eq!(
        spec.containers[1]
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|env| env.name == memory::PROJECTION_FENCE_ENV)
            .and_then(|env| env.value.as_deref()),
        Some(memory::projection_fence_value(&effect_fence).as_str()),
        "KM1/E5-E6"
    );
}

#[test]
fn build_pod_projects_inline_content_as_configmap_subpath_volumes() {
    /* Cause/effect projection decision table — KP3:
     * C1 an ordinary inline bind is outside the managed input root; C2 a managed
     * inline bind precedes it; C3 a ref-only bind has no carried bytes.
     * C1+C2+C3 => E1 only the ordinary bind becomes a ConfigMap/subPath, E2 its
     * stable content-bind index remains cfg-1 in both build/create, and E3 neither
     * the managed bind nor ref-only bind creates a parallel ConfigMap path.
     */
    let mut plan = plan_with_memory(Vec::new());
    plan.binds = vec![
        crate::BindPlan {
            source_ref: String::new(),
            mount_path: "/mnt/session/uploads/current/index.html".into(),
            read_only: true,
            content: Some("<h1>managed</h1>".into()),
            content_bytes: None,
            secret_content: None,
            secret_writeback: false,
            credential_file_path: None,
        },
        crate::BindPlan {
            source_ref: String::new(),
            mount_path: "/acp-config/config.toml".into(),
            read_only: true,
            content: Some("[mcp_servers.gh]\nx\n".into()),
            content_bytes: None,
            secret_content: None,
            secret_writeback: false,
            credential_file_path: None,
        },
        // A ref-backed bind (no content) must NOT become a ConfigMap volume.
        crate::BindPlan {
            source_ref: "blob-123".into(),
            mount_path: "/data/in".into(),
            read_only: true,
            content: None,
            content_bytes: None,
            secret_content: None,
            secret_writeback: false,
            credential_file_path: None,
        },
    ];
    let spec = build_pod("run-9", &plan, &None, None, &[]).spec.unwrap();

    // One ConfigMap volume (only the ordinary content bind), named cfg-1, plus the
    // 2 writable-rootfs emptyDirs. The ref-backed bind adds nothing on this tier.
    let volumes = spec.volumes.as_ref().unwrap();
    let cfg = volumes
        .iter()
        .find(|v| v.config_map.is_some())
        .expect("a configmap volume");
    assert_eq!(cfg.name, "cfg-1");
    assert_eq!(cfg.config_map.as_ref().unwrap().name, "awaken-run-9-cfg-1");
    assert_eq!(volumes.iter().filter(|v| v.config_map.is_some()).count(), 1);

    // The agent mounts it as a single file at the exact path (subPath = the CM key).
    let agent = &spec.containers[0];
    let m = agent
        .volume_mounts
        .as_ref()
        .unwrap()
        .iter()
        .find(|m| m.name == "cfg-1")
        .expect("the configmap mount");
    assert_eq!(m.mount_path, "/acp-config/config.toml");
    assert_eq!(m.sub_path.as_deref(), Some("content"));
    assert_eq!(m.read_only, Some(true));
}

#[test]
fn native_credential_is_seeded_from_a_secret_into_a_writable_file() {
    let credential = br#"{"tokens":{"refresh_token":"never-log-me"}}"#.to_vec(); // awaken-allow: secret -- synthetic test fixture
    let mut plan = plan_with_memory(Vec::new());
    plan.binds = vec![crate::BindPlan {
        source_ref: "credential://acp/native/codex".into(),
        mount_path: "/acp-config".into(),
        read_only: false,
        content: None,
        content_bytes: None,
        secret_content: Some(crate::SecretBytes::new(credential.clone())),
        secret_writeback: true,
        credential_file_path: Some("/acp-config/auth.json".into()),
    }];

    let pod = build_pod("oauth", &plan, &None, None, &[]);
    let spec = pod.spec.unwrap();
    let init = spec
        .init_containers
        .as_ref()
        .and_then(|containers| containers.first())
        .expect("credential init container");
    assert_eq!(init.name, "credential-init-0");
    assert!(
        init.command
            .as_ref()
            .is_some_and(|command| command.iter().any(|part| part.contains("chmod 600")))
    );
    let agent = spec
        .containers
        .iter()
        .find(|container| container.name == "agent")
        .unwrap();
    let auth = agent
        .volume_mounts
        .as_ref()
        .unwrap()
        .iter()
        .find(|mount| mount.mount_path == "/acp-config")
        .unwrap();
    assert_eq!(auth.read_only, Some(false));
    assert_eq!(auth.sub_path, None);
    assert!(
        spec.volumes
            .as_ref()
            .unwrap()
            .iter()
            .any(|v| v.secret.is_some())
    );

    let secret = build_credential_secret("oauth", 0, "auth.json", &credential, &None);
    assert_eq!(
        secret
            .data
            .as_ref()
            .and_then(|data| data.get("auth.json"))
            .map(|bytes| bytes.0.as_slice()),
        Some(credential.as_slice())
    );
    assert!(!format!("{plan:?}").contains("never-log-me"));
}

#[test]
fn readonly_secret_is_projected_as_the_exact_file_not_a_directory() {
    let mut plan = plan_with_memory(Vec::new());
    plan.binds = vec![crate::BindPlan {
        source_ref: "credential://github".into(),
        mount_path: "/run/secrets/awaken-git-credential-0".into(),
        read_only: true,
        content: None,
        content_bytes: None,
        secret_content: Some(crate::SecretBytes::new(b"synthetic-token".to_vec())),
        secret_writeback: false,
        credential_file_path: None,
    }];

    let spec = build_pod("git", &plan, &None, None, &[]).spec.unwrap();
    assert!(spec.init_containers.is_none());
    let secret = spec
        .volumes
        .as_ref()
        .unwrap()
        .iter()
        .find_map(|volume| volume.secret.as_ref())
        .expect("read-only credential Secret volume");
    assert_eq!(secret.default_mode, Some(0o440));
    let mount = spec.containers[0]
        .volume_mounts
        .as_ref()
        .unwrap()
        .iter()
        .find(|mount| mount.mount_path == "/run/secrets/awaken-git-credential-0")
        .expect("exact credential file mount");
    assert_eq!(mount.sub_path.as_deref(), Some(CONFIGMAP_KEY));
    assert_eq!(mount.read_only, Some(true));
}

#[test]
fn build_configmap_holds_the_bytes_under_the_key_and_is_immutable() {
    let cm = build_configmap("run-9", 0, Some("hello-inline"), None, &None);
    assert_eq!(cm.metadata.name.as_deref(), Some("awaken-run-9-cfg-0"));
    assert_eq!(
        cm.data.as_ref().unwrap().get("content").unwrap(),
        "hello-inline"
    );
    assert_eq!(cm.immutable, Some(true));
    // Labeled so `remove` reaps it by the Pod name (the container_id).
    assert_eq!(
        cm.metadata
            .labels
            .as_ref()
            .unwrap()
            .get("awaken-cfg-owner")
            .map(String::as_str),
        Some("awaken-run-9")
    );
}

#[test]
fn build_configmap_uses_binary_data_for_non_utf8_bytes() {
    // A binary File (non-UTF-8) rides `binaryData`, not text `data` — else the k8s API
    // rejects the invalid UTF-8. The same `content` key is projected by the volume subPath.
    let cm = build_configmap("run-9", 1, None, Some(&[0xff, 0xfe, 0x00, 0x01]), &None);
    assert!(cm.data.is_none(), "binary content must not ride text data");
    assert_eq!(
        cm.binary_data.as_ref().unwrap().get("content").unwrap().0,
        vec![0xff, 0xfe, 0x00, 0x01]
    );
    assert_eq!(cm.immutable, Some(true));
}

#[test]
fn build_pod_carries_only_non_secret_base_environment() {
    // ContainerPlan is the environment-container creation layer and carries
    // public base env only. Process credentials are materialized for exec later;
    // the Kubernetes adapter currently rejects that operation because the exec
    // API would otherwise expose the value in argv.
    let mut plan = plan_with_memory(Vec::new());
    plan.env = vec![
        ("AWAKEN_ACP_GATEWAY_URL".into(), "http://gw.internal".into()),
        ("HTTPS_PROXY".into(), "http://gw.internal:8888".into()),
    ];
    let spec = build_pod("r", &plan, &None, None, &[]).spec.unwrap();
    let env = spec.containers[0].env.clone().unwrap();
    assert!(env.iter().any(|e| e.name == "AWAKEN_ACP_GATEWAY_URL"));
    assert!(
        env.iter()
            .all(|e| e.name != "ANTHROPIC_API_KEY" && e.name != "AWAKEN_ACP_LEASE_TOKEN")
    );
}

#[test]
fn build_pod_without_memory_mounts_still_isolates_the_input_projector() {
    let pod = build_pod("r", &plan_with_memory(Vec::new()), &None, None, &[]);
    let spec = pod.spec.unwrap();
    // No memoryd sidecar; only the Agent and its runtime-owned input projector.
    // The Agent gets three writable-rootfs emptyDirs plus one read-only input tree.
    assert_eq!(spec.containers.len(), 2);
    assert_eq!(spec.containers[1].name, live_inputs::PROJECTOR);
    assert_eq!(spec.volumes.as_ref().unwrap().len(), 4);
    assert_eq!(spec.containers[0].volume_mounts.as_ref().unwrap().len(), 4);
}

#[test]
fn build_pod_hardens_the_untrusted_agent() {
    let spec = build_pod("r", &plan_with_memory(Vec::new()), &None, None, &[])
        .spec
        .unwrap();
    // No SA token → the agent cannot reach the kube API.
    assert_eq!(spec.automount_service_account_token, Some(false));
    let sc = spec.containers[0].security_context.as_ref().unwrap();
    assert_eq!(sc.allow_privilege_escalation, Some(false));
    assert_eq!(sc.read_only_root_filesystem, Some(true));
    assert_eq!(
        sc.capabilities.as_ref().unwrap().drop.as_deref(),
        Some(&["ALL".to_string()][..])
    );
    // The writable app paths (outputs + /tmp) are backed by emptyDir mounts so a
    // read-only rootfs doesn't break the agent's writes.
    let mounts = spec.containers[0].volume_mounts.as_ref().unwrap();
    let paths: Vec<&str> = mounts.iter().map(|m| m.mount_path.as_str()).collect();
    assert!(paths.contains(&"/mnt/session/outputs"));
    assert!(paths.contains(&"/tmp"));
}

#[test]
fn build_pod_injects_the_reverse_dial_rendezvous() {
    let plan = plan_with_memory(Vec::new());
    // Without a rendezvous, no such env.
    let no_rv = build_pod("r", &plan, &None, None, &[]);
    let env0 = no_rv.spec.unwrap().containers[0].env.clone().unwrap();
    assert!(env0.iter().all(|e| e.name != "AWAKEN_ACP_RENDEZVOUS"));
    // With one, the agent is told where to dial out.
    let with_rv = build_pod("r", &plan, &None, Some("10.0.0.5:9000"), &[]);
    let env1 = with_rv.spec.unwrap().containers[0].env.clone().unwrap();
    assert!(
        env1.iter()
            .any(|e| e.name == "AWAKEN_ACP_RENDEZVOUS"
                && e.value.as_deref() == Some("10.0.0.5:9000"))
    );
}

#[tokio::test]
async fn accept_reverse_receives_the_pods_outbound_dial() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Cause/effect rule: C1 an ephemeral listener stays bound under one owner;
    // C2 a stand-in Pod dials its exact resolved address. R1 C1+C2 accepts the
    // bytes on that same listener without a drop/rebind port-allocation race.
    let listener = crate::net::ReverseListen::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();

    let host = tokio::spawn(async move { accept_reverse_on(listener).await });
    // Give the host a moment to bind, then dial like an egress-fenced pod would.
    let mut pod = loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => break s,
            Err(_) => tokio::task::yield_now().await,
        }
    };
    pod.write_all(b"ping\n").await.unwrap();
    pod.flush().await.unwrap();

    let mut chan = host.await.unwrap().unwrap();
    let mut buf = [0u8; 5];
    chan.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping\n");
}

#[test]
fn build_pod_labels_the_egress_posture_for_a_networkpolicy() {
    // Restricted intent is labeled for platform observation, while provider
    // capability remains false until an installed policy is verified.
    let mut plan = plan_with_memory(Vec::new());
    plan.network = crate::NetworkMode::None;
    let pod = build_pod("r", &plan, &None, None, &[]);
    let labels = pod.metadata.labels.unwrap();
    assert_eq!(
        labels.get("awaken-egress").map(String::as_str),
        Some("restricted")
    );

    plan.network = crate::NetworkMode::Open;
    let open = build_pod("r", &plan, &None, None, &[]);
    assert_eq!(
        open.metadata
            .labels
            .unwrap()
            .get("awaken-egress")
            .map(String::as_str),
        Some("open")
    );

    // No-network policy is also `restricted`, never `open` — a fail-open label
    // here would let a NetworkPolicy grant egress to a pod that asked for none.
    plan.network = crate::NetworkMode::None;
    let denied = build_pod("r", &plan, &None, None, &[]);
    assert_eq!(
        denied
            .metadata
            .labels
            .unwrap()
            .get("awaken-egress")
            .map(String::as_str),
        Some("restricted")
    );
}
