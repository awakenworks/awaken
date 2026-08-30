use super::*;
use awaken_provisioning_contract::{
    IsolationClass, MountAccess, MountLifetime, MountRequirement, MountSource, NetworkPolicy,
    ResourceLimits, Sandbox, SandboxSpec,
};

struct Broker;

#[async_trait]
impl pc::SecretBroker for Broker {
    async fn materialize(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        assert_eq!(reference, "broker://k");
        Ok(b"broker-secret".to_vec())
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::new("process secrets are not supported"))
    }

    async fn write_back(&self, _reference: &str, _bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        unreachable!("the Workdir provider accepts only read-only brokered secrets")
    }
}

fn secret_spec(scope: &str) -> SandboxSpec {
    SandboxSpec {
        scope: scope.into(),
        isolation: IsolationClass::Workdir,
        mounts: vec![MountRequirement {
            mount_id: "auth".into(),
            source: MountSource::Secret {
                reference: "broker://k".into(),
                content_hash: None,
            },
            mount_path: "/workspace/.auth".into(),
            access: MountAccess::ReadWrite,
            lifetime: MountLifetime::PerRun,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: ResourceLimits::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    }
}

#[tokio::test]
async fn dispose_shreds_a_materialized_secret_before_reaping() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path()).with_blob("broker://k", b"sk-secret".to_vec());
    let sandbox = provider
        .create_sandbox(&secret_spec("t-shred"))
        .await
        .unwrap();

    // The credential is materialized in the sandbox FS and tracked for shredding.
    assert_eq!(sandbox.secret_paths.len(), 1);
    let path = sandbox.secret_paths[0].clone();
    assert_eq!(std::fs::read(&path).unwrap(), b"sk-secret");

    // Shred overwrites the bytes with zeros (runs before the directory is reaped).
    sandbox.shred_secrets().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), vec![0u8; 9]);

    // A non-secret mount is NOT tracked (only credentials are shredded).
    sandbox.dispose().await.unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn a_read_only_secret_uses_the_dedicated_broker() {
    let source = MountSource::Secret {
        reference: "broker://k".into(),
        content_hash: None,
    };
    let broker: Arc<dyn pc::SecretBroker> = Arc::new(Broker);
    let bytes = resolve_source(&source, &HashMap::new(), &None, Some(&broker))
        .await
        .unwrap();

    assert_eq!(bytes.unwrap(), b"broker-secret");
}

#[tokio::test]
async fn a_brokered_writable_secret_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path()).with_secret_broker(Arc::new(Broker));

    let error = match provider
        .create_sandbox(&secret_spec("t-broker-write"))
        .await
    {
        Ok(_) => panic!("brokered writable Secret must fail closed"),
        Err(error) => error,
    };
    assert!(error.0.contains("cannot write back"));
}

#[tokio::test]
async fn clearing_resource_projection_revokes_stale_mounts_but_keeps_workspace_files() {
    let tmp = tempfile::tempdir().unwrap();
    let mut spec = secret_spec("clear-projection");
    spec.mounts.clear();
    let sandbox = LocalProvider::new(tmp.path())
        .create_sandbox(&spec)
        .await
        .unwrap();
    let root = sandbox.root.root();
    std::fs::create_dir_all(root.join(".mnt/private")).unwrap();
    std::fs::write(root.join(".mnt/private/secret.txt"), "secret").unwrap();
    std::fs::write(root.join("work.txt"), "keep").unwrap();

    sandbox.clear_resource_projection().unwrap();

    assert!(!root.join(".mnt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("work.txt")).unwrap(),
        "keep"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_materialized_secret_is_owner_only_on_disk() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path()).with_blob("broker://k", b"sk-secret".to_vec());
    let sandbox = provider
        .create_sandbox(&secret_spec("t-perms"))
        .await
        .unwrap();

    let path = sandbox.secret_paths[0].clone();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "a realized secret must be owner-only, got {mode:o}"
    );
}

