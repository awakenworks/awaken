use super::*;
use pc::Sandbox as _;
use tokio::io::AsyncReadExt as _;

struct Broker;

#[async_trait]
impl pc::SecretBroker for Broker {
    async fn materialize(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        assert_eq!(reference, "broker://namespace");
        Ok(b"namespace-secret".to_vec())
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::new("process secrets are not supported"))
    }

    async fn write_back(&self, _reference: &str, _bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        unreachable!("the Namespace provider accepts only read-only brokered secrets")
    }
}

fn input<'a>(
    ws: &'a std::path::Path,
    out: &'a std::path::Path,
    mounts: &'a [RenderMount],
    env: &'a [(String, String)],
    net: &'a pc::NetworkPolicy,
    argv: &'a [String],
) -> RenderInput<'a> {
    RenderInput {
        host_workspace: ws,
        host_outputs: out,
        outputs_path: "/mnt/session/outputs",
        mounts,
        env,
        network: net,
        cwd: "",
        argv,
    }
}

fn ns_spec(scope: &str, mounts: Vec<pc::MountRequirement>) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Namespace,
        mounts,
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    }
}

#[tokio::test]
async fn ready_create_replay_projects_namespace_receipt_without_remounting() {
    // Namespace create-replay table: C1 lifecycle is Creating/Ready; C2
    // effect is exact/drifted. N1 Creating materializes and records one
    // Bind result; N2 Ready+exact only reprojects that receipt/layout and
    // preserves post-Ready bytes; N3 drift rejects. A rematerialization
    // would replace `after-ready` with the frozen Inline value.
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let spec = ns_spec(
        "namespace-ready-receipt",
        vec![pc::MountRequirement {
            mount_id: "inline".into(),
            source: pc::MountSource::Inline {
                contents: "declared".into(),
            },
            mount_path: "/workspace/receipt.txt".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
    );
    let effect = pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX).unwrap();
    let first = provider
        .create_sandbox_for_effect(&spec, &effect, None)
        .await
        .expect("N1");
    let first_handle = pc::Sandbox::handle(&first);
    std::fs::write(
        first.workspace_root().root().join("receipt.txt"),
        b"after-ready",
    )
    .unwrap();

    let replay = provider
        .create_sandbox_for_effect(&spec, &effect, None)
        .await
        .expect("N2");
    assert_eq!(pc::Sandbox::handle(&replay), first_handle, "N2 receipt");
    assert_eq!(
        std::fs::read(replay.workspace_root().root().join("receipt.txt")).unwrap(),
        b"after-ready",
        "N2 zero rematerialization"
    );
    assert!(
        provider
            .create_sandbox_for_effect(
                &spec,
                &pc::SandboxEffectFence::new("drifted", "owner", "runtime", 1, u64::MAX,).unwrap(),
                None,
            )
            .await
            .is_err(),
        "N3"
    );
}

#[tokio::test]
async fn legacy_namespace_adoption_reemits_v1_without_invented_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let scope = "legacy-namespace-v1";
    std::fs::create_dir_all(crate::sandbox_dir(tmp.path(), scope)).unwrap();
    let legacy = pc::SandboxHandle::namespace(
        NamespaceProvider::provider_kind(),
        scope,
        pc::NamespaceSandboxHandleV1 {
            outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.into(),
            base_env: Vec::new(),
            network: pc::NetworkPolicy::Unrestricted,
            control_services: Default::default(),
        },
    );
    let adopted = provider.adopt_sandbox(&legacy).await.unwrap();
    let emitted = pc::Sandbox::handle(&adopted);
    assert_eq!(emitted, legacy);
    assert!(emitted.realization_fingerprint().is_none());
    assert!(emitted.owned_paths().is_none());
}

/// Repository provider rule: C1 an explicit non-workspace absolute path is
/// carried in the plan; E1 reject it before Git, E2 create neither the exact
/// target nor the former silently-relocated `/workspace/...` target.
#[tokio::test]
async fn repository_realization_rejects_non_workspace_paths_without_relocation() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = NamespaceProvider::new(tmp.path())
        .create_sandbox(&ns_spec("namespace-repository-path", Vec::new()))
        .await
        .unwrap();
    let plan = pc::RepositoryRealizationPlan {
        repository_id: "repo".into(),
        mount_path: "/repo".into(),
        source_remote_url: "file:///does-not-matter".into(),
        transport_url: "file:///does-not-matter".into(),
        initial_branch: None,
        initial_commit: None,
        access: pc::MountAccess::ReadWrite,
    };

    let error = sandbox
        .provision_repo(&plan, None)
        .expect_err("C1/E1 unsupported path");
    assert!(error.to_string().contains("current providers require"));
    assert!(!sandbox.root.root().join("repo").exists(), "E2 exact");
    assert!(
        !sandbox.workspace_root().root().join("repo").exists(),
        "E2 no relocation"
    );
}

