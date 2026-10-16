// @formal evidence sandbox.docker_absence_requires_explicit_backend_not_found
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
//! Feature decision rule: C1 `docker` is enabled; C2 `podman` is disabled.
//! E1 this Docker test binary still compiles and retains its Docker residue
//! cleanup; the Podman-only runtime arm is unreachable. With both features
//! enabled, each daemon retains its own cleanup arm. The two feature-matrix
//! builds exercise these rules before the live OOM case below runs.
//!
//! Run with: `cargo test -p awaken-sandbox-container --features docker --test pairwise_docker`
#![cfg(feature = "docker")]

mod common;
#[path = "common/residue.rs"]
mod residue;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use awaken_memory_store::{MemoryWriter, VolatileMemoryRepository};
use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerHandleCodec as _;
use awaken_sandbox_container::docker::DockerRuntime;
use awaken_sandbox_container::{ContainerProvider, ForwardProxy, command_of};
use awaken_sandbox_memoryd::MemoryStoreMounter;
use common::memory_mount;

const AGENT_PORT: u16 = 8080;

struct CredentialBroker {
    bytes: Mutex<Vec<u8>>,
}

#[async_trait]
impl pc::SecretBroker for CredentialBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(self.bytes.lock().unwrap().clone())
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
        *self.bytes.lock().unwrap() = bytes;
        Ok(())
    }
}

async fn setup() -> Option<(ContainerProvider<DockerRuntime>, Arc<DockerRuntime>)> {
    let rt = Arc::new(DockerRuntime::connect_local(AGENT_PORT).ok()?);
    if rt.ping().await.is_err() {
        eprintln!("skipping: no reachable Docker daemon");
        return None;
    }
    Some((
        ContainerProvider::new(rt.clone(), "busybox:latest")
            .with_clock(awaken_time::system_clock()),
        rt,
    ))
}

/// A process-as-container that exits 0 when it has a default route (egress path), 1 when
/// it does not — a deterministic, image-local probe that needs no external host.
fn egress_probe_spec(scope: &str, network: pc::NetworkPolicy) -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec![
            "sh".into(),
            "-c".into(),
            "ip route 2>/dev/null | grep -q default".into(),
        ],
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
    }
}

/// Realize `spec` as a real container and return its main-process exit code via `poll`
/// (inspect-based, so a non-zero exit is a real code — bollard's `wait` surfaces
/// non-zero as a stream error instead).
async fn run_to_exit(
    provider: &ContainerProvider<DockerRuntime>,
    scope: &str,
    spec: &pc::SandboxSpec,
) -> Option<i32> {
    run_to_exit_disposing(provider, scope, spec, Disposal::MustSucceed).await
}

/// Whether the row may end without a clean disposal.
///
/// A container whose cgroup is out of memory cannot start a process at all, so
/// it can neither reap the execs it started nor be harvested, and disposal
/// fails closed on exactly that. The memory-cap row is the one place that
/// state is deliberate, so it is the one place the failure is admitted — by
/// name, not by softening every other row. The scope guard still removes the
/// container either way.
#[derive(Clone, Copy, PartialEq)]
enum Disposal {
    MustSucceed,
    MayFailUnderMemoryPressure,
}

async fn run_to_exit_disposing(
    provider: &ContainerProvider<DockerRuntime>,
    scope: &str,
    spec: &pc::SandboxSpec,
    disposal: Disposal,
) -> Option<i32> {
    // D2 causes: C1 another gate may run the same fixed pairwise row at once;
    // C2 a prior attempt may leave this process's scope behind. Effect: only
    // this process's scope is swept, created, observed, and disposed; another
    // gate cannot remove a live row while its owner is disposing it.
    let scope = format!("{scope}-{}", std::process::id());
    let mut spec = spec.clone();
    spec.scope = scope.clone();
    let _residue = residue::TestResidue::arm(&scope, residue::Backend::Docker);

    let sandbox = provider.create(&spec).await.expect("create real container");
    let command = command_of(&spec);
    assert!(
        !command.is_empty(),
        "test spec must declare an exec command"
    );
    let proc = sandbox
        .spawn(pc::Command::new(command))
        .await
        .expect("exec command");

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
    let physical = sandbox
        .handle()
        .container_payload()
        .expect("typed container handle")
        .container_id
        .clone();
    let disposed = sandbox.dispose_unattached().await;
    match disposal {
        Disposal::MustSucceed => disposed.expect("dispose"),
        Disposal::MayFailUnderMemoryPressure => assert!(
            !daemon_holds(&physical),
            "an environment whose harvest was skipped is removed all the same",
        ),
    }
    code
}