#[tokio::test]
async fn a_non_secret_mount_is_not_tracked_for_shredding() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path()).with_blob("file-x", b"data".to_vec());
    let mut spec = secret_spec("t-nosecret");
    spec.mounts[0].source = MountSource::File {
        file_id: "file-x".into(),
        content_hash: None,
    };
    let sandbox = provider.create_sandbox(&spec).await.unwrap();
    assert!(
        sandbox.secret_paths.is_empty(),
        "only Secret mounts are shredded"
    );
}

#[tokio::test]
async fn resolve_source_handles_every_mount_variant() {
    let mut blobs = HashMap::new();
    blobs.insert("f1".to_string(), b"file".to_vec());
    blobs.insert("r1".to_string(), b"res".to_vec());
    let none_store: Option<Arc<dyn pc::BlobSource>> = None;

    let file = MountSource::File {
        file_id: "f1".into(),
        content_hash: None,
    };
    assert_eq!(
        resolve_source(&file, &blobs, &none_store, None)
            .await
            .unwrap(),
        Some(b"file".to_vec())
    );
    let resource = MountSource::Resource {
        resource_id: "r1".into(),
        content_hash: None,
    };
    assert_eq!(
        resolve_source(&resource, &blobs, &none_store, None)
            .await
            .unwrap(),
        Some(b"res".to_vec())
    );
    // Typed inline content short-circuits before any store hit.
    let inline = MountSource::Inline {
        contents: "inline".into(),
    };
    assert_eq!(
        resolve_source(&inline, &blobs, &none_store, None)
            .await
            .unwrap(),
        Some(b"inline".to_vec())
    );
    let binary = vec![0, 0xff, 0x80, b'\n'];
    let carried = MountSource::InlineBytes {
        contents: binary.clone(),
        content_hash: Some(content_fingerprint(&binary)),
    };
    assert_eq!(
        resolve_source(&carried, &blobs, &none_store, None)
            .await
            .unwrap(),
        Some(binary.clone()),
        "binary input must round-trip without UTF-8 coercion"
    );
    assert!(verify(&carried, &binary).is_ok());
    assert!(verify(&carried, b"corrupt").is_err());
    // A memory store is not byte-resolvable through this path.
    let mem = MountSource::MemoryStore {
        store_id: "m".into(),
        materialization_reference: None,
        write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
    };
    assert_eq!(
        resolve_source(&mem, &blobs, &none_store, None)
            .await
            .unwrap(),
        None
    );
    // An unknown id resolves to nothing.
    let missing = MountSource::File {
        file_id: "nope".into(),
        content_hash: None,
    };
    assert_eq!(
        resolve_source(&missing, &blobs, &none_store, None)
            .await
            .unwrap(),
        None
    );
}

#[test]
fn declared_hash_and_verify_are_fail_closed_per_variant() {
    assert_eq!(
        declared_hash(&MountSource::Resource {
            resource_id: "r".into(),
            content_hash: Some("h".into()),
        }),
        Some("h")
    );
    // Non-hashable variants have no declared hash.
    assert_eq!(
        declared_hash(&MountSource::MemoryStore {
            store_id: "m".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        }),
        None
    );
    assert_eq!(
        declared_hash(&MountSource::InlineBytes {
            contents: Vec::new(),
            content_hash: Some("carried-hash".into()),
        }),
        Some("carried-hash")
    );

    let src = MountSource::File {
        file_id: "f".into(),
        content_hash: Some(content_fingerprint(b"right")),
    };
    assert!(verify(&src, b"right").is_ok());
    assert!(verify(&src, b"wrong").is_err());
    // No declared hash → nothing to verify against.
    assert!(
        verify(
            &MountSource::Inline {
                contents: String::new()
            },
            b"any"
        )
        .is_ok()
    );
}

fn bare_spec(scope: &str) -> SandboxSpec {
    let mut s = secret_spec(scope);
    s.mounts = Vec::new();
    s
}

