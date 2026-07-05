//! LocalProvider (ADR-0041 Slice 1) end-to-end over the neutral contract:
//! prepare → create → spawn → artifacts, plus mounts, handle/adopt, and the
//! process lifecycle. Trusted Workdir tier (not tool-transparent).

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxProvider;
use awaken_sandbox_local::{FileStore, FsFileStore, LocalProvider};

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Workdir,
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

#[tokio::test]
async fn capabilities_are_workdir_and_not_tool_transparent() {
    let tmp = tempfile::tempdir().unwrap();
    let caps = LocalProvider::new(tmp.path()).capabilities();
    assert_eq!(caps.isolation, pc::IsolationClass::Workdir);
    assert!(
        !caps.tool_transparent,
        "local tier must not host opaque agents"
    );
    assert!(!caps.enforced_readonly);
}

#[tokio::test]
async fn spawn_agent_yields_a_duplex_channel_to_the_process() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let sandbox = provider.create_sandbox(&spec("t-agent")).await.unwrap();

    // `cat` stands in for an opaque agent: it echoes stdin back on stdout, proving
    // the pipe-backed AgentChannel round-trips in both directions.
    let (proc, mut chan) = sandbox
        .spawn_agent(pc::Command::new(["cat"]))
        .await
        .unwrap();

    chan.write_all(b"ping\n").await.unwrap();
    chan.flush().await.unwrap();
    let mut buf = vec![0u8; 5];
    chan.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping\n");

    // Closing the write half ends `cat`; the process exits 0.
    drop(chan);
    assert_eq!(proc.wait().await.unwrap().code, Some(0));
}

#[tokio::test]
async fn secret_mount_materializes_the_brokered_credential_into_the_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    // The broker is faked by the seed map keyed on the secret reference.
    let provider =
        LocalProvider::new(tmp.path()).with_blob("broker://anthropic/key", b"sk-secret".to_vec());
    let mut spec = spec("t-secret");
    spec.mounts.push(pc::MountRequirement {
        mount_id: "auth".into(),
        source: pc::MountSource::Secret {
            reference: "broker://anthropic/key".into(),
            content_hash: None,
        },
        mount_path: "/workspace/.auth".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Durable,
        required: true,
    });
    assert!(spec.mounts[0].is_secret_writeback());

    let sandbox = provider.create(&spec).await.unwrap();
    assert_eq!(sandbox.realized().len(), 1);

    let proc = sandbox
        .spawn(sh(
            r#"cat workspace/.auth > "$AWAKEN_OUTPUTS_DIR/seen.txt""#,
        ))
        .await
        .unwrap();
    assert_eq!(proc.wait().await.unwrap().code, Some(0));
    let arts = sandbox.artifacts().await.unwrap();
    let seen = arts.iter().find(|a| a.path.ends_with("/seen.txt")).unwrap();
    assert_eq!(sandbox.read_artifact(&seen.id).await.unwrap(), b"sk-secret");
}

#[tokio::test]
async fn spawn_writes_output_and_artifacts_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let sandbox = provider.create(&spec("t-out")).await.unwrap();

    let proc = sandbox
        .spawn(sh(r#"printf 'hello' > "$AWAKEN_OUTPUTS_DIR/out.txt""#))
        .await
        .unwrap();
    let status = proc.wait().await.unwrap();
    assert_eq!(status.code, Some(0));

    let artifacts = sandbox.artifacts().await.unwrap();
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].path.ends_with("/out.txt"));
    assert_eq!(artifacts[0].size_bytes, 5);

    let bytes = sandbox.read_artifact(&artifacts[0].id).await.unwrap();
    assert_eq!(bytes, b"hello");
}

