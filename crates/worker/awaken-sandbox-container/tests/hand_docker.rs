//! C5 hand-in-container transport, against a REAL Docker daemon: the missing piece for
//! a HAND (tool executor) inside a network-DENIED sandbox. The container tier's TCP
//! port-publish path is suppressed under `--network none` (Docker forbids it), so a
//! hand needs a transport that crosses the boundary WITHOUT a network — a unix socket
//! in a host<->container bind-mount rendezvous (the host then dials `DialAddr::Unix`).
//!
//! This proves the whole path works with EXISTING container primitives: a `CacheVolume`
//! mount is already a RW host-dir bind, `pc::SandboxProvider::create` runs the hand as a
//! process-as-container under `network: None`, and the host reaches the unix socket the
//! hand bound in the shared dir. (The fake hand is a python unix-echo — self-contained,
//! no egress install — standing in for `awaken-sandbox hand`.)
//!
//! Gated on the `docker` feature AND a reachable daemon (self-skips otherwise).
//! Run: `cargo test -p awaken-sandbox-container --features docker --test hand_docker`
#![cfg(feature = "docker")]

use std::sync::Arc;
use std::time::Duration;

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxProvider;
use awaken_sandbox_container::ContainerProvider;
use awaken_sandbox_container::docker::DockerRuntime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const AGENT_PORT: u16 = 8080;

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A fake hand: bind a unix socket in the rendezvous dir, accept one connection, and
/// reply `hand:<prompt>` — the shape of a tool-execution reply, without a real hand.
fn fake_hand_argv() -> Vec<String> {
    let script = "import socket,os\n\
try:\n os.unlink('/rv/hand.sock')\n\
except FileNotFoundError:\n pass\n\
s=socket.socket(socket.AF_UNIX); s.bind('/rv/hand.sock'); s.listen()\n\
os.chmod('/rv/hand.sock',0o777)\n\
c,_=s.accept(); d=c.recv(64); c.sendall(b'hand:'+d)\n";
    vec!["python3".into(), "-u".into(), "-c".into(), script.into()]
}

#[tokio::test]
async fn a_hand_in_a_network_denied_container_is_reached_over_a_unix_rendezvous() {
    if !docker_available() {
        eprintln!("skipping: no reachable Docker daemon");
        return;
    }
    let _ = std::process::Command::new("docker")
        .args(["pull", "-q", "python:3-slim"])
        .status();

    // A host rendezvous dir bind-mounted RW into the container (a CacheVolume: an
    // in-place, node-local, never-harvested host-dir bind — exactly a socket rendezvous).
    let rv = std::env::temp_dir().join(format!("awaken-hand-rv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&rv);
    std::fs::create_dir_all(&rv).expect("create rendezvous dir");
    let host_sock = rv.join("hand.sock");
    let _ = std::fs::remove_file(&host_sock);

    let runtime = DockerRuntime::connect_local(AGENT_PORT).expect("docker client");
    let provider = ContainerProvider::new(Arc::new(runtime), "python:3-slim");

    let spec = pc::SandboxSpec {
        scope: "hand-rv".into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "rendezvous".into(),
            source: pc::MountSource::CacheVolume {
                host_path: rv.to_string_lossy().into_owned(),
                key: String::new(),
            },
            mount_path: "/rv".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
        env: Vec::new(),
        // Network DENIED — no published port is possible; the unix rendezvous is the
        // only path across the boundary. This is the exact case that had no transport.
        network: pc::NetworkPolicy::None,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": fake_hand_argv() })),
    };

    let sandbox = provider
        .create(&spec)
        .await
        .expect("create the hand container under --network none");

    // The brain (host side) dials the hand's unix socket — DialAddr::Unix. Retry while
    // the container boots and the hand binds the socket.
    let mut got = String::new();
    let mut connected = false;
    for _ in 0..100 {
        if let Ok(mut stream) = UnixStream::connect(&host_sock).await {
            stream.write_all(b"ping").await.expect("write prompt");
            stream.shutdown().await.ok();
            let mut buf = Vec::new();
            let _ = stream.read_to_end(&mut buf).await;
            got = String::from_utf8_lossy(&buf).into_owned();
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    sandbox.dispose().await.expect("dispose");
    let _ = std::fs::remove_dir_all(&rv);

    assert!(
        connected,
        "the host must reach the hand's unix socket across the --network none boundary"
    );
    assert!(
        got.contains("hand:ping"),
        "the network-denied in-container hand served the host over the unix rendezvous: {got:?}"
    );
}