#[tokio::test]
async fn legacy_local_adoption_reemits_v1_without_invented_evidence() {
    // Compatibility rule: a V1 row may support a non-Repository adoption,
    // but it cannot become V2 or gain owned paths merely by crossing a new
    // provider process.
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let scope = "legacy-local-v1";
    std::fs::create_dir_all(crate::sandbox_dir(tmp.path(), scope)).unwrap();
    let legacy = pc::SandboxHandle::local(
        scope,
        pc::LocalSandboxHandleV1 {
            outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.into(),
            base_env: Vec::new(),
            continuation_excluded_paths: Vec::new(),
            deny_tool_egress: false,
        },
    );
    let adopted = provider.adopt_sandbox(&legacy).await.unwrap();
    let emitted = Sandbox::handle(&adopted);
    assert_eq!(emitted, legacy);
    assert!(emitted.realization_fingerprint().is_none());
    assert!(emitted.owned_paths().is_none());
}

#[tokio::test]
async fn a_nested_resolvable_mount_and_an_optional_unresolvable_one_both_realize() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path()).with_blob("data-x", b"payload".to_vec());
    let mut spec = bare_spec("t-mounts");
    spec.mounts = vec![
        // Resolvable at a nested path → parent dirs are created (Some branch).
        MountRequirement {
            mount_id: "data".into(),
            source: MountSource::File {
                file_id: "data-x".into(),
                content_hash: None,
            },
            mount_path: "/workspace/deep/data.bin".into(),
            access: MountAccess::ReadWrite,
            lifetime: MountLifetime::PerRun,
            required: true,
        },
        // Optional + unresolvable → an empty placeholder (None branch).
        MountRequirement {
            mount_id: "opt".into(),
            source: MountSource::File {
                file_id: "absent".into(),
                content_hash: None,
            },
            mount_path: "/workspace/deep2/opt.bin".into(),
            access: MountAccess::ReadWrite,
            lifetime: MountLifetime::PerRun,
            required: false,
        },
    ];
    let sandbox = provider.create_sandbox(&spec).await.unwrap();
    let realized = sandbox.realized();
    assert_eq!(realized.len(), 2);
    let opt = realized.iter().find(|m| m.mount_id == "opt").unwrap();
    assert_eq!(opt.content_hash, None, "the placeholder carries no hash");
    let data = realized.iter().find(|m| m.mount_id == "data").unwrap();
    assert!(data.content_hash.is_some());
}

#[tokio::test]
async fn build_command_honors_cwd_and_inline_env_and_rejects_empty_argv() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let sandbox = provider.create_sandbox(&bare_spec("t-cmd")).await.unwrap();
    assert_eq!(
        sandbox.workspace_path(),
        crate::sandbox_dir(tmp.path(), "t-cmd")
    );

    let mut cmd = pc::Command::new(["/bin/true"]);
    cmd.cwd = "/work".into();
    cmd.env = vec![pc::EnvVar {
        name: "K".into(),
        value: pc::EnvValue::Inline { value: "V".into() },
        visibility: pc::EnvVisibility::Process,
    }];
    assert!(sandbox.build_command(cmd).await.is_ok());

    let empty = pc::Command::new(Vec::<String>::new());
    assert!(sandbox.build_command(empty).await.is_err());
}

#[tokio::test]
async fn a_spawned_local_process_reports_its_pid_and_reaps() {
    use awaken_provisioning_contract::ProcessHandle;
    let mut command = successful_command();
    let child = command.spawn().unwrap();
    let proc = LocalProcess::spawned(child);
    assert!(!proc.id().is_empty());
    let status = proc.wait().await.unwrap();
    assert_eq!(status.code, Some(0));
}

#[cfg(windows)]
fn successful_command() -> TokioCommand {
    let mut command = TokioCommand::new("cmd.exe");
    command.args(["/D", "/C", "exit /b 0"]);
    command
}

#[cfg(not(windows))]
fn successful_command() -> TokioCommand {
    TokioCommand::new("true")
}
