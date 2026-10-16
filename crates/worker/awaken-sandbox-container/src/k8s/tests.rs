// @formal evidence session_execution.one_physical_attempt_per_thread
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
    let current = crate::ContainerContinuationHandle::KubernetesContinuationV2 {
        pod_uid: "pod-a".into(),
        claim_uid: Some("claim-a".into()),
    };
    assert_eq!(
        runtime_contract::deletion_incarnation(Some(&current)).unwrap(),
        (Some("pod-a"), Some("claim-a")),
        "KH1a/E1",
    );
    let legacy = crate::ContainerContinuationHandle::KubernetesContinuation {
        claim_uid: "claim-a".into(),
    };
    assert!(
        runtime_contract::deletion_incarnation(Some(&legacy)).is_err(),
        "KH1b/E2",
    );

    let host_bind = crate::ContainerContinuationHandle::HostBindRestoration(
        crate::HostBindRestorationHandle::for_restore(
            std::env::temp_dir()
                .join("awaken-acp-stage-k8s-reject")
                .to_string_lossy()
                .into_owned(),
        )
        .unwrap(),
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
            purpose: crate::ContainerObservationPurpose::Recovery,
            retained_attempt: None,
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
    /* Terminal-purpose Pod/PVC observation table KCO1. Causes: C1 exact source Pod A is
     * present/absent; C2 A is live/deleting; C3 Awaken's cleanup finalizer is
     * held; C4 durable V2 expects claim P and observes P live/terminating/
     * absent/foreign; C5 the aggregate terminal fence is present; C6 the
     * total-absence proof carries exact fenced V2 claim evidence. Effects:
     * E1 ordinary A+P remains Live; E2 own finalizer proves every required
     * source-durability effect preceded physical cleanup and projects Disposing, including the PVC
     * DELETE response-loss cut before A DELETE; E3 deleting A without the
     * finalizer projects Disposing only when exact P is already terminating
     * (finalizer-release response loss); E4 absent A+terminating P projects
     * Disposing so cleanup only awaits P; E5 an aggregate-fenced absent A+live
     * P raises the canonical unsafe-cleanup error; E6 a foreign P,
     * deleting-without-proof, or missing fence fails closed; E7 exact A/P total
     * absence is Live here and becomes DefinitivelyUnavailable only in the
     * shared physical-observation owner.
     *
     * | Rule | A | deleting | own F | P | fence | total proof | Effect |
     * | K1 | present | no  | no  | exact live | yes/no | no | E1 Live |
     * | K2 | present | no/yes | yes | exact live/term | yes | no | E2 Disposing |
     * | K3 | present | yes | no | exact term | yes | no | E3 Disposing |
     * | K4 | absent | n/a | no | exact term | yes | no | E4 Disposing |
     * | K5 | absent | n/a | no | exact live | yes | no | E5 error |
     * | K6 | present | yes | no | exact live | yes | no | E6 Indeterminate |
     * | K7 | any | any | any | foreign | yes | any | E6 Incompatible |
     * | K8 | absent | n/a | no | absent | yes | yes | E7 Live |
     * | K9 | absent | n/a | no | absent | no | no | E6 Incompatible | */
    use ContinuationObservationDisposition::{Disposing, Incompatible, Live};

    let classify = |pod_present,
                    pod_deleting,
                    cleanup_gate,
                    observed_claim_uid,
                    claim_terminating,
                    effect_fenced,
                    fenced_total_absence| {
        let evidence = ContinuationObservationEvidence {
            purpose: ContainerObservationPurpose::TerminalCleanup,
            pod_present,
            pod_deleting,
            cleanup_gate_held: cleanup_gate,
            original_terminal_guard: false,
            expected_uid: Some("p"),
            observed_uid: observed_claim_uid,
            observed_terminating: claim_terminating,
            effect_fenced,
            effect_fenced_total_absence: fenced_total_absence,
        };
        let terminal = continuation_observation_disposition(evidence);
        if pod_present {
            // Purpose cannot bypass a present source's cleanup gate, deleting
            // state, or UID mismatch. Cross every present-Pod KCO1 rule with
            // Recovery as a metamorphic identity; KCO2 owns absent-Pod rows.
            let recovery = continuation_observation_disposition(ContinuationObservationEvidence {
                purpose: ContainerObservationPurpose::Recovery,
                ..evidence
            });
            assert_eq!(
                recovery.map_err(|error| error.to_string()),
                terminal
                    .as_ref()
                    .copied()
                    .map_err(|error| error.to_string())
            );
        }
        terminal
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
    let orphaned = classify(false, false, false, Some("p"), false, true, false)
        .expect_err("K5 exact live orphan must reject cleanup");
    assert!(
        orphaned
            .to_string()
            .contains(continuation::ORPHANED_LIVE_CLAIM_ERROR),
        "K5: {orphaned}"
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
fn absent_pod_observation_separates_recovery_from_terminal_absence() {
    /* KCO2 cause-effect graph: purpose + exact retained UID + live/terminating
     * claim + effect fence -> one continuation disposition. Constraint: Pod is
     * absent; expected claim P is durable. No API mutations occur here.
     * Decision table (cross both purposes and both fence values):
     * R1 exact live P + Recovery => Live (shared observer then reports A absent);
     * R2 exact live P + TerminalCleanup => error if fenced, else Incompatible;
     * R3 exact terminating P => Disposing only if fenced, else Incompatible;
     * R4 foreign P => Incompatible, never recovery or completed cleanup;
     * R5 absent P => Live only with exact fenced total-absence evidence.
     * KPV5 attaches these rows to real provider observation, preparation,
     * disposal and Rebuild; the K3d node-loss scenario checks the Host/Worker
     * path and retained bytes. A pure classifier pass is not that I/O proof. */
    use ContainerObservationPurpose::{Recovery, TerminalCleanup};
    use ContinuationObservationDisposition::{Disposing, Incompatible, Live};
    for purpose in [Recovery, TerminalCleanup] {
        for fenced in [false, true] {
            for terminating in [false, true] {
                for observed_uid in [Some("p"), Some("foreign"), None] {
                    for total_absence in [false, true] {
                        let result =
                            continuation_observation_disposition(ContinuationObservationEvidence {
                                purpose,
                                pod_present: false,
                                pod_deleting: false,
                                cleanup_gate_held: false,
                                original_terminal_guard: false,
                                expected_uid: Some("p"),
                                observed_uid,
                                observed_terminating: terminating,
                                effect_fenced: fenced,
                                effect_fenced_total_absence: fenced && total_absence,
                            });
                        let expected = match observed_uid {
                            Some("foreign") => Some(Incompatible),
                            None if fenced && total_absence => Some(Live),
                            None => Some(Incompatible),
                            Some(_) if !terminating && purpose == Recovery => Some(Live),
                            Some(_) if terminating && fenced => Some(Disposing),
                            Some(_) if !fenced => Some(Incompatible),
                            Some(_) => None,
                        };
                        assert_eq!(
                            result.ok(),
                            expected,
                            "purpose={purpose:?} fenced={fenced} term={terminating} uid={observed_uid:?} total={total_absence}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn continuation_claim_api_is_required_only_by_an_owned_authority() {
    /* Claim-observation capability table. Causes: C1 continuation provisioning
     * is configured; C2 a durable runtime handle expects an exact claim UID.
     * Effects: E1 query the PVC API so allocation/recovery can fence exact
     * physical continuity; E2 perform no PVC API call, preserving least
     * privilege for ephemeral deployments. Rules: A1 !C1+!C2=>E2;
     * A2 C1+!C2=>E1 (detect configured-policy orphans); A3 !C1+C2=>E1
     * (finish recovery after policy removal); A4 C1+C2=>E1. A Pod that binds a
     * claim without C2 is rejected by the preceding immutable-binding check and
     * therefore never widens this capability rule. */
    assert!(!continuation_claim_observation_required(false, false), "A1");
    assert!(continuation_claim_observation_required(true, false), "A2");
    assert!(continuation_claim_observation_required(false, true), "A3");
    assert!(continuation_claim_observation_required(true, true), "A4");
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
        (
            SANDBOX_EFFECT_ANNOTATION.into(),
            fence.operation_id().to_owned(),
        ),
        (
            SANDBOX_EFFECT_OWNER_ANNOTATION.into(),
            fence.owner().to_owned(),
        ),
        (
            SANDBOX_EFFECT_RUNTIME_ANNOTATION.into(),
            fence.runtime_incarnation().to_owned(),
        ),
        (
            SANDBOX_EFFECT_EPOCH_ANNOTATION.into(),
            fence.epoch().to_string(),
        ),
        (
            SANDBOX_EFFECT_EXPIRY_ANNOTATION.into(),
            fence.expires_at_unix_ms().to_string(),
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
    // V1 historical emptyDir -> enabled PVC reproduces the independently
    // identified Classroom drift. V2 size/class changes must remain distinct;
    // V3 replay of each exact policy is stable. Only continuation_volume may
    // differ: changing several knobs together would mask an omitted field.
    // This proves configuration identity, NOT terminal deletion authority.
    let baseline = base.realization_configuration().unwrap();
    assert_eq!(baseline["continuation_volume"], "none");
    let mut identities = std::collections::BTreeSet::new();
    for (class, size) in [
        (Some("local-path"), "1Gi"),
        (Some("local-path"), "2Gi"),
        (None, "1Gi"),
    ] {
        let configuration = || {
            K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap())
                .with_continuation_volume(crate::K8sContinuationVolume {
                    storage_class_name: class.map(str::to_owned),
                    size: size.into(),
                })
                .realization_configuration()
                .unwrap()
        };
        let mut current = configuration();
        assert_eq!(current, configuration(), "V3 exact replay");
        let volume = current.remove("continuation_volume").unwrap();
        assert!(identities.insert(volume), "V1/V2 distinct volume policy");
        current.insert("continuation_volume".into(), "none".into());
        assert_eq!(current, baseline, "V1/V2 no unrelated configuration drift");
    }
}

#[tokio::test]
async fn historical_terminal_configuration_requires_explicit_no_claim_evidence() {
    // HC4 exact V2 Pod identity + explicitly absent claim => historical none;
    // HC5 legacy/missing/blank identity or retained PVC => no guessed history.
    // The returned candidate changes only storage policy. Backend observation
    // must still match actual Pod/PVC UIDs and fingerprints before any effect.
    let runtime = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap()).with_continuation_volume(
        crate::K8sContinuationVolume {
            storage_class_name: Some("local-path".into()),
            size: "1Gi".into(),
        },
    );
    let handle = crate::ContainerContinuationHandle::KubernetesContinuationV2 {
        pod_uid: "exact-pod".into(),
        claim_uid: None,
    };
    let mut expected = runtime.realization_configuration().unwrap();
    expected.insert("continuation_volume".into(), "none".into());
    assert_eq!(
        runtime
            .historical_terminal_configuration(Some(&handle))
            .unwrap(),
        Some(expected)
    );
    for invalid in [
        None,
        Some(
            crate::ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid: "".into(),
                claim_uid: None,
            },
        ),
        Some(
            crate::ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid: "exact-pod".into(),
                claim_uid: Some("claim".into()),
            },
        ),
        Some(crate::ContainerContinuationHandle::KubernetesContinuation {
            claim_uid: "claim".into(),
        }),
    ] {
        assert!(
            runtime
                .historical_terminal_configuration(invalid.as_ref())
                .unwrap()
                .is_none()
        );
    }
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
    install_rustls_crypto_provider();
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
    let packages = awaken_environment_contract::PackageRequirements {
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

    async fn write_back(
        &self,
        _target: pc::SecretWritebackTarget<'_>,
        _bytes: Vec<u8>,
    ) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::unsupported("not supported"))
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
            outcome: None,
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

#[tokio::test]
async fn attachment_lifetime_preserves_status_and_never_fabricates_missing_exit() {
    // C1 pending observer; C2 status is success/failure/absent; C3 task is
    // normally completed or cancelled. E1 C1 retains the attachment whose
    // stdin Drop closes a v4 WebSocket; E2 C2 returns exactly that status,
    // including None; E3 cancellation releases the attachment but supplies no
    // exit receipt. Rules A1=C1=>E1; A2=C1+C2=>E1+E2; A3=C1+C3=>E3.
    // A drop-before-await mutant closes the sender and fails A1/A2.
    for status in [Some(success_status(0)), Some(success_status(7)), None] {
        let (released, mut observed_release) = tokio::sync::oneshot::channel::<()>();
        let (supply, observe) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(super::process::observe_attached_completion(
            released,
            async { observe.await.unwrap() },
        ));
        tokio::task::yield_now().await;
        assert!(
            matches!(
                observed_release.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "A1"
        );
        supply.send(status.clone()).unwrap();
        assert_eq!(task.await.unwrap(), status, "A2");
        assert!(
            observed_release.await.is_err(),
            "A2 owner released after observation"
        );
    }
    let (released, observed_release) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(super::process::observe_attached_completion(
        released,
        std::future::pending(),
    ));
    tokio::task::yield_now().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled(), "A3");
    assert!(observed_release.await.is_err(), "A3");
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

#[tokio::test(start_paused = true)]
async fn exec_cancelled_wait_retains_the_exact_remote_completion() {
    // Cause/effect graph: C1 remote status is pending; C2 caller polls wait once
    // then drops it (Supervisor deadline); C3 that same remote exec later ends.
    // W1 C1+C2 => no cached exit, retain the sole completion owner; W2 +C3 =>
    // next wait and poll return the exact exit repeatedly. A timeout is neither
    // process termination nor permission to discard its eventual status.
    for code in [0, 7] {
        let (finish, finished) = tokio::sync::oneshot::channel();
        let process = exec_process(Some(tokio::spawn(async move {
            finished.await.unwrap();
            Some(success_status(code))
        })));
        let mut waiting = Box::pin(process.wait());
        std::future::poll_fn(|cx| {
            assert!(
                waiting.as_mut().poll(cx).is_pending(),
                "W1 actual wait entered"
            );
            std::task::Poll::Ready(())
        })
        .await;
        drop(waiting);
        assert_eq!(
            process.poll().await.unwrap(),
            None,
            "W1 cancellation keeps pending status"
        );
        finish.send(()).unwrap();
        assert_eq!(
            process.wait().await.unwrap().code,
            Some(code),
            "W2 exact exit survives cancelled observer"
        );
        assert_eq!(
            process.poll().await.unwrap().unwrap().code,
            Some(code),
            "W2 repeat observation"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn exec_pending_wait_does_not_block_poll_or_another_waiter() {
    // W3 C1 one waiter is pending + C2 another observer polls => immediate None,
    // not the first waiter's lifetime lock. W4 producer completes => both waiters
    // return the same cached exit. Explicit first polling fixes the interleaving;
    // the virtual deadline detects a lock inversion without wall-clock guesses.
    let (finish, finished) = tokio::sync::oneshot::channel();
    let process = exec_process(Some(tokio::spawn(async move {
        finished.await.unwrap();
        Some(success_status(0))
    })));
    let mut first = Box::pin(process.wait());
    std::future::poll_fn(|cx| {
        assert!(
            first.as_mut().poll(cx).is_pending(),
            "W3 first waiter entered"
        );
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), process.poll())
            .await
            .expect("W3 poll must not wait for process exit")
            .unwrap(),
        None
    );
    let mut second = Box::pin(process.wait());
    std::future::poll_fn(|cx| {
        assert!(
            second.as_mut().poll(cx).is_pending(),
            "W4 second waiter entered"
        );
        std::task::Poll::Ready(())
    })
    .await;
    finish.send(()).unwrap();
    let (a, b) = tokio::join!(first, second);
    assert_eq!(a.unwrap().code, Some(0));
    assert_eq!(b.unwrap().code, Some(0));
}

#[tokio::test(start_paused = true)]
async fn exec_poll_distinguishes_running_missing_and_finished_status() {
    // X1 pending -> None then exact successful status; X2 missing observer ->
    // error; X3 transport ends without a Status frame -> error, with no
    // invented code 0. KPV6's external fault must not weaken X3 for a real
    // Session-owned exec whose status was lost.
    for code in [0, 7] {
        let (finish, finished) = tokio::sync::oneshot::channel();
        let (observe, observed) = tokio::sync::oneshot::channel();
        let completion = tokio::spawn(async move {
            finished.await.expect("X1 remote completion released");
            observe.send(()).expect("X1 completion witness retained");
            Some(success_status(code))
        });
        let completion_state = completion.abort_handle();
        let process = exec_process(Some(completion));
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), process.poll())
                .await
                .expect("X1 pending poll must not wait for completion")
                .unwrap(),
            None,
            "X1 held completion has no exit status"
        );
        finish.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), observed)
            .await
            .expect("X1 producer must complete")
            .expect("X1 producer reported completion");
        // The current-thread runtime returns the producer before the awakened
        // witness runs. Check the actual JoinHandle state before terminal poll;
        // waiting through ProcessHandle would cache the result and hide this path.
        assert!(completion_state.is_finished(), "X1 observer returned");
        assert_eq!(process.poll().await.unwrap().unwrap().code, Some(code));
        assert_eq!(
            process.poll().await.unwrap().unwrap().code,
            Some(code),
            "X1 terminal poll retains the exact status"
        );
    }

    let missing = exec_process(None);
    assert!(missing.poll().await.is_err());
    assert!(missing.wait().await.is_err());
    assert_eq!(k8s_exit_status(None).code, None);
    let no_status_frame = exec_process(Some(tokio::spawn(async { None })));
    assert!(
        no_status_frame.wait().await.is_err(),
        "X3 no synthetic exit"
    );
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
    // The enumerable directory, not a bare `/tmp` name: the reaper finds an
    // attempt by listing this path, so the wrapper must write into it.
    assert_eq!(inline.pid_file, "/tmp/awaken-exec/inline.pid");
    assert!(
        inline
            .argv
            .iter()
            .any(|value| value.contains("mkdir -p /tmp/awaken-exec")),
        "the wrapper creates the directory it writes its record into"
    );
    assert!(inline.argv.iter().any(|value| value == "MODE=test"));
    assert!(inline.argv.iter().any(|value| value == "/workspace"));
    assert!(inline.secret_stdin.is_empty());

    let rt = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
    let mut piped = pc::MaterializedCommand::new(["echo", "value"]);
    piped.stdio = pc::Stdio::Piped;
    assert!(
        rt.spawn(crate::AdmittedExec::unrecorded("pod"), piped)
            .await
            .is_err()
    );
    assert!(
        rt.spawn_agent(
            crate::AdmittedExec::unrecorded("pod"),
            pc::MaterializedCommand::new(Vec::<String>::new())
        )
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

pub(super) fn plan_with_memory(mounts: Vec<crate::MemoryMount>) -> ContainerPlan {
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
fn repository_gateway_trust_is_exact_pod_transport_not_a_second_credential() {
    // Causes: C1=private trust absent/present; C2=proxy absent/present;
    // C3=host/Secret config valid/invalid. Effects: E1=default Pod unchanged;
    // E2=public CA read-only, exact host alias, Git TLS trust, proxy bypass only
    // for the Gateway; E3=invalid config rejected before Pod creation.
    // Rules: T1(0,any,valid)->E1; T2(1,1,valid)->E2;
    // T3(1,0,valid)->E2 without NO_PROXY; T4(1,any,invalid)->E3.
    let mut plan = plan_with_memory(Vec::new());
    let plain = build_pod("plain", &plan, &None, None, &[]);
    let plain_spec = plain.spec.unwrap();
    assert!(plain_spec.host_aliases.is_none(), "T1");
    assert!(
        !plain_spec
            .volumes
            .unwrap_or_default()
            .iter()
            .any(|volume| { volume.name == "repository-gateway-trust" }),
        "T1"
    );

    let host = "gateway.awaken-full.svc";
    let trust = K8sRepositoryGatewayTrust::new(
        host,
        "10.1.2.3".parse().unwrap(),
        8444,
        "internal-ca",
        "ca.crt",
    )
    .unwrap();
    plan.env.extend([
        ("HTTPS_PROXY".into(), "http://10.1.2.3:8091".into()),
        ("NO_PROXY".into(), "localhost,127.0.0.1".into()),
    ]);
    let pod = build_pod_with_continuation(
        "mediated",
        &plan,
        &None,
        None,
        &[],
        None,
        None,
        None,
        Some(&trust),
    );
    let spec = pod.spec.unwrap();
    assert_eq!(
        spec.host_aliases.unwrap()[0].hostnames.as_ref().unwrap(),
        &[host],
        "T2"
    );
    let volume = spec
        .volumes
        .unwrap()
        .into_iter()
        .find(|volume| volume.name == "repository-gateway-trust")
        .unwrap();
    assert_eq!(
        volume.secret.unwrap().secret_name.as_deref(),
        Some("internal-ca"),
        "T2"
    );
    let agent = &spec.containers[0];
    assert!(
        agent.volume_mounts.as_ref().unwrap().iter().any(|mount| {
            mount.mount_path == "/var/run/awaken/repository-trust" && mount.read_only == Some(true)
        }),
        "T2"
    );
    let env = agent.env.as_ref().unwrap();
    assert!(
        !env.iter().any(|variable| variable.name == "GIT_SSL_CAINFO"),
        "T2 only Repository clone/push processes select the private CA"
    );
    assert!(
        env.iter().any(|variable| variable.name == "NO_PROXY"
            && variable.value.as_deref() == Some("localhost,127.0.0.1,gateway.awaken-full.svc")),
        "T2"
    );

    plan.env.retain(|(name, _)| name != "HTTPS_PROXY");
    let direct = build_pod_with_continuation(
        "direct",
        &plan,
        &None,
        None,
        &[],
        None,
        None,
        None,
        Some(&trust),
    );
    assert!(
        direct.spec.unwrap().containers[0]
            .env
            .as_ref()
            .unwrap()
            .iter()
            .any(|variable| variable.name == "NO_PROXY"
                && variable.value.as_deref() == Some("localhost,127.0.0.1")),
        "T3"
    );
    assert!(
        K8sRepositoryGatewayTrust::new(
            "gateway.invalid/path",
            "10.1.2.3".parse().unwrap(),
            8444,
            "internal-ca",
            "ca.crt"
        )
        .is_err(),
        "T4"
    );
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
        &projection::gated_agent_command(&plan.command, &effect_fence),
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
        2,
        "the Memory projector owns only its two writable snapshots"
    );
    assert_eq!(spec.containers[2].name, live_inputs::PROJECTOR);
    assert_eq!(
        spec.containers[2].volume_mounts.as_ref().unwrap().len(),
        2,
        "the input projector owns Files and the shared completion marker"
    );
    assert_eq!(
        spec.containers[1]
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|env| env.name == projection::PROJECTION_FENCE_ENV)
            .and_then(|env| env.value.as_deref()),
        Some(projection::projection_fence_value(&effect_fence).as_str()),
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
    let credential = br#"{"tokens":{"refresh_token":"never-log-me"}}"#.to_vec(); // awaken-allow(secret): -- synthetic test fixture
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
    // K1 all agent/helper containers share the non-root identity, dropped
    // capabilities, no privilege escalation and RuntimeDefault seccomp.
    // K2 service links and SA token are absent; K3 writable volume mounts
    // remain present and the volume group matches that execution identity.
    let spec = build_pod("r", &plan_with_memory(Vec::new()), &None, None, &[])
        .spec
        .unwrap();
    // No SA token → the agent cannot reach the kube API.
    assert_eq!(spec.automount_service_account_token, Some(false));
    assert_eq!(spec.enable_service_links, Some(false));
    assert_eq!(
        spec.security_context.as_ref().unwrap().fs_group,
        Some(crate::runtime::SANDBOX_EXECUTION_ID)
    );
    for container in spec
        .containers
        .iter()
        .chain(spec.init_containers.iter().flatten())
    {
        let context = container.security_context.as_ref().unwrap();
        assert_eq!(context.run_as_non_root, Some(true), "K1 {}", container.name);
        assert_eq!(
            context.run_as_user,
            Some(crate::runtime::SANDBOX_EXECUTION_ID)
        );
        assert_eq!(
            context.run_as_group,
            Some(crate::runtime::SANDBOX_EXECUTION_ID)
        );
        assert_eq!(
            context.seccomp_profile.as_ref().unwrap().type_,
            "RuntimeDefault"
        );
        assert_eq!(context.allow_privilege_escalation, Some(false));
    }
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
    // C3 Pod half-close -> EOF on the canonical accepted channel while its
    // response still reaches the Pod. This exercises the migrated adapter,
    // not just the shared transport in isolation.
    let listener = awaken_connection_plan::bind_tcp(&ConnectionPlan::tcp_listen("127.0.0.1:0"))
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
    pod.shutdown().await.unwrap();

    let mut chan = host.await.unwrap().unwrap();
    let mut buf = [0u8; 5];
    chan.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping\n");
    assert_eq!(chan.read(&mut buf).await.unwrap(), 0);
    chan.write_all(b"pong\n").await.unwrap();
    chan.shutdown().await.unwrap();
    pod.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"pong\n");
    assert_eq!(pod.read(&mut buf).await.unwrap(), 0);
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

#[tokio::test]
async fn remote_completion_requires_explicit_process_exit_evidence() {
    // D10 transport closes / drops its status sender / sends an unrecognized
    // status => poll and wait fail without caching a fabricated exit code 1.
    // D11 a Kubernetes Success or explicit numeric ExitCode => exact stable exit.
    // Stream EOF is not physical quiescence; only a received exit can permit ACK.
    for status in [
        None,
        Some(Status::default()),
        Some(Status {
            status: Some("Failure".into()),
            ..Default::default()
        }),
    ] {
        let process = exec_process(Some(tokio::spawn(async move { status })));
        let first = process.wait().await.unwrap_err().to_string();
        let repeated = process.poll().await.unwrap_err().to_string();
        assert_eq!(first, repeated, "D10 repeat retains the original diagnosis");
        assert!(
            process
                .state
                .lock()
                .await
                .outcome
                .as_ref()
                .unwrap()
                .is_err(),
            "D10 observation failure is not cached as an exit"
        );
    }
    for code in [0, 7, 137] {
        let process = exec_process(Some(tokio::spawn(
            async move { Some(success_status(code)) },
        )));
        assert_eq!(process.wait().await.unwrap().code, Some(code), "D11");
        assert_eq!(
            process.poll().await.unwrap().unwrap().code,
            Some(code),
            "D11 stable exact exit"
        );
    }
}

#[tokio::test]
async fn failed_exec_observation_retains_safe_diagnostics_without_claiming_exit() {
    // Cause/effect graph: C1 no exit receipt; C2 API failure frame / cancelled
    // observer / panicked observer; C3 repeated poll or wait. R1 C1+C2 -> error,
    // never exit/stop authority; R2 C1+C2+C3 -> identical cached diagnosis. R3 an
    // API code is retained but arbitrary provider message/reason and panic text
    // are excluded. D11 above is the positive exact-exit control.
    let task = tokio::spawn(async {
        Some(Status {
            status: Some("Failure".into()),
            code: Some(502),
            reason: Some("fixture-sensitive-reason".into()),
            message: Some("fixture-sensitive-message".into()),
            ..Default::default()
        })
    });
    let cancelled = tokio::spawn(std::future::pending::<Option<Status>>());
    cancelled.abort();
    let panicked = tokio::spawn(async { panic!("fixture-sensitive-panic") });
    for (completion, expected) in [
        (task, "api_code=Some(502)"),
        (cancelled, "observer was cancelled"),
        (panicked, "observer failed"),
    ] {
        let process = exec_process(Some(completion));
        let first = process.wait().await.unwrap_err().to_string();
        assert!(first.contains(expected), "R1/R3 {first}");
        assert!(!first.contains("fixture-sensitive"), "R3 no arbitrary text");
        assert_eq!(
            process.poll().await.unwrap_err().to_string(),
            first,
            "R2 poll"
        );
        assert_eq!(
            process.wait().await.unwrap_err().to_string(),
            first,
            "R2 wait"
        );
        assert!(
            process
                .state
                .lock()
                .await
                .outcome
                .as_ref()
                .unwrap()
                .is_err(),
            "R1 no exit"
        );
    }
}

#[cfg(unix)]
#[test]
fn null_secret_exec_observes_eof_without_closing_transport_stdin() {
    // S1 fixed-length secret prelude; S2 Null command attempts to read; S3
    // caller keeps stdin open to retain a v4 WebSocket. S1+S2+S3 => env installed,
    // command sees EOF and exits; no secret enters argv. Execute the actual
    // generated shell. Missing Null redirection fails the bounded child wait.
    use std::io::Write;
    let mut command = pc::MaterializedCommand::new([
        "sh",
        "-c",
        "IFS= read -r value && exit 9; printf '%s' \"$TOKEN\"",
    ]);
    command.stdio = pc::Stdio::Null;
    command.env.push(pc::MaterializedEnvVar {
        name: "TOKEN".into(),
        value: pc::MaterializedEnvValue::Secret(awaken_runtime_contract::RedactedString::new(
            "fixture",
        )),
    });
    let execution = k8s_exec_argv("null-secret-test", command).unwrap();
    let mut child = std::process::Command::new(&execution.argv[0])
        .args(&execution.argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"fixture").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("S2/S3 Null command waited for transport EOF");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    drop(stdin);
    assert!(output.status.success(), "S1-S3: {:?}", output.status);
    assert_eq!(output.stdout, b"fixture");
    let _ = std::fs::remove_file(execution.pid_file);
}

#[test]
fn retained_original_source_observation_never_grants_absence_or_execution() {
    // C1 exact/absent/foreign claim UID; C2 retained terminal original guard;
    // C3 current effect; C4 claim is live/terminating; C5 Recovery/TerminalCleanup.
    // R1 foreign/absent -> Incompatible. R2 exact terminating + current effect
    // retains the existing Disposing result. R3 exact live + terminal guard +
    // current TerminalCleanup -> RetainedOriginal, never absence/Disposing.
    // R4 every remaining deleting-source combination stays unresolved.
    for purpose in [
        ContainerObservationPurpose::Recovery,
        ContainerObservationPurpose::TerminalCleanup,
    ] {
        for observed in [None, Some("claim"), Some("foreign")] {
            for guard in [false, true] {
                for fenced in [false, true] {
                    for terminating in [false, true] {
                        let result =
                            continuation_observation_disposition(ContinuationObservationEvidence {
                                purpose,
                                pod_present: true,
                                pod_deleting: true,
                                cleanup_gate_held: false,
                                original_terminal_guard: guard,
                                expected_uid: Some("claim"),
                                observed_uid: observed,
                                observed_terminating: terminating,
                                effect_fenced: fenced,
                                effect_fenced_total_absence: false,
                            });
                        if observed != Some("claim") {
                            assert_eq!(
                                result.unwrap(),
                                ContinuationObservationDisposition::Incompatible
                            );
                        } else if terminating && fenced {
                            assert_eq!(
                                result.unwrap(),
                                ContinuationObservationDisposition::Disposing
                            );
                        } else if purpose == ContainerObservationPurpose::TerminalCleanup
                            && guard
                            && fenced
                        {
                            assert_eq!(
                                result.unwrap(),
                                ContinuationObservationDisposition::RetainedOriginal
                            );
                        } else {
                            assert!(result.is_err());
                        }
                    }
                }
            }
        }
    }
}
