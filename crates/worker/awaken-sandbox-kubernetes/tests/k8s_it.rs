// @formal evidence sandbox_continuation.distinct_mutable_roots_share_one_retained_claim_without_aliasing
//! Real Kubernetes integration (ADR-0041 Slice 5), gated on a
//! reachable cluster (in-cluster SA or kubeconfig) — skips cleanly otherwise.
//!
//! Run with: `cargo test -p awaken-sandbox-kubernetes --test k8s_it`

// The container provider owns the exact restore fixture every engine uses.
use awaken_container_contract::{
    CreateTicket, CreateTicketRequest, RequestJoin, TicketAdmission, TicketResult,
};
use awaken_container_contract::{K8sCreateRequest, K8sCreateResponse};
#[path = "../../awaken-sandbox-container/tests/common/restore.rs"]
mod common;
mod k8s_common;

#[path = "k8s_common/finalizer_tests.rs"]
mod finalizer_tests;

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxProvider;
use awaken_sandbox_container::ContainerHandleCodec as _;

use awaken_container_contract::K8sContinuationVolume;
use awaken_sandbox_container::{
    ContainerPlan, ContainerProvider, ContainerRuntime, ContainerState, NetworkMode, RootfsPlan,
};
use awaken_sandbox_kubernetes::K8sRuntime;
use k8s_common::{
    container_id, control_image, disposal_authorization, effect_fence, fixture_image,
    spec_with_command,
};
use pc::test_support::EffectAuthority;

fn command(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|value| (*value).to_string()).collect()
}

fn only_bound_claim_name(pod: &k8s_openapi::api::core::v1::Pod) -> String {
    let claims = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .into_iter()
        .flatten()
        .filter_map(|volume| volume.persistent_volume_claim.as_ref())
        .map(|source| source.claim_name.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        claims.len(),
        1,
        "retained live fixture has exactly one API-observed PVC binding"
    );
    claims.into_iter().next().unwrap()
}

