//! Real rootless-Podman integration, gated on the `podman` feature AND a working
//! `podman` binary. Optional developer runs report unavailable prerequisites;
//! `AWAKEN_TEST_REQUIRE_PODMAN=1` makes the release substrate row fail closed.
//!
//! Run with: `cargo test -p awaken-sandbox-container --features podman --test podman_it`
#![cfg(feature = "podman")]

#[path = "common/disposal.rs"]
mod disposal;
#[path = "common/host_staging.rs"]
mod host_staging;
#[path = "common/process.rs"]
mod process_lifecycle;
#[path = "common/residue.rs"]
mod residue;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerHandleCodec as _;
use awaken_sandbox_container::podman::PodmanRuntime;
use awaken_sandbox_container::{
    ContainerPlan, ContainerProvider, ContainerRuntime, ContainerState, NetworkMode, RootfsPlan,
    command_of,
};

const AGENT_PORT: u16 = 8080;

fn live_prerequisite(
    info_responds: bool,
    oci_starts: bool,
    required: bool,
) -> Result<bool, &'static str> {
    let ready = info_responds && oci_starts;
    if required && !ready {
        Err("required Podman substrate needs both info and actual OCI execution")
    } else {
        Ok(ready)
    }
}

struct CredentialBroker(Mutex<Vec<u8>>);

#[async_trait]
impl pc::SecretBroker for CredentialBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(self.0.lock().unwrap().clone())
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "process secrets are not configured",
        ))
    }

    async fn write_back(
        &self,
        _target: pc::SecretWritebackTarget<'_>,
        bytes: Vec<u8>,
    ) -> Result<(), pc::SandboxError> {
        *self.0.lock().unwrap() = bytes;
        Ok(())
    }
}

struct ProcessSecretBroker;

#[async_trait]
impl pc::SecretBroker for ProcessSecretBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "file secrets are not configured",
        ))
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(b"podman-process-secret".to_vec())
    }

    async fn write_back(
        &self,
        _target: pc::SecretWritebackTarget<'_>,
        _bytes: Vec<u8>,
    ) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "secret write-back is not configured",
        ))
    }
}

fn plan(cmd: &[&str], rootfs: RootfsPlan) -> ContainerPlan {
    ContainerPlan {
        image: "docker.io/library/busybox:latest".into(),
        command: cmd.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        control_services: Default::default(),
        packages: Default::default(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: NetworkMode::Open,
        egress_identity: Default::default(),
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits {
            memory_bytes: Some(256 * 1024 * 1024),
            ..Default::default()
        },
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        memory_mounts: Vec::new(),
        rootfs,
    }
}

async fn runtime() -> Option<PodmanRuntime> {
    let rt = PodmanRuntime::new(AGENT_PORT);
    static PODMAN_READY: tokio::sync::OnceCell<(bool, bool)> = tokio::sync::OnceCell::const_new();
    // Cause/effect graph: C1 `podman info` works; C2 the OCI runtime can start a
    // minimal container; C3 the substrate profile requires live evidence.
    // R1 C1,C2 -> run the live suite; R2 !C1|!C2,!C3 -> report unavailable;
    // R3 !C1|!C2,C3 -> fail qualification. The raw probe intentionally
    // bypasses `PodmanRuntime::create`, so a regression in our argv/adapter still
    // reaches the actual tests and fails instead of being hidden by this gate.
    let ready = PODMAN_READY
        .get_or_init(|| async {
            let info_responds = match rt.ping().await {
                Ok(()) => true,
                Err(error) => {
                    eprintln!("Podman prerequisite unavailable: info probe failed: {error}");
                    false
                }
            };
            if !info_responds {
                return (false, false);
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
                        "Podman prerequisite unavailable: OCI probe failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                    false
                }
                Err(error) => {
                    eprintln!("Podman prerequisite unavailable: OCI probe failed: {error}");
                    false
                }
            };
            (info_responds, oci_starts)
        })
        .await;
    let required = std::env::var("AWAKEN_TEST_REQUIRE_PODMAN").as_deref() == Ok("1");
    if !live_prerequisite(ready.0, ready.1, required)
        .expect("release Podman qualification cannot become a successful prerequisite skip")
    {
        return None;
    }
    Some(rt)
}

#[test]
fn live_prerequisite_requires_both_info_and_oci_execution() {
    // Cause/effect decision table: C1 info; C2 OCI execution; C3 required
    // substrate profile. R1-R3 !C1|!C2 skip only when !C3, and fail when C3;
    // R4 C1+C2 runs the real lifecycle/Secret/filesystem cases for either C3.
    // The Podman-only compilation of this suite also proves the shared residue
    // owner has no unguarded Docker client dependency; cleanup remains exact.
    // `oci=true` with `info=false` is logically unreachable in the dynamic probe
    // but retained here to make the conjunction total and regression-resistant.
    for info in [false, true] {
        for oci in [false, true] {
            assert_eq!(live_prerequisite(info, oci, false), Ok(info && oci));
            if info && oci {
                assert_eq!(live_prerequisite(info, oci, true), Ok(true), "R4");
            } else {
                assert!(live_prerequisite(info, oci, true).is_err(), "R1-R3");
            }
        }
    }
}