/// Namespace crash-cut adapter rule: C1 the exact `/workspace` final name
/// already contains an incomplete/non-Repository directory; C2 the durable
/// handle may reserve that path. N1 C1(+either C2 value) => E1 reject before
/// Git, E2 preserve the directory and its bytes. Absent, exact replay, and
/// publish-race rows are owned by the shared `provision_repo_at` decision
/// table, so Namespace does not maintain a second realization state machine.
#[tokio::test]
async fn repository_realization_preserves_an_incomplete_namespace_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = NamespaceProvider::new(tmp.path())
        .create_sandbox(&ns_spec("namespace-repository-occupied", Vec::new()))
        .await
        .unwrap();
    let occupied = sandbox.workspace_root().root().join("repo");
    std::fs::create_dir(&occupied).unwrap();
    std::fs::write(occupied.join("PRESERVED"), "user bytes").unwrap();
    let plan = pc::RepositoryRealizationPlan {
        repository_id: "repo".into(),
        mount_path: "/workspace/repo".into(),
        source_remote_url: "https://network-must-not-run.invalid/repository".into(),
        transport_url: "https://network-must-not-run.invalid/repository".into(),
        initial_branch: None,
        initial_commit: None,
        access: pc::MountAccess::ReadWrite,
    };

    sandbox
        .provision_repo(&plan, None)
        .expect_err("N1 incomplete destination fails closed");
    assert_eq!(
        std::fs::read_to_string(occupied.join("PRESERVED")).unwrap(),
        "user bytes",
        "N1/E2 existing bytes are never deleted"
    );
}

#[tokio::test]
async fn create_sandbox_realizes_a_resolvable_and_an_optional_unresolvable_mount() {
    use pc::Sandbox;
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path()).with_blob("blob-x", b"payload".to_vec());
    let spec = ns_spec(
        "t-ns-mounts",
        vec![
            pc::MountRequirement {
                mount_id: "data".into(),
                source: pc::MountSource::File {
                    file_id: "blob-x".into(),
                    content_hash: None,
                },
                mount_path: "/workspace/deep/data.bin".into(),
                access: pc::MountAccess::ReadOnly,
                lifetime: pc::MountLifetime::PerRun,
                required: true,
            },
            pc::MountRequirement {
                mount_id: "opt".into(),
                source: pc::MountSource::File {
                    file_id: "absent".into(),
                    content_hash: None,
                },
                mount_path: "/workspace/deep2/opt.bin".into(),
                access: pc::MountAccess::ReadOnly,
                lifetime: pc::MountLifetime::PerRun,
                required: false,
            },
        ],
    );
    let sandbox = provider.create_sandbox(&spec).await.unwrap();
    assert_eq!(sandbox.realized().len(), 2);
}