fn plan(cmd: &[&str]) -> ContainerPlan {
    ContainerPlan {
        // The live-suite script imports this exact image into the isolated k3d
        // node. One injected source of truth prevents a tag mismatch from
        // silently turning the test into an external-registry availability test.
        image: fixture_image(),
        command: cmd.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        control_services: Default::default(),
        packages: Default::default(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: NetworkMode::Open,
        egress_identity: Default::default(),
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        memory_mounts: Vec::new(),
        rootfs: RootfsPlan::HostUserland,
    }
}

async fn live_runtime() -> Option<K8sRuntime> {
    let required = std::env::var("AWAKEN_K8S_E2E").as_deref() == Ok("1");
    if !required {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 to require the live Kubernetes suite");
        return None;
    }
    let addr = "127.0.0.1:8080".parse().unwrap();
    // The other live Kubernetes suites already use this one namespace setting.
    // C1 absent -> the documented default; C2 explicit -> a policy-ready,
    // isolated test namespace. Both paths must still pass the runtime's
    // canonical NetworkPolicy admission before any Pod is created.
    let namespace = std::env::var("AWAKEN_K8S_NAMESPACE").unwrap_or_else(|_| "default".into());
    let runtime = match k8s_common::connect_with_clock(&namespace, addr).await {
        Ok(runtime) => runtime,
        Err(error) => panic!("AWAKEN_K8S_E2E=1 requires a kube client: {error}"),
    };
    match runtime.ping().await {
        // Cause: an exact fenced creation needs the same process clock that
        // production composition installs. Without it the physical result is
        // indeterminate, so no live lifecycle row actually reaches Kubernetes.
        Ok(()) => Some(runtime),
        Err(error) => panic!("AWAKEN_K8S_E2E=1 requires a reachable apiserver: {error}"),
    }
}

#[tokio::test]
async fn k8s_pod_lifecycle_against_a_real_cluster() {
    /* Managed lifecycle decision table K1. Causes: C1 an opaque Session scope
     * contains `_`/`:` and is unique to this live run; C2 the aggregate effect
     * and frozen spec are exact/different; C3 the apiserver supplies a current Pod
     * UID handle; C4 terminal cleanup carries a later authorized effect. Effects:
     * E1 the one namespace+scope identity becomes a bounded DNS-safe locator;
     * E2 exact replay returns the same locator and physical UID; E3 a changed
     * spec rejects without mutation; E4 disposal preparation leaves the Pod
     * intact; E5 physical cleanup deletes only the handle's Pod UID. Rules:
     * K1a C1+exact(C2)+C3=>E1+E2; K1b C1+different(C2)=>E3;
     * K1c C3+C4=>E4->E5. Exact hash bytes remain owned by the pure name projection;
     * this live test does not duplicate that algorithm or use name-only delete. */
    let scope = format!("sesn_fnv1a64:a13b83a56e2f77d0:{}", std::process::id());

    let Some(rt) = live_runtime().await else {
        return;
    };
    let runtime = Arc::new(rt);
    let provider = ContainerProvider::new(runtime.clone(), fixture_image())
        .with_clock(awaken_time::system_clock());
    let desired = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let create_fence = effect_fence(&scope, "create", "lifecycle-owner", 1);
    let first = SandboxProvider::claim_for_effect(
        &provider,
        &desired,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create_fence,
            authority: &EffectAuthority::new(&create_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("create a real Pod via the apiserver");
    let first_handle = first.handle();
    let id = container_id(&first_handle);
    assert!(id.starts_with("awaken-h-"), "K1a bounded opaque name: {id}");
    assert!(id.len() <= 63, "K1a DNS label length: {id}");

    let retried = SandboxProvider::claim_for_effect(
        &provider,
        &desired,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create_fence,
            authority: &EffectAuthority::new(&create_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("an identical retry must adopt the existing realization");
    let retried_handle = retried.handle();
    assert_eq!(container_id(&retried_handle), id, "K1a locator");
    assert_eq!(
        retried_handle.container_physical_incarnation().unwrap(),
        first_handle.container_physical_incarnation().unwrap(),
        "K1a physical UID"
    );
    let changed = spec_with_command(&scope, fixture_image(), command(&["sleep", "31"]));
    let mismatch = SandboxProvider::claim_for_effect(
        &provider,
        &changed,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create_fence,
            authority: &EffectAuthority::new(&create_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .err()
    .expect("a changed plan must not adopt a same-name Pod");
    assert!(
        mismatch.to_string().contains("different adoption evidence"),
        "K1b: {mismatch}"
    );

    assert!(matches!(
        runtime.inspect(&id).await.unwrap(),
        ContainerState::Running
    ));
    let terminal_fence = effect_fence(&scope, "terminal", "lifecycle-owner", 2);
    // K1d deployment enables PVC after this exact no-claim Pod was created.
    // Recovery must still reject drift. Only fenced terminal observation may
    // recover its historical configuration; preparation/disposal reuse the
    // original handle and existing participant/UID/fence validation. No Create
    // call is made on the replacement provider, so no replacement PVC is owned.
    let replacement_runtime = Arc::new(live_runtime().await.unwrap().with_continuation_volume(
        K8sContinuationVolume {
            storage_class_name: Some("local-path".into()),
            size: "1Gi".into(),
        },
    ));
    let replacement = ContainerProvider::new(replacement_runtime, fixture_image())
        .with_clock(awaken_time::system_clock());
    let mut observation =
        awaken_provisioning_contract::SandboxAdoption::new(&desired, &first_handle);
    assert!(
        matches!(
            replacement
                .observe(observation, observation.effect_basis(&terminal_fence))
                .await
                .unwrap(),
            pc::SandboxObservation::Incompatible { .. }
        ),
        "K1d ordinary recovery cannot consume historical policy"
    );
    observation.observation_purpose =
        awaken_provisioning_contract::SandboxObservationPurpose::TerminalCleanup;
    // K1e0 a V3 carrier with contradictory UID fields rejects at the codec.
    // K1e1 matching all carrier fields to a foreign UID still cannot authorize
    // that physical Pod: the real API observation must reject it. K1e2 changing
    // only the original-epoch owner also rejects. Every row leaves the real
    // Pod unchanged; no malformed carrier is relaxed into current evidence.
    let mut foreign_handle = serde_json::to_value(&first_handle).unwrap();
    let uid = foreign_handle
        .pointer_mut("/payload/previous/runtime_handle/pod_uid")
        .expect("current fixture owns an explicit Pod UID");
    *uid = serde_json::json!("not-the-created-pod");
    let contradictory_handle: pc::SandboxHandle =
        serde_json::from_value(foreign_handle.clone()).unwrap();
    let mut contradictory_observation = observation;
    contradictory_observation.handle = &contradictory_handle;
    let invalid = replacement
        .observe(
            contradictory_observation,
            contradictory_observation.effect_basis(&terminal_fence),
        )
        .await
        .expect_err("K1e0 contradictory V3 UID fields reject before backend observation");
    assert!(matches!(invalid, pc::SandboxError::Invalid(_)), "K1e0");
    *foreign_handle
        .pointer_mut("/payload/previous/runtime_handle/participants/pod_uid")
        .expect("current carrier retains its original participant Pod UID") =
        serde_json::json!("not-the-created-pod");
    if let Some(uid) =
        foreign_handle.pointer_mut("/payload/previous/sandbox_control_incarnation/uid")
    {
        *uid = serde_json::json!("not-the-created-pod");
    }
    let foreign_handle: pc::SandboxHandle = serde_json::from_value(foreign_handle).unwrap();
    let mut foreign_observation = observation;
    foreign_observation.handle = &foreign_handle;
    assert!(
        matches!(
            replacement
                .observe(
                    foreign_observation,
                    foreign_observation.effect_basis(&terminal_fence)
                )
                .await
                .unwrap(),
            pc::SandboxObservation::Incompatible { .. }
        ),
        "K1e foreign UID rejected"
    );
    let foreign_fence = effect_fence(&scope, "terminal", "foreign-owner", 1);
    assert!(
        matches!(
            replacement
                .observe(observation, observation.effect_basis(&foreign_fence))
                .await
                .unwrap(),
            pc::SandboxObservation::Incompatible { .. }
        ),
        "K1e foreign same-epoch owner rejected"
    );
    assert_eq!(
        runtime.inspect(&id).await.unwrap(),
        ContainerState::Running,
        "K1e no destructive observation"
    );
    assert!(
        matches!(
            replacement
                .observe(observation, observation.effect_basis(&terminal_fence))
                .await
                .unwrap(),
            pc::SandboxObservation::Ready | pc::SandboxObservation::Terminal { .. }
        ),
        "K1d terminal observation retains exact instance"
    );
    let retried = replacement
        .prepare_terminal_for_effect(
            &desired,
            Some(&first_handle),
            Some(&create_fence),
            &terminal_fence,
            &EffectAuthority::new(&terminal_fence),
        )
        .await
        .unwrap()
        .expect("K1d old instance remains for cleanup");
    assert_eq!(
        retried.handle(),
        first_handle,
        "K1d no rewritten historical binding"
    );
    retried
        .prepare_disposal_for_effect(
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
        )
        .await
        .expect("K1c prepare source durability without deleting the Pod");
    assert_eq!(
        runtime.inspect(&id).await.unwrap(),
        ContainerState::Running,
        "K1c/E4 preparation has zero physical effect"
    );
    retried
        .dispose_for_effect(
            &disposal_authorization(&terminal_fence),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&terminal_fence).effect_fence(),
            ),
        )
        .await
        .expect("K1c/E5 delete the exact Pod UID");
    assert_eq!(runtime.inspect(&id).await.unwrap(), ContainerState::Gone);
}

#[tokio::test]
async fn create_replay_keeps_a_later_managed_file_generation() {
    /* Live File projection decision table. Causes: C1 an exact fenced Create
     * finishes with no initial File; C2 the same Pod receives a read-only File
     * through the canonical live attach port; C3 the original Create is replayed
     * after that later Resource generation. Effects: C1+C2 makes the bytes
     * visible to the Agent; C1+C2+C3 retains both the Pod UID and exact bytes,
     * instead of clearing the uploads tree from the stale creation plan. Rule
     * KLI3. The existing KLI2 test owns replacement/removal; this test owns
     * only late attach followed by a delayed Create replay.
     */
    let Some(runtime) = live_runtime().await else {
        return;
    };
    let runtime = Arc::new(runtime);
    let scope = format!("k8s-file-create-replay-{}", std::process::id());
    let image = fixture_image();
    let provider = ContainerProvider::new(runtime.clone(), image.clone())
        .with_clock(awaken_time::system_clock());
    let desired = spec_with_command(&scope, image, command(&["sleep", "30"]));
    let create_fence = effect_fence(&scope, "create", "file-owner", 1);
    let first = SandboxProvider::claim_for_effect(
        &provider,
        &desired,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create_fence,
            authority: &EffectAuthority::new(&create_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("KLI3 initial fenced Create");
    let original = first.handle();
    let pod = container_id(&original);
    let path = "/mnt/session/uploads/customer/evidence.txt";
    pc::Sandbox::attach(
        first.as_ref(),
        pc::MountRequirement {
            mount_id: "customer-evidence".into(),
            source: pc::MountSource::Inline {
                contents: "later-resource-generation".into(),
            },
            mount_path: path.into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::Session,
            required: true,
        },
    )
    .await
    .expect("KLI3 late File projection");
    let namespace = std::env::var("AWAKEN_K8S_NAMESPACE").unwrap_or_else(|_| "default".into());
    let read = || {
        let output = std::process::Command::new("kubectl")
            .args([
                "-n",
                namespace.as_str(),
                "exec",
                pod.as_str(),
                "-c",
                "agent",
                "--",
                "cat",
                path,
            ])
            .output()
            .expect("KLI3 independent Pod read");
        assert!(
            output.status.success(),
            "KLI3 read: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    assert_eq!(read(), b"later-resource-generation", "KLI3 before replay");

    let replay = SandboxProvider::claim_for_effect(
        &provider,
        &desired,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create_fence,
            authority: &EffectAuthority::new(&create_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("KLI3 same-effect Create replay");
    assert_eq!(replay.handle(), original, "KLI3 one Pod and physical UID");
    assert_eq!(
        read(),
        b"later-resource-generation",
        "KLI3 later File retained"
    );

    let terminal_fence = effect_fence(&scope, "terminal", "file-owner", 2);
    replay
        .prepare_disposal_for_effect(&terminal_fence, &EffectAuthority::new(&terminal_fence))
        .await
        .expect("KLI3 preserve the source before disposal");
    let authorization = disposal_authorization(&terminal_fence);
    replay
        .dispose_for_effect(
            &authorization,
            &EffectAuthority::new(authorization.effect_fence()),
        )
        .await
        .expect("KLI3 delete only its Pod");
    assert_eq!(runtime.inspect(&pod).await.unwrap(), ContainerState::Gone);
}

#[tokio::test]
async fn k8s_exact_restore_survives_provider_replacement_with_the_same_pvc_fence() {
    /* Live Kubernetes exact-restore table. C1 Pod/PVC target absent; C2 the
     * first provider wrapper drops before aggregate CAS; C3 a fresh kube client
     * retries the exact tuple; C4 the same effect carries another generation.
     * Effects: E1 create one annotated Pod/PVC and publish both physical UID fences;
     * E2 C2+C3 recover the identical handle; E3 C4 fails without reaping either
     * object; E4 terminal disposal removes both. Rules KR1=C1=>E1;
     * KR2=C2+C3=>E2; KR3=C4=>E3; KR4=dispose=>E4. A new live
     * invocation has a distinct restore effect, so a retained failed fixture
     * cannot collide with KR1. Retries within this invocation retain the exact
     * request; no previous fixture is deleted or adopted to satisfy KR1. */
    let Some(first_runtime) = live_runtime().await else {
        return;
    };
    let first_runtime = first_runtime.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    });
    let invocation = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let scope = format!("k8s-restore-{}-{invocation}", std::process::id());
    let spec = common::exact_restore_spec(&scope);
    let mut request = common::exact_restore_request(&scope);
    request.effect_id = format!("blake3:{}", blake3::hash(scope.as_bytes()));
    // KR4 carries the caller's durable preparation/successor authorization
    // through every provider layer; the backend cannot manufacture it.
    let authorization = disposal_authorization(&effect_fence(
        &scope,
        "restore-terminal",
        "terminal-owner",
        2,
    ));
    let image =
        std::env::var("AWAKEN_K8S_FIXTURE_IMAGE").unwrap_or_else(|_| "awaken-bb:1".to_string());
    let first = ContainerProvider::new(Arc::new(first_runtime), image.clone())
        .with_clock(awaken_time::system_clock())
        .acquire_restore_environment(&spec, &request)
        .await
        .expect("KR1 exact Pod/PVC create");
    assert_eq!(
        first.disposition(),
        pc::SandboxRestoreTargetDisposition::Created,
        "KR1/E1",
    );
    let handle = pc::Sandbox::handle(first.target().as_ref());
    let payload = handle.container_payload().expect("KR1 container payload");
    let retained_participants = payload.runtime_handle.clone();
    let pod_name = payload.container_id.clone();
    let (pod_uid, claim_uid) = match payload.runtime_handle.as_ref() {
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
            pod_uid,
            claim_uid: Some(claim_uid),
            ..
        }) => (pod_uid.clone(), claim_uid.clone()),
        other => panic!("KR1/E1 missing current Pod/PVC incarnation fences: {other:?}"),
    };
    let claim_name = format!(
        "awc-{}",
        pod_name
            .strip_prefix("awaken-")
            .expect("KR1 managed Pod name"),
    );
    drop(first);

    let Some(retry_runtime) = live_runtime().await else {
        panic!("KR2 apiserver disappeared after exact target creation");
    };
    let retry_runtime = Arc::new(
        retry_runtime.with_continuation_volume(K8sContinuationVolume {
            storage_class_name: None,
            size: "1Gi".into(),
        }),
    );
    let retry = Arc::new(
        ContainerProvider::new(retry_runtime.clone(), image)
            .with_clock(awaken_time::system_clock()),
    );
    let mut mismatch = request.clone();
    mismatch.generation_id.push_str("-other");
    assert!(
        retry
            .acquire_restore_environment(&spec, &mismatch)
            .await
            .is_err(),
        "KR3/E3",
    );
    let recovered = retry
        .acquire_restore_environment(&spec, &request)
        .await
        .expect("KR2 fresh provider recovery");
    assert_eq!(
        recovered.disposition(),
        pc::SandboxRestoreTargetDisposition::Recovered,
        "KR2/E2",
    );
    assert_eq!(
        pc::Sandbox::handle(recovered.target().as_ref()),
        handle,
        "KR2/E2"
    );
    drop(recovered);
    // KR5 response-loss cuts: D1 Pod+PVC both terminating, D2 Pod absent
    // while PVC terminates, D3 both absent. Ordinary acquisition rejects D1;
    // authorized restoration replays D1. Once D2 loses the Pod carrier, only
    // the persisted original handle can finish; a request alone must reject.
    // Original-handle cleanup waits for physical absence and D3 is idempotent.
    // Abort/join the caller only after real API deletion intent;
    // release only test-owned finalizers and require the product result.
    const TEST_FINALIZER: &str = "awaken.dev/k8s-it-restore-hold";
    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .expect("KR4 read cleanup state");
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client.clone(), "default");
    let claims = kube::Api::<k8s_openapi::api::core::v1::PersistentVolumeClaim>::namespaced(
        client, "default",
    );
    k8s_common::set_test_finalizer(&pods, &pod_name, TEST_FINALIZER, true).await;
    k8s_common::set_test_finalizer(&claims, &claim_name, TEST_FINALIZER, true).await;
    let start_disposal = || {
        let provider = retry.clone();
        let spec = spec.clone();
        let request = request.clone();
        let authorization = authorization.clone();
        tokio::spawn(async move {
            provider
                .dispose_restored_environment(
                    &spec,
                    &request,
                    &authorization,
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(
                        authorization.effect_fence(),
                    ),
                )
                .await
        })
    };
    let first_disposal = start_disposal();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let pod = pods.get(&pod_name).await.unwrap();
            let claim = claims.get(&claim_name).await.unwrap();
            if pod.metadata.deletion_timestamp.is_some()
                && claim.metadata.deletion_timestamp.is_some()
            {
                break;
            }
            assert!(
                !first_disposal.is_finished(),
                "KR5 D1 cleanup must wait for held objects"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("KR5 D1 real deletion intent");
    first_disposal.abort();
    assert!(
        first_disposal.await.unwrap_err().is_cancelled(),
        "KR5 D1 lose caller response"
    );
    assert!(
        retry
            .acquire_restore_environment(&spec, &request)
            .await
            .is_err(),
        "KR5 D1 terminating target cannot become runnable"
    );
    let mut replay = start_disposal();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), &mut replay)
            .await
            .is_err(),
        "KR5 D1 replay waits for held Pod"
    );
    k8s_common::set_test_finalizer(&pods, &pod_name, TEST_FINALIZER, false).await;
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while pods.get_opt(&pod_name).await.unwrap().is_some() {
            assert!(
                !replay.is_finished(),
                "KR5 D2 cleanup cannot complete before Pod absence"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("KR5 D2 Pod absence");
    assert!(
        claims
            .get(&claim_name)
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_some(),
        "KR5 D2 PVC remains terminating"
    );
    replay.abort();
    assert!(
        replay.await.unwrap_err().is_cancelled(),
        "KR5 D2 lose second caller response"
    );
    let refused = start_disposal().await.unwrap().unwrap_err();
    assert!(
        refused.to_string().contains("original participant carrier"),
        "KR5 D2 request alone cannot manufacture absence: {refused}"
    );
    let retained = claims.get(&claim_name).await.unwrap();
    assert_eq!(retained.metadata.uid.as_deref(), Some(claim_uid.as_str()));
    assert!(retained.metadata.deletion_timestamp.is_some());
    assert!(
        retained
            .metadata
            .finalizers
            .as_ref()
            .unwrap()
            .iter()
            .any(|value| value == TEST_FINALIZER),
        "KR5 D2 refusal cannot remove the held participant"
    );
    let start_original_cleanup = || {
        let runtime = retry_runtime.clone();
        let pod_name = pod_name.clone();
        let handle = retained_participants.clone();
        let authorization = authorization.clone();
        tokio::spawn(async move {
            runtime
                .remove_exact_incarnation(
                    &pod_name,
                    handle.as_ref(),
                    &authorization,
                    &EffectAuthority::new(authorization.effect_fence()),
                )
                .await
        })
    };
    let mut replay = start_original_cleanup();
    let held_claim = tokio::time::timeout(std::time::Duration::from_millis(300), &mut replay).await;
    assert!(
        held_claim.is_err(),
        "KR5 D2 original-handle cleanup waits for PVC: {held_claim:?}"
    );
    k8s_common::set_test_finalizer(&claims, &claim_name, TEST_FINALIZER, false).await;
    tokio::time::timeout(std::time::Duration::from_secs(60), replay)
        .await
        .expect("KR5 physical completion")
        .expect("KR5 join replay")
        .expect("KR4 product cleanup succeeds");
    start_original_cleanup()
        .await
        .unwrap()
        .expect("KR5 D3 original-handle absent replay");
    assert!(
        start_disposal().await.unwrap().is_err(),
        "KR5 D3 a bare 404 is not original-participant evidence"
    );

    assert!(pods.get_opt(&pod_name).await.unwrap().is_none(), "KR4/E4");
    assert!(
        claims.get_opt(&claim_name).await.unwrap().is_none(),
        "KR4/E4"
    );
    assert!(!pod_uid.is_empty(), "KR1/E1 Pod UID fence");
    assert!(!claim_uid.is_empty(), "KR1/E1 PVC UID fence");
}

#[tokio::test]
async fn create_fails_closed_on_a_pids_limit_k8s_cannot_enforce() {
    /* Resource-enforcement FMECA decision rule KP1. Causes: C1 the caller
     * requires a finite pids cap; C2 Kubernetes exposes no per-Pod pids field.
     * C1+C2 => E1 reject before create, E2 name the unsupported limit, E3 leave
     * no Pod. The complementary enforceable-resource projection is owned by the
     * pure K8s plan tests; this live rule proves the apiserver sees no residue.
     */
    let Some(rt) = live_runtime().await else {
        return;
    };
    let scope = format!("pids-reject-{}", std::process::id());
    let mut p = plan(&["sleep", "5"]);
    p.limits = pc::ResourceLimits {
        pids: Some(64),
        ..Default::default()
    };
    let err = rt
        .create(&scope, &p)
        .await
        .expect_err("a pids-limited spec must fail closed on k8s, not silently drop the cap");
    assert!(
        format!("{err}").contains("pids"),
        "the error names the unenforceable limit: {err}"
    );
    // The release annotation is the product-owned inverse from API state back
    // to this Session scope. The test intentionally does not duplicate the
    // adapter's private stable-name projection merely to prove zero mutation.
    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client, "default");
    let leaked = pods
        .list(&kube::api::ListParams::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .any(|pod| {
            pod.metadata
                .annotations
                .as_ref()
                .and_then(|annotations| {
                    annotations
                        .get(awaken_package_image::package_realization::SANDBOX_SCOPE_ANNOTATION)
                })
                .is_some_and(|observed| observed == &scope)
        });
    assert!(
        !leaked,
        "the fail-closed admission path must leave no Pod for the exact scope"
    );
}

#[tokio::test]
async fn a_terminal_pod_is_replaced_under_its_observed_identity_fence() {
    /* Terminal-recovery decision table KTR1. Causes: C1 one frozen product
     * spec and Create fence produce running Pod UID A plus retained claim UID P;
     * C2 an attached process writes P and terminates the provider-owned PID 1;
     * C3 the durable V3 original-set handle supplies A+P; C4 an authorized Rebuild uses the
     * exact same frozen spec, A+P, and a later realization fence; C5 terminal
     * cleanup has a later fence. Effects: E1 only terminal A is replaced; E2 the
     * stable locator now names UID B; E3 B reuses P and an attached validation
     * reads the prior marker; E4 fenced cleanup deletes B and P. Rule KTR1
     * C1+C2+C3+C4=>E1+E2+E3; KTR2 KTR1+C5=>E4. Missing/foreign source rows
     * remain owned by the pure rebuild table. `ContainerProvider` deliberately
     * owns the immutable keepalive plan rather than SandboxSpec.command, so both
     * generations use that exact product plan; attached exec only injects the
     * crash and validates retained bytes, and is not a second realization path.
     */
    let Some(rt) = live_runtime().await else {
        return;
    };
    let runtime = Arc::new(rt.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    }));
    let scope = format!("terminal-replace-{}", std::process::id());
    let provider = ContainerProvider::new(runtime.clone(), fixture_image())
        .with_clock(awaken_time::system_clock());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sh", "-c", "sleep 300"]));
    let create_fence = effect_fence(&scope, "create", "terminal-owner", 1);
    let first_environment = SandboxProvider::claim_for_effect(
        &provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create_fence,
            authority: &EffectAuthority::new(&create_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("create first incarnation");
    let first_handle = first_environment.handle();
    let first = container_id(&first_handle);
    let source_incarnation = first_handle
        .container_physical_incarnation()
        .expect("KTR1 V3 original-set handle carries Pod UID")
        .to_owned();
    let source_runtime_handle = first_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .clone();
    assert!(
        matches!(
            source_runtime_handle.as_ref(),
            Some(
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
                    claim_uid: Some(_),
                    ..
                }
            )
        ),
        "KTR1 V3 original-set handle carries retained claim UID"
    );
    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .expect("connect a read-only test client");
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client, "default");
    let first_uid = pods
        .get(&first)
        .await
        .expect("read first incarnation")
        .metadata
        .uid
        .expect("apiserver supplies a UID");
    let marker = pc::Sandbox::spawn(
        first_environment.as_ref(),
        pc::Command::new(["sh", "-c", "printf preserved > /workspace/recovery-proof"]),
    )
    .await
    .expect("KTR1 write retained marker through the product exec seam")
    .wait()
    .await
    .expect("KTR1 wait for retained marker write");
    assert_eq!(marker.code, Some(0), "KTR1 retained marker write");
    let terminator = pc::Sandbox::spawn(
        first_environment.as_ref(),
        pc::Command::new(["sh", "-c", "kill -TERM 1"]),
    )
    .await
    .expect("KTR1 inject provider PID 1 termination through attached exec");
    let _ = terminator.wait().await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let terminated = pods
            .get(&first)
            .await
            .expect("observe first incarnation")
            .status
            .and_then(|status| status.container_statuses)
            .and_then(|statuses| statuses.into_iter().find(|status| status.name == "agent"))
            .and_then(|status| status.state)
            .is_some_and(|state| state.terminated.is_some());
        if terminated {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Pod never completed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    assert_eq!(first_uid, source_incarnation, "KTR1 source handle");
    drop(first_environment);

    let rebuild_fence = effect_fence(&scope, "rebuild", "terminal-owner", 2);
    let replacement_environment = SandboxProvider::claim_for_effect(
        &provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &rebuild_fence,
            authority: &EffectAuthority::new(&rebuild_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Rebuild {
            source: Box::new(first_handle.clone()),
        },
    )
    .await
    .expect("replace the terminal incarnation");
    let replacement_handle = replacement_environment.handle();
    let replacement = container_id(&replacement_handle);
    assert_eq!(replacement, first, "KTR1 deterministic name");
    let replacement_uid = pods
        .get(&replacement)
        .await
        .expect("read replacement")
        .metadata
        .uid
        .expect("apiserver supplies a replacement UID");
    assert_ne!(replacement_uid, first_uid, "KTR1 exact incarnation changed");
    assert_eq!(
        runtime.inspect(&replacement).await.unwrap(),
        ContainerState::Running,
        "KTR1 the replacement Pod reattached the retained PVC bytes"
    );
    let validation = pc::Sandbox::spawn(
        replacement_environment.as_ref(),
        pc::Command::new([
            "sh",
            "-c",
            "test \"$(cat /workspace/recovery-proof)\" = preserved",
        ]),
    )
    .await
    .expect("KTR1 validate retained bytes through the replacement exec seam")
    .wait()
    .await
    .expect("KTR1 wait for retained-byte validation");
    assert_eq!(validation.code, Some(0), "KTR1 retained bytes");
    let terminal_fence = effect_fence(&scope, "terminal", "terminal-owner", 3);
    replacement_environment
        .prepare_disposal_for_effect(
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
        )
        .await
        .expect("KTR2 prepare replacement disposal");
    replacement_environment
        .dispose_for_effect(
            &disposal_authorization(&terminal_fence),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&terminal_fence).effect_fence(),
            ),
        )
        .await
        .expect("KTR2 delete replacement and retained claim");
}

#[tokio::test]
async fn terminal_control_pod_is_cleaned_without_reopening_live_control() {
    // Live table KC1: a real marker-ready Pod records demanded control identity;
    // its agent PID1 terminates -> ordinary control binding is unavailable;
    // exact handle + current effect fence -> terminal preparation retains that
    // identity without opening control; authorized disposal removes the same
    // Pod UID. This is backend lifecycle evidence, not business publication or
    // proof of arbitrary external effects. Foreign/stale rows remain in the
    // provider/HTTP decision tables; no name-only deletion is used here.
    let Some(runtime) = live_runtime().await else {
        return;
    };
    let image = fixture_image();
    let runtime = runtime.with_sandbox_control_forwarder(
        awaken_sandbox_kubernetes::K8sSandboxControlForwarder::new(
            control_image(),
            "/usr/local/bin/awaken-sandbox",
            awaken_sandbox_kubernetes::DEFAULT_REPOSITORY_GIT_CONTROL_PORT,
        )
        .unwrap(),
    );
    let provider = ContainerProvider::new(Arc::new(runtime), image.clone())
        .with_clock(awaken_time::system_clock());
    let scope = format!("terminal-control-{}", std::process::id());
    // KC1 requires a PID-1 process that accepts TERM. Use the same canonical
    // Session keepalive as production; a bare PID-1 sleep has no TERM handler.
    let mut desired = spec_with_command(
        &scope,
        image,
        awaken_sandbox_container::engine::environment_keepalive_command(),
    );
    desired
        .control_services
        .insert(pc::SandboxControlServiceKind::RepositoryGitCredential);
    let create = effect_fence(&scope, "create", "control-owner", 1);
    let original = SandboxProvider::claim_for_effect(
        &provider,
        &desired,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create,
            authority: &EffectAuthority::new(&create),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("KC1 create real control Pod");
    let handle = original.handle();
    let pod = container_id(&handle);
    let recorded_control = handle
        .container_payload()
        .unwrap()
        .sandbox_control_incarnation
        .clone();
    assert!(recorded_control.is_some(), "KC1 exact control identity");
    let process = pc::Sandbox::spawn(
        original.as_ref(),
        pc::Command::new(["sh", "-c", "kill -TERM 1"]),
    )
    .await
    .expect("KC1 terminate only owned fixture PID1");
    let _ = process.wait().await;
    let cleanup = effect_fence(&scope, "cleanup", "control-owner", 2);
    // Kubernetes owns the wait for its exact stored status value. No private
    // poll loop, elapsed-time success, or higher-layer Managed test dependency.
    // The provider then verifies the original physical UID under our fence.
    let namespace = std::env::var("AWAKEN_K8S_NAMESPACE").unwrap_or_else(|_| "default".into());
    let waited = std::process::Command::new("kubectl")
        .args([
            "-n", &namespace, "wait", &format!("pod/{pod}"),
            "--for=jsonpath={.status.containerStatuses[?(@.name==\"agent\")].state.terminated.finishedAt}",
            "--timeout=30s",
        ])
        .output().expect("KC1 Kubernetes status-value waiter");
    assert!(
        waited.status.success(),
        "KC1 status value not observed: {}",
        String::from_utf8_lossy(&waited.stderr)
    );
    let mut adoption = awaken_provisioning_contract::SandboxAdoption::new(&desired, &handle);
    adoption.observation_purpose =
        awaken_provisioning_contract::SandboxObservationPurpose::TerminalCleanup;
    assert!(
        matches!(
            provider
                .observe(adoption, adoption.effect_basis(&cleanup))
                .await
                .unwrap(),
            pc::SandboxObservation::Terminal { .. }
        ),
        "KC1 exact provider Terminal receipt"
    );
    drop(original);
    let terminal = SandboxProvider::prepare_terminal_for_effect(
        &provider,
        &desired,
        Some(&handle),
        None,
        &cleanup,
        &EffectAuthority::new(&cleanup),
    )
    .await
    .expect("KC1 no Ready-only control binding")
    .expect("KC1 existing physical cleanup participant");
    assert_eq!(
        terminal
            .handle()
            .container_payload()
            .unwrap()
            .sandbox_control_incarnation,
        recorded_control,
        "KC1 original control evidence retained"
    );
    terminal
        .prepare_disposal_for_effect(&cleanup, &EffectAuthority::new(&cleanup))
        .await
        .expect("KC1 data preparation with no writable participants");
    let authorization = disposal_authorization(&cleanup);
    terminal
        .dispose_for_effect(
            &authorization,
            &EffectAuthority::new(authorization.effect_fence()),
        )
        .await
        .expect("KC1 exact physical disposal");
    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client, &namespace);
    assert!(
        pods.get_opt(&pod).await.unwrap().is_none(),
        "KC1 observed Pod absence"
    );
}

#[tokio::test]
async fn stale_disposal_cannot_delete_a_recreated_claim_incarnation() {
    /* PVC-disposal decision table KPV1. C1 an old durable handle binds PVC UID A;
     * C2 authorized disposal removes A; C3 the same Session realization name is
     * recreated with PVC UID B; C4 a delayed stale disposer presents A.
     * R1 C1+C2+C3+C4 => reject before deleting Pod/PVC B; R2 current handle B =>
     * delete both. FMECA: name-only deletion can destroy the next generation's
     * active filesystem (S5/O2/D4=40).
     */
    let Some(rt) = live_runtime().await else {
        return;
    };
    let runtime = Arc::new(rt.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    }));
    let provider = ContainerProvider::new(runtime.clone(), fixture_image())
        .with_clock(awaken_time::system_clock());
    let scope = format!("claim-fence-{}", std::process::id());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let first_fence = effect_fence(&scope, "create-a", "claim-owner", 1);
    let first_environment = SandboxProvider::claim_for_effect(
        &provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &first_fence,
            authority: &EffectAuthority::new(&first_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .unwrap();
    let first_handle = first_environment.handle();
    let first = container_id(&first_handle);
    let old = first_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .clone()
        .expect("KPV1 first V3 original-set handle");
    let first_terminal = effect_fence(&scope, "dispose-a", "claim-owner", 2);
    first_environment
        .prepare_disposal_for_effect(
            &first_terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&first_terminal),
        )
        .await
        .unwrap();
    first_environment
        .dispose_for_effect(
            &disposal_authorization(&first_terminal),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&first_terminal).effect_fence(),
            ),
        )
        .await
        .unwrap();

    let second_fence = effect_fence(&scope, "create-b", "claim-owner", 3);
    let second_environment = SandboxProvider::claim_for_effect(
        &provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &second_fence,
            authority: &EffectAuthority::new(&second_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .unwrap();
    let second_handle = second_environment.handle();
    let second = container_id(&second_handle);
    let current = second_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .clone()
        .expect("KPV1 current V3 original-set handle");
    assert_ne!(old, current, "KPV1 distinct PVC incarnations");
    assert!(
        runtime
            .remove_with_handle(&second, Some(&old))
            .await
            .is_err(),
        "KPV1 stale deletion is fenced"
    );
    assert_eq!(
        runtime.inspect(&second).await.unwrap(),
        ContainerState::Running
    );
    let second_terminal = effect_fence(&scope, "dispose-b", "claim-owner", 4);
    second_environment
        .prepare_disposal_for_effect(
            &second_terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&second_terminal),
        )
        .await
        .unwrap();
    second_environment
        .dispose_for_effect(
            &disposal_authorization(&second_terminal),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&second_terminal).effect_fence(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(runtime.inspect(&first).await.unwrap(), ContainerState::Gone);
}

#[tokio::test]
async fn orphaned_claim_replays_only_under_the_same_create_effect() {
    /* Claim-only crash-cut table KPV3. Causes: C1 exact Create effect A has
     * persisted retained claim UID P; C2 fault injection removes Pod UID A with
     * Kubernetes UID/resourceVersion preconditions while preserving P; C3 the
     * next caller presents foreign effect B or exact replay A; C4 terminal
     * cleanup is authorized after recovery. Effects: E1 B rejects before Pod or
     * PVC mutation and P remains; E2 A creates Pod UID A2, persists a V3 original-set handle,
     * and reuses exactly P; E3 terminal cleanup removes A2 and P. Rules: KPV3a
     * C1+C2+B=>E1; KPV3b C1+C2+A=>E2; KPV3c E2+C4=>E3. This live backend cut is
     * equivalent to response loss after PVC creation but before a Pod UID can be
     * published; the PVC stores the existing aggregate fence, not a new state. */
    let Some(runtime) = live_runtime().await else {
        return;
    };
    let runtime = Arc::new(runtime.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    }));
    let provider = ContainerProvider::new(runtime.clone(), fixture_image())
        .with_clock(awaken_time::system_clock());
    let scope = format!("claim-orphan-{}", std::process::id());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let create_fence = effect_fence(&scope, "create", "orphan-owner-a", 1);
    let first = SandboxProvider::claim_for_effect(
        &provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create_fence,
            authority: &EffectAuthority::new(&create_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("KPV3 C1 create retained realization");
    let first_handle = first.handle();
    let pod_name = container_id(&first_handle);
    let (first_pod_uid, claim_uid) = match first_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .as_ref()
    {
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
            pod_uid,
            claim_uid: Some(claim_uid),
            ..
        }) => (pod_uid.clone(), claim_uid.clone()),
        other => panic!("KPV3 C1 expected Pod+claim V3 original-set handle, got {other:?}"),
    };

    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client.clone(), "default");
    let claims = kube::Api::<k8s_openapi::api::core::v1::PersistentVolumeClaim>::namespaced(
        client, "default",
    );
    let observed = pods.get(&pod_name).await.unwrap();
    let claim_name = only_bound_claim_name(&observed);
    let resource_version = observed
        .metadata
        .resource_version
        .expect("KPV3 fault injection observes Pod resourceVersion");
    pods.delete(
        &pod_name,
        &kube::api::DeleteParams::default().preconditions(kube::api::Preconditions {
            uid: Some(first_pod_uid.clone()),
            resource_version: Some(resource_version),
        }),
    )
    .await
    .expect("KPV3 C2 exact fault-injection Pod delete");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if pods.get_opt(&pod_name).await.unwrap().is_none() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "KPV3 C2 Pod deletion timed out"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    drop(first);

    let foreign_fence = effect_fence(&scope, "create", "orphan-owner-b", 1);
    let foreign = SandboxProvider::claim_for_effect(
        &provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &foreign_fence,
            authority: &EffectAuthority::new(&foreign_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .err()
    .expect("KPV3a foreign claim-only recovery rejects");
    assert!(
        foreign.to_string().contains("continuation PVC is owned"),
        "KPV3a: {foreign}"
    );
    assert!(pods.get_opt(&pod_name).await.unwrap().is_none(), "KPV3a");
    assert_eq!(
        claims
            .get(&claim_name)
            .await
            .unwrap()
            .metadata
            .uid
            .as_deref(),
        Some(claim_uid.as_str()),
        "KPV3a P remains"
    );

    let recovered = SandboxProvider::claim_for_effect(
        &provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create_fence,
            authority: &EffectAuthority::new(&create_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("KPV3b exact response-loss replay");
    let recovered_handle = recovered.handle();
    let (recovered_pod_uid, recovered_claim_uid) =
        match recovered_handle.container_payload().unwrap().runtime_handle {
            Some(
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
                    pod_uid,
                    claim_uid: Some(claim_uid),
                    ..
                },
            ) => (pod_uid, claim_uid),
            other => panic!("KPV3b expected Pod+claim V3 original-set handle, got {other:?}"),
        };
    assert_ne!(recovered_pod_uid, first_pod_uid, "KPV3b new Pod UID");
    assert_eq!(recovered_claim_uid, claim_uid, "KPV3b exact claim UID");
    let terminal_fence = effect_fence(&scope, "terminal", "orphan-owner-a", 2);
    recovered
        .prepare_disposal_for_effect(
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
        )
        .await
        .expect("KPV3c source-dependent preparation");
    recovered
        .dispose_for_effect(
            &disposal_authorization(&terminal_fence),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&terminal_fence).effect_fence(),
            ),
        )
        .await
        .expect("KPV3c exact cleanup");
    assert!(
        claims.get_opt(&claim_name).await.unwrap().is_none(),
        "KPV3c"
    );
}

#[tokio::test]
async fn missing_handle_reconstructs_a_live_original_pod_and_claim_for_terminal_disposal() {
    /* Live provider decision rule KPV4. Causes: C1 one broker-mode typed
     * fixture forwards the original Pod/PVC Creates to a real API and retains
     * both API-assigned UIDs; C2 the replacement receives its complete
     * in-memory join; C3 frozen plan/effect and live immutable projections
     * still match. Effects: E1 terminal preparation returns one typed V2
     * Pod/PVC handle; E2 the existing Pod cleanup gate and UID-fenced disposal
     * remove both objects. Rule KPV4a=C1+C2+C3->E1+E2. Unit tests own every
     * incomplete/foreign decision row; the independent ledger live test owns
     * durable CAS. This row qualifies real API defaulting and cleanup but its
     * in-memory join is not a product Worker-exit receipt. */
    use awaken_container_contract::RuntimeError;
    use awaken_worker_fleet_kubernetes::K8sCreateTransport;
    use awaken_worker_fleet_kubernetes::WorkerCreateLedgerState;

    const GROUP: &str = "live-joined-pair";

    struct JoinedPair {
        client: kube::Client,
        namespace: String,
        scope: String,
        predecessor: pc::SandboxEffectFence,
        state: std::sync::Mutex<WorkerCreateLedgerState>,
        completed_api_creates: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl K8sCreateTransport for JoinedPair {
        async fn create(
            &self,
            request: K8sCreateRequest,
        ) -> Result<K8sCreateResponse, RuntimeError> {
            let pod_path = format!("/api/v1/namespaces/{}/pods", self.namespace);
            let claim_path = format!(
                "/api/v1/namespaces/{}/persistentvolumeclaims",
                self.namespace
            );
            let resource = if request.path == pod_path {
                "pods"
            } else if request.path == claim_path {
                "persistentvolumeclaims"
            } else {
                return Err(RuntimeError::Backend(
                    "test broker rejects another Kubernetes collection".into(),
                ));
            };
            let name = request.object["metadata"]["name"]
                .as_str()
                .ok_or_else(|| RuntimeError::Backend("test Create omitted its exact name".into()))?
                .to_owned();
            let original_body = serde_json::to_vec(&request.object)
                .map_err(|error| RuntimeError::Backend(error.to_string()))?;
            let fingerprint = awaken_agent_contract::collision_resistant_fingerprint(
                "k8s-create-request-v1",
                &[request.path.as_bytes(), &original_body],
            );
            let digest =
                awaken_agent_contract::collision_resistant_fingerprint_digest(&fingerprint)
                    .expect("canonical request fingerprint has a digest");
            let ticket = CreateTicketRequest::new(GROUP, resource, &self.namespace, &name, digest)?;
            // KPV4 admission uses the SAME pure production transitions. Retain
            // one nonce before the API call; an in-flight request cannot become
            // Complete merely because sealing raced its response. The counter
            // below observes effects only and never grants admission or join.
            let admitted: CreateTicket = {
                let mut state = self.state.lock().unwrap();
                let nonce = uuid::Uuid::new_v4().simple().to_string();
                if state.admit(&ticket, &nonce)? != TicketAdmission::Forward {
                    return Err(RuntimeError::Backend(
                        "test Create did not win its original admission".into(),
                    ));
                }
                state
                    .ticket(&ticket)
                    .expect("admitted ticket retained")
                    .clone()
            };
            // Valid fixture input for the original-request verifier: the API
            // stores the same admitted nonce/digest, and the original Pod guard
            // remains until its existing authorized disposal owner releases it.
            let mut requested = request.object;
            requested["metadata"]["annotations"]
                [awaken_container_contract::ISOLATION_ID_ANNOTATION] =
                admitted.isolation_id.clone().into();
            requested["metadata"]["annotations"]
                [awaken_container_contract::REQUEST_DIGEST_ANNOTATION] =
                admitted.request.digest.clone().into();
            if resource == "pods" {
                let metadata = requested["metadata"].as_object_mut().unwrap();
                let finalizers = metadata
                    .entry("finalizers")
                    .or_insert_with(|| serde_json::json!([]))
                    .as_array_mut()
                    .expect("typed Pod finalizers are an array");
                finalizers.push(awaken_container_contract::ORIGINAL_GUARD_FINALIZER.into());
            }
            let (uid, object) = if resource == "pods" {
                let desired: k8s_openapi::api::core::v1::Pod = serde_json::from_value(requested)
                    .map_err(|error| RuntimeError::Backend(error.to_string()))?;
                let created = kube::Api::namespaced(self.client.clone(), &self.namespace)
                    .create(&kube::api::PostParams::default(), &desired)
                    .await
                    .map_err(|error| RuntimeError::Backend(error.to_string()))?;
                (
                    created.metadata.uid.clone(),
                    serde_json::to_value(created)
                        .map_err(|error| RuntimeError::Backend(error.to_string()))?,
                )
            } else {
                let desired: k8s_openapi::api::core::v1::PersistentVolumeClaim =
                    serde_json::from_value(requested)
                        .map_err(|error| RuntimeError::Backend(error.to_string()))?;
                let created = kube::Api::namespaced(self.client.clone(), &self.namespace)
                    .create(&kube::api::PostParams::default(), &desired)
                    .await
                    .map_err(|error| RuntimeError::Backend(error.to_string()))?;
                (
                    created.metadata.uid.clone(),
                    serde_json::to_value(created)
                        .map_err(|error| RuntimeError::Backend(error.to_string()))?,
                )
            };
            let uid =
                uid.ok_or_else(|| RuntimeError::Backend("Kubernetes Create has no UID".into()))?;
            self.state
                .lock()
                .unwrap()
                .finish(&ticket, TicketResult::Created { uid })?;
            self.completed_api_creates
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(K8sCreateResponse {
                status: 201,
                object,
            })
        }

        async fn seal_predecessor(
            &self,
            scope: &str,
            predecessor: &awaken_provisioning_contract::SandboxEffectFence,
            _: &str,
        ) -> Result<RequestJoin, RuntimeError> {
            if scope != self.scope || !predecessor.same_effect_identity(&self.predecessor) {
                return Ok(RequestJoin::Unknown);
            }
            let mut state = self.state.lock().unwrap();
            state.seal_worker();
            state.seal_group(GROUP)?;
            Ok(state.join_sealed_worker_group(GROUP))
        }
    }

    let Some(first_runtime) = live_runtime().await else {
        return;
    };
    let namespace = std::env::var("AWAKEN_K8S_NAMESPACE").unwrap_or_else(|_| "default".into());
    let volume = K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    };
    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    let scope = format!("joined-pvc-{}", uuid::Uuid::new_v4());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let create_fence = effect_fence(&scope, "create", "joined-owner", 1);
    let joined = Arc::new(JoinedPair {
        client: client.clone(),
        namespace: namespace.clone(),
        scope: scope.clone(),
        predecessor: create_fence.clone(),
        state: std::sync::Mutex::new(WorkerCreateLedgerState::default()),
        completed_api_creates: std::sync::atomic::AtomicUsize::new(0),
    });
    let first_provider = ContainerProvider::new(
        Arc::new(
            first_runtime
                .with_continuation_volume(volume.clone())
                .with_create_transport(joined.clone())
                .unwrap(),
        ),
        fixture_image(),
    )
    .with_clock(awaken_time::system_clock());
    let first = SandboxProvider::claim_for_effect(
        &first_provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create_fence,
            authority: &EffectAuthority::new(&create_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("KPV4 C1 real Pod/PVC Create");
    let first_handle = first.handle();
    let pod_name = container_id(&first_handle);
    let (pod_uid, claim_uid) = match first_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .as_ref()
    {
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
            pod_uid,
            claim_uid: Some(claim_uid),
            ..
        }) => (pod_uid.clone(), claim_uid.clone()),
        other => panic!("KPV4 C1 expected typed original pair: {other:?}"),
    };
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client.clone(), &namespace);
    let claims = kube::Api::<k8s_openapi::api::core::v1::PersistentVolumeClaim>::namespaced(
        client, &namespace,
    );
    let claim_name = only_bound_claim_name(&pods.get(&pod_name).await.unwrap());
    assert_eq!(
        claims
            .get(&claim_name)
            .await
            .unwrap()
            .metadata
            .uid
            .as_deref(),
        Some(claim_uid.as_str()),
        "KPV4 C1 original PVC remains present"
    );
    drop(first);

    assert_eq!(
        joined
            .completed_api_creates
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "KPV4 C2 two API UID results"
    );
    let replacement = live_runtime()
        .await
        .expect("KPV4 replacement reaches the same Kubernetes API")
        .with_continuation_volume(volume)
        .with_create_transport(joined.clone())
        .unwrap();
    let replacement_provider = ContainerProvider::new(Arc::new(replacement), fixture_image())
        .with_clock(awaken_time::system_clock());
    let terminal_fence = effect_fence(&scope, "terminal", "joined-owner", 2);
    let terminal = SandboxProvider::prepare_terminal_for_effect(
        &replacement_provider,
        &frozen,
        None,
        Some(&create_fence),
        &terminal_fence,
        &EffectAuthority::new(&terminal_fence),
    )
    .await
    .expect("KPV4a complete joined pair prepares terminal cleanup")
    .expect("KPV4a original pair has a typed terminal owner");
    let RequestJoin::Complete(tickets) =
        joined.state.lock().unwrap().join_sealed_worker_group(GROUP)
    else {
        panic!("KPV4a the original Worker group must actually be sealed and complete");
    };
    assert_eq!(tickets.len(), 2, "KPV4a original closed pair");
    assert_eq!(
        terminal.handle().container_physical_incarnation().unwrap(),
        pod_uid,
        "KPV4a exact original Pod UID"
    );
    terminal
        .prepare_disposal_for_effect(&terminal_fence, &EffectAuthority::new(&terminal_fence))
        .await
        .expect("KPV4a original terminal preparation");
    let authorization = disposal_authorization(&terminal_fence);
    terminal
        .dispose_for_effect(
            &authorization,
            &EffectAuthority::new(authorization.effect_fence()),
        )
        .await
        .expect("KPV4a existing UID-fenced disposal");
    assert!(pods.get_opt(&pod_name).await.unwrap().is_none(), "KPV4 E2");
    assert!(
        claims.get_opt(&claim_name).await.unwrap().is_none(),
        "KPV4 E2"
    );
}

