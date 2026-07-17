//! Real Docker end-to-end: launch a process-as-container agent in a `busybox` image,
//! publish + dial its port, and exchange the newline ACP wire — proving the Docker
//! `ContainerRuntime` + `create_container`/`open_channel` path against a live daemon.
//!
//! Gated on the `docker` feature AND a reachable daemon: it self-skips (does not fail)
//! when Docker is absent, so a machine without it still passes `cargo test`.
#![cfg(feature = "docker")]

use std::sync::Arc;
use std::time::Duration;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerProvider;
use awaken_sandbox_container::docker::DockerRuntime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The fixture agent, as a `busybox nc` command: listen on the agent port and, per
/// connection, read the prompt line and reply with the newline-wire message + turn_end
/// (the same stand-in the namespace-tier ACP e2e uses, bridged onto a TCP socket).
fn agent_argv(port: u16) -> Vec<String> {
    let script = "read _p; \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"sandboxed reply\"}'; \
        printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";
    vec![
        "nc".into(),
        "-lk".into(),
        "-p".into(),
        port.to_string(),
        "-e".into(),
        "sh".into(),
        "-c".into(),
        script.into(),
    ]
}

fn spec(scope: &str, port: u16) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        // Process-as-container: the agent argv IS the container's main command.
        extra: Some(serde_json::json!({ "command": agent_argv(port) })),
    }
}

/// Is a Docker daemon reachable? (`docker version` exits 0.)
fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn a_containerized_agent_speaks_the_wire_over_a_published_port() {
    if !docker_available() {
        eprintln!("skipping: no reachable Docker daemon");
        return;
    }
    // Ensure the fixture image is present (pull is a no-op if cached).
    let _ = std::process::Command::new("docker")
        .args(["pull", "-q", "busybox:latest"])
        .status();

    let port = 8080;
    // A per-process scope so parallel/re-runs never collide on the container name; a
    // best-effort pre-removal reaps any leftover from an interrupted prior run.
    let scope = format!("docker-e2e-{}", std::process::id());
    let _ = std::process::Command::new("docker")
        .args(["rm", "-f", &format!("awaken-{scope}")])
        .status();

    let runtime = DockerRuntime::connect_local(port).expect("docker client");
    let provider = ContainerProvider::new(Arc::new(runtime), "busybox:latest");

    // Create the container running the agent; retry the dial while it boots + nc binds.
    let sandbox = provider
        .create_container(&spec(&scope, port))
        .await
        .expect("create container");

    // Retry the WHOLE exchange (open + write + read), not just the dial: Docker's
    // port-proxy accepts a connection the instant the container starts, but the `nc`
    // agent inside takes a moment to bind :8080 — so an early dial "succeeds" yet the
    // proxied read returns empty (the backend isn't listening yet). Re-opening a fresh
    // channel per attempt until the reply arrives makes the first-turn cold-start
    // deterministic (a warm agent answers on the first attempt).
    let mut got = String::new();
    for _ in 0..50 {
        let Ok(mut channel) = awaken_agent_channel::AgentTransport::open_channel(&sandbox).await
        else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        if channel.write_all(b"hello\n").await.is_err() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        channel.flush().await.ok();
        let mut buf = vec![0u8; 512];
        let mut this = String::new();
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_secs(1), channel.read(&mut buf)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    this.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if this.contains("turn_end") {
                        break;
                    }
                }
                _ => break,
            }
        }
        if this.contains("turn_end") {
            got = this;
            break;
        }
        // Empty/partial reply → the agent wasn't ready; back off and re-open.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Clean up the container before asserting, so a failure still reaps it.
    let _ = pc::Sandbox::dispose(&sandbox).await;

    assert!(
        got.contains("sandboxed reply"),
        "the containerized agent's reply must reach the host over the dialed port: {got:?}"
    );
    assert!(got.contains("turn_end"), "the turn completed: {got:?}");
}