#[tokio::test]
async fn file_mount_is_realized_and_readable_by_a_process() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path()).with_blob("file_x", b"data123".to_vec());
    let mut spec = spec("t-mount");
    spec.mounts.push(pc::MountRequirement {
        mount_id: "in".into(),
        source: pc::MountSource::File {
            file_id: "file_x".into(),
            content_hash: None,
        },
        mount_path: "/workspace/in.txt".into(),
        // The Workdir tier cannot OS-enforce read-only, so a read-only mount is
        // (correctly) rejected by prepare_environment; a real ro guarantee needs
        // the Bwrap/Container tier. Here we realize a read-write input copy.
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    });

    let sandbox = provider.create(&spec).await.unwrap();
    assert_eq!(sandbox.realized().len(), 1);
    assert!(sandbox.realized()[0].content_hash.is_some());
    assert_eq!(sandbox.realized()[0].realization, pc::Realization::Copy);

    // cwd defaults to the jail root, so the mount is reachable at a relative path.
    let proc = sandbox
        .spawn(sh(
            r#"cat workspace/in.txt > "$AWAKEN_OUTPUTS_DIR/copy.txt""#,
        ))
        .await
        .unwrap();
    assert_eq!(proc.wait().await.unwrap().code, Some(0));

    let artifacts = sandbox.artifacts().await.unwrap();
    let copy = artifacts
        .iter()
        .find(|a| a.path.ends_with("/copy.txt"))
        .unwrap();
    assert_eq!(sandbox.read_artifact(&copy.id).await.unwrap(), b"data123");
}

#[tokio::test]
async fn required_mount_without_a_resolvable_source_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let mut spec = spec("t-badmount");
    spec.mounts.push(pc::MountRequirement {
        mount_id: "missing".into(),
        source: pc::MountSource::File {
            file_id: "nope".into(),
            content_hash: None,
        },
        mount_path: "/workspace/x".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    });
    assert!(provider.create(&spec).await.is_err());
}

#[tokio::test]
async fn env_is_injected_into_the_process() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let mut spec = spec("t-env");
    spec.env.push(pc::EnvVar {
        name: "GREETING".into(),
        value: pc::EnvValue::Inline {
            value: "salut".into(),
        },
        visibility: pc::EnvVisibility::Process,
    });
    let sandbox = provider.create(&spec).await.unwrap();
    let proc = sandbox
        .spawn(sh(
            r#"printf '%s' "$GREETING" > "$AWAKEN_OUTPUTS_DIR/env.txt""#,
        ))
        .await
        .unwrap();
    proc.wait().await.unwrap();
    let arts = sandbox.artifacts().await.unwrap();
    assert_eq!(sandbox.read_artifact(&arts[0].id).await.unwrap(), b"salut");
}

#[tokio::test]
async fn handle_serializes_and_adopt_reconnects() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let sandbox = provider.create(&spec("t-adopt")).await.unwrap();

    let handle = sandbox.handle();
    assert_eq!(handle.provider_kind, "local");
    assert_eq!(handle.sandbox_id, "t-adopt");

    // Persist → (simulated restart) → adopt from the wire.
    let wire = serde_json::to_string(&handle).unwrap();
    drop(sandbox);
    let recovered: pc::SandboxHandle = serde_json::from_str(&wire).unwrap();
    let adopted = provider.adopt(&recovered).await.unwrap();
    assert_eq!(adopted.id(), "t-adopt");
    assert!(matches!(
        adopted.status().await.unwrap(),
        pc::SandboxStatus::Ready
    ));
    adopted.renew_lease().await.unwrap();
}

#[tokio::test]
async fn exit_code_propagates() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = LocalProvider::new(tmp.path())
        .create(&spec("t-exit"))
        .await
        .unwrap();
    let proc = sandbox.spawn(sh("exit 7")).await.unwrap();
    assert_eq!(proc.wait().await.unwrap().code, Some(7));
}

#[tokio::test]
async fn poll_is_none_while_running_then_signal_terminates() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = LocalProvider::new(tmp.path())
        .create(&spec("t-poll"))
        .await
        .unwrap();
    let proc = sandbox.spawn(sh("sleep 5")).await.unwrap();
    assert!(proc.poll().await.unwrap().is_none(), "still running");
    proc.signal(pc::Signal::Kill).await.unwrap();
    let status = proc.wait().await.unwrap();
    assert!(status.signaled || status.code.is_none());
}