#[tokio::test]
async fn a_read_only_secret_is_materialized_by_the_dedicated_broker() {
    use pc::Sandbox;
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path()).with_secret_broker(Arc::new(Broker));
    let spec = ns_spec(
        "t-ns-secret",
        vec![pc::MountRequirement {
            mount_id: "auth".into(),
            source: pc::MountSource::Secret {
                reference: "broker://namespace".into(),
                content_hash: None,
            },
            mount_path: "/workspace/.auth".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
    );

    let sandbox = provider.create_sandbox(&spec).await.unwrap();
    assert_eq!(sandbox.realized().len(), 1);
    assert_eq!(
        std::fs::read(&sandbox.secret_paths[0]).unwrap(),
        b"namespace-secret"
    );
}

#[tokio::test]
async fn a_memory_store_mount_without_a_mounter_fails_loud() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let spec = ns_spec(
        "t-ns-mem",
        vec![pc::MountRequirement {
            mount_id: "mem".into(),
            source: pc::MountSource::MemoryStore {
                store_id: "s1".into(),
                materialization_reference: None,
                write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
            },
            mount_path: "/workspace/mem".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
    );
    assert!(provider.create_sandbox(&spec).await.is_err());
}

#[tokio::test]
async fn cold_namespace_copy_requires_exact_effect_scoped_ack() {
    // Namespace terminal-copy preparation/authorization table: C1 the current
    // handle carries the complete canonical copy evidence; C2 ack evidence is
    // exact/mismatched; C3 terminal fence is live same-effect/stale/same-lease
    // successor/foreign; C4 aggregate preparation is/is not durably accepted.
    // N1 C1+exact+live records that effect without rebuilding a MemoryMount and
    // is idempotent after a lost response; N2 every invalid C2/C3 row leaves the
    // exact root untouched; N3 prepare is rejected until N1, while exact prepare
    // and its replay have zero physical effect; C5 the C response is lost and
    // same-operation renewal D retries before/after the C root CAS. N4 both
    // orders return immutable C; shorter/foreign inputs have zero mutation. N5
    // only C4 permits the separate C->D physical-disposal port to remove the root.
    // Guard draining itself is provider-shared and covered by the mixed
    // Copy/FUSE table in `memory_mount_release_tests`.
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let spec = ns_spec(
        "terminal-namespace-memory-copy",
        vec![pc::MountRequirement {
            mount_id: "memory".into(),
            source: pc::MountSource::MemoryStore {
                store_id: "store".into(),
                materialization_reference: None,
                write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
            },
            mount_path: "/workspace/memory".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Session,
            required: true,
        }],
    );
    let create =
        pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX - 2).unwrap();
    let terminal =
        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX - 1).unwrap();
    let stale = pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, 0).unwrap();
    let successor =
        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX).unwrap();
    let foreign =
        pc::SandboxEffectFence::new("foreign", "other-owner", "runtime", 1, u64::MAX).unwrap();
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec);
    let root = crate::sandbox_dir(tmp.path(), &spec.scope);
    let mut creation =
        crate::realization_marker::begin(&root, &fingerprint, &create, None, None).unwrap();
    creation.prepare_root().unwrap();
    std::fs::create_dir_all(root.join("workspace/memory")).unwrap();
    std::fs::write(root.join("workspace/memory/value"), b"candidate").unwrap();
    let evidence = pc::MemoryMaterializationEvidence::new(
        "store",
        "/workspace/memory",
        vec![pc::MemoryMaterializationHead {
            path: "value".into(),
            id: "head".into(),
            content_sha256: "digest".into(),
        }],
    )
    .unwrap();
    let completion = crate::realization_marker::RealizationCompletionReceipt::new(
        &[pc::RealizedMount {
            mount_id: "memory".into(),
            mount_path: "/workspace/memory".into(),
            access: pc::MountAccess::ReadWrite,
            realization: pc::Realization::Copy,
            content_hash: None,
        }],
        vec![evidence.clone()],
    )
    .unwrap();
    let realization = creation.complete(&completion).unwrap();
    let handle = pc::SandboxHandle::namespace_v2(
        NamespaceProvider::provider_kind(),
        spec.scope.clone(),
        pc::NamespaceSandboxHandleV2 {
            previous: pc::NamespaceSandboxHandleV1 {
                outputs_path: spec.outputs_path.clone(),
                base_env: spec.env.clone(),
                network: spec.network.clone(),
                control_services: Default::default(),
            },
            realization_fingerprint: fingerprint,
            effect_fence: create,
            physical_incarnation: realization.physical_incarnation().to_owned(),
            owned_paths: vec!["/workspace/memory".into()],
        },
    )
    .with_memory_materializations(vec![evidence.clone()])
    .unwrap();
    let cold = provider
        .prepare_terminal_sandbox_for_effect(&spec, Some(&handle), None, &terminal)
        .await
        .unwrap()
        .expect("N1");
    assert!(cold.memory_mounts.lock().await.is_empty(), "N1");
    assert!(
        cold.prepare_disposal_for_effect(&terminal).await.is_err(),
        "N3 preparation requires ack"
    );
    let mismatched = pc::MemoryMaterializationEvidence::new(
        "store",
        "/workspace/memory",
        vec![pc::MemoryMaterializationHead {
            path: "value".into(),
            id: "head".into(),
            content_sha256: "different".into(),
        }],
    )
    .unwrap();
    assert!(
        pc::Sandbox::acknowledge_memory_reconciliation(&cold, &terminal, &[mismatched],)
            .await
            .is_err(),
        "N2 mismatched evidence"
    );
    assert!(
        pc::Sandbox::acknowledge_memory_reconciliation(
            &cold,
            &stale,
            std::slice::from_ref(&evidence),
        )
        .await
        .is_err(),
        "N2 stale"
    );
    assert!(root.is_dir(), "N2 zero physical effect");
    pc::Sandbox::acknowledge_memory_reconciliation(
        &cold,
        &terminal,
        std::slice::from_ref(&evidence),
    )
    .await
    .expect("N1 exact");
    pc::Sandbox::acknowledge_memory_reconciliation(
        &cold,
        &terminal,
        std::slice::from_ref(&evidence),
    )
    .await
    .expect("N1 replay");
    let prepared = cold
        .prepare_disposal_for_effect(&terminal)
        .await
        .expect("N3 exact preparation");
    let replayed = cold
        .prepare_disposal_for_effect(&terminal)
        .await
        .expect("N3 preparation response-loss replay");
    assert_eq!(prepared, terminal, "N3 exact durable predecessor");
    assert_eq!(replayed, terminal, "N3 response-loss predecessor");
    assert!(root.is_dir(), "N3 preparation has zero physical effect");
    pc::Sandbox::acknowledge_memory_reconciliation(
        &cold,
        &successor,
        std::slice::from_ref(&evidence),
    )
    .await
    .expect("N4 same-lease successor");
    assert!(
        pc::Sandbox::acknowledge_memory_reconciliation(
            &cold,
            &foreign,
            std::slice::from_ref(&evidence),
        )
        .await
        .is_err(),
        "N4 foreign lease"
    );
    assert!(root.is_dir(), "N4 rejected effects are non-destructive");
    let renewed_replay = cold
        .prepare_disposal_for_effect(&successor)
        .await
        .expect("N4 successor preparation");
    assert_eq!(renewed_replay, terminal, "N4 C/D CAS orders share C");
    assert!(
        cold.prepare_disposal_for_effect(&stale).await.is_err(),
        "N4 shorter retry"
    );
    assert!(
        cold.prepare_disposal_for_effect(&foreign).await.is_err(),
        "N4 foreign retry"
    );
    assert!(root.is_dir(), "N4 preparation cannot dispose the root");
    let disposal = crate::test_disposal_authorization_for_current(&terminal, &successor);
    cold.dispose_for_effect(&disposal)
        .await
        .expect("N5 aggregate-authorized physical disposal");
}

