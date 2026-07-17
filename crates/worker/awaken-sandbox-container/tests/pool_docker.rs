//! G6 warm pool against a REAL Docker daemon: pre-warm process-as-container agents,
//! prove a warm one is handed out to a matching session over the real ACP wire
//! (cold-start paid off-path), and that shutdown reaps every warm container.
//!
//! Gated on the `docker` feature AND a reachable daemon (self-skips otherwise).
//! Run with: `cargo test -p awaken-sandbox-container --features docker --test pool_docker`
#![cfg(feature = "docker")]

use std::sync::Arc;
use std::time::Duration;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::docker::DockerRuntime;
use awaken_sandbox_container::{AgentContainerProvider, ContainerProvider, WarmContainerPool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const AGENT_PORT: u16 = 8080;

/// A busybox `nc` agent: listen on the port and reply with the newline ACP wire.
fn agent_argv() -> Vec<String> {
    let script = "read _p; \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"warm reply\"}'; \
        printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";
    vec![
        "nc".into(),
        "-lk".into(),
        "-p".into(),
        AGENT_PORT.to_string(),
        "-e".into(),
        "sh".into(),
        "-c".into(),
        script.into(),
    ]
}

/// A mount-less process-as-container agent spec (poolable). `scope` is the session
/// name; the pool assigns its own scopes to warm containers.
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
        extra: Some(serde_json::json!({ "command": agent_argv() })),
    }
}

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
async fn a_warm_pool_pre_provisions_capacity_and_hands_a_ready_agent_to_a_session() {
    if !docker_available() {
        eprintln!("skipping: no reachable Docker daemon");
        return;
    }
    let _ = std::process::Command::new("docker")
        .args(["pull", "-q", "busybox:latest"])
        .status();

    let runtime = DockerRuntime::connect_local(AGENT_PORT).expect("docker client");
    let provider = Arc::new(ContainerProvider::new(Arc::new(runtime), "busybox:latest"));
    let pool = WarmContainerPool::new(provider, 2);
    let session_spec = spec("pool-session");

    // Pre-provision 2 warm agents for this shape (cold-start capacity).
    pool.prewarm(&session_spec, 2)
        .await
        .expect("pre-warm two agents");
    assert_eq!(
        pool.ready_len(&session_spec),
        2,
        "the pool holds two ready warm agents"
    );

    // A matching session is handed a WARM agent (no create on the request path); the
    // ready count drops by one immediately (checked before any await lets the
    // off-path replenish run on this current-thread runtime).
    let session = AgentContainerProvider::open_agent(&pool, &session_spec)
        .await
        .expect("open_agent hands out a warm agent");
    assert_eq!(
        pool.ready_len(&session_spec),
        1,
        "handing out a session consumed one warm agent"
    );

    // The warm agent is fully usable: drive the real ACP wire over its dialed port.
    let mut channel = session.channel;
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
    drop(channel);

    // Teardown reaps EVERY warm container (and stops replenishment) — no leak.
    pool.shutdown().await;
    assert_eq!(
        pool.ready_len(&session_spec),
        0,
        "shutdown disposed every warm container"
    );
    // The session's own container is disposed via its process handle (kill).
    let _ = session.process.signal(pc::Signal::Kill).await;

    assert!(
        got.contains("warm reply") && got.contains("turn_end"),
        "the warm agent served the session over the real wire: {got:?}"
    );
}

/// A spec that declares a mount is NOT pooled — it is created fresh so its
/// per-session bytes are present, never served a shape-matched warm container.
#[tokio::test]
async fn a_session_with_a_mount_bypasses_the_pool() {
    if !docker_available() {
        eprintln!("skipping: no reachable Docker daemon");
        return;
    }
    let runtime = DockerRuntime::connect_local(AGENT_PORT).expect("docker client");
    let provider = Arc::new(ContainerProvider::new(Arc::new(runtime), "busybox:latest"));
    let pool = WarmContainerPool::new(provider, 2);

    // A poolable base shape, pre-warmed.
    let base = spec("mount-base");
    pool.prewarm(&base, 1).await.expect("pre-warm");
    assert_eq!(pool.ready_len(&base), 1);

    // The same shape BUT with a mount: not poolable, so the warm one is untouched.
    let mut mounted = spec("mount-session");
    mounted.mounts = vec![pc::MountRequirement {
        mount_id: "cfg".into(),
        source: pc::MountSource::Inline {
            contents: "hello".into(),
        },
        mount_path: "/etc/agent.conf".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    }];
    // Open a mounted session. Its channel dial may race the freshly-created
    // container's port bind (a pre-existing cold-create property `open_agent`
    // shares with the no-pool path — a warm container avoids it by being bound
    // ahead of time); that outcome is irrelevant here. What must hold is that the
    // pool was BYPASSED: the shape-matched warm base capacity is untouched.
    if let Ok(session) = AgentContainerProvider::open_agent(&pool, &mounted).await {
        let _ = session.process.signal(pc::Signal::Kill).await;
    }
    assert_eq!(
        pool.ready_len(&base),
        1,
        "a mounted session did NOT consume the shape-matched warm agent (it bypassed the pool)"
    );

    pool.shutdown().await;
}
