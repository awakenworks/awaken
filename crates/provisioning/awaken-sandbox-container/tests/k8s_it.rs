//! Real Kubernetes integration (ADR-0041 Slice 5), gated on the `k8s` feature AND a
//! reachable cluster (in-cluster SA or kubeconfig) — skips cleanly otherwise.
//!
//! Run with: `cargo test -p awaken-sandbox-container --features k8s --test k8s_it`
#![cfg(feature = "k8s")]

use std::time::Duration;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::k8s::K8sRuntime;
use awaken_sandbox_container::{ContainerPlan, ContainerRuntime, ContainerState, NetworkMode};

fn plan(cmd: &[&str]) -> ContainerPlan {
    ContainerPlan {
        image: "busybox:latest".into(),
        command: cmd.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: NetworkMode::Open,
        limits: pc::ResourceLimits::default(),
    }
}

#[tokio::test]
async fn k8s_pod_lifecycle_against_a_real_cluster() {
    let addr = "127.0.0.1:8080".parse().unwrap();
    let Ok(rt) = K8sRuntime::connect("default", addr).await else {
        eprintln!("skipping: no kube client (no in-cluster SA / kubeconfig)");
        return;
    };
    if rt.ping().await.is_err() {
        eprintln!("skipping: kube apiserver unreachable");
        return;
    }

    // Clear a leftover pod and wait for termination to settle.
    let _ = rt.remove("awaken-it-pod").await;
    for _ in 0..30 {
        if rt.inspect("awaken-it-pod").await.is_err() {
            break; // gone
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // process-as-container Pod (busybox sleep as the container command).
    let id = rt
        .create("it-pod", &plan(&["sleep", "30"]))
        .await
        .expect("create a real Pod via the apiserver");
    assert_eq!(id, "awaken-it-pod");

    // Pending/Running both project to a live sandbox.
    assert!(matches!(
        rt.inspect(&id).await.unwrap(),
        ContainerState::Running
    ));

    // Teardown deletes the Pod.
    rt.remove(&id).await.expect("delete the Pod");
}
