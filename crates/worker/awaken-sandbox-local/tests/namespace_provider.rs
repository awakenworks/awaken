//! NamespaceProvider (ADR-0041 Slice 2, bubblewrap tier). The pure argv renderer
//! is unit-tested in the crate; here we cover the provider's capabilities and
//! fail-closed rules (no tool needed) plus a real bwrap exec proving path fidelity
//! (gated on bwrap being usable, so CI without namespaces still passes).

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::{Sandbox, SandboxProvider};
mod common;
use awaken_file_store::{FileStore, FsFileStore};
use awaken_sandbox_local::NamespaceProvider;

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Namespace,
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: None,
    }
}

fn sh(script: &str) -> pc::Command {
    let mut c = pc::Command::new(["sh", "-c", script]);
    c.stdio = pc::Stdio::Null;
    c
}

/// True only when bwrap exists AND unprivileged user namespaces are enabled.
async fn bwrap_works() -> bool {
    tokio::process::Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "--", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// True only when macOS `sandbox-exec` (Seatbelt) can run a trivial profile.
async fn seatbelt_works() -> bool {
    tokio::process::Command::new("sandbox-exec")
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether the OS-native isolator this platform uses actually runs here.
async fn os_native_sandbox_works() -> bool {
    if cfg!(target_os = "macos") {
        seatbelt_works().await
    } else {
        bwrap_works().await
    }
}

#[tokio::test]
async fn capabilities_are_tool_transparent_namespace() {
    let tmp = tempfile::tempdir().unwrap();
    let caps = NamespaceProvider::new(tmp.path()).capabilities();
    assert_eq!(caps.isolation, pc::IsolationClass::Namespace);
    assert!(caps.tool_transparent, "bwrap tier may host opaque agents");
    assert!(caps.path_fidelity);
    assert!(caps.enforced_readonly);
}

#[tokio::test]
async fn probe_ready_reflects_the_os_native_sandbox_availability() {
    // Ungated: the probe must return exactly whether the OS-native isolator runs here
    // — bwrap userns on Linux, Seatbelt on macOS. This is the signal `select_provider`
    // fails closed on.
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let first = provider.probe_ready().await.is_ok();
    assert_eq!(first, os_native_sandbox_works().await);
    // Memoized: a second call (even from a different provider) yields the same result.
    let other = NamespaceProvider::new(tmp.path());
    assert_eq!(other.probe_ready().await.is_ok(), first);
}

#[tokio::test]
async fn seatbelt_confines_a_real_spawn_on_macos() {
    // Self-skips off macOS (no `sandbox-exec`). On macOS, `spawn` renders via
    // `sandbox_exec_argv`, so a trivial command runs OS-confined under Seatbelt.
    if !seatbelt_works().await {
        eprintln!("skipping: Seatbelt/sandbox-exec unavailable (non-macOS)");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = NamespaceProvider::new(tmp.path())
        .create_sandbox(&spec("t-seatbelt"))
        .await
        .unwrap();
    let proc = sandbox.spawn(sh("exit 0")).await.unwrap();
    assert_eq!(proc.wait().await.unwrap().code, Some(0));
    sandbox.dispose().await.unwrap();
}

#[tokio::test]
async fn dispose_reaps_and_shreds_a_secret_mounted_sandbox() {
    // Ungated: create/realize/dispose need no bwrap (only spawn does). Exercises the
    // secret-path capture in realize_layout and the shred-then-reap in dispose.
    let tmp = tempfile::tempdir().unwrap();
    let mut s = spec("t-sec");
    s.mounts.push(pc::MountRequirement {
        mount_id: "auth".into(),
        source: pc::MountSource::Secret {
            reference: "broker://k".into(),
            content_hash: None,
        },
        mount_path: "/workspace/.auth".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    });
    let provider =
        NamespaceProvider::new(tmp.path()).with_blob("broker://k", b"sk-secret".to_vec());
    let sandbox = provider.create_sandbox(&s).await.unwrap();
    assert_eq!(sandbox.realized().len(), 1);
    sandbox.dispose().await.unwrap();
    assert!(matches!(
        sandbox.status().await.unwrap(),
        pc::SandboxStatus::Terminated
    ));
}

#[tokio::test]
async fn host_allowlist_egress_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let mut spec = spec("t-net");
    spec.network = pc::NetworkPolicy::Allowlist {
        hosts: vec!["api.anthropic.com".into()],
    };
    // bwrap can share/unshare net but not enforce a host allowlist → fail closed.
    assert!(
        NamespaceProvider::new(tmp.path())
            .create(&spec)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn bwrap_exec_has_path_fidelity_and_collects_outputs() {
    if !bwrap_works().await {
        eprintln!("skipping: bwrap/userns unavailable on this host");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let sandbox = provider.create(&spec("t-bwrap")).await.unwrap();

    // /mnt/session/outputs is a REAL absolute path inside the sandbox (fidelity).
    let proc = sandbox
        .spawn(sh("printf hi > /mnt/session/outputs/o.txt"))
        .await
        .unwrap();
    assert_eq!(proc.wait().await.unwrap().code, Some(0));

    let arts = sandbox.artifacts().await.unwrap();
    assert_eq!(arts.len(), 1);
    assert!(arts[0].path.ends_with("/o.txt"));
    assert_eq!(sandbox.read_artifact(&arts[0].id).await.unwrap(), b"hi");
    sandbox.dispose().await.unwrap();
}

#[tokio::test]
async fn bwrap_read_only_mount_is_enforced() {
    if !bwrap_works().await {
        eprintln!("skipping: bwrap/userns unavailable on this host");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path()).with_blob("f", b"seed".to_vec());
    let mut spec = spec("t-ro");
    spec.mounts.push(pc::MountRequirement {
        mount_id: "in".into(),
        source: pc::MountSource::File {
            file_id: "f".into(),
            content_hash: None,
        },
        mount_path: "/data/in.txt".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    });
    let sandbox = provider.create(&spec).await.unwrap();

    // Read succeeds; write to the read-only bind fails at the OS.
    let read = sandbox
        .spawn(sh("cat /data/in.txt > /mnt/session/outputs/copy.txt"))
        .await
        .unwrap();
    assert_eq!(read.wait().await.unwrap().code, Some(0));
    let arts = sandbox.artifacts().await.unwrap();
    let copy = arts.iter().find(|a| a.path.ends_with("/copy.txt")).unwrap();
    assert_eq!(sandbox.read_artifact(&copy.id).await.unwrap(), b"seed");

    let write = sandbox
        .spawn(sh("echo mutate > /data/in.txt"))
        .await
        .unwrap();
    assert_ne!(
        write.wait().await.unwrap().code,
        Some(0),
        "read-only bind must reject writes"
    );
    sandbox.dispose().await.unwrap();
}

// ── Non-gated: realization + lifecycle without executing a process (no bwrap) ──

#[tokio::test]
async fn create_realizes_env_and_lifecycle_without_executing() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let sandbox = provider.create(&spec("t-nx")).await.unwrap();

    assert_eq!(sandbox.id(), "t-nx");
    let handle = sandbox.handle();
    assert_eq!(handle.provider_kind, "bwrap");
    assert!(matches!(
        sandbox.status().await.unwrap(),
        pc::SandboxStatus::Ready
    ));
    assert!(sandbox.artifacts().await.unwrap().is_empty());
    assert!(sandbox.read_artifact("nope").await.is_err());
    assert!(sandbox.realized().is_empty());
    assert!(sandbox.renew_lease().await.is_ok());

    // Runtime attach / process reattach are not offered by this local tier.
    let req = pc::MountRequirement {
        mount_id: "late".into(),
        source: pc::MountSource::Other(serde_json::json!({ "content": "x" })),
        mount_path: "/data/late".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::PerRun,
        required: false,
    };
    assert!(sandbox.attach(req).await.is_err());
    assert!(sandbox.process("p").await.is_err());

    sandbox.dispose().await.unwrap();
    assert!(matches!(
        sandbox.status().await.unwrap(),
        pc::SandboxStatus::Terminated
    ));
}

#[tokio::test]
async fn adopt_reconnects_from_a_persisted_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let handle = provider.create(&spec("t-adopt")).await.unwrap().handle();
    let wire = serde_json::to_string(&handle).unwrap();
    let recovered: pc::SandboxHandle = serde_json::from_str(&wire).unwrap();
    let adopted = provider.adopt(&recovered).await.unwrap();
    assert_eq!(adopted.id(), "t-adopt");
    assert!(matches!(
        adopted.status().await.unwrap(),
        pc::SandboxStatus::Ready
    ));
}

#[tokio::test]
async fn file_store_mount_is_realized_as_a_bind() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FsFileStore::open(tmp.path().join("blobs")).await.unwrap());
    let id = store.put(b"seed-bytes").await.unwrap();
    let provider = NamespaceProvider::new(tmp.path().join("envs"))
        .with_blob_source(common::blob_source(store));
    let mut spec = spec("t-fs");
    spec.mounts.push(pc::MountRequirement {
        mount_id: "in".into(),
        source: pc::MountSource::File {
            file_id: id.clone(),
            content_hash: Some(id.clone()),
        },
        mount_path: "/data/in.txt".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    });
    let sandbox = provider.create(&spec).await.unwrap();
    assert_eq!(sandbox.realized().len(), 1);
    assert_eq!(sandbox.realized()[0].realization, pc::Realization::Bind);
    assert_eq!(
        sandbox.realized()[0].content_hash.as_deref(),
        Some(id.as_str())
    );
}

#[tokio::test]
async fn content_hash_mismatch_and_all_or_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    // mismatch
    let mut spec = spec("t-hash");
    spec.mounts.push(pc::MountRequirement {
        mount_id: "in".into(),
        source: pc::MountSource::File {
            file_id: "f".into(),
            content_hash: Some("0000000000000000".into()),
        },
        mount_path: "/data/in".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    });
    let provider = NamespaceProvider::new(tmp.path()).with_blob("f", b"real".to_vec());
    assert!(provider.create(&spec).await.is_err());

    // all-or-nothing: a failed required mount reaps the env
    let mut spec2 = spec2_missing_required();
    spec2.scope = "t-aon".into();
    assert!(
        NamespaceProvider::new(tmp.path())
            .create(&spec2)
            .await
            .is_err()
    );
    assert!(!tmp.path().join("t-aon").exists());
}