#[tokio::test]
async fn namespace_lifecycle_helpers_cover_adoption_and_projection_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let sandbox = provider
        .create_sandbox(&ns_spec("t-ns-lifecycle", Vec::new()))
        .await
        .unwrap();

    let wrong = pc::SandboxHandle::new("local", "t-ns-lifecycle");
    assert!(provider.adopt_sandbox(&wrong).await.is_err());

    sandbox
        .materialize_inline("nested/value.txt", b"value")
        .unwrap();
    assert_eq!(
        sandbox.list_files("nested").unwrap(),
        vec![("value.txt".to_string(), b"value".to_vec())]
    );
    sandbox.remove_inline("nested").unwrap();
    sandbox.remove_inline("nested").unwrap();

    let outputs = sandbox.root.resolve("/outputs").unwrap();
    std::fs::create_dir_all(&outputs).unwrap();
    std::fs::write(outputs.join("result.txt"), b"result").unwrap();
    assert_eq!(
        sandbox.list_files("/outputs").unwrap(),
        vec![("result.txt".to_string(), b"result".to_vec())]
    );

    let projection = sandbox
        .root
        .resolve(pc::WorkspaceLayout::RESOURCE_PROJECTION_ROOT)
        .unwrap();
    std::fs::create_dir_all(projection.join("resource")).unwrap();
    sandbox.clear_resource_projection().unwrap();
    sandbox.clear_resource_projection().unwrap();

    std::fs::write(&projection, b"stale").unwrap();
    sandbox.clear_resource_projection().unwrap();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outputs.join("result.txt"), &projection).unwrap();
        sandbox.clear_resource_projection().unwrap();
    }
}

