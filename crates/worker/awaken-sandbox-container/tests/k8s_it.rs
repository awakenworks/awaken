//! Real Kubernetes integration (ADR-0041 Slice 5), gated on the `k8s` feature AND a
//! reachable cluster (in-cluster SA or kubeconfig) — skips cleanly otherwise.
//!
//! Run with: `cargo test -p awaken-sandbox-container --features k8s --test k8s_it`
#![cfg(feature = "k8s")]

#[path = "common/restore.rs"]
mod common;
mod k8s_common;

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::k8s::K8sRuntime;
use awaken_sandbox_container::{
    ContainerEnvironmentProvider, ContainerPlan, ContainerProvider, ContainerRealizationIntent,
    ContainerRuntime, ContainerState, K8sContinuationVolume, NetworkMode, RootfsPlan,
};
use k8s_common::{
    container_id, disposal_authorization, effect_fence, fixture_image, spec_with_command,
};

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
    let runtime = match K8sRuntime::connect("default", addr).await {
        Ok(runtime) => runtime,
        Err(error) => panic!("AWAKEN_K8S_E2E=1 requires a kube client: {error}"),
    };
    match runtime.ping().await {
        Ok(()) => Some(runtime),
        Err(error) => panic!("AWAKEN_K8S_E2E=1 requires a reachable apiserver: {error}"),
    }
}