fn spec2_missing_required() -> pc::SandboxSpec {
    let mut s = spec("x");
    s.mounts.push(pc::MountRequirement {
        mount_id: "missing".into(),
        source: pc::MountSource::File {
            file_id: "absent".into(),
            content_hash: None,
        },
        mount_path: "/data/x".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    });
    s
}

#[tokio::test]
async fn memory_store_realizes_as_copy_and_harvests_on_dispose() {
    use awaken_memory_store::{InMemoryFs, MemoryFs};
    use awaken_sandbox_memoryd::MemoryStoreMounter;

    let fs = Arc::new(InMemoryFs::new());
    fs.create("s", "/note.md", "v1").await.unwrap();
    // The bwrap tier cannot splice a host FUSE into its namespace yet (ADR-0053
    // item 2), so it uses a copy-only mounter: materialize on create, harvest on
    // dispose. This is deterministic regardless of /dev/fuse on the host.
    let mounter = Arc::new(MemoryStoreMounter::copy_only(fs.clone()));
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path()).with_memory_mounter(mounter);

    let mut s = spec("t-nsmem");
    s.mounts.push(pc::MountRequirement {
        mount_id: "mem".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "s".into(),
        },
        mount_path: "/workspace/memory".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Durable,
        required: true,
    });

    let sandbox = provider.create(&s).await.unwrap();
    assert_eq!(sandbox.realized()[0].realization, pc::Realization::Copy);

    // The materialized file is present and binds into the namespace layout.
    let note = tmp.path().join("t-nsmem/workspace/memory/note.md");
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "v1");

    // Edit + dispose → harvested back to the durable store.
    std::fs::write(&note, "v2").unwrap();
    sandbox.dispose().await.unwrap();
    assert_eq!(
        fs.get_by_path("s", "/note.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("v2"),
    );
}