#[tokio::test]
async fn adoption_preserves_network_policy_and_rejects_corrupt_handles() {
    // Invariant: recovery may preserve or narrow an isolation decision; it
    // must never replace `None` with a more permissive network policy.
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let mut spec = ns_spec("t-ns-network-recovery", Vec::new());
    spec.network = pc::NetworkPolicy::None;
    let original = provider.create_sandbox(&spec).await.unwrap();
    let handle = pc::Sandbox::handle(&original);
    let adopted = provider.adopt_sandbox(&handle).await.unwrap();
    assert_eq!(adopted.network, pc::NetworkPolicy::None);

    let mut corrupt = serde_json::to_value(&handle).unwrap();
    corrupt["payload"]
        .as_object_mut()
        .unwrap()
        .remove("network");
    assert!(serde_json::from_value::<pc::SandboxHandle>(corrupt).is_err());

    let legacy = pc::SandboxHandle::namespace(
        NamespaceProvider::provider_kind(),
        "t-ns-network-recovery",
        pc::NamespaceSandboxHandleV1 {
            outputs_path: spec.outputs_path.clone(),
            base_env: spec.env.clone(),
            network: pc::NetworkPolicy::None,
            control_services: Default::default(),
        },
    );
    let legacy = serde_json::from_value::<pc::SandboxHandle>(serde_json::to_value(legacy).unwrap())
        .expect("legacy V1 remains decodable");
    provider
        .adopt_sandbox(&legacy)
        .await
        .expect("legacy non-Repository namespace remains adoptable");

    let mut unknown_schema = serde_json::to_value(&handle).unwrap();
    unknown_schema["payload"]["schema"] = serde_json::json!("namespace_v3");
    assert!(serde_json::from_value::<pc::SandboxHandle>(unknown_schema).is_err());
}

/// Live attach cause/effect decision table:
/// | source | path | access | effect |
/// |---|---|---|---|
/// | InlineBytes | absolute | read-only | one runtime-owned file + RO bind |
/// | unresolved external source | any | any | reject, layout unchanged |
/// Detach removes both the bind entry and backing file, so a later process
/// cannot observe a stale mount.
#[tokio::test]
async fn live_inline_mount_updates_and_revokes_the_namespace_bind_layout() {
    use pc::Sandbox;

    let tmp = tempfile::tempdir().unwrap();
    let sandbox = NamespaceProvider::new(tmp.path())
        .create_sandbox(&ns_spec("t-ns-live-mount", Vec::new()))
        .await
        .unwrap();
    let mount_path = "/mnt/session/uploads/workspace/live.txt";
    let realized = sandbox
        .attach(pc::MountRequirement {
            mount_id: "file_live".into(),
            source: pc::MountSource::InlineBytes {
                contents: b"live".to_vec(),
                content_hash: None,
            },
            mount_path: mount_path.into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        })
        .await
        .unwrap();
    assert_eq!(realized.mount_path, mount_path);
    assert!(
        sandbox
            .layout
            .read()
            .unwrap()
            .iter()
            .any(|mount| mount.dest == mount_path && mount.read_only)
    );
    let backing = sandbox.root.resolve(mount_path).unwrap();
    assert_eq!(std::fs::read(&backing).unwrap(), b"live");

    let before = sandbox.layout.read().unwrap().len();
    assert!(
        sandbox
            .attach(pc::MountRequirement {
                mount_id: "external".into(),
                source: pc::MountSource::File {
                    file_id: "unresolved".into(),
                    content_hash: None,
                },
                mount_path: "/mnt/session/uploads/external".into(),
                access: pc::MountAccess::ReadOnly,
                lifetime: pc::MountLifetime::PerRun,
                required: true,
            })
            .await
            .is_err()
    );
    assert_eq!(sandbox.layout.read().unwrap().len(), before);

    sandbox.remove_mount(mount_path).unwrap();
    assert!(!backing.exists());
    assert!(sandbox.layout.read().unwrap().is_empty());
}