#[tokio::test]
async fn dispose_removes_the_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = LocalProvider::new(tmp.path())
        .create(&spec("t-dispose"))
        .await
        .unwrap();
    assert!(matches!(
        sandbox.status().await.unwrap(),
        pc::SandboxStatus::Ready
    ));
    sandbox.dispose().await.unwrap();
    assert!(matches!(
        sandbox.status().await.unwrap(),
        pc::SandboxStatus::Terminated
    ));
    sandbox.dispose().await.unwrap(); // idempotent
}

#[tokio::test]
async fn spawn_missing_program_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = LocalProvider::new(tmp.path())
        .create(&spec("t-missing"))
        .await
        .unwrap();
    assert!(
        sandbox
            .spawn(pc::Command::new(["definitely-not-a-real-binary-xyzzy"]))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn empty_argv_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = LocalProvider::new(tmp.path())
        .create(&spec("t-empty"))
        .await
        .unwrap();
    let empty: [&str; 0] = [];
    assert!(sandbox.spawn(pc::Command::new(empty)).await.is_err());
}

#[tokio::test]
async fn stronger_isolation_than_the_backend_offers_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let mut spec = spec("t-iso");
    spec.isolation = pc::IsolationClass::Namespace; // Workdir backend can't honor it
    assert!(LocalProvider::new(tmp.path()).create(&spec).await.is_err());
}

#[tokio::test]
async fn runtime_attach_and_process_reattach_are_unsupported_locally() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = LocalProvider::new(tmp.path())
        .create(&spec("t-unsup"))
        .await
        .unwrap();
    let req = pc::MountRequirement {
        mount_id: "late".into(),
        source: pc::MountSource::Other(serde_json::json!({ "content": "x" })),
        mount_path: "/workspace/late.txt".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::PerRun,
        required: false,
    };
    assert!(sandbox.attach(req).await.is_err());
    assert!(sandbox.process("proc-1").await.is_err());
    assert!(sandbox.read_artifact("no-such-id").await.is_err());
}

#[tokio::test]
async fn mount_resolves_from_a_content_addressed_file_store() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FsFileStore::open(tmp.path().join("blobs")).await.unwrap());
    let id = store.put(b"from-store").await.unwrap();

    let provider = LocalProvider::new(tmp.path().join("envs")).with_file_store(store);
    let mut spec = spec("t-fs");
    spec.mounts.push(pc::MountRequirement {
        mount_id: "in".into(),
        source: pc::MountSource::File {
            file_id: id.clone(),
            content_hash: Some(id.clone()), // content-addressed: id == hash
        },
        mount_path: "/workspace/in.txt".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    });
    let sandbox = provider.create(&spec).await.unwrap();
    assert_eq!(
        sandbox.realized()[0].content_hash.as_deref(),
        Some(id.as_str())
    );

    let proc = sandbox
        .spawn(sh(r#"cat workspace/in.txt > "$AWAKEN_OUTPUTS_DIR/o""#))
        .await
        .unwrap();
    proc.wait().await.unwrap();
    let arts = sandbox.artifacts().await.unwrap();
    assert_eq!(
        sandbox.read_artifact(&arts[0].id).await.unwrap(),
        b"from-store"
    );
}

#[tokio::test]
async fn declared_content_hash_mismatch_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path()).with_blob("f", b"real".to_vec());
    let mut spec = spec("t-hash");
    spec.mounts.push(pc::MountRequirement {
        mount_id: "in".into(),
        source: pc::MountSource::File {
            file_id: "f".into(),
            content_hash: Some("0000000000000000".into()), // wrong
        },
        mount_path: "/workspace/in.txt".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    });
    assert!(provider.create(&spec).await.is_err());
}

#[tokio::test]
async fn all_or_nothing_reaps_the_env_on_a_failed_required_mount() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let mut spec = spec("t-aon");
    spec.mounts.push(pc::MountRequirement {
        mount_id: "missing".into(),
        source: pc::MountSource::File {
            file_id: "absent".into(),
            content_hash: None,
        },
        mount_path: "/workspace/x".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    });
    assert!(provider.create(&spec).await.is_err());
    // No partial environment left behind.
    assert!(!tmp.path().join("t-aon").exists());
}
