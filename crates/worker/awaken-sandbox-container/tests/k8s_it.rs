//! Real Kubernetes integration (ADR-0041 Slice 5), gated on the `k8s` feature AND a
//! reachable cluster (in-cluster SA or kubeconfig) — skips cleanly otherwise.
//!
//! Run with: `cargo test -p awaken-sandbox-container --features k8s --test k8s_it`
#![cfg(feature = "k8s")]

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::k8s::K8sRuntime;
use awaken_sandbox_container::{
    ContainerPlan, ContainerRuntime, ContainerState, NetworkMode, RootfsPlan,
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
        packages: Default::default(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: NetworkMode::Open,
        limits: pc::ResourceLimits::default(),
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

/// C6: a `pids`-limited spec must FAIL CLOSED on the k8s tier (k8s has no per-Pod pids
/// field), rather than be placed with the cap silently dropped. Verified against the
/// real apiserver: create returns an error and creates no Pod.
#[tokio::test]
async fn create_fails_closed_on_a_pids_limit_k8s_cannot_enforce() {
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
     * resourceVersion. Effects: E1 create deletes only that observed incarnation,
     * E2 recreates the deterministic name with a different UID, and E3 reaches
     * Running. The pure decision table owns missing-identity C4 => preserve.
     * Rule KTR1=C1+C2+C3=>E1+E2+E3.
     */
    let Some(rt) = live_runtime().await else {
        return;
    };
    let scope = format!("terminal-replace-{}", std::process::id());
    let first = rt
        .create(&scope, &plan(&["sh", "-c", "sleep 1"]))
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
        .create(&scope, &plan(&["sleep", "30"]))
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
        ContainerState::Running
    );
    rt.remove(&replacement).await.expect("delete replacement");
}
