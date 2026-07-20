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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxProvider;
use awaken_sandbox_container::docker::DockerRuntime;
use awaken_sandbox_container::{ContainerProvider, ContainerRuntime, EgressProxy};

const AGENT_PORT: u16 = 8080;

struct CredentialBroker {
    bytes: Mutex<Vec<u8>>,
}

#[async_trait]
impl pc::SecretBroker for CredentialBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(self.bytes.lock().unwrap().clone())
    }

    async fn write_back(&self, _reference: &str, bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
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

/// A process-as-container running `command` with a caller-owned host file bound read-only
/// at `/data/in.txt`. A `CacheVolume` is the source for binding a host path *in place*
/// (ADR-0056) — `File` now means a content-store id resolved via `BlobSource`, so a raw
/// host-path bind uses `CacheVolume`. This drives a real byte bind on the Docker tier.
fn file_bind_spec(scope: &str, host_file: &str, command: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "in".into(),
            source: pc::MountSource::CacheVolume {
                host_path: host_file.into(),
                key: "in-cache".into(),
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
async fn inline_content_is_materialized_and_readable_in_a_real_container() {
    let Some((provider, rt)) = setup().await else {
        return;
    };
    // An `Inline` mount carries self-contained bytes (no host file, no store id) — the
    // provider stages them to a host file and binds it. This is the codex `config.toml` /
    // ADR-0038 resource path on the container tier. The container must read the content.
    let spec = pc::SandboxSpec {
        scope: "pw-inline".into(),
        isolation: pc::IsolationClass::Container,
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
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({
            "command": ["sh", "-c", "grep -q hello-inline-content /data/config.toml"]
        })),
    };
    let exit = run_to_exit(&provider, &rt, "pw-inline", &spec).await;
    assert_eq!(
        exit,
        Some(0),
        "inline content must be materialized to a host file and readable in the container"
    );
}

#[tokio::test]
async fn a_real_container_rotates_and_persists_a_native_credential_file() {
    let Some((_, rt)) = setup().await else {
        return;
    };
    let initial = br#"{"tokens":{"access_token":"old","refresh_token":"old-refresh"}}"#; // awaken-allow: secret -- synthetic test fixture
    let refreshed = br#"{"tokens":{"access_token":"new","refresh_token":"rotated"}}"#; // awaken-allow: secret -- synthetic test fixture
    let broker = Arc::new(CredentialBroker {
        bytes: Mutex::new(initial.to_vec()),
    });
    let provider =
        ContainerProvider::new(rt.clone(), "busybox:latest").with_secret_broker(broker.clone());
    let spec = pc::SandboxSpec {
        scope: "pw-native-credential".into(),
        isolation: pc::IsolationClass::Container,
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
        network: pc::NetworkPolicy::None,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({
            "command": ["sh", "-c", format!("test -f /acp-config/auth.json || exit 11; test \"$(cat /acp-config/auth.json)\" = '{}' || exit 12; printf '%s' '{}' > /acp-config/auth.json || exit 13", String::from_utf8_lossy(initial), String::from_utf8_lossy(refreshed))]
        })),
    };

    let exit = run_to_exit(&provider, &rt, "pw-native-credential", &spec).await;
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
        .with_blob("blob-42", b"resolved-from-the-store".to_vec());
    let spec = pc::SandboxSpec {
        scope: "pw-file-blob".into(),
        isolation: pc::IsolationClass::Container,
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
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({
            "command": ["sh", "-c", "grep -q resolved-from-the-store /data/in.txt"]
        })),
    };
    let exit = run_to_exit(&provider, &rt, "pw-file-blob", &spec).await;
    assert_eq!(
        exit,
        Some(0),
        "a File mount resolved from the BlobSource must be readable in the container"
    );
}