/// Whether the daemon still has this container, asked of the daemon rather
/// than of the provider that just gave it up.
fn daemon_holds(container_id: &str) -> bool {
    std::process::Command::new("docker")
        .args(["inspect", container_id])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// `Some(0)` = had egress, `Some(1)` = confined.
async fn probe_exit(
    provider: &ContainerProvider<DockerRuntime>,
    scope: &str,
    network: pc::NetworkPolicy,
) -> Option<i32> {
    run_to_exit(provider, scope, &egress_probe_spec(scope, network)).await
}

/// A process-as-container running `command` with a caller-owned host file bound read-only
/// at `/data/in.txt`. A `CacheVolume` is the source for binding a host path *in place*
/// (ADR-0056) — `File` now means a content-store id resolved via `BlobSource`, so a raw
/// host-path bind uses `CacheVolume`. This drives a real byte bind on the Docker tier.
fn file_bind_spec(scope: &str, host_file: &str, command: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec!["sh".into(), "-c".into(), command.into()],
        deny_tool_egress: false,
        mounts: vec![pc::MountRequirement {
            mount_id: "in".into(),
            source: pc::MountSource::CacheVolume {
                location: pc::CacheVolumeLocation::HostPath {
                    path: host_file.into(),
                },
                key: "in-cache".into(),
            },
            mount_path: "/data/in.txt".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
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
    }
}

#[tokio::test]
async fn deny_egress_confines_a_real_container() {
    // D2: an open and a denied row may overlap another gate's identical
    // pairwise suite. Process-owned scopes keep both physical containers
    // distinct; each row still proves its own default-route outcome.
    let Some((provider, _)) = setup().await else {
        return;
    };

    // Unrestricted: the container has a default route (the daemon bridge).
    let open = probe_exit(&provider, "pw-egress-open", pc::NetworkPolicy::Unrestricted).await;
    assert_eq!(
        open,
        Some(0),
        "an unrestricted container must have egress (default route)"
    );

    // None: the daemon gives it an empty network — the SAME probe must now fail. This
    // proves the adapter *applies* the planned `NetworkMode::None`, not just plans it.
    let denied = probe_exit(&provider, "pw-egress-none", pc::NetworkPolicy::None).await;
    assert_eq!(
        denied,
        Some(1),
        "a deny-egress container must have NO default route (network none enforced)"
    );
}

#[tokio::test]
async fn inline_content_is_materialized_and_readable_in_a_real_container() {
    let Some((provider, _)) = setup().await else {
        return;
    };
    // An `Inline` mount carries self-contained bytes (no host file, no store id) — the
    // provider stages them to a host file and binds it. This is the codex `config.toml` /
    // ADR-0038 resource path on the container tier. The container must read the content.
    let spec = pc::SandboxSpec {
        process_tree_supervision: false,
        scope: "pw-inline".into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec![
            "sh".into(),
            "-c".into(),
            "grep -q hello-inline-content /data/config.toml".into(),
        ],
        deny_tool_egress: false,
        mounts: vec![pc::MountRequirement {
            mount_id: "cfg".into(),
            source: pc::MountSource::Inline {
                contents: "[mcp_servers.gh]\nhello-inline-content\n".into(),
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
        limits: Default::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
    };
    let exit = run_to_exit(&provider, "pw-inline", &spec).await;
    assert_eq!(
        exit,
        Some(0),
        "inline content must be materialized to a host file and readable in the container"
    );
}

#[tokio::test]
async fn multiple_memory_stores_enforce_exact_access_and_seal_the_parent_in_real_docker() {
    let Some((provider, _)) = setup().await else {
        return;
    };
    let memory = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    memory
        .create("rw-store", "/note.md", "rw-seed")
        .await
        .unwrap();
    memory
        .create("ro-store", "/note.md", "ro-seed")
        .await
        .unwrap();
    provider.install_memory_mounter(Arc::new(MemoryStoreMounter::copy_only(memory.clone())));

    // Cause/effect decision table for the real runtime boundary:
    // C1=two stores share only `/mnt/memory`; C2=RW child; C3=RO child;
    // C4=write targets unbound parent. R1 C1+C2 => E1 read+write and durable
    // harvest; R2 C1+C3 => E2 read succeeds, write fails, durable head unchanged;
    // R3 C1+C4 => E3 parent write fails and cannot create an unowned store path.
    // Constraint: assertions execute as the real container user against daemon
    // bind flags and the read-only image root, not against the planner model.
    let spec = pc::SandboxSpec {
        process_tree_supervision: false,
        scope: "pw-memory-boundary".into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec![
            "sh".into(),
            "-c".into(),
            concat!(
                "test \"$(cat /mnt/memory/rw/note.md)\" = rw-seed && ",
                "test \"$(cat /mnt/memory/ro/note.md)\" = ro-seed && ",
                "printf rw-updated > /mnt/memory/rw/note.md && ",
                "! sh -c 'printf forbidden > /mnt/memory/ro/note.md' && ",
                "! sh -c 'printf escaped > /mnt/memory/unbound.md'"
            )
            .into(),
        ],
        deny_tool_egress: false,
        mounts: vec![
            memory_mount(
                "rw-memory",
                "rw-store",
                "/mnt/memory/rw",
                pc::MountAccess::ReadWrite,
            ),
            memory_mount(
                "ro-memory",
                "ro-store",
                "/mnt/memory/ro",
                pc::MountAccess::ReadOnly,
            ),
        ],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::None,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
    };

    assert_eq!(
        run_to_exit(&provider, "pw-memory-boundary", &spec).await,
        Some(0),
        "real Docker must enforce both child access modes and the sealed parent",
    );
    assert_eq!(
        awaken_resource_contract::MemoryQueries::get_by_path(
            memory.as_ref(),
            "rw-store",
            "/note.md"
        )
        .await
        .unwrap()
        .unwrap()
        .content
        .as_deref(),
        Some("rw-updated"),
        "RW copy is harvested through the canonical mounter",
    );
    assert_eq!(
        awaken_resource_contract::MemoryQueries::get_by_path(
            memory.as_ref(),
            "ro-store",
            "/note.md"
        )
        .await
        .unwrap()
        .unwrap()
        .content
        .as_deref(),
        Some("ro-seed"),
        "RO copy cannot mutate the durable head",
    );
}

#[tokio::test]
async fn a_real_container_rotates_and_persists_a_native_credential_file() {
    let Some((_, rt)) = setup().await else {
        return;
    };
    let initial = br#"{"tokens":{"access_token":"old","refresh_token":"old-refresh"}}"#; // awaken-allow(secret): -- synthetic test fixture
    let refreshed = br#"{"tokens":{"access_token":"new","refresh_token":"rotated"}}"#; // awaken-allow(secret): -- synthetic test fixture
    let broker = Arc::new(CredentialBroker {
        bytes: Mutex::new(initial.to_vec()),
    });
    let provider = ContainerProvider::new(rt.clone(), "busybox:latest")
        .with_clock(awaken_time::system_clock())
        .with_secret_broker(broker.clone());
    let spec = pc::SandboxSpec {
        process_tree_supervision: false,
        scope: "pw-native-credential".into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec![
            "sh".into(),
            "-c".into(),
            format!(
                "test -f /acp-config/auth.json || exit 11; test \"$(cat /acp-config/auth.json)\" = '{}' || exit 12; printf '%s' '{}' > /acp-config/auth.json || exit 13",
                String::from_utf8_lossy(initial),
                String::from_utf8_lossy(refreshed)
            ),
        ],
        deny_tool_egress: false,
        mounts: vec![pc::MountRequirement {
            mount_id: "codex-auth".into(),
            source: pc::MountSource::Secret {
                reference: "credential://acp/native/codex".into(),
                content_hash: None,
            },
            mount_path: "/acp-config/auth.json".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Durable,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::None,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
    };

    let exit = run_to_exit(&provider, "pw-native-credential", &spec).await;
    assert_eq!(exit, Some(0));
    assert_eq!(broker.bytes.lock().unwrap().as_slice(), refreshed);
}

#[tokio::test]
async fn a_file_mount_resolved_from_the_blob_source_is_readable_in_a_real_container() {
    let Some((_, rt)) = setup().await else {
        return;
    };
    // A `File` mount carries a content id, not bytes. The provider resolves it through the
    // injected `BlobSource` (seeded here), verifies the hash, stages the bytes, and binds
    // them — the container reads the resolved file. This is the container analogue of the
    // bwrap tier's `resolve_source`, proving the store→bind wiring end-to-end.
    let provider = ContainerProvider::new(rt.clone(), "busybox:latest")
        .with_clock(awaken_time::system_clock())
        .with_blob("blob-42", b"resolved-from-the-store".to_vec());
    let spec = pc::SandboxSpec {
        process_tree_supervision: false,
        scope: "pw-file-blob".into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec![
            "sh".into(),
            "-c".into(),
            "grep -q resolved-from-the-store /data/in.txt".into(),
        ],
        deny_tool_egress: false,
        mounts: vec![pc::MountRequirement {
            mount_id: "in".into(),
            source: pc::MountSource::File {
                file_id: "blob-42".into(),
                content_hash: None,
            },
            mount_path: "/data/in.txt".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
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
    let exit = run_to_exit(&provider, "pw-file-blob", &spec).await;
    assert_eq!(
        exit,
        Some(0),
        "a File mount resolved from the BlobSource must be readable in the container"
    );
}

#[tokio::test]
async fn a_cachevolume_binds_a_host_directory_the_repo_checkout_shape() {
    let Some((provider, _)) = setup().await else {
        return;
    };
    // A repo checkout on the container tier reuses the CacheVolume primitive: the host clones
    // (or otherwise stages) the repo into a host DIRECTORY, and CacheVolume binds that dir in
    // place — no new mechanism. This proves CacheVolume binds a directory (nested files, RW),
    // which is exactly the repo-into-container shape (host stages, container binds).
    let dir = std::env::temp_dir().join(format!("awaken-repo-{}", std::process::id()));
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rs"), b"// repo-dir-marker\n").unwrap();
    std::fs::write(dir.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    let host_dir = dir.to_string_lossy().to_string();

    let spec = pc::SandboxSpec {
        process_tree_supervision: false,
        scope: "pw-repodir".into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec![
            "sh".into(),
            "-c".into(),
            "grep -q repo-dir-marker /workspace/repo/src/main.rs && test -f /workspace/repo/.git/HEAD".into(),
        ],
        deny_tool_egress: false,
        mounts: vec![pc::MountRequirement {
            mount_id: "repo".into(),
            source: pc::MountSource::CacheVolume {
                location: pc::CacheVolumeLocation::HostPath { path: host_dir },
                key: "repo".into(),
            },
            mount_path: "/workspace/repo".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Session,
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
    let exit = run_to_exit(&provider, "pw-repodir", &spec).await;
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        exit,
        Some(0),
        "a CacheVolume must bind a host directory (nested files + .git) into the container"
    );
}

#[tokio::test]
async fn file_bind_is_readable_and_read_only_is_enforced_in_a_real_container() {
    let Some((provider, _)) = setup().await else {
        return;
    };

    // A real host file to bind into the container (the daemon shares this host's fs).
    let host_file = std::env::temp_dir().join(format!("awaken-bind-{}.txt", std::process::id()));
    std::fs::write(&host_file, b"seed-bytes").unwrap();
    let host_file = host_file.to_string_lossy().to_string();

    // Read: the bound resource is visible inside the container.
    let read = run_to_exit(
        &provider,
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

/// G5: a memory cap the adapter set (`CgroupCaps::from_limits` -> bollard
/// `HostConfig.memory` + swap pinned to it) is ENFORCED BY THE KERNEL, not merely
/// planned. `dd` is the container's main process (process-as-container), so a cgroup
/// OOM-kill surfaces as its exit code (137 = 128+SIGKILL). Writing 32 MiB to tmpfs
/// `/dev/shm` (a memory cgroup accounts tmpfs pages) blows a 16 MiB cap but fits the
/// default 64 MiB shm, so the SAME write under no cap completes (exit 0). The
/// difference is the cgroup, not the workload — this is what a dropped/ignored limit
/// (fail-open) would miss, the memory analogue of the egress `deny_egress` probe.
#[tokio::test]
async fn a_memory_cap_oom_kills_an_over_allocating_container() {
    let Some((provider, _)) = setup().await else {
        return;
    };

    let hog = |scope: &str, limits: pc::ResourceLimits| pc::SandboxSpec {
        process_tree_supervision: false,
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec![
            "dd".into(),
            "if=/dev/zero".into(),
            "of=/dev/shm/x".into(),
            "bs=1M".into(),
            "count=32".into(),
        ],
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::None,
        outputs_path: "/mnt/session/outputs".into(),
        requests: pc::ResourceRequests::default(),
        limits,
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
    };

    // Capped at 16 MiB: the 32 MiB allocation blows the cgroup -> OOM-kill (137).
    let capped = run_to_exit_disposing(
        &provider,
        "mem-capped",
        &hog(
            "mem-capped",
            pc::ResourceLimits {
                memory_bytes: Some(16 * 1024 * 1024),
                ..Default::default()
            },
        ),
        Disposal::MayFailUnderMemoryPressure,
    )
    .await;
    assert_eq!(
        capped,
        Some(137),
        "a memory-capped over-allocator is OOM-killed (exit 137); got {capped:?}"
    );

    // Uncapped: the identical allocation completes (exit 0) — proving the kill above
    // was the cgroup, not the workload.
    let uncapped = run_to_exit(
        &provider,
        "mem-uncapped",
        &hog("mem-uncapped", pc::ResourceLimits::default()),
    )
    .await;
    assert_eq!(
        uncapped,
        Some(0),
        "the same allocation without a cap completes (exit 0); got {uncapped:?}"
    );
}

/// A forward-proxy environment is not an allowlist boundary: arbitrary code can
/// remove it and use the Docker bridge directly. Both configured and unconfigured
/// cases must therefore fail before the daemon creates a container.
#[tokio::test]
async fn allowlist_rejects_a_forward_proxy_before_docker_creation() {
    let Some((_, rt)) = setup().await else {
        return;
    };
    let provider = ContainerProvider::new(rt.clone(), "busybox:latest")
        .with_clock(awaken_time::system_clock())
        .with_forward_proxy(ForwardProxy {
            url: "http://127.0.0.1:1/".into(),
        });
    let spec = pc::SandboxSpec {
        process_tree_supervision: false,
        scope: "pw-allowlist-proxy".into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec!["true".into()],
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        },
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
    };
    assert!(provider.create(&spec).await.is_err());
    let no_proxy = ContainerProvider::new(rt.clone(), "busybox:latest")
        .with_clock(awaken_time::system_clock());
    assert!(
        no_proxy.create(&spec).await.is_err(),
        "an Allowlist without enforcement must fail closed"
    );
}

/// Residue decision table: C1 two live scopes share one test-process owner;
/// C2 the guard for scope A is dropped; C3 the guard for scope B is dropped.
/// R1 C1+C2 -> A is gone and B remains executable. R2 C1+C2+C3 -> B is gone.
/// A scoped guard may remove only its own scope; process-wide residue cleanup
/// belongs to an isolated one-test process with unnameable capacity scopes.
#[tokio::test]
async fn scoped_residue_drop_preserves_another_live_container_in_the_same_process() {
    let Some((provider, _)) = setup().await else {
        return;
    };
    let scope_a = format!("residue-a-{}", std::process::id());
    let scope_b = format!("residue-b-{}", std::process::id());
    let guard_a = residue::TestResidue::arm(&scope_a, residue::Backend::Docker);
    let a = provider
        .create(&egress_probe_spec(&scope_a, pc::NetworkPolicy::None))
        .await
        .expect("create A");
    let guard_b = residue::TestResidue::arm(&scope_b, residue::Backend::Docker);
    let b = provider
        .create(&egress_probe_spec(&scope_b, pc::NetworkPolicy::None))
        .await
        .expect("create B");
    let a_id = a.handle().container_payload().unwrap().container_id.clone();
    let b_id = b.handle().container_payload().unwrap().container_id.clone();
    assert!(daemon_holds(&a_id) && daemon_holds(&b_id), "C1");

    drop(guard_a);
    assert!(!daemon_holds(&a_id), "R1: A was removed");
    assert!(daemon_holds(&b_id), "R1: B was not removed by A's guard");
    let process = b
        .spawn(pc::Command::new(["true"]))
        .await
        .expect("R1: B still execs");
    assert_eq!(process.wait().await.unwrap().code, Some(0), "R1");

    drop(guard_b);
    assert!(!daemon_holds(&b_id), "R2: B was removed by its own guard");
}
