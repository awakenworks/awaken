//! NamespaceProvider (ADR-0041 Slice 2, bubblewrap tier). The pure argv renderer
//! is unit-tested in the crate; here we cover the provider's capabilities and
//! fail-closed rules (no tool needed) plus a real bwrap exec proving path fidelity
//! (gated on bwrap being usable, so CI without namespaces still passes).

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxProvider;
use awaken_sandbox_local::{FileStore, FsFileStore, NamespaceProvider};

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
    let store = Arc::new(FsFileStore::open(tmp.path().join("blobs")).unwrap());
    let id = store.put(b"seed-bytes").unwrap();
    let provider = NamespaceProvider::new(tmp.path().join("envs")).with_file_store(store);
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