#[tokio::test]
async fn a_cachevolume_binds_a_host_directory_the_repo_checkout_shape() {
    let Some((provider, rt)) = setup().await else {
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
        scope: "pw-repodir".into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "repo".into(),
            source: pc::MountSource::CacheVolume {
                host_path: host_dir,
                key: "repo".into(),
            },
            mount_path: "/workspace/repo".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Session,
            required: true,
        }],
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({
            "command": ["sh", "-c",
                "grep -q repo-dir-marker /workspace/repo/src/main.rs && test -f /workspace/repo/.git/HEAD"]
        })),
    };
    let exit = run_to_exit(&provider, &rt, "pw-repodir", &spec).await;
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        exit,
        Some(0),
        "a CacheVolume must bind a host directory (nested files + .git) into the container"
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
    let Some((provider, rt)) = setup().await else {
        return;
    };

    let hog = |scope: &str, limits: pc::ResourceLimits| pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::None,
        outputs_path: "/mnt/session/outputs".into(),
        limits,
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({
            "command": ["dd", "if=/dev/zero", "of=/dev/shm/x", "bs=1M", "count=32"],
        })),
    };

    // Capped at 16 MiB: the 32 MiB allocation blows the cgroup -> OOM-kill (137).
    let capped = run_to_exit(
        &provider,
        &rt,
        "mem-capped",
        &hog(
            "mem-capped",
            pc::ResourceLimits {
                memory_bytes: Some(16 * 1024 * 1024),
                ..Default::default()
            },
        ),
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
        &rt,
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

/// Allowlist egress, verified against a REAL Docker daemon: unlike `None` (which the
/// daemon severs — proven by `deny_egress_confines_a_real_container`), the container
/// tier does NOT sever an `Allowlist` container's network at the daemon. It keeps the
/// bridge so the container can reach the brokered proxy CHOKEPOINT, and enforcement is
/// the proxy's job — so the enforceable, daemon-observable artifact is that the brokered
/// `HTTPS_PROXY` env is actually injected into the running container (a raw agent that
/// honors the proxy is then confined to the allowlist by the gateway). This proves the
/// real bollard create applies the `egress_plan`'s proxy env end to end, and that an
/// Allowlist without a configured proxy fails closed BEFORE any container is created.
///
/// NOTE (adjudication): "an Allowlist container is BLOCKED from a non-allowlisted host"
/// is NOT a bare-daemon invariant on this tier — the allowlist lives at the proxy, so a
/// true block test needs a real gateway deployed (out of this crate's scope). The
/// daemon-level severing invariant is the `None` case, already covered.
#[tokio::test]
async fn allowlist_egress_injects_the_brokered_proxy_into_a_real_container() {
    let Some((_, rt)) = setup().await else {
        return;
    };
    let proxy_url = "http://127.0.0.1:1/"; // never dialed; the container only reads the env
    let provider =
        ContainerProvider::new(rt.clone(), "busybox:latest").with_egress_proxy(EgressProxy {
            url: proxy_url.into(),
        });
    let spec = pc::SandboxSpec {
        scope: "pw-allowlist-proxy".into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        },
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({
            "command": ["sh", "-c", format!("[ \"$HTTPS_PROXY\" = \"{proxy_url}\" ]")],
        })),
    };
    let exit = run_to_exit(&provider, &rt, "pw-allowlist-proxy", &spec).await;
    assert_eq!(
        exit,
        Some(0),
        "an Allowlist container must run with the brokered HTTPS_PROXY injected (got {exit:?})"
    );

    // Fail-closed: without a configured proxy the same Allowlist spec is refused before
    // the daemon is touched (no silent full-egress container).
    let no_proxy = ContainerProvider::new(rt.clone(), "busybox:latest");
    assert!(
        no_proxy.create(&spec).await.is_err(),
        "an Allowlist without a broker must fail closed, never open egress silently"
    );
}

/// A long-lived process-as-container (sleep) used to prove cross-provider adoption.
fn sleeper_spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::None,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": ["sleep", "30"] })),
    }
}

/// G2 / ADR-0056 SPOF elimination on a shared substrate, verified against a REAL
/// daemon: a container OUTLIVES the provider (worker) that created it, so a PEER
/// provider over the SAME daemon re-ADOPTS the same running container from its durable
/// handle — no dispose, no re-create, so in-flight sandbox state survives a worker
/// crash. The handle round-trips through JSON first (the opaque `Claimed.sandbox`
/// form). A handle whose container is gone fails closed (adopt refuses a dead sandbox).
#[tokio::test]
async fn a_peer_provider_re_adopts_a_live_container_from_its_durable_handle() {
    let Some((provider_a, rt)) = setup().await else {
        return;
    };
    let _ = rt.remove("awaken-adopt-me").await;

    // Worker A creates a long-lived container and persists its handle as the opaque
    // durable string, then "crashes": the sandbox is dropped WITHOUT dispose, so the
    // container keeps running on the shared substrate.
    let handle: pc::SandboxHandle = {
        let sandbox = provider_a
            .create(&sleeper_spec("adopt-me"))
            .await
            .expect("worker A creates a real container");
        let wire = serde_json::to_string(&sandbox.handle()).expect("handle serializes");
        serde_json::from_str(&wire).expect("handle round-trips as the durable form")
        // `sandbox` dropped here — worker A crashes; the container is NOT disposed.
    };

    // Worker B: a fresh provider over the SAME daemon re-adopts the SAME container.
    // adopt's internal `inspect` succeeding is itself the proof the container survived.
    let (provider_b, _rt_b) = setup().await.expect("peer provider");
    let adopted = provider_b
        .adopt(&handle)
        .await
        .expect("a peer re-adopts the crashed worker's still-live container");
    assert_eq!(
        adopted.id(),
        handle.sandbox_id,
        "the adopted sandbox is the SAME one (same id), not a fresh create"
    );
    // Its main process is still running (poll = None) — state survived the crash.
    let proc = adopted
        .spawn(pc::Command::new(["true"]))
        .await
        .expect("handle to the still-running main process");
    assert!(
        proc.poll().await.expect("poll").is_none(),
        "the adopted container is still running (its state survived worker A's crash)"
    );

    // Teardown reaps it; a later adopt of the now-gone handle fails closed.
    adopted
        .dispose()
        .await
        .expect("dispose reaps the adopted container");
    for _ in 0..30 {
        if provider_b.adopt(&handle).await.is_err() {
            return; // gone -> adopt fails closed, as required
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("adopting a disposed/gone container must fail closed, but it kept succeeding");
}