#[tokio::test]
async fn podman_ordinary_host_staging_survives_creator_replacement() {
    let Some(first) = runtime().await else { return };
    let Some(peer) = runtime().await else {
        panic!("Podman disappeared after readiness")
    };
    host_staging::verify_ordinary_host_staging_recovery(
        Arc::new(first),
        Arc::new(peer),
        "docker.io/library/busybox:latest",
        residue::Backend::Podman,
    )
    .await;
}

#[tokio::test]
async fn podman_full_lifecycle_against_a_real_binary() {
    let Some(rt) = runtime().await else { return };
    // process-as-container: the sleep IS the container's main process, cgroup-capped.
    let container = residue::create_owned(
        &rt,
        "it-podlife",
        &plan(&["sleep", "30"], RootfsPlan::HostUserland),
        residue::Backend::Podman,
    )
    .await;
    let id = container.id();

    assert!(matches!(
        rt.inspect(id).await.unwrap(),
        ContainerState::Running
    ));
    assert!(rt.poll(id).await.unwrap().is_none(), "still running");
    process_lifecycle::verify_security_floor(&rt, id).await;

    // reap + teardown; a removed container is gone.
    rt.signal(id, pc::Signal::Kill).await.unwrap();
    rt.remove(id).await.unwrap();
    assert!(matches!(
        rt.inspect(id).await.unwrap(),
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
    let provider = ContainerProvider::new(Arc::new(rt), "docker.io/library/busybox:latest")
        .with_clock(awaken_lease_contract::system_clock());

    let _residue = residue::TestResidue::arm("pod-inline", residue::Backend::Podman);
    let spec = pc::SandboxSpec {
        process_tree_supervision: false,
        scope: "pod-inline".into(),
        isolation: pc::IsolationClass::Container,
        environment: Some(pc::EnvironmentKind::Image {
            reference: "docker.io/library/busybox:latest".into(),
        }),
        command: vec![
            "sh".into(),
            "-c".into(),
            "grep -q hello-podman-inline /data/config.toml".into(),
        ],
        deny_tool_egress: false,
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
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
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
    let _ = sandbox.dispose_unattached().await;
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
    let rt = Arc::new(rt);
    let provider = ContainerProvider::new(rt.clone(), "docker.io/library/busybox:latest")
        .with_clock(awaken_lease_contract::system_clock())
        .with_secret_broker(Arc::new(ProcessSecretBroker));
    let _residue = residue::TestResidue::arm("pod-exec-env", residue::Backend::Podman);
    let spec = pc::SandboxSpec {
        process_tree_supervision: false,
        scope: "pod-exec-env".into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec!["sleep".into(), "30".into()],
        deny_tool_egress: false,
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
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
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
    let container_id = sandbox
        .handle()
        .container_payload()
        .unwrap()
        .container_id
        .clone();
    process_lifecycle::verify_wait_cancellation_and_signal(rt.as_ref(), &container_id).await;
    sandbox
        .dispose_unattached()
        .await
        .expect("dispose environment");
    assert_eq!(status.code, Some(0), "R3: E1-E4 must hold");
}

#[tokio::test]
async fn podman_reports_the_agent_exit_code() {
    let Some(rt) = runtime().await else { return };
    // A short-lived agent that exits non-zero; wait must surface the code.
    let container = residue::create_owned(
        &rt,
        "it-podexit",
        &plan(&["sh", "-c", "exit 7"], RootfsPlan::HostUserland),
        residue::Backend::Podman,
    )
    .await;
    let id = container.id();
    let status = rt.wait(id).await.expect("wait for exit");
    assert_eq!(status.code, Some(7), "podman wait surfaces the exit code");
}

#[tokio::test]
async fn podman_rotates_and_persists_a_native_credential_file() {
    let Some(rt) = runtime().await else { return };
    let initial = br#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"old"}}"#;
    let refreshed = br#"{"claudeAiOauth":{"accessToken":"new","refreshToken":"rotated"}}"#;
    let broker = Arc::new(CredentialBroker(Mutex::new(initial.to_vec())));
    let provider = ContainerProvider::new(Arc::new(rt), "docker.io/library/busybox:latest")
        .with_clock(awaken_lease_contract::system_clock())
        .with_secret_broker(broker.clone());
    let _residue = residue::TestResidue::arm("pod-native-credential", residue::Backend::Podman);
    let spec = pc::SandboxSpec {
        process_tree_supervision: false,
        scope: "pod-native-credential".into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec![
            "sh".into(),
            "-c".into(),
            format!(
                "test \"$(cat /acp-config/.credentials.json)\" = '{}' && printf '%s' '{}' > /acp-config/.credentials.json",
                String::from_utf8_lossy(initial),
                String::from_utf8_lossy(refreshed)
            ),
        ],
        deny_tool_egress: false,
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
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
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
    sandbox.dispose_unattached().await.unwrap();
    assert_eq!(broker.0.lock().unwrap().as_slice(), refreshed);
}