#[tokio::test]
async fn absent_source_pod_with_live_claim_fails_closed_until_rebuild_serializes_cleanup() {
    /* Absent-primary terminal table KPV5. Causes: C1 the durable V3 original-set handle
     * identifies Pod UID A plus retained claim UID P; C2 an exact API fault cut
     * removes A while P remains live; C3 terminal preparation carries the same
     * frozen spec and a later aggregate-authorized effect; C4 authorized
     * Rebuild recreates Pod B against P; C5 exact B cleanup succeeds but its
     * response is lost. Effects: E1 orphan P rejects terminal preparation with
     * zero PVC deletion effect; E2 Rebuild preserves exact P under B; E3 B's
     * Pod-finalizer-serialized cleanup removes B+P; E4 same V2/effect replay
     * observes total absence and returns `None` idempotently. Rules: KPV5a
     * C1+C2+C3=>E1; KPV5b E1+C4=>E2; KPV5c E2+C5=>E3+E4.
     * Expanded purpose rows KPV5r/d: the SAME A/P cut is observed through the
     * public provider port with and without an effect fence. Both report exact
     * A unavailable, permitting the existing Rebuild policy; neither deletes P.
     * Direct dispose_authorized with a default Recovery expectation must still
     * reject P, as must terminal preparation. These paired callers prevent a
     * shared classifier from confusing execution absence with cleanup completion.
     * KPV5 current-authority precondition: each fenced create/rebuild supplies
     * the existing EffectAuthority fixture; None rejects before any API call and
     * cannot validate these lifecycle effects. The provider unit suite owns that
     * negative authority case. KPV5b must execute both prepare and activate of
     * the new Pod, never merely observe that the old source was absent. */
    let Some(runtime) = live_runtime().await else {
        return;
    };
    let runtime = Arc::new(runtime.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    }));
    let provider = ContainerProvider::new(runtime.clone(), fixture_image())
        .with_clock(awaken_time::system_clock());
    let scope = format!("claim-only-terminal-{}", std::process::id());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let create_fence = effect_fence(&scope, "create", "claim-only-owner", 1);
    let source = SandboxProvider::claim_for_effect(
        &provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create_fence,
            authority: &EffectAuthority::new(&create_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("KPV5 C1 create retained source");
    let source_handle = source.handle();
    let pod_name = container_id(&source_handle);
    let (pod_uid, claim_uid) = match source_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .as_ref()
    {
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
            pod_uid,
            claim_uid: Some(claim_uid),
            ..
        }) => (pod_uid.clone(), claim_uid.clone()),
        other => panic!("KPV5 C1 expected Pod+claim V3 original-set handle, got {other:?}"),
    };

    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client.clone(), "default");
    let claims = kube::Api::<k8s_openapi::api::core::v1::PersistentVolumeClaim>::namespaced(
        client, "default",
    );
    let observed_pod = pods.get(&pod_name).await.unwrap();
    let claim_name = only_bound_claim_name(&observed_pod);
    assert_eq!(
        claims
            .get(&claim_name)
            .await
            .unwrap()
            .metadata
            .uid
            .as_deref(),
        Some(claim_uid.as_str()),
        "KPV5 C1 exact retained claim"
    );
    pods.delete(
        &pod_name,
        &kube::api::DeleteParams::default().preconditions(kube::api::Preconditions {
            uid: Some(pod_uid.clone()),
            resource_version: Some(
                observed_pod
                    .metadata
                    .resource_version
                    .expect("KPV5 C2 observes Pod resourceVersion"),
            ),
        }),
    )
    .await
    .expect("KPV5 C2 exact fault-injection Pod delete");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if pods.get_opt(&pod_name).await.unwrap().is_none() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "KPV5 C2 Pod deletion timed out"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        claims.get_opt(&claim_name).await.unwrap().is_some(),
        "KPV5 C2"
    );
    drop(source);

    for fenced in [false, true] {
        let adoption = awaken_provisioning_contract::SandboxAdoption::new(&frozen, &source_handle);
        let observed = if fenced {
            provider
                .observe(adoption, adoption.effect_basis(&create_fence))
                .await
        } else {
            provider
                .observe(
                    adoption,
                    awaken_provisioning_contract::SandboxObservationBasis::Unattached,
                )
                .await
        }
        .expect("KPV5r read-only observation preserves recovery admission");
        assert!(
            matches!(observed, pc::SandboxObservation::DefinitivelyUnavailable {
            physical_incarnation: Some(ref identity),
        } if identity == &pod_uid),
            "KPV5r exact A unavailable: {observed:?}"
        );
    }

    let terminal_fence = effect_fence(&scope, "terminal", "claim-only-owner", 2);
    let disposal = disposal_authorization(&terminal_fence);
    let decoded_source =
        awaken_sandbox_container::DecodedContainerHandle::decode(&source_handle).unwrap();
    let expectation =
        awaken_sandbox_container::ContainerObservationExpectation::from_decoded_for_effect(
            &decoded_source,
            disposal.effect_fence(),
        );
    let rejected = runtime
        .dispose_authorized(
            expectation,
            &disposal,
            &pc::test_support::EffectAuthority::new(disposal.effect_fence()),
        )
        .await
        .expect_err("KPV5d physical disposal must own terminal observation purpose");
    assert!(
        rejected.to_string().contains(
            "live Kubernetes continuation PVC cannot be disposed after its source Pod is absent"
        ),
        "KPV5d must reach claim admission, not fail on an invalid fence: {rejected}"
    );

    let error = match SandboxProvider::prepare_terminal_for_effect(
        &provider,
        &frozen,
        Some(&source_handle),
        Some(&create_fence),
        &terminal_fence,
        &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
    )
    .await
    {
        Ok(_) => panic!("KPV5a orphan live claim must fail closed"),
        Err(error) => error,
    };
    // KPV5a reaches the absent-Pod claim admission owner: an exact live PVC
    // cannot be deleted because Rebuild may bind it after this observation.
    // The error, unchanged UID, zero DELETE, Rebuild and replay effects jointly
    // distinguish this rule from a generic observation mismatch.
    assert!(
        error.to_string().contains(
            "live Kubernetes continuation PVC cannot be disposed after its source Pod is absent"
        ),
        "KPV5a: {error}"
    );
    assert!(
        claims
            .get(&claim_name)
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none(),
        "KPV5a zero PVC deletion effect"
    );
    assert_eq!(
        claims
            .get(&claim_name)
            .await
            .unwrap()
            .metadata
            .uid
            .as_deref(),
        Some(claim_uid.as_str()),
        "KPV5a rejected cleanup preserves the exact live claim incarnation"
    );

    let rebuild_fence = effect_fence(&scope, "rebuild", "claim-only-owner", 2);
    let replacement = SandboxProvider::claim_for_effect(
        &provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &rebuild_fence,
            authority: &EffectAuthority::new(&rebuild_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Rebuild {
            source: Box::new(source_handle.clone()),
        },
    )
    .await
    .expect("KPV5b authorized Rebuild restores the Pod cleanup gate");
    let replacement_handle = replacement.handle();
    assert_eq!(container_id(&replacement_handle), pod_name, "KPV5b");
    assert_eq!(
        replacement_handle
            .container_payload()
            .unwrap()
            .runtime_handle
            .as_ref()
            .and_then(|handle| match handle {
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 { claim_uid, .. } =>
                    claim_uid.as_deref(),
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuation { .. }
                | awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV2 { .. } => None,
                // KPV5b cause: authorized Rebuild has a runnable replacement
                // Pod. Effect: a terminal-only guarded claim is inadmissible;
                // it can authorize cleanup but cannot serve this Rebuild.
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesGuardedClaim(_) => {
                    panic!("KPV5b Rebuild returned a terminal-only guarded claim")
                }
                awaken_sandbox_container::ContainerContinuationHandle::HostBindRestoration(_) => {
                    panic!("KPV5b Kubernetes replacement returned a host-bind handle")
                }
            }),
        Some(claim_uid.as_str()),
        "KPV5b exact P"
    );
    let replacement_terminal_fence = effect_fence(&scope, "terminal-b", "claim-only-owner", 3);
    replacement
        .prepare_disposal_for_effect(
            &replacement_terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                &replacement_terminal_fence,
            ),
        )
        .await
        .expect("KPV5c source-dependent preparation");
    replacement
        .dispose_for_effect(
            &disposal_authorization(&replacement_terminal_fence),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&replacement_terminal_fence).effect_fence(),
            ),
        )
        .await
        .expect("KPV5c Pod-gated B+P cleanup");
    assert!(pods.get_opt(&pod_name).await.unwrap().is_none(), "KPV5c");
    assert!(
        claims.get_opt(&claim_name).await.unwrap().is_none(),
        "KPV5c"
    );

    // Response-loss row: the persisted current V3 original-set handle and same live effect
    // authorize only the zero-mutation total-absence result.
    let replay = SandboxProvider::prepare_terminal_for_effect(
        &provider,
        &frozen,
        Some(&replacement_handle),
        Some(&rebuild_fence),
        &replacement_terminal_fence,
        &awaken_provisioning_contract::test_support::EffectAuthority::new(
            &replacement_terminal_fence,
        ),
    )
    .await
    .expect("KPV5c exact total-absence observation");
    assert!(replay.is_none(), "KPV5c exact disposed source has no work");
    assert!(pods.get_opt(&pod_name).await.unwrap().is_none(), "KPV5c");
    assert!(
        claims.get_opt(&claim_name).await.unwrap().is_none(),
        "KPV5c zero foreign effect"
    );
}