#[tokio::test]
async fn k8s_pod_lifecycle_against_a_real_cluster() {
    /* Managed lifecycle decision table K1. Causes: C1 an opaque Session scope
     * contains `_`/`:` and is unique to this live run; C2 the aggregate effect
     * and frozen spec are exact/different; C3 the apiserver supplies a V2 Pod
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
    let provider = ContainerProvider::new(runtime.clone(), fixture_image());
    let desired = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let create_fence = effect_fence(&scope, "create", "lifecycle-owner", 1);
    let first = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &desired,
        Some(&create_fence),
        None,
        ContainerRealizationIntent::Create,
    )
    .await
    .expect("create a real Pod via the apiserver");
    let first_handle = first.handle();
    let id = container_id(&first_handle);
    assert!(id.starts_with("awaken-h-"), "K1a bounded opaque name: {id}");
    assert!(id.len() <= 63, "K1a DNS label length: {id}");

    let retried = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &desired,
        Some(&create_fence),
        None,
        ContainerRealizationIntent::Create,
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
    let mismatch = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &changed,
        Some(&create_fence),
        None,
        ContainerRealizationIntent::Create,
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
    retried
        .prepare_disposal_for_effect(&terminal_fence)
        .await
        .expect("K1c prepare source durability without deleting the Pod");
    assert_eq!(
        runtime.inspect(&id).await.unwrap(),
        ContainerState::Running,
        "K1c/E4 preparation has zero physical effect"
    );
    retried
        .dispose_for_effect(&disposal_authorization(&terminal_fence))
        .await
        .expect("K1c/E5 delete the exact Pod UID");
    assert_eq!(runtime.inspect(&id).await.unwrap(), ContainerState::Gone);
}

#[tokio::test]
async fn k8s_exact_restore_survives_provider_replacement_with_the_same_pvc_fence() {
    /* Live Kubernetes exact-restore table. C1 Pod/PVC target absent; C2 the
     * first provider wrapper drops before aggregate CAS; C3 a fresh kube client
     * retries the exact tuple; C4 the same effect carries another generation.
     * Effects: E1 create one annotated Pod/PVC and publish both physical UID fences;
     * E2 C2+C3 recover the identical handle; E3 C4 fails without reaping either
     * object; E4 terminal disposal removes both. Rules KR1=C1=>E1;
     * KR2=C2+C3=>E2; KR3=C4=>E3; KR4=dispose=>E4. */
    let Some(first_runtime) = live_runtime().await else {
        return;
    };
    let first_runtime = first_runtime.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    });
    let scope = format!("k8s-restore-{}", std::process::id());
    let spec = common::exact_restore_spec(&scope);
    let request = common::exact_restore_request(&scope);
    let image =
        std::env::var("AWAKEN_K8S_FIXTURE_IMAGE").unwrap_or_else(|_| "awaken-bb:1".to_string());
    let first = ContainerProvider::new(Arc::new(first_runtime), image.clone())
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
    let pod_name = payload.container_id.clone();
    let (pod_uid, claim_uid) = match payload.runtime_handle.as_ref() {
        Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 {
            pod_uid,
            claim_uid: Some(claim_uid),
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
    let retry_runtime = retry_runtime.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    });
    let retry = ContainerProvider::new(Arc::new(retry_runtime), image);
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
    retry
        .dispose_restored_environment(&spec, &request)
        .await
        .expect("KR4 provider exact terminal cleanup");
    retry
        .dispose_restored_environment(&spec, &request)
        .await
        .expect("KR4 provider absent replay");

    let client = kube::Client::try_default()
        .await
        .expect("KR4 read cleanup state");
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client.clone(), "default");
    let claims = kube::Api::<k8s_openapi::api::core::v1::PersistentVolumeClaim>::namespaced(
        client, "default",
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
    let client = kube::Client::try_default().await.unwrap();
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
                    annotations.get(
                        awaken_sandbox_container::k8s_package_realization::SANDBOX_SCOPE_ANNOTATION,
                    )
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
     * C3 the durable V2 handle supplies A+P; C4 an authorized Rebuild uses the
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
    let provider = ContainerProvider::new(runtime.clone(), fixture_image());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sh", "-c", "sleep 300"]));
    let create_fence = effect_fence(&scope, "create", "terminal-owner", 1);
    let first_environment = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &frozen,
        Some(&create_fence),
        None,
        ContainerRealizationIntent::Create,
    )
    .await
    .expect("create first incarnation");
    let first_handle = first_environment.handle();
    let first = container_id(&first_handle);
    let source_incarnation = first_handle
        .container_physical_incarnation()
        .expect("KTR1 V2 handle carries Pod UID")
        .to_owned();
    let source_runtime_handle = first_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .clone();
    assert!(
        matches!(
            source_runtime_handle.as_ref(),
            Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 {
                claim_uid: Some(_),
                ..
            })
        ),
        "KTR1 V2 handle carries retained claim UID"
    );
    let client = kube::Client::try_default()
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
    let replacement_environment = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &frozen,
        Some(&rebuild_fence),
        None,
        ContainerRealizationIntent::Rebuild {
            source_incarnation,
            source_runtime_handle,
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
        .prepare_disposal_for_effect(&terminal_fence)
        .await
        .expect("KTR2 prepare replacement disposal");
    replacement_environment
        .dispose_for_effect(&disposal_authorization(&terminal_fence))
        .await
        .expect("KTR2 delete replacement and retained claim");
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
    let provider = ContainerProvider::new(runtime.clone(), fixture_image());
    let scope = format!("claim-fence-{}", std::process::id());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let first_fence = effect_fence(&scope, "create-a", "claim-owner", 1);
    let first_environment = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &frozen,
        Some(&first_fence),
        None,
        ContainerRealizationIntent::Create,
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
        .expect("KPV1 first V2 handle");
    let first_terminal = effect_fence(&scope, "dispose-a", "claim-owner", 2);
    first_environment
        .prepare_disposal_for_effect(&first_terminal)
        .await
        .unwrap();
    first_environment
        .dispose_for_effect(&disposal_authorization(&first_terminal))
        .await
        .unwrap();

    let second_fence = effect_fence(&scope, "create-b", "claim-owner", 3);
    let second_environment = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &frozen,
        Some(&second_fence),
        None,
        ContainerRealizationIntent::Create,
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
        .expect("KPV1 current V2 handle");
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
        .prepare_disposal_for_effect(&second_terminal)
        .await
        .unwrap();
    second_environment
        .dispose_for_effect(&disposal_authorization(&second_terminal))
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
     * PVC mutation and P remains; E2 A creates Pod UID A2, persists a V2 handle,
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
    let provider = ContainerProvider::new(runtime.clone(), fixture_image());
    let scope = format!("claim-orphan-{}", std::process::id());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let create_fence = effect_fence(&scope, "create", "orphan-owner-a", 1);
    let first = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &frozen,
        Some(&create_fence),
        None,
        ContainerRealizationIntent::Create,
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
        Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 {
            pod_uid,
            claim_uid: Some(claim_uid),
        }) => (pod_uid.clone(), claim_uid.clone()),
        other => panic!("KPV3 C1 expected Pod+claim V2 handle, got {other:?}"),
    };

    let client = kube::Client::try_default().await.unwrap();
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
    let foreign = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &frozen,
        Some(&foreign_fence),
        None,
        ContainerRealizationIntent::Create,
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

    let recovered = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &frozen,
        Some(&create_fence),
        None,
        ContainerRealizationIntent::Create,
    )
    .await
    .expect("KPV3b exact response-loss replay");
    let recovered_handle = recovered.handle();
    let (recovered_pod_uid, recovered_claim_uid) = match recovered_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .as_ref()
    {
        Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 {
            pod_uid,
            claim_uid: Some(claim_uid),
        }) => (pod_uid, claim_uid),
        other => panic!("KPV3b expected Pod+claim V2 handle, got {other:?}"),
    };
    assert_ne!(recovered_pod_uid, &first_pod_uid, "KPV3b new Pod UID");
    assert_eq!(recovered_claim_uid, &claim_uid, "KPV3b exact claim UID");
    let terminal_fence = effect_fence(&scope, "terminal", "orphan-owner-a", 2);
    recovered
        .prepare_disposal_for_effect(&terminal_fence)
        .await
        .expect("KPV3c source-dependent preparation");
    recovered
        .dispose_for_effect(&disposal_authorization(&terminal_fence))
        .await
        .expect("KPV3c exact cleanup");
    assert!(
        claims.get_opt(&claim_name).await.unwrap().is_none(),
        "KPV3c"
    );
}

