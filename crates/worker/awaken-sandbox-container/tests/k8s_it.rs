//! Real Kubernetes integration (ADR-0041 Slice 5), gated on the `k8s` feature AND a
//! reachable cluster (in-cluster SA or kubeconfig) — skips cleanly otherwise.
//!
//! Run with: `cargo test -p awaken-sandbox-container --features k8s --test k8s_it`
#![cfg(feature = "k8s")]

#[path = "common/restore.rs"]
mod common;

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::k8s::K8sRuntime;
use awaken_sandbox_container::{
    ContainerEnvironmentProvider, ContainerPlan, ContainerProvider, ContainerRuntime,
    ContainerState, K8sContinuationVolume, NetworkMode, RootfsPlan,
};

fn plan(cmd: &[&str]) -> ContainerPlan {
    ContainerPlan {
        // The live-suite script imports this exact image into the isolated k3d
        // node. One injected source of truth prevents a tag mismatch from
        // silently turning the test into an external-registry availability test.
        image: std::env::var("AWAKEN_K8S_FIXTURE_IMAGE")
            .unwrap_or_else(|_| "awaken-bb:1".to_string()),
        command: cmd.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        control_services: Default::default(),
        packages: Default::default(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: NetworkMode::Open,
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
    // Cause/effect decision-table rule K1:
    // - Cause: a managed Session scope contains `_` and `:`, both invalid in a
    //   Kubernetes DNS label, while the cluster is reachable.
    // - Effect: create maps the opaque scope to the exact reversible runtime
    //   identity, the apiserver accepts it, an identical retry adopts that exact
    //   realization, a changed plan fails closed, inspect observes a live sandbox,
    //   and remove waits until that same Pod is absent.
    // This is the original production failure path; using a DNS-safe fixture
    // here would not prove the adapter boundary handles managed Session IDs.
    const SCOPE: &str = "sesn_fnv1a64:a13b83a56e2f77d0";
    const POD_NAME: &str = "awaken-sesn-5ffnv1a64-3aa13b83a56e2f77d0";

    let Some(rt) = live_runtime().await else {
        return;
    };

    // Clear a leftover Pod. `remove` returns only after the API observes 404, so
    // recreating the deterministic name cannot race a terminating incarnation.
    let _ = rt.remove(POD_NAME).await;

    // process-as-container Pod (busybox sleep as the container command).
    let desired = plan(&["sleep", "30"]);
    let id = rt
        .create(SCOPE, &desired)
        .await
        .expect("create a real Pod via the apiserver");
    assert_eq!(id, POD_NAME);

    let retried = rt
        .create(SCOPE, &desired)
        .await
        .expect("an identical retry must adopt the existing realization");
    assert_eq!(retried, id);
    let mismatch = rt
        .create(SCOPE, &plan(&["sleep", "31"]))
        .await
        .expect_err("a changed plan must not adopt a same-name Pod");
    assert!(mismatch.to_string().contains("different realization"));

    // Pending/Running both project to a live sandbox.
    assert!(matches!(
        rt.inspect(&id).await.unwrap(),
        ContainerState::Running
    ));

    // Teardown deletes the Pod.
    rt.remove(&id).await.expect("delete the Pod");
}

#[tokio::test]
async fn k8s_exact_restore_survives_provider_replacement_with_the_same_pvc_fence() {
    /* Live Kubernetes exact-restore table. C1 Pod/PVC target absent; C2 the
     * first provider wrapper drops before aggregate CAS; C3 a fresh kube client
     * retries the exact tuple; C4 the same effect carries another generation.
     * Effects: E1 create one annotated Pod/PVC and publish its PVC UID fence;
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
    let claim_uid = match payload.runtime_handle.as_ref() {
        Some(pc::ContainerContinuationHandle::KubernetesContinuation { claim_uid }) => {
            claim_uid.clone()
        }
        other => panic!("KR1/E1 missing PVC incarnation fence: {other:?}"),
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
    assert!(!claim_uid.is_empty(), "KR1/E1");
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
    let mut p = plan(&["sleep", "5"]);
    p.limits = pc::ResourceLimits {
        pids: Some(64),
        ..Default::default()
    };
    let err = rt
        .create("pids-reject", &p)
        .await
        .expect_err("a pids-limited spec must fail closed on k8s, not silently drop the cap");
    assert!(
        format!("{err}").contains("pids"),
        "the error names the unenforceable limit: {err}"
    );
    // Fail-closed BEFORE the Pod: nothing was created to clean up.
    assert_eq!(
        rt.inspect("pids-reject").await.unwrap(),
        ContainerState::Gone,
        "the fail-closed admission path must leave no Pod behind"
    );
}

#[tokio::test]
async fn a_terminal_pod_is_replaced_under_its_observed_identity_fence() {
    /* Terminal-recovery FMECA graph on a real apiserver. C1 the deterministic
     * Pod name exists; C2 it reaches Succeeded; C3 Kubernetes supplies UID and
     * resourceVersion; C4 its non-Pod-owned continuation PVC contains a marker.
     * Effects: E1 create deletes only that observed Pod incarnation, E2 recreates
     * the deterministic name with a different UID, E3 reattaches the PVC and
     * reaches Running only after reading the marker, and E4 explicit remove
     * cleans up Pod and claim. The pure decision table owns missing-identity
     * C5 => preserve. Rule KTR1=C1+C2+C3+C4=>E1+E2+E3, then dispose=>E4.
     * FMECA mitigation exercised: terminal-Pod GC must not cascade into active
     * volume loss (S5/O2/D3=30).
     */
    let Some(rt) = live_runtime().await else {
        return;
    };
    let rt = rt.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    });
    let scope = format!("terminal-replace-{}", std::process::id());
    let first = rt
        .create(
            &scope,
            &plan(&[
                "sh",
                "-c",
                "printf preserved > /workspace/recovery-proof && sleep 1",
            ]),
        )
        .await
        .expect("create first incarnation");
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

    let replacement = rt
        .create(
            &scope,
            &plan(&[
                "sh",
                "-c",
                "test \"$(cat /workspace/recovery-proof)\" = preserved && sleep 30",
            ]),
        )
        .await
        .expect("replace the terminal incarnation");
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
        rt.inspect(&replacement).await.unwrap(),
        ContainerState::Running,
        "KTR1 the replacement Pod reattached the retained PVC bytes"
    );
    rt.remove(&replacement).await.expect("delete replacement");
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
    let rt = rt.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    });
    let scope = format!("claim-fence-{}", std::process::id());
    let first = rt.create(&scope, &plan(&["sleep", "30"])).await.unwrap();
    let old = rt.handle_extra(&first).await.unwrap().unwrap();
    rt.remove_with_handle(&first, Some(&old)).await.unwrap();

    let second = rt.create(&scope, &plan(&["sleep", "30"])).await.unwrap();
    let current = rt.handle_extra(&second).await.unwrap().unwrap();
    assert_ne!(old, current, "KPV1 distinct PVC incarnations");
    assert!(
        rt.remove_with_handle(&second, Some(&old)).await.is_err(),
        "KPV1 stale deletion is fenced"
    );
    assert_eq!(rt.inspect(&second).await.unwrap(), ContainerState::Running);
    rt.remove_with_handle(&second, Some(&current))
        .await
        .unwrap();
}