#[tokio::test]
async fn rebuild_delete_wins_before_cleanup_gate_without_mutating_the_retained_claim() {
    /* Terminal/PVC pre-intent interleaving table KPV6. Causes: C1 V2 evidence
     * identifies live Pod UID A and claim UID P; C2 a Rebuild DELETE wins and
     * makes A terminating before terminal cleanup acquires its production Pod
     * finalizer; C3 a test-only finalizer holds A so the interleaving is
     * observable; C4 P is still exact/live; C5 aggregate preparation already
     * made every source-dependent effect durable. Effects: E0 preparation has
     * zero Pod/PVC mutation; E1 stale cleanup rejects
     * before PVC I/O; E2 P has no deletionTimestamp; E3 after the test hold is
     * released, the already-authorized Rebuild creates B and reuses exact P;
     * E4 ordinary fenced cleanup removes B+P. Rule KPV6 C1+C2+C3+C4+C5=>
     * E0->E1+E2->E3->E4. The test finalizer delays only Rebuild's already-accepted
     * Pod deletion; it is teardown control, never cleanup authority or oracle. */
    const TEST_FINALIZER: &str = "awaken.dev/k8s-it-rebuild-wins";

    let Some(runtime) = live_runtime().await else {
        return;
    };
    let runtime = Arc::new(runtime.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    }));
    let fault_runtime = runtime.clone();
    let provider = Arc::new(
        ContainerProvider::new(runtime, fixture_image()).with_clock(awaken_time::system_clock()),
    );
    let scope = format!("claim-delete-rebuild-wins-{}", std::process::id());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let create_fence = effect_fence(&scope, "create", "delete-race-owner", 1);
    let source = SandboxProvider::claim_for_effect(
        provider.as_ref(),
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create_fence,
            authority: &EffectAuthority::new(&create_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("KPV6 C1 create retained source");
    let source_handle = source.handle();
    let pod_name = container_id(&source_handle);
    let (pod_uid, claim_uid) = match source_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .as_ref()
    {
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
            pod_uid,
            claim_uid: Some(claim_uid),
            ..
        }) => (pod_uid.clone(), claim_uid.clone()),
        other => panic!("KPV6 C1 expected Pod+claim V3 original-set handle, got {other:?}"),
    };

    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client.clone(), "default");
    let claims = kube::Api::<k8s_openapi::api::core::v1::PersistentVolumeClaim>::namespaced(
        client, "default",
    );
    let observed_pod = pods.get(&pod_name).await.unwrap();
    let claim_name = only_bound_claim_name(&observed_pod);
    assert_eq!(
        claims
            .get(&claim_name)
            .await
            .unwrap()
            .metadata
            .uid
            .as_deref(),
        Some(claim_uid.as_str()),
        "KPV6 C1 exact P"
    );
    k8s_common::set_test_finalizer(&pods, &pod_name, TEST_FINALIZER, true).await;

    // C2 is a test-owned external Pod fault, not a Session exec attempt. The
    // latter would leave a legitimate unobserved-exec quiescence debt when
    // PID 1 dies before its status frame. KPV6 tests the independent Rebuild
    // DELETE versus terminal-cleanup race, while the exec-status unit rows
    // retain the fail-closed unknown-status outcome.
    let terminator = fault_runtime
        .spawn(
            awaken_sandbox_container::AdmittedExec::unrecorded(&pod_name),
            pc::MaterializedCommand::new(["sh", "-c", "kill -TERM 1"]),
        )
        .await
        .expect("KPV6 make A terminal without a Session exec attempt");
    let _ = terminator.wait().await;
    let terminal_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let terminal = pods
            .get(&pod_name)
            .await
            .expect("KPV6 observe A")
            .status
            .and_then(|status| status.container_statuses)
            .and_then(|statuses| statuses.into_iter().find(|status| status.name == "agent"))
            .and_then(|status| status.state)
            .is_some_and(|state| state.terminated.is_some());
        if terminal {
            break;
        }
        assert!(
            tokio::time::Instant::now() < terminal_deadline,
            "KPV6 A never became terminal"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let terminal_fence = effect_fence(&scope, "terminal", "delete-race-owner", 2);
    source
        .prepare_disposal_for_effect(
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
        )
        .await
        .expect("KPV6/E0 persist source-dependent effects before physical cleanup");
    assert!(
        pods.get(&pod_name)
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none(),
        "KPV6/E0 preparation has zero Pod deletion effect"
    );
    assert!(
        claims
            .get(&claim_name)
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none(),
        "KPV6/E0 preparation has zero PVC deletion effect"
    );

    let rebuild_provider = provider.clone();
    let rebuild_spec = frozen.clone();
    let rebuild_scope = scope.clone();
    let rebuild_source_handle = source_handle.clone();
    let rebuild = tokio::spawn(async move {
        let fence = effect_fence(&rebuild_scope, "rebuild", "delete-race-owner", 2);
        SandboxProvider::claim_for_effect(
            rebuild_provider.as_ref(),
            &rebuild_spec,
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &fence,
                authority: &EffectAuthority::new(&fence),
            },
            awaken_provisioning_contract::SandboxRealizationIntent::Rebuild {
                source: Box::new(rebuild_source_handle),
            },
        )
        .await
    });
    let delete_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if pods
            .get(&pod_name)
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_some()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < delete_deadline,
            "KPV6 C2 Rebuild Pod deletion did not start"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let authorization = disposal_authorization(&terminal_fence);
    let stale_cleanup = tokio::spawn(async move {
        source
            .dispose_for_effect(
                &authorization,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    authorization.effect_fence(),
                ),
            )
            .await
    });
    let cleanup_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !stale_cleanup.is_finished()
        && claims
            .get(&claim_name)
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none()
        && tokio::time::Instant::now() < cleanup_deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let claim_terminating_before_release = claims
        .get(&claim_name)
        .await
        .unwrap()
        .metadata
        .deletion_timestamp
        .is_some();
    let cleanup_rejected_before_release = stale_cleanup.is_finished();

    k8s_common::set_test_finalizer(&pods, &pod_name, TEST_FINALIZER, false).await;

    let stale_cleanup_result =
        tokio::time::timeout(std::time::Duration::from_secs(60), stale_cleanup)
            .await
            .expect("KPV6 stale cleanup terminates")
            .expect("KPV6 stale cleanup task joins");
    let replacement_result = tokio::time::timeout(std::time::Duration::from_secs(60), rebuild)
        .await
        .expect("KPV6 Rebuild terminates")
        .expect("KPV6 Rebuild task joins");
    if let Ok(replacement) = replacement_result.as_ref() {
        let replacement_terminal = effect_fence(&scope, "terminal-b", "delete-race-owner", 3);
        replacement
            .prepare_disposal_for_effect(
                &replacement_terminal,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    &replacement_terminal,
                ),
            )
            .await
            .expect("KPV6 E4 prepare exact replacement cleanup");
        replacement
            .dispose_for_effect(
                &disposal_authorization(&replacement_terminal),
                &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    disposal_authorization(&replacement_terminal).effect_fence(),
                ),
            )
            .await
            .expect("KPV6 E4 exact replacement cleanup");
    }

    assert!(cleanup_rejected_before_release, "KPV6/E1");
    assert!(stale_cleanup_result.is_err(), "KPV6/E1");
    assert!(!claim_terminating_before_release, "KPV6/E2");
    let replacement = replacement_result.expect("KPV6/E3 Rebuild reuses exact live P");
    let replacement_handle = replacement.handle();
    assert_ne!(
        replacement_handle.container_physical_incarnation().unwrap(),
        pod_uid,
        "KPV6/E3 B has a new Pod UID"
    );
    assert!(
        claims.get_opt(&claim_name).await.unwrap().is_none(),
        "KPV6/E4"
    );
}