/// The bwrap FUSE-splice (ADR-0053 item 2): a memory store FUSE-mounted on the host is
/// bound into the bwrap mount+user namespace, so the agent reads/writes it LIVE inside
/// the sandbox (write-through), not a harvested copy. This proves the host FUSE mount
/// survives the bind into bwrap's namespace under `--unshare-user` (the mount's owner
/// uid is identity-mapped, so no `allow_other` is needed). Gated on bwrap + /dev/fuse;
/// a host without either realizes the copy fallback (covered by `memory_mount.rs`), so
/// this asserts the live path only when FUSE is actually realized.
#[tokio::test]
async fn bwrap_splices_a_live_fuse_memory_mount_into_the_namespace() {
    if !bwrap_works().await {
        eprintln!("skipping: bwrap/userns unavailable on this host");
        return;
    }
    if !std::path::Path::new("/dev/fuse").exists() {
        eprintln!("skipping: no /dev/fuse (copy fallback is covered by memory_mount.rs)");
        return;
    }
    use awaken_memory_store::{InMemoryFs, MemoryFs};
    use awaken_sandbox_memoryd::MemoryStoreMounter;

    let fs = Arc::new(InMemoryFs::new());
    fs.create("s", "/note.md", "v1").await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    // The FUSE-preferring mounter (not `copy_only`): it FUSE-mounts the store at the
    // host path the provider then binds into the namespace.
    let provider = NamespaceProvider::new(tmp.path())
        .with_memory_mounter(Arc::new(MemoryStoreMounter::new(fs.clone())));

    let mut spec = spec("t-mem-fuse");
    spec.mounts.push(pc::MountRequirement {
        mount_id: "mem".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "s".into(),
        },
        mount_path: "/mnt/memory".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Durable,
        required: true,
    });
    let sandbox = provider.create(&spec).await.unwrap();

    // Only assert the SPLICE when FUSE was actually realized; otherwise it fell back to
    // copy (a legitimate outcome this host doesn't exercise), covered elsewhere.
    let is_fuse = sandbox
        .realized()
        .iter()
        .any(|m| m.mount_path == "/mnt/memory" && matches!(m.realization, pc::Realization::Fuse));
    if !is_fuse {
        eprintln!("skipping: memory realized as copy here, not FUSE (fallback covered elsewhere)");
        sandbox.dispose().await.ok();
        return;
    }

    // INSIDE the bwrap namespace: read the seeded memory (proves the live FUSE mount is
    // visible through the bind) and write an edit back through it (write-through).
    let proc = sandbox
        .spawn(sh(
            "cat /mnt/memory/note.md > /mnt/session/outputs/read.txt && printf v2 > /mnt/memory/note.md",
        ))
        .await
        .unwrap();
    assert_eq!(
        proc.wait().await.unwrap().code,
        Some(0),
        "the agent read + wrote the FUSE memory mount inside the namespace"
    );
    let arts = sandbox.artifacts().await.unwrap();
    let read = arts
        .iter()
        .find(|a| a.path.ends_with("/read.txt"))
        .expect("read.txt artifact");
    assert_eq!(
        sandbox.read_artifact(&read.id).await.unwrap(),
        b"v1",
        "the seeded memory was visible LIVE inside the bwrap namespace (spliced FUSE mount)"
    );

    // dispose unmounts the FUSE mount (write-through already flushed the edit to the store).
    sandbox.dispose().await.unwrap();
    assert_eq!(
        fs.get_by_path("s", "/note.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("v2"),
        "the in-namespace write propagated LIVE to the durable store through the FUSE mount"
    );
}
