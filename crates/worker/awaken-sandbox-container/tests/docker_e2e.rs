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
    let runtime = DockerRuntime::connect_local(port).expect("docker client");
    let provider = ContainerProvider::new(Arc::new(runtime), "busybox:latest");

    // Create the container running the agent; retry the dial while it boots + nc binds.
    let sandbox = provider
        .create_container(&spec("docker-e2e", port))
        .await
        .expect("create container");

    let mut channel = None;
    for _ in 0..100 {
        match awaken_agent_channel::AgentTransport::open_channel(&sandbox).await {
            Ok(c) => {
                channel = Some(c);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let mut channel = channel.expect("dial the published agent port");

    // Drive the newline wire: write a prompt line, read the reply lines.
    channel.write_all(b"hello\n").await.expect("write prompt");
    channel.flush().await.ok();

    let mut buf = vec![0u8; 512];
    let mut got = String::new();
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

    // Clean up the container before asserting, so a failure still reaps it.
    let _ = pc::Sandbox::dispose(&sandbox).await;

    assert!(
        got.contains("sandboxed reply"),
        "the containerized agent's reply must reach the host over the dialed port: {got:?}"
    );
    assert!(got.contains("turn_end"), "the turn completed: {got:?}");
}