#[tokio::test]
async fn absent_source_pod_with_live_claim_fails_closed_until_rebuild_serializes_cleanup() {
    /* Absent-primary terminal table KPV5. Causes: C1 the durable V2 handle
     * identifies Pod UID A plus retained claim UID P; C2 an exact API fault cut
     * removes A while P remains live; C3 terminal preparation carries the same
     * frozen spec and a later aggregate-authorized effect; C4 authorized
     * Rebuild recreates Pod B against P; C5 exact B cleanup succeeds but its
     * response is lost. Effects: E1 orphan P rejects terminal preparation with
     * zero PVC deletion effect; E2 Rebuild preserves exact P under B; E3 B's
     * Pod-finalizer-serialized cleanup removes B+P; E4 same V2/effect replay
     * observes total absence and returns `None` idempotently. Rules: KPV5a
     * C1+C2+C3=>E1; KPV5b E1+C4=>E2; KPV5c E2+C5=>E3+E4. */
    let Some(runtime) = live_runtime().await else {
        return;
    };
    let runtime = Arc::new(runtime.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    }));
    let provider = ContainerProvider::new(runtime, fixture_image());
    let scope = format!("claim-only-terminal-{}", std::process::id());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let create_fence = effect_fence(&scope, "create", "claim-only-owner", 1);
    let source = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &frozen,
        Some(&create_fence),
        None,
        ContainerRealizationIntent::Create,
    )
    .await
    .expect("KPV5 C1 create retained source");
    let source_handle = source.handle();
    let pod_name = container_id(&source_handle);
    let (pod_uid, claim_uid, source_runtime_handle) = match source_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .as_ref()
    {
        Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 {
            pod_uid,
            claim_uid: Some(claim_uid),
        }) => (
            pod_uid.clone(),
            claim_uid.clone(),
            source_handle
                .container_payload()
                .unwrap()
                .runtime_handle
                .clone(),
        ),
        other => panic!("KPV5 C1 expected Pod+claim V2 handle, got {other:?}"),
    };

    let client = kube::Client::try_default().await.unwrap();
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

    let terminal_fence = effect_fence(&scope, "terminal", "claim-only-owner", 2);
    let error = match ContainerEnvironmentProvider::prepare_terminal_environment_for_effect(
        &provider,
        &frozen,
        Some(&source_handle),
        Some(&create_fence),
        &terminal_fence,
    )
    .await
    {
        Ok(_) => panic!("KPV5a orphan live claim must fail closed"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("cannot be disposed"),
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

    let rebuild_fence = effect_fence(&scope, "rebuild", "claim-only-owner", 2);
    let replacement = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &frozen,
        Some(&rebuild_fence),
        None,
        ContainerRealizationIntent::Rebuild {
            source_incarnation: pod_uid,
            source_runtime_handle,
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
                pc::ContainerContinuationHandle::KubernetesContinuationV2 { claim_uid, .. } =>
                    claim_uid.as_deref(),
                pc::ContainerContinuationHandle::KubernetesContinuation { .. } => None,
                pc::ContainerContinuationHandle::HostBindRestoration(_) => {
                    panic!("KPV5b Kubernetes replacement returned a host-bind handle")
                }
            }),
        Some(claim_uid.as_str()),
        "KPV5b exact P"
    );
    let replacement_terminal_fence = effect_fence(&scope, "terminal-b", "claim-only-owner", 3);
    replacement
        .prepare_disposal_for_effect(&replacement_terminal_fence)
        .await
        .expect("KPV5c source-dependent preparation");
    replacement
        .dispose_for_effect(&disposal_authorization(&replacement_terminal_fence))
        .await
        .expect("KPV5c Pod-gated B+P cleanup");
    assert!(pods.get_opt(&pod_name).await.unwrap().is_none(), "KPV5c");
    assert!(
        claims.get_opt(&claim_name).await.unwrap().is_none(),
        "KPV5c"
    );

    // Response-loss row: the persisted current V2 handle and same live effect
    // authorize only the zero-mutation total-absence result.
    let replay = ContainerEnvironmentProvider::prepare_terminal_environment_for_effect(
        &provider,
        &frozen,
        Some(&replacement_handle),
        Some(&rebuild_fence),
        &replacement_terminal_fence,
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
    let provider = Arc::new(ContainerProvider::new(runtime, fixture_image()));
    let scope = format!("claim-delete-rebuild-wins-{}", std::process::id());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let create_fence = effect_fence(&scope, "create", "delete-race-owner", 1);
    let source = ContainerEnvironmentProvider::create_environment_for_effect(
        provider.as_ref(),
        &frozen,
        Some(&create_fence),
        None,
        ContainerRealizationIntent::Create,
    )
    .await
    .expect("KPV6 C1 create retained source");
    let source_handle = source.handle();
    let pod_name = container_id(&source_handle);
    let (pod_uid, claim_uid, runtime_handle) = match source_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .as_ref()
    {
        Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 {
            pod_uid,
            claim_uid: Some(claim_uid),
        }) => (
            pod_uid.clone(),
            claim_uid.clone(),
            source_handle
                .container_payload()
                .unwrap()
                .runtime_handle
                .clone(),
        ),
        other => panic!("KPV6 C1 expected Pod+claim V2 handle, got {other:?}"),
    };

    let client = kube::Client::try_default().await.unwrap();
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
    let mut finalizers = observed_pod.metadata.finalizers.unwrap_or_default();
    finalizers.push(TEST_FINALIZER.into());
    pods.patch(
        &pod_name,
        &kube::api::PatchParams::default(),
        &kube::api::Patch::Merge(serde_json::json!({
            "metadata": { "finalizers": finalizers }
        })),
    )
    .await
    .expect("KPV6 C3 hold the accepted Rebuild deletion");

    let terminator = pc::Sandbox::spawn(
        source.as_ref(),
        pc::Command::new(["sh", "-c", "kill -TERM 1"]),
    )
    .await
    .expect("KPV6 make A terminal without changing the frozen realization plan");
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
        .prepare_disposal_for_effect(&terminal_fence)
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
    let rebuild_source_uid = pod_uid.clone();
    let rebuild = tokio::spawn(async move {
        let fence = effect_fence(&rebuild_scope, "rebuild", "delete-race-owner", 2);
        ContainerEnvironmentProvider::create_environment_for_effect(
            rebuild_provider.as_ref(),
            &rebuild_spec,
            Some(&fence),
            None,
            ContainerRealizationIntent::Rebuild {
                source_incarnation: rebuild_source_uid,
                source_runtime_handle: runtime_handle,
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
    let stale_cleanup =
        tokio::spawn(async move { source.dispose_for_effect(&authorization).await });
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

    let held = pods.get(&pod_name).await.unwrap();
    let remaining = held
        .metadata
        .finalizers
        .unwrap_or_default()
        .into_iter()
        .filter(|finalizer| finalizer != TEST_FINALIZER)
        .collect::<Vec<_>>();
    pods.patch(
        &pod_name,
        &kube::api::PatchParams::default(),
        &kube::api::Patch::Merge(serde_json::json!({
            "metadata": { "finalizers": remaining }
        })),
    )
    .await
    .expect("KPV6 release test-only Pod hold");

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
            .prepare_disposal_for_effect(&replacement_terminal)
            .await
            .expect("KPV6 E4 prepare exact replacement cleanup");
        replacement
            .dispose_for_effect(&disposal_authorization(&replacement_terminal))
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
    let provider = ContainerProvider::new(runtime, fixture_image());
    let scope = format!("claim-delete-order-{}", std::process::id());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let create_fence = effect_fence(&scope, "create", "delete-order-owner", 1);
    let source = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &frozen,
        Some(&create_fence),
        None,
        ContainerRealizationIntent::Create,
    )
    .await
    .expect("KPV4 C1 create retained source");
    let source_handle = source.handle();
    let pod_name = container_id(&source_handle);
    let (pod_uid, claim_uid, runtime_handle) = match source_handle
        .container_payload()
        .unwrap()
        .runtime_handle
        .as_ref()
    {
        Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 {
            pod_uid,
            claim_uid: Some(claim_uid),
        }) => (
            pod_uid.clone(),
            claim_uid.clone(),
            source_handle
                .container_payload()
                .unwrap()
                .runtime_handle
                .clone(),
        ),
        other => panic!("KPV4 C1 expected Pod+claim V2 handle, got {other:?}"),
    };

    let client = kube::Client::try_default().await.unwrap();
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client.clone(), "default");
    let claims = kube::Api::<k8s_openapi::api::core::v1::PersistentVolumeClaim>::namespaced(
        client, "default",
    );
    let observed_pod = pods.get(&pod_name).await.unwrap();
    let claim_name = only_bound_claim_name(&observed_pod);
    let mut pod_finalizers = observed_pod.metadata.finalizers.unwrap_or_default();
    pod_finalizers.push(TEST_FINALIZER.into());
    pods.patch(
        &pod_name,
        &kube::api::PatchParams::default(),
        &kube::api::Patch::Merge(serde_json::json!({
            "metadata": { "finalizers": pod_finalizers }
        })),
    )
    .await
    .expect("KPV4 hold Pod deletion");
    let observed_claim = claims.get(&claim_name).await.unwrap();
    assert_eq!(
        observed_claim.metadata.uid.as_deref(),
        Some(claim_uid.as_str()),
        "KPV4 C1 exact claim"
    );
    let mut claim_finalizers = observed_claim.metadata.finalizers.unwrap_or_default();
    claim_finalizers.push(TEST_FINALIZER.into());
    claims
        .patch(
            &claim_name,
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(serde_json::json!({
                "metadata": { "finalizers": claim_finalizers }
            })),
        )
        .await
        .expect("KPV4 hold claim deletion");

    let terminal_fence = effect_fence(&scope, "terminal", "delete-order-owner", 2);
    source
        .prepare_disposal_for_effect(&terminal_fence)
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
    let disposal = tokio::spawn(async move { source.dispose_for_effect(&authorization).await });
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

    let observed_disposing = ContainerEnvironmentProvider::observe_environment_for_effect(
        &provider,
        awaken_sandbox_container::ContainerEnvironmentAdoption::new(&frozen, &source_handle),
        &terminal_fence,
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
    let physical_cleanup = ContainerEnvironmentProvider::prepare_terminal_environment_for_effect(
        &provider,
        &frozen,
        Some(&source_handle),
        Some(&create_fence),
        &terminal_fence,
    )
    .await
    .expect("KPV4d prepare exact Disposing cleanup")
    .expect("KPV4d exact Pod/PVC cut still has a physical cleanup owner");
    drop(physical_cleanup);

    let held_pod = pods.get(&pod_name).await.unwrap();
    let remaining_pod_finalizers = held_pod
        .metadata
        .finalizers
        .unwrap_or_default()
        .into_iter()
        .filter(|finalizer| finalizer != TEST_FINALIZER)
        .collect::<Vec<_>>();
    pods.patch(
        &pod_name,
        &kube::api::PatchParams::default(),
        &kube::api::Patch::Merge(serde_json::json!({
            "metadata": { "finalizers": remaining_pod_finalizers }
        })),
    )
    .await
    .expect("KPV4 release Pod deletion");

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
    let rebuild = ContainerEnvironmentProvider::create_environment_for_effect(
        &provider,
        &frozen,
        Some(&rebuild_fence),
        None,
        ContainerRealizationIntent::Rebuild {
            source_incarnation: pod_uid,
            source_runtime_handle: runtime_handle,
        },
    )
    .await;
    let no_replacement_pod = pods.get_opt(&pod_name).await.unwrap().is_none();

    let held_claim = claims.get(&claim_name).await.unwrap();
    let remaining_claim_finalizers = held_claim
        .metadata
        .finalizers
        .unwrap_or_default()
        .into_iter()
        .filter(|finalizer| finalizer != TEST_FINALIZER)
        .collect::<Vec<_>>();
    claims
        .patch(
            &claim_name,
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(serde_json::json!({
                "metadata": { "finalizers": remaining_claim_finalizers }
            })),
        )
        .await
        .expect("KPV4 release claim deletion");
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
    let plain_provider = ContainerProvider::new(plain_runtime.clone(), fixture_image());
    let scope = format!("claim-preflight-{}", std::process::id());
    let frozen = spec_with_command(&scope, fixture_image(), command(&["sleep", "30"]));
    let owner_fence = effect_fence(&scope, "create-a", "preflight-owner-a", 1);
    let owner_environment = ContainerEnvironmentProvider::create_environment_for_effect(
        &plain_provider,
        &frozen,
        Some(&owner_fence),
        None,
        ContainerRealizationIntent::Create,
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
            Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 {
                claim_uid: None,
                ..
            })
        ),
        "KPV2 C1 Pod-only V2 handle"
    );
    let Some(retained) = live_runtime().await else {
        let terminal_fence = effect_fence(&scope, "terminal-a", "preflight-owner-a", 2);
        owner_environment
            .prepare_disposal_for_effect(&terminal_fence)
            .await
            .unwrap();
        owner_environment
            .dispose_for_effect(&disposal_authorization(&terminal_fence))
            .await
            .unwrap();
        return;
    };
    let retained_runtime = Arc::new(retained.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    }));
    let retained_provider = ContainerProvider::new(retained_runtime, fixture_image());
    let foreign_fence = effect_fence(&scope, "create-b", "preflight-owner-b", 1);
    let error = ContainerEnvironmentProvider::create_environment_for_effect(
        &retained_provider,
        &frozen,
        Some(&foreign_fence),
        None,
        ContainerRealizationIntent::Create,
    );
    let error = error.await.err().expect("KPV2 foreign effect rejects");
    assert!(
        error.to_string().contains("newer or conflicting effect"),
        "KPV2 E1: {error}"
    );

    let client = kube::Client::try_default().await.unwrap();
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
                .is_some_and(|operation| operation == &foreign_fence.operation_id)
        });
    assert!(!foreign_claim_created, "KPV2 E1 zero PVC creation");
    assert_eq!(
        plain_runtime.inspect(&pod).await.unwrap(),
        ContainerState::Running,
        "KPV2 E2"
    );
    let terminal_fence = effect_fence(&scope, "terminal-a", "preflight-owner-a", 2);
    owner_environment
        .prepare_disposal_for_effect(&terminal_fence)
        .await
        .expect("KPV2 E3 prepare exact V2 cleanup");
    owner_environment
        .dispose_for_effect(&disposal_authorization(&terminal_fence))
        .await
        .expect("KPV2 E3 exact V2 cleanup");
}