#[tokio::test]
async fn opaque_processes_and_runtime_projections_share_the_workspace_root() {
    if !bwrap_usable().await {
        return;
    }
    // Cause-effect graph: C1=runtime projects a workspace-relative file;
    // C2=an opaque process reads the same sandbox path; C3=a same-named path
    // does not exist at the outer namespace root. E1=projected bytes are
    // readable through the one process launcher; E2=the outer root cannot
    // become a competing tool workspace.
    //
    // | Rule | C1 | C2 | C3 | Effects |
    // | W1   | yes | yes | yes | E1,E2 |
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let sandbox = provider
        .create_sandbox(&ns_spec("t-ns-tool-workspace", Vec::new()))
        .await
        .unwrap();
    sandbox
        .materialize_inline(".awaken/tool-results/result.txt", b"complete")
        .unwrap();

    let (process, mut channel) = sandbox
        .spawn_agent(pc::Command::new([
            "/bin/sh",
            "-c",
            "cat .awaken/tool-results/result.txt",
        ]))
        .await
        .unwrap();
    let mut output = String::new();
    channel.read_to_string(&mut output).await.unwrap();
    assert_eq!(process.wait().await.unwrap().code, Some(0));

    assert_eq!(output, "complete", "W1/E1");
    assert!(
        !sandbox
            .root
            .resolve(".awaken/tool-results/result.txt")
            .unwrap()
            .exists(),
        "W1/E2"
    );
}

