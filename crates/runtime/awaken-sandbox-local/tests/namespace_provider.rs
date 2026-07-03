//! NamespaceProvider (ADR-0041 Slice 2, bubblewrap tier). The pure argv renderer
//! is unit-tested in the crate; here we cover the provider's capabilities and
//! fail-closed rules (no tool needed) plus a real bwrap exec proving path fidelity
//! (gated on bwrap being usable, so CI without namespaces still passes).

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxProvider;
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
