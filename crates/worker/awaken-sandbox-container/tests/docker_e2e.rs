//! Real Docker end-to-end: create one long-lived Session environment, exec a stdio
//! agent inside it, and exchange the newline ACP wire over that exact exec process.
//!
//! Gated on the `docker` feature AND a reachable daemon: it self-skips (does not fail)
//! when Docker is absent, so a machine without it still passes `cargo test`.
#![cfg(feature = "docker")]

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerProvider;
use awaken_sandbox_container::docker::DockerRuntime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn agent_argv() -> Vec<String> {
    let script = "read _p; \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"sandboxed reply\"}'; \
        printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'; \
        sleep 0.1";
    vec!["sh".into(), "-c".into(), script.into()]
}

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
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
        .create_container(&spec(&scope))
        .await
        .expect("create container");
    let mut agent = sandbox
        .spawn_agent(pc::Command {
            stdio: pc::Stdio::Piped,
            ..pc::Command::new(agent_argv())
        })
        .await
        .expect("exec stdio agent");
    agent.channel.write_all(b"hello\n").await.unwrap();
    agent.channel.flush().await.unwrap();
    let mut got = String::new();
    agent.channel.read_to_string(&mut got).await.unwrap();
    assert_eq!(agent.process.wait().await.unwrap().code, Some(0));

    // Clean up the container before asserting, so a failure still reaps it.
    let _ = pc::Sandbox::dispose(&sandbox).await;

    assert!(
        got.contains("sandboxed reply"),
        "the containerized agent's reply must reach the host over the dialed port: {got:?}"
    );
    assert!(got.contains("turn_end"), "the turn completed: {got:?}");
}