#[tokio::test]
async fn failed_realization_rolls_back_only_its_unbound_claim() {
    /* Partial-create decision table KPV2. C1 a live conflicting Pod occupies the
     * deterministic name without a continuation claim; C2 a new attempt creates
     * its PVC then fails Pod realization verification. R1 C1+C2 => preserve the
     * observed Pod and delete only the unbound newly-created PVC. A Worker crash
     * is intentionally different: the deterministic claim remains adoptable.
     */
    let Some(plain) = live_runtime().await else {
        return;
    };
    let scope = format!("claim-rollback-{}", std::process::id());
    let pod = plain.create(&scope, &plan(&["sleep", "30"])).await.unwrap();
    let Some(retained) = live_runtime().await else {
        plain.remove(&pod).await.unwrap();
        return;
    };
    let retained = retained.with_continuation_volume(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    });
    assert!(
        retained
            .create(&scope, &plan(&["sleep", "30"]))
            .await
            .is_err()
    );

    let client = kube::Client::try_default().await.unwrap();
    let claims = kube::Api::<k8s_openapi::api::core::v1::PersistentVolumeClaim>::namespaced(
        client, "default",
    );
    let claim = format!("awc-{}", pod.strip_prefix("awaken-").unwrap());
    assert!(
        claims.get_opt(&claim).await.unwrap().is_none(),
        "KPV2 rollback"
    );
    assert_eq!(plain.inspect(&pod).await.unwrap(), ContainerState::Running);
    plain.remove(&pod).await.unwrap();
}
