//! Real rootless-Podman integration, gated on the `podman` feature AND a working
//! `podman` binary — skips cleanly otherwise (same discipline as the docker/bwrap
//! tests).
//!
//! Run with: `cargo test -p awaken-sandbox-container --features podman --test podman_it`
#![cfg(feature = "podman")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxProvider;
use awaken_sandbox_container::podman::PodmanRuntime;
use awaken_sandbox_container::{
    ContainerPlan, ContainerProvider, ContainerRuntime, ContainerState, NetworkMode, RootfsPlan,
};

const AGENT_PORT: u16 = 8080;

struct CredentialBroker(Mutex<Vec<u8>>);

#[async_trait]
impl pc::SecretBroker for CredentialBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(self.0.lock().unwrap().clone())
    }

    async fn write_back(&self, _reference: &str, bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        *self.0.lock().unwrap() = bytes;
        Ok(())
    }
}

fn plan(cmd: &[&str], rootfs: RootfsPlan) -> ContainerPlan {
    ContainerPlan {
        image: "docker.io/library/busybox:latest".into(),
        command: cmd.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: NetworkMode::Open,
        limits: pc::ResourceLimits {
            memory_bytes: Some(256 * 1024 * 1024),
            ..Default::default()
        },
        memory_mounts: Vec::new(),
        rootfs,
    }
}

async fn runtime() -> Option<PodmanRuntime> {
    let rt = PodmanRuntime::new(AGENT_PORT);
    if rt.ping().await.is_err() {
        eprintln!("skipping: no working `podman` binary");
        return None;
    }
    Some(rt)
}

#[tokio::test]
async fn podman_full_lifecycle_against_a_real_binary() {
    let Some(rt) = runtime().await else { return };
    let _ = rt.remove("awaken-it-podlife").await; // clear any leftover

    // process-as-container: the sleep IS the container's main process, cgroup-capped.
    let id = rt
        .create(
            "it-podlife",
            &plan(&["sleep", "30"], RootfsPlan::HostUserland),
        )
        .await
        .expect("create+start a rootless container");

    assert!(matches!(
        rt.inspect(&id).await.unwrap(),
        ContainerState::Running
    ));
    assert!(rt.poll(&id).await.unwrap().is_none(), "still running");

    // reap + teardown; a removed container is gone.
    rt.signal(&id, pc::Signal::Kill).await.unwrap();
    rt.remove(&id).await.unwrap();
    assert!(matches!(
        rt.inspect(&id).await.unwrap(),
        ContainerState::Gone
    ));
}

#[tokio::test]
async fn podman_materializes_inline_content_through_the_provider() {
    // Inline content (codex config.toml / ADR-0038 resource bytes) has no host file — the
    // shared `ContainerProvider::create` stages it to a host temp file and `podman run`
    // binds it read-only at the mount_path. This drives the SAME materialize path the
    // Docker e2e verifies, but through the rootless-podman argv + a real `podman` binary.
    let Some(rt) = runtime().await else { return };
    let provider = ContainerProvider::new(Arc::new(rt), "docker.io/library/busybox:latest");

    let spec = pc::SandboxSpec {
        scope: "pod-inline".into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "cfg".into(),
            source: pc::MountSource::Inline {
                contents: "[mcp_servers.gh]\nhello-podman-inline\n".into(),
            },
            mount_path: "/data/config.toml".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({
            "command": ["sh", "-c", "grep -q hello-podman-inline /data/config.toml"],
            "image": "docker.io/library/busybox:latest",
        })),
    };

    let sandbox = provider
        .create(&spec)
        .await
        .expect("create rootless container");
    let proc = sandbox
        .spawn(pc::Command::new(["true"]))
        .await
        .expect("handle to the process-as-container");
    let mut code = None;
    for _ in 0..100 {
        match proc.poll().await.expect("poll") {
            Some(exit) => {
                code = exit.code;
                break;
            }
            None => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let _ = sandbox.dispose().await;
    assert_eq!(
        code,
        Some(0),
        "inline content must be staged to a host file and readable in the rootless container"
    );
}

#[tokio::test]
async fn podman_reports_the_agent_exit_code() {
    let Some(rt) = runtime().await else { return };
    let _ = rt.remove("awaken-it-podexit").await;

    // A short-lived agent that exits non-zero; wait must surface the code.
    let id = rt
        .create(
            "it-podexit",
            &plan(&["sh", "-c", "exit 7"], RootfsPlan::HostUserland),
        )
        .await
        .expect("create a short-lived container");
    let status = rt.wait(&id).await.expect("wait for exit");
    assert_eq!(status.code, Some(7), "podman wait surfaces the exit code");
    let _ = rt.remove(&id).await;
}

#[tokio::test]
async fn podman_rotates_and_persists_a_native_credential_file() {
    let Some(rt) = runtime().await else { return };
    let initial = br#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"old"}}"#;
    let refreshed = br#"{"claudeAiOauth":{"accessToken":"new","refreshToken":"rotated"}}"#;
    let broker = Arc::new(CredentialBroker(Mutex::new(initial.to_vec())));
    let provider = ContainerProvider::new(Arc::new(rt), "docker.io/library/busybox:latest")
        .with_secret_broker(broker.clone());
    let spec = pc::SandboxSpec {
        scope: "pod-native-credential".into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "claude-auth".into(),
            source: pc::MountSource::Secret {
                reference: "credential://acp/native/claude".into(),
                content_hash: None,
            },
            mount_path: "/acp-config/.credentials.json".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Durable,
            required: true,
        }],
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({
            "command": ["sh", "-c", format!("test \"$(cat /acp-config/.credentials.json)\" = '{}' && printf '%s' '{}' > /acp-config/.credentials.json", String::from_utf8_lossy(initial), String::from_utf8_lossy(refreshed))]
        })),
    };
    let sandbox = provider.create(&spec).await.unwrap();
    let process = sandbox.spawn(pc::Command::new(["true"])).await.unwrap();
    for _ in 0..100 {
        if process.poll().await.unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(broker.0.lock().unwrap().as_slice(), refreshed);
    sandbox.dispose().await.unwrap();
}