#[tokio::test]
async fn terminal_cleanup_fences_the_claim_before_releasing_the_source_pod() {
    /* Terminal/PVC interleaving table KPV4. Causes: C1 authoritative V2
     * evidence identifies live Pod UID A and retained claim UID P; C2 exact
     * terminal cleanup starts while test finalizers hold both objects; C3 an
     * authorized Rebuild B is attempted after A is absent but P is still
     * terminating; C4 a replacement Host observes the exact cut after PVC
     * DELETE acceptance and before physical completion; C5 aggregate
     * preparation already made every source-dependent effect durable. Effects:
     * E0 preparation has zero Pod/PVC deletion effect; E1 the
     * UID+resourceVersion-fenced PVC DELETE intent is observable while A still
     * occupies the stable Pod name; E2 cleanup then deletes only A and waits for
     * P; E3 B rejects at continuation admission and creates no Pod; E4 releasing
     * the claim finalizer lets the same cleanup finish; E5 C4 projects typed
     * Disposing and prepares only the exact physical cleanup owner, never a
     * runnable Environment. Rules: KPV4a C1+C2=>E1+E2; KPV4b E1+C3=>E3;
     * KPV4c E2+release(P)=>E4; KPV4d C1+C2+C4=>E5;
     * KPV4e C1+C5=>E0. Holding both API objects
     * creates the otherwise sub-millisecond ABA/crash window deterministically:
     * a reread after Pod deletion cannot satisfy KPV4a, while
     * delete-intent-before-Pod can. Test finalizer removal is teardown only and
     * never substitutes for the production dispose result. */
    const TEST_FINALIZER: &str = "awaken.dev/k8s-it-hold";

    let Some(runtime) = live_runtime().await else {
        return;
    };
    let runtime = Arc::new(runtime.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    }));
    let provider =
        ContainerProvider::new(runtime, fixture_image()).with_clock(awaken_time::system_clock());
    let scope = format!("claim-delete-order-{}", std::process::id());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let create_fence = effect_fence(&scope, "create", "delete-order-owner", 1);
    let source = SandboxProvider::claim_for_effect(
        &provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &create_fence,
            authority: &EffectAuthority::new(&create_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("KPV4 C1 create retained source");
    let source_handle = source.handle();
    let pod_name = container_id(&source_handle);
    let (pod_uid, claim_uid) = match source_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .as_ref()
    {
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
            pod_uid,
            claim_uid: Some(claim_uid),
            ..
        }) => (pod_uid.clone(), claim_uid.clone()),
        other => panic!("KPV4 C1 expected Pod+claim V3 original-set handle, got {other:?}"),
    };

    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client.clone(), "default");
    let claims = kube::Api::<k8s_openapi::api::core::v1::PersistentVolumeClaim>::namespaced(
        client, "default",
    );
    let observed_pod = pods.get(&pod_name).await.unwrap();
    let claim_name = only_bound_claim_name(&observed_pod);
    k8s_common::set_test_finalizer(&pods, &pod_name, TEST_FINALIZER, true).await;
    let observed_claim = claims.get(&claim_name).await.unwrap();
    assert_eq!(
        observed_claim.metadata.uid.as_deref(),
        Some(claim_uid.as_str()),
        "KPV4 C1 exact claim"
    );
    k8s_common::set_test_finalizer(&claims, &claim_name, TEST_FINALIZER, true).await;

    let terminal_fence = effect_fence(&scope, "terminal", "delete-order-owner", 2);
    source
        .prepare_disposal_for_effect(
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
        )
        .await
        .expect("KPV4e persist source-dependent effects before physical cleanup");
    assert!(
        pods.get(&pod_name)
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none(),
        "KPV4e/E0 preparation has zero Pod deletion effect"
    );
    assert!(
        claims
            .get(&claim_name)
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none(),
        "KPV4e/E0 preparation has zero PVC deletion effect"
    );
    let authorization = disposal_authorization(&terminal_fence);
    let disposal = tokio::spawn(async move {
        source
            .dispose_for_effect(
                &authorization,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    authorization.effect_fence(),
                ),
            )
            .await
    });
    let early_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let deletion_started_while_source_present = loop {
        let pod = pods.get_opt(&pod_name).await.unwrap();
        let claim = claims.get(&claim_name).await.unwrap();
        if pod
            .as_ref()
            .is_some_and(|pod| pod.metadata.uid.as_deref() == Some(pod_uid.as_str()))
            && claim.metadata.deletion_timestamp.is_some()
        {
            break true;
        }
        if tokio::time::Instant::now() >= early_deadline {
            break false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    let observed_disposing = SandboxProvider::observe(
        &provider,
        awaken_provisioning_contract::SandboxAdoption::new(&frozen, &source_handle),
        awaken_provisioning_contract::SandboxAdoption::new(&frozen, &source_handle)
            .effect_basis(&terminal_fence),
    )
    .await
    .expect("KPV4d observe the exact gated cleanup cut");
    assert!(
        matches!(
            observed_disposing,
            pc::SandboxObservation::Disposing { ref physical_incarnation }
                if physical_incarnation == &pod_uid
        ),
        "KPV4d/E5: {observed_disposing:?}"
    );
    let physical_cleanup = SandboxProvider::prepare_terminal_for_effect(
        &provider,
        &frozen,
        Some(&source_handle),
        Some(&create_fence),
        &terminal_fence,
        &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
    )
    .await
    .expect("KPV4d prepare exact Disposing cleanup")
    .expect("KPV4d exact Pod/PVC cut still has a physical cleanup owner");
    drop(physical_cleanup);

    k8s_common::set_test_finalizer(&pods, &pod_name, TEST_FINALIZER, false).await;

    let absent_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let pod_absent = pods.get_opt(&pod_name).await.unwrap().is_none();
        let claim_terminating = claims
            .get(&claim_name)
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_some();
        if pod_absent && claim_terminating {
            break;
        }
        assert!(
            tokio::time::Instant::now() < absent_deadline,
            "KPV4 source Pod deletion or claim termination timed out"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let rebuild_fence = effect_fence(&scope, "rebuild", "delete-order-owner", 2);
    let rebuild = SandboxProvider::claim_for_effect(
        &provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &rebuild_fence,
            authority: &EffectAuthority::new(&rebuild_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Rebuild {
            source: Box::new(source_handle.clone()),
        },
    )
    .await;
    let no_replacement_pod = pods.get_opt(&pod_name).await.unwrap().is_none();

    k8s_common::set_test_finalizer(&claims, &claim_name, TEST_FINALIZER, false).await;
    tokio::time::timeout(std::time::Duration::from_secs(60), disposal)
        .await
        .expect("KPV4 production dispose completes after finalizer release")
        .expect("KPV4 dispose task joins")
        .expect("KPV4 production dispose succeeds");

    assert!(
        deletion_started_while_source_present,
        "KPV4a PVC deletion intent must precede releasing Pod A"
    );
    let rebuild_error = match rebuild {
        Ok(_) => panic!("KPV4b terminating claim must reject Rebuild"),
        Err(error) => error,
    };
    assert!(
        rebuild_error.to_string().contains("terminating"),
        "KPV4b: {rebuild_error}"
    );
    assert!(no_replacement_pod, "KPV4b zero Pod effect");
    assert!(
        claims.get_opt(&claim_name).await.unwrap().is_none(),
        "KPV4c"
    );
}

#[tokio::test]
async fn foreign_pod_is_rejected_before_continuation_claim_creation() {
    /* Stable-scope preflight table KPV2. Causes: C1 exact effect A owns a live
     * Pod with V2 Pod UID and no continuation claim; C2 a retained provider
     * presents conflicting effect B at the same generation; C3 the canonical
     * PVC is absent. Effects: E1 reject B before PVC creation; E2 preserve A;
     * E3 cleanup A only through its V2 UID and a later authorized fence. Rule
     * KPV2 C1+C2+C3=>E1+E2, then terminal fence=>E3. The retired test claimed a
     * claim had first been created and rolled back, but unified preflight now
     * rejects before that mutation; this comment and assertion own the real cut.
     */
    let Some(plain) = live_runtime().await else {
        return;
    };
    let plain_runtime = Arc::new(plain);
    let plain_provider = ContainerProvider::new(plain_runtime.clone(), fixture_image())
        .with_clock(awaken_time::system_clock());
    let scope = format!("claim-preflight-{}", std::process::id());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let owner_fence = effect_fence(&scope, "create-a", "preflight-owner-a", 1);
    let owner_environment = SandboxProvider::claim_for_effect(
        &plain_provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &owner_fence,
            authority: &EffectAuthority::new(&owner_fence),
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .unwrap();
    let owner_handle = owner_environment.handle();
    let pod = container_id(&owner_handle);
    assert!(
        matches!(
            owner_handle
                .container_payload()
                .unwrap()
                .runtime_handle
                .as_ref(),
            Some(
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
                    claim_uid: None,
                    ..
                }
            )
        ),
        "KPV2 C1 Pod-only V3 original-set handle"
    );
    let Some(retained) = live_runtime().await else {
        let terminal_fence = effect_fence(&scope, "terminal-a", "preflight-owner-a", 2);
        owner_environment
            .prepare_disposal_for_effect(
                &terminal_fence,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
            )
            .await
            .unwrap();
        owner_environment
            .dispose_for_effect(
                &disposal_authorization(&terminal_fence),
                &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    disposal_authorization(&terminal_fence).effect_fence(),
                ),
            )
            .await
            .unwrap();
        return;
    };
    let retained_runtime = Arc::new(retained.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    }));
    let retained_provider = ContainerProvider::new(retained_runtime, fixture_image())
        .with_clock(awaken_time::system_clock());
    let foreign_fence = effect_fence(&scope, "create-b", "preflight-owner-b", 1);
    let authority = EffectAuthority::new(&foreign_fence);
    let error = SandboxProvider::claim_for_effect(
        &retained_provider,
        &frozen,
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &foreign_fence,
            authority: &authority,
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    );
    let error = error.await.err().expect("KPV2 foreign effect rejects");
    assert!(
        error.to_string().contains("newer or conflicting effect"),
        "KPV2 E1: {error}"
    );

    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    let claims = kube::Api::<k8s_openapi::api::core::v1::PersistentVolumeClaim>::namespaced(
        client, "default",
    );
    // A PVC created by this attempt must carry the same aggregate effect
    // operation. Querying that persisted authority avoids duplicating the
    // adapter-private Pod-to-claim name projection in the fixture.
    let foreign_claim_created = claims
        .list(&kube::api::ListParams::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .any(|claim| {
            claim
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get("awaken.dev/sandbox-effect"))
                .is_some_and(|operation| operation == foreign_fence.operation_id())
        });
    assert!(!foreign_claim_created, "KPV2 E1 zero PVC creation");
    assert_eq!(
        plain_runtime.inspect(&pod).await.unwrap(),
        ContainerState::Running,
        "KPV2 E2"
    );
    let terminal_fence = effect_fence(&scope, "terminal-a", "preflight-owner-a", 2);
    owner_environment
        .prepare_disposal_for_effect(
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
        )
        .await
        .expect("KPV2 E3 prepare exact V2 cleanup");
    owner_environment
        .dispose_for_effect(
            &disposal_authorization(&terminal_fence),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&terminal_fence).effect_fence(),
            ),
        )
        .await
        .expect("KPV2 E3 exact V2 cleanup");
}