async fn bwrap_usable() -> bool {
    tokio::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "true"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn spawn_agent_launches_an_opaque_process_confined_by_bwrap() {
    if !bwrap_usable().await {
        eprintln!("skipping: no usable bwrap / user namespaces");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let sandbox = provider
        .create_sandbox(&ns_spec("t-ns-spawn", Vec::new()))
        .await
        .unwrap();
    let (proc, _channel) = sandbox
        .spawn_agent(pc::Command::new(["true"]))
        .await
        .unwrap();
    assert!(!proc.id().is_empty());
    assert_eq!(proc.wait().await.unwrap().code, Some(0));
}

#[test]
fn bubblewrap_binds_workspace_outputs_and_ends_with_argv() {
    let ws = PathBuf::from("/host/ws");
    let out = PathBuf::from("/host/out");
    let argv = vec![s("claude"), s("--acp")];
    let a = bubblewrap_argv(&input(
        &ws,
        &out,
        &[],
        &[],
        &pc::NetworkPolicy::Unrestricted,
        &argv,
    ));

    assert_eq!(a[0], "bwrap");
    // workspace + outputs binds present
    let joined = a.join(" ");
    assert!(joined.contains("--bind /host/ws /workspace"));
    assert!(joined.contains("--bind /host/out /mnt/session/outputs"));
    assert!(
        !joined.contains("--setenv"),
        "environment is injected through the cleared wrapper env, never argv"
    );
    assert!(joined.contains("--chdir /workspace"));
    // program follows the -- separator, in order
    let sep = a.iter().position(|x| x == "--").unwrap();
    assert_eq!(&a[sep + 1..], &["claude", "--acp"]);
    // unrestricted net => no --unshare-net
    assert!(!a.iter().any(|x| x == "--unshare-net"));
    assert!(
        a.windows(3).any(|window| {
            window
                == [
                    "--ro-bind-try",
                    "/run/systemd/resolve",
                    "/run/systemd/resolve",
                ]
        }),
        "systemd-resolved's resolv.conf target is visible"
    );
    assert!(
        a.windows(3).any(|window| {
            window
                == [
                    "--ro-bind-try",
                    "/run/NetworkManager",
                    "/run/NetworkManager",
                ]
        }),
        "NetworkManager's resolv.conf target is visible"
    );
}

#[test]
fn bubblewrap_projects_explicit_non_system_path_runtimes_read_only() {
    let ws = PathBuf::from("/host/ws");
    let out = PathBuf::from("/host/out");
    let argv = vec![s("npx"), s("agent")];
    let env = vec![(
        "PATH".to_string(),
        "/home/u/.nvm/versions/node/v22.22.0/bin:/home/u/.local/bin:/usr/bin".to_string(),
    )];
    let rendered = bubblewrap_argv(&input(
        &ws,
        &out,
        &[],
        &env,
        &pc::NetworkPolicy::Unrestricted,
        &argv,
    ));
    let joined = rendered.join(" ");
    assert!(joined.contains(
        "--ro-bind-try /home/u/.nvm/versions/node/v22.22.0 \
             /home/u/.nvm/versions/node/v22.22.0"
    ));
    assert!(joined.contains("--ro-bind-try /home/u/.local/bin /home/u/.local/bin"));
    assert_eq!(
        projected_runtime_roots(&env),
        vec![
            "/home/u/.nvm/versions/node/v22.22.0".to_string(),
            "/home/u/.local/bin".to_string(),
        ]
    );
    assert!(
        !rendered.iter().any(|value| value == "--clearenv"),
        "the Tokio wrapper clears and rebuilds env before bwrap"
    );
}

#[test]
fn bubblewrap_projects_a_python_virtualenv_root_for_symlinked_clis() {
    let env = vec![(
        "PATH".to_string(),
        "/home/u/.hermes/hermes-agent/venv/bin:\
             /home/u/.local/share/uv/python/cpython-3.11/bin:\
             /home/u/.local/bin:/usr/bin"
            .to_string(),
    )];
    assert_eq!(
        projected_runtime_roots(&env),
        vec![
            "/home/u/.hermes/hermes-agent".to_string(),
            "/home/u/.local/share/uv/python".to_string(),
            "/home/u/.local/bin".to_string(),
        ]
    );
}

/// Runtime projection cause/effect decision table:
/// | argv[0] | location | effect |
/// |---|---|---|
/// | absolute | non-system | project its parent read-only |
/// | absolute | system root | reuse the canonical system projection |
/// | relative | any | resolve only through the declared PATH projections |
/// This covers both an operator-installed ACP executable and the Runtime's
/// own Session Hand without adding a second Hand-specific mount mechanism.
#[test]
fn bubblewrap_projects_an_explicit_non_system_executable_read_only() {
    let ws = PathBuf::from("/host/ws");
    let out = PathBuf::from("/host/out");
    let argv = vec![s("/opt/awaken/bin/awaken-sandbox"), s("hand")];
    let rendered = bubblewrap_argv(&input(
        &ws,
        &out,
        &[],
        &[],
        &pc::NetworkPolicy::Unrestricted,
        &argv,
    ));

    assert!(
        rendered
            .windows(3)
            .any(|window| { window == ["--ro-bind-try", "/opt/awaken/bin", "/opt/awaken/bin"] })
    );
    assert!(projected_runtime_roots_for_command(&[], &[s("/usr/bin/bash")]).is_empty());
    assert!(projected_runtime_roots_for_command(&[], &[s("bash")]).is_empty());
}

#[test]
fn bubblewrap_chdirs_into_a_custom_cwd_when_the_command_sets_one() {
    // The cwd decision branch: an empty cwd renders `--chdir /workspace` (covered
    // elsewhere); a non-empty sandbox-absolute cwd must render `--chdir <cwd>` so a
    // launched process starts in the directory the command asked for.
    let ws = PathBuf::from("/w");
    let out = PathBuf::from("/o");
    let argv = vec![s("true")];
    let mut inp = input(&ws, &out, &[], &[], &pc::NetworkPolicy::Unrestricted, &argv);
    inp.cwd = "/workspace/sub";
    let a = bubblewrap_argv(&inp);
    // The chdir target is the custom cwd, not the /workspace default.
    let pos = a.iter().position(|x| x == "--chdir").unwrap();
    assert_eq!(a[pos + 1], "/workspace/sub");
}

#[test]
fn bubblewrap_unshares_net_when_not_unrestricted() {
    let ws = PathBuf::from("/w");
    let out = PathBuf::from("/o");
    let argv = vec![s("true")];
    let a = bubblewrap_argv(&input(&ws, &out, &[], &[], &pc::NetworkPolicy::None, &argv));
    assert!(a.iter().any(|x| x == "--unshare-net"));
}

#[test]
fn bubblewrap_renders_ro_and_rw_mounts_and_env() {
    let ws = PathBuf::from("/w");
    let out = PathBuf::from("/o");
    let mounts = vec![
        RenderMount {
            host: PathBuf::from("/h/in"),
            dest: "/workspace/in.txt".into(),
            read_only: true,
            boundary: RenderMountBoundary::General,
        },
        RenderMount {
            host: PathBuf::from("/h/rw"),
            dest: ".mnt/data".into(),
            read_only: false,
            boundary: RenderMountBoundary::General,
        },
    ];
    let env = vec![("TZ".to_string(), "UTC".to_string())];
    let argv = vec![s("sh")];
    let a = bubblewrap_argv(&input(
        &ws,
        &out,
        &mounts,
        &env,
        &pc::NetworkPolicy::Unrestricted,
        &argv,
    ));
    let j = a.join(" ");
    assert!(j.contains("--ro-bind /h/in /workspace/in.txt"));
    assert!(j.contains("--bind /h/rw /workspace/.mnt/data"));
    assert!(!j.contains("UTC"), "environment values stay out of argv");
}

#[test]
fn bubblewrap_seals_managed_memory_parent_before_binding_store_children() {
    // Cause/effect graph: C1 a typed ManagedMemoryStore mount targets a
    // child of `/mnt/memory`; C2 the child is ReadWrite. E1 the renderer
    // creates the child target; E2 it seals the parent read-only first; E3
    // it then binds the Store writable at only that child.
    // Decision rule MM1: C1+C2 => E1+E2+E3. A General mount is covered by
    // the adjacent renderer test and must not trigger this boundary.
    let ws = PathBuf::from("/w");
    let out = PathBuf::from("/o");
    let mounts = vec![RenderMount {
        host: PathBuf::from("/host/notes"),
        dest: "/mnt/memory/notes".into(),
        read_only: false,
        boundary: RenderMountBoundary::ManagedMemoryStore,
    }];
    let argv = vec![s("true")];
    let rendered = bubblewrap_argv(&input(
        &ws,
        &out,
        &mounts,
        &[],
        &pc::NetworkPolicy::Unrestricted,
        &argv,
    ));
    let joined = rendered.join(" ");
    assert!(joined.contains("--dir /mnt/memory/notes"), "MM1/E1");
    let seal = rendered
        .windows(2)
        .position(|args| args == ["--remount-ro", "/mnt/memory"])
        .expect("MM1/E2 parent seal");
    let bind = rendered
        .windows(3)
        .position(|args| args == ["--bind", "/host/notes", "/mnt/memory/notes"])
        .expect("MM1/E3 child bind");
    assert!(seal < bind, "MM1 parent is sealed before child bind");
}

#[test]
fn relative_and_workspace_prefixed_projections_share_the_workspace_root() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = IsolatedRoot::new(root_dir.path());
    let workspace = root_dir.path().join("workspace");
    assert_eq!(
        host_projection_path(&root, &workspace, ".mnt/notes").unwrap(),
        workspace.join(".mnt/notes")
    );
    assert_eq!(
        host_projection_path(&root, &workspace, "/workspace/repo").unwrap(),
        workspace.join("repo")
    );
    assert_eq!(workspace_relative("workspace/repo"), "repo");
    assert_eq!(
        host_projection_path(&root, &workspace, "/outputs/result").unwrap(),
        root_dir.path().join("outputs/result")
    );
}

#[test]
fn sandbox_exec_wraps_argv_with_a_profile() {
    let ws = PathBuf::from("/w");
    let out = PathBuf::from("/o");
    let argv = vec![s("claude")];
    let a = sandbox_exec_argv(&input(
        &ws,
        &out,
        &[],
        &[],
        &pc::NetworkPolicy::Unrestricted,
        &argv,
    ));
    assert_eq!(a[0], "sandbox-exec");
    assert_eq!(a[1], "-p");
    assert!(a[2].contains("(deny default)"));
    assert!(a[2].contains("(import \"system.sb\")"));
    assert!(a[2].contains("/w"));
    assert!(a[2].contains("(allow network*)"));
    assert_eq!(a.last().unwrap(), "claude");
}

#[test]
fn sandbox_exec_renders_mount_permissions_none_network_and_escaped_paths() {
    let ws = PathBuf::from("/host/w\"s");
    let out = PathBuf::from("/host/out");
    let mounts = vec![
        RenderMount {
            host: PathBuf::from("/host/w\"s/readonly"),
            dest: "/workspace/readonly".into(),
            read_only: true,
            boundary: RenderMountBoundary::General,
        },
        RenderMount {
            host: PathBuf::from("/host/rw"),
            dest: "/data".into(),
            read_only: false,
            boundary: RenderMountBoundary::General,
        },
    ];
    let argv = vec![s("true")];
    let rendered = sandbox_exec_argv(&input(
        &ws,
        &out,
        &mounts,
        &[],
        &pc::NetworkPolicy::None,
        &argv,
    ));
    let profile = &rendered[2];
    assert!(profile.contains("/host/w\\\"s"));
    assert!(profile.contains("(deny file-write*"));
    assert!(profile.contains("/host/rw"));
    assert!(!profile.contains("(allow network*)"));
}
