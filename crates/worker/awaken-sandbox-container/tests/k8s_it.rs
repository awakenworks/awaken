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
        image: "busybox:1.37.0".into(),
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

    let addr = "127.0.0.1:8080".parse().unwrap();
    let Ok(rt) = K8sRuntime::connect("default", addr).await else {
        eprintln!("skipping: no kube client (no in-cluster SA / kubeconfig)");
        return;
    };
    if rt.ping().await.is_err() {
        eprintln!("skipping: kube apiserver unreachable");
        return;
    }

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
    let addr = "127.0.0.1:8080".parse().unwrap();
    let Ok(rt) = K8sRuntime::connect("default", addr).await else {
        eprintln!("skipping: no kube client");
        return;
    };
    if rt.ping().await.is_err() {
        eprintln!("skipping: kube apiserver unreachable");
        return;
    }
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
