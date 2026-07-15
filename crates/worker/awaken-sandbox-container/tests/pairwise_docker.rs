//! Container-tier egress, verified against a REAL Docker daemon (not just planned).
//!
//! The pure planners (`egress_plan`, `network_of`) and the fake-runtime provider tests
//! prove the container tier *plans* the right `NetworkMode`; they cannot prove the
//! Docker adapter *applies* it. This does: a process-as-container probes for a default
//! route, and the result must differ by policy — `Unrestricted` reaches the network,
//! `None` does not. This is the container analogue of the bwrap `deny_egress_*` test,
//! and it is what caught the adapter dropping the planned `network_mode` (fail-open).
//!
//! Gated on the `docker` feature AND a reachable daemon (self-skips otherwise, same
//! discipline as `docker_it.rs`). Requires `busybox:latest` to be present locally.
//!
//! Run with: `cargo test -p awaken-sandbox-container --features docker --test pairwise_docker`
#![cfg(feature = "docker")]

use std::sync::Arc;
use std::time::Duration;

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxProvider;
use awaken_sandbox_container::docker::DockerRuntime;
use awaken_sandbox_container::{ContainerProvider, ContainerRuntime};

const AGENT_PORT: u16 = 8080;

async fn setup() -> Option<(ContainerProvider<DockerRuntime>, Arc<DockerRuntime>)> {
    let rt = Arc::new(DockerRuntime::connect_local(AGENT_PORT).ok()?);
    if rt.ping().await.is_err() {
        eprintln!("skipping: no reachable Docker daemon");
        return None;
    }
    Some((ContainerProvider::new(rt.clone(), "busybox:latest"), rt))
}

/// A process-as-container that exits 0 when it has a default route (egress path), 1 when
/// it does not — a deterministic, image-local probe that needs no external host.
fn egress_probe_spec(scope: &str, network: pc::NetworkPolicy) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        network,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({
            "command": ["sh", "-c", "ip route 2>/dev/null | grep -q default"],
        })),
    }
}

/// Realize `spec` as a real container and return its main-process exit code via `poll`
/// (inspect-based, so a non-zero exit is a real code — bollard's `wait` surfaces
/// non-zero as a stream error instead).
async fn run_to_exit(
    provider: &ContainerProvider<DockerRuntime>,
    rt: &Arc<DockerRuntime>,
    scope: &str,
    spec: &pc::SandboxSpec,
) -> Option<i32> {
    // Clear any container left by a previously-aborted run (name = `awaken-<scope>`).
    let _ = rt.remove(&format!("awaken-{scope}")).await;

    let sandbox = provider.create(spec).await.expect("create real container");
    // Process-as-container: `spawn` returns a handle to the main process (the probe).
    let proc = sandbox
        .spawn(pc::Command::new(["true"]))
        .await
        .expect("handle");

    let mut code = None;
    for _ in 0..50 {
        match proc.poll().await.expect("poll") {
            Some(exit) => {
                code = exit.code;
                break;
            }
            None => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    sandbox.dispose().await.expect("dispose");
    code
}

/// `Some(0)` = had egress, `Some(1)` = confined.
async fn probe_exit(
    provider: &ContainerProvider<DockerRuntime>,
    rt: &Arc<DockerRuntime>,
    scope: &str,
    network: pc::NetworkPolicy,
) -> Option<i32> {
    run_to_exit(provider, rt, scope, &egress_probe_spec(scope, network)).await
}

/// A process-as-container running `command` with a host file bound read-only at
/// `/data/in.txt`. The container tier binds a `File` mount's `source_ref` directly as a
/// host path (`mount_ref`), so this drives a real byte bind on the Docker tier.
fn file_bind_spec(scope: &str, host_file: &str, command: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "in".into(),
            source: pc::MountSource::File {
                file_id: host_file.into(),
                content_hash: None,
            },
            mount_path: "/data/in.txt".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": ["sh", "-c", command] })),
    }
}

#[tokio::test]
async fn deny_egress_confines_a_real_container() {
    let Some((provider, rt)) = setup().await else {
        return;
    };

    // Unrestricted: the container has a default route (the daemon bridge).
    let open = probe_exit(
        &provider,
        &rt,
        "pw-egress-open",
        pc::NetworkPolicy::Unrestricted,
    )
    .await;
    assert_eq!(
        open,
        Some(0),
        "an unrestricted container must have egress (default route)"
    );

    // None: the daemon gives it an empty network — the SAME probe must now fail. This
    // proves the adapter *applies* the planned `NetworkMode::None`, not just plans it.
    let denied = probe_exit(&provider, &rt, "pw-egress-none", pc::NetworkPolicy::None).await;
    assert_eq!(
        denied,
        Some(1),
        "a deny-egress container must have NO default route (network none enforced)"
    );
}

#[tokio::test]
async fn file_bind_is_readable_and_read_only_is_enforced_in_a_real_container() {
    let Some((provider, rt)) = setup().await else {
        return;
    };

    // A real host file to bind into the container (the daemon shares this host's fs).
    let host_file = std::env::temp_dir().join(format!("awaken-bind-{}.txt", std::process::id()));
    std::fs::write(&host_file, b"seed-bytes").unwrap();
    let host_file = host_file.to_string_lossy().to_string();

    // Read: the bound resource is visible inside the container.
    let read = run_to_exit(
        &provider,
        &rt,
        "pw-file-read",
        &file_bind_spec(
            "pw-file-read",
            &host_file,
            "grep -q seed-bytes /data/in.txt",
        ),
    )
    .await;
    assert_eq!(
        read,
        Some(0),
        "a File byte-bind must be readable in the container"
    );

    // Read-only enforced: a write to the `:ro` bind fails at the OS.
    let write = run_to_exit(
        &provider,
        &rt,
        "pw-file-ro",
        &file_bind_spec("pw-file-ro", &host_file, "echo mutate > /data/in.txt"),
    )
    .await;
    assert_ne!(
        write,
        Some(0),
        "a read-only File bind must reject writes in the container"
    );
    // The host file is untouched by the confined write attempt.
    assert_eq!(std::fs::read(&host_file).unwrap(), b"seed-bytes");
}
