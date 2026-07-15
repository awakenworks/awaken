//! Real Kubernetes end-to-end (k3d): launch a process-as-container agent Pod running a
//! `busybox` newline-wire fixture, port-forward its agent port, and exchange the ACP
//! wire — proving the `K8sRuntime` create/inspect/open_channel path against a live
//! cluster (the multi-node cloud tier of ADR-0041/0056).
//!
//! Gated on the `k8s` feature AND `AWAKEN_K8S_E2E=1` with a reachable cluster
//! (KUBECONFIG pointing at it). It self-skips otherwise, so a machine without a
//! cluster still passes `cargo test`. The cluster + the `awaken-bb:1` fixture image
//! are set up by `scripts/e2e/k8s_container_e2e.sh`, which runs this test.
#![cfg(feature = "k8s")]

use std::sync::Arc;
use std::time::Duration;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerProvider;
use awaken_sandbox_container::k8s::K8sRuntime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The busybox `nc` fixture: listen on 8080 and, per connection, read the prompt and
/// reply with the newline-wire message + turn_end (the same stand-in the Docker e2e
/// and the namespace-tier ACP e2e use).
fn agent_argv() -> Vec<String> {
    let script = "read _p; \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"sandboxed reply\"}'; \
        printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";
    vec![
        "nc".into(),
        "-lk".into(),
        "-p".into(),
        "8080".into(),
        "-e".into(),
        "sh".into(),
        "-c".into(),
        script.into(),
    ]
}

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": agent_argv(), "image": "awaken-bb:1" })),
    }
}

fn kubectl(args: &[&str]) -> std::process::Output {
    std::process::Command::new("kubectl")
        .args(args)
        .output()
        .expect("kubectl runs")
}

#[tokio::test]
async fn a_pod_agent_speaks_the_wire_over_a_port_forward() {
    if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 with a reachable cluster to run");
        return;
    }
    // The cluster must be reachable (kubeconfig points at it).
    if !kubectl(&["get", "nodes"]).status.success() {
        eprintln!("skipping: no reachable Kubernetes cluster");
        return;
    }

    // A fixed local port the runtime direct-dials; `kubectl port-forward` bridges it to
    // the Pod's 8080 once the Pod is Running.
    let local_port: u16 = 18080;
    let addr = format!("127.0.0.1:{local_port}").parse().unwrap();
    let scope = format!("k8s-e2e-{}", std::process::id());
    let pod = format!("awaken-{scope}");
    // Reap any leftover Pod from an interrupted prior run.
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    let runtime = K8sRuntime::connect("default", addr)
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), "awaken-bb:1");

    let sandbox = provider
        .create_container(&spec(&scope))
        .await
        .expect("create the agent Pod");

    // Wait for the Pod to be Running before forwarding to it.
    let ready = {
        let mut ok = false;
        for _ in 0..120 {
            let phase = kubectl(&["get", "pod", &pod, "-o", "jsonpath={.status.phase}"]);
            if String::from_utf8_lossy(&phase.stdout) == "Running" {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        ok
    };

    // Forward the Pod's agent port to the fixed local port the runtime dials.
    let mut forward = std::process::Command::new("kubectl")
        .args([
            "port-forward",
            &format!("pod/{pod}"),
            &format!("{local_port}:8080"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn port-forward");

    // Dial + exchange the wire (retry while the forward + nc come up).
    let mut got = String::new();
    if ready {
        let mut channel = None;
        for _ in 0..100 {
            match awaken_agent_channel::AgentTransport::open_channel(&sandbox).await {
                Ok(c) => {
                    channel = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        if let Some(mut channel) = channel {
            let _ = channel.write_all(b"hello\n").await;
            let _ = channel.flush().await;
            let mut buf = vec![0u8; 512];
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_secs(2), channel.read(&mut buf)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        got.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if got.contains("turn_end") {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        }
    }

    // Clean up: stop the forward and reap the Pod before asserting.
    let _ = forward.kill();
    let _ = pc::Sandbox::dispose(&sandbox).await;
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    assert!(ready, "the agent Pod must reach Running");
    assert!(
        got.contains("sandboxed reply"),
        "the Pod agent's reply must reach the host over the forwarded port: {got:?}"
    );
    assert!(got.contains("turn_end"), "the turn completed: {got:?}");
}
