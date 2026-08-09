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
    command_of,
};

const AGENT_PORT: u16 = 8080;

fn live_prerequisite(info_responds: bool, oci_starts: bool) -> bool {
    info_responds && oci_starts
}

struct CredentialBroker(Mutex<Vec<u8>>);

#[async_trait]
impl pc::SecretBroker for CredentialBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(self.0.lock().unwrap().clone())
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::new("process secrets are not configured"))
    }

    async fn write_back(&self, _reference: &str, bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        *self.0.lock().unwrap() = bytes;
        Ok(())
    }
}

struct ProcessSecretBroker;

#[async_trait]
impl pc::SecretBroker for ProcessSecretBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::new("file secrets are not configured"))
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(b"podman-process-secret".to_vec())
    }

    async fn write_back(&self, _reference: &str, _bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::new("secret write-back is not configured"))
    }
}

fn plan(cmd: &[&str], rootfs: RootfsPlan) -> ContainerPlan {
    ContainerPlan {
        image: "docker.io/library/busybox:latest".into(),
        command: cmd.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        packages: Default::default(),
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
    static PODMAN_READY: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    // Cause/effect graph: C1 `podman info` works; C2 the OCI runtime can start a
    // minimal container. Decision table: R1 C1,C2 -> run the live suite; R2
    // !C1|!C2 -> external prerequisite unavailable, skip without converting a
    // host D-Bus/runc outage into a product failure. The raw probe intentionally
    // bypasses `PodmanRuntime::create`, so a regression in our argv/adapter still
    // reaches the actual tests and fails instead of being hidden by this gate.
    let ready = PODMAN_READY
        .get_or_init(|| async {
            let info_responds = match rt.ping().await {
                Ok(()) => true,
                Err(error) => {
                    eprintln!("skipping Podman integration: info probe failed: {error}");
                    false
                }
            };
            if !info_responds {
                return false;
            }
            let oci_starts = match tokio::process::Command::new("podman")
                .args([
                    "run",
                    "--rm",
                    "--network=none",
                    "docker.io/library/busybox:latest",
                    "true",
                ])
                .output()
                .await
            {
                Ok(output) if output.status.success() => true,
                Ok(output) => {
                    eprintln!(
                        "skipping Podman integration: OCI probe failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                    false
                }
                Err(error) => {
                    eprintln!("skipping Podman integration: OCI probe failed: {error}");
                    false
                }
            };
            live_prerequisite(info_responds, oci_starts)
        })
        .await;
    if !*ready {
        return None;
    }
    Some(rt)
}

#[test]
fn live_prerequisite_requires_both_info_and_oci_execution() {
    // Cause/effect decision table for the external test gate: R1 !info,!oci;
    // R2 !info,oci; R3 info,!oci all skip; only R4 info,oci runs live tests.
    // `oci=true` with `info=false` is logically unreachable in the dynamic probe
    // but retained here to make the conjunction total and regression-resistant.
    assert!(!live_prerequisite(false, false), "R1");
    assert!(!live_prerequisite(false, true), "R2");
    assert!(!live_prerequisite(true, false), "R3");
    assert!(live_prerequisite(true, true), "R4");
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
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({
            "command": ["sh", "-c", "grep -q hello-podman-inline /data/config.toml"],
            "environment": {
                "kind": "image",
                "reference": "docker.io/library/busybox:latest"
            },
        })),
    };

    let sandbox = provider
        .create(&spec)
        .await
        .expect("create rootless container");
    let proc = sandbox
        .spawn(pc::Command::new(command_of(&spec)))
        .await
        .expect("exec content probe");
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
async fn podman_separates_container_environment_from_cli_environment() {
    // Cause/effect graph: C1 runtime-owned HOME/XDG inline values; C2 a
    // process-secret target; C3 rootless Podman reads its own host environment.
    // Effects: E1 Podman keeps its host storage/config roots and starts exec;
    // E2 the container sees exact runtime paths and secret target; E3 no
    // adapter alias survives into the launched command; E4 the secret is absent
    // from Podman's argv. Decision table: R1 C1,!C2 -> E1+E2; R2 !C1,C2 ->
    // E1+E2+E3+E4; R3 C1,C2 (this case) -> all effects together. The existing
    // inline-content test covers R1; this mixed case covers the secret branch.
    let Some(rt) = runtime().await else { return };
    let provider = ContainerProvider::new(Arc::new(rt), "docker.io/library/busybox:latest")
        .with_secret_broker(Arc::new(ProcessSecretBroker));
    let spec = pc::SandboxSpec {
        scope: "pod-exec-env".into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: vec![pc::EnvVar {
            name: "PODMAN_E2E_SECRET".into(),
            value: pc::EnvValue::Secret {
                reference: "credential://podman/process".into(),
            },
            visibility: pc::EnvVisibility::Process,
        }],
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": ["sleep", "30"] })),
    };
    let sandbox = provider.create(&spec).await.expect("create environment");
    let process = sandbox
        .spawn(pc::Command::new([
            "sh",
            "-c",
            "test \"$HOME\" = /workspace && test \"$XDG_CONFIG_HOME\" = /workspace/.config && test \"$PODMAN_E2E_SECRET\" = podman-process-secret && ! env | grep '^AWAKEN_PODMAN_EXEC_SECRET_'",
        ]))
        .await
        .expect("spawn environment probe");
    let status = process.wait().await.expect("wait for environment probe");
    sandbox.dispose().await.expect("dispose environment");
    assert_eq!(status.code, Some(0), "R3: E1-E4 must hold");
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
async fn podman_peer_adoption_renews_reaper_ownership() {
    let Some(runtime_a) = runtime().await else {
        return;
    };
    let runtime_a = Arc::new(runtime_a);
    let provider_a = ContainerProvider::new(runtime_a, "docker.io/library/busybox:latest");
    let spec = pc::SandboxSpec {
        scope: "pod-adopt".into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": ["sleep", "30"] })),
    };
    let handle = {
        let sandbox = provider_a.create(&spec).await.expect("create by worker A");
        sandbox.handle()
    };

    let Some(runtime_b) = runtime().await else {
        return;
    };
    let runtime_b = Arc::new(runtime_b);
    let provider_b = ContainerProvider::new(runtime_b.clone(), "docker.io/library/busybox:latest");
    let adopted = provider_b.adopt(&handle).await.expect("peer adoption");
    adopted
        .renew_lease()
        .await
        .expect("renew adopted ownership");
    let physical_id = handle
        .extra
        .as_ref()
        .and_then(|extra| extra["container_id"].as_str())
        .expect("physical container id");
    assert!(
        runtime_b
            .list_managed()
            .await
            .unwrap()
            .into_iter()
            .any(|container| container.owned_by_current_runtime),
        "the peer runtime protects the canonical id returned by podman ps"
    );
    assert_eq!(
        runtime_b.inspect(physical_id).await.unwrap(),
        ContainerState::Running
    );
    adopted.dispose().await.unwrap();
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
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({
            "command": ["sh", "-c", format!("test \"$(cat /acp-config/.credentials.json)\" = '{}' && printf '%s' '{}' > /acp-config/.credentials.json", String::from_utf8_lossy(initial), String::from_utf8_lossy(refreshed))]
        })),
    };
    let sandbox = provider.create(&spec).await.unwrap();
    let process = sandbox
        .spawn(pc::Command::new(command_of(&spec)))
        .await
        .unwrap();
    for _ in 0..100 {
        if process.poll().await.unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    sandbox.dispose().await.unwrap();
    assert_eq!(broker.0.lock().unwrap().as_slice(), refreshed);
}
