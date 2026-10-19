//! One live Session environment shared by native tools, processes, ACP, and Skill paths.

use super::*;

#[tokio::test]
async fn revoked_container_rebinds_only_a_proven_owned_path_successor() {
    // Causes: C1 a revoked wrapper retains container V2 handle A; C2 the
    // aggregate has committed A plus one owned input path on the same
    // substrate; C3 the incoming handle instead names a foreign substrate.
    // R1=C1+C2 => adopt the existing container once with a fresh wrapper and
    // preserve its exact output root. R2=C1+C3 => reject before any adoption.
    // This fixture proves provider routing and fences; real Pod identity and
    // process exit remain Kubernetes acceptance obligations.
    let provider = Arc::new(FakeContainerProvider::default());
    let configured = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider();
    let environment = configured.create(&spec()).await.unwrap();
    let before = environment.lifecycle().handle();
    let mut next_json = serde_json::to_value(&before).unwrap();
    next_json["payload"]["owned_paths"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("/mnt/session/uploads/confirmed-2.json"));
    let next: pc::SandboxHandle = serde_json::from_value(next_json).unwrap();
    assert!(before.owned_paths_are_monotonic_to(&next));
    let rebound = configured
        .adoption()
        .rebind_retained(
            &spec(),
            &next,
            &environment,
            awaken_provisioning_contract::SandboxEffect::Unattached,
        )
        .await
        .unwrap();
    assert_eq!(rebound.lifecycle().handle(), next, "R1 exact new handle");
    assert_eq!(
        provider.creates.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "R1 no second physical create"
    );
    assert_eq!(
        provider.adopts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "R1 canonical provider adoption"
    );
    let mut foreign = next;
    foreign.sandbox_id = "another-container".into();
    assert!(
        configured
            .adoption()
            .rebind_retained(
                &spec(),
                &foreign,
                &environment,
                awaken_provisioning_contract::SandboxEffect::Unattached,
            )
            .await
            .is_err(),
        "R2 foreign substrate"
    );
    assert_eq!(provider.adopts.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn trusted_host_installed_hand_is_lazy_and_never_falls_back_after_launch_failure() {
    // Causes: H1 trusted provider; H2 installed Hand missing at dispatch;
    // H3 concurrent first calls; H4 provider relocated/adopted. Effects:
    // E1 create remains lazy; E2 all calls fail, no in-process file effect;
    // E3 the same mandatory Hand config survives reconstruction; E4 ordinary
    // weaker Local without a Hand uses its explicit rooted executor (not a fallback).
    // Decision rules: H1+H2+H3 => E1+E2; +H4 => E3+E2; no Hand configured => E4.
    let base = tempfile::tempdir().unwrap();
    let mut provider =
        SandboxEnvironmentProvider::trusted_host_workdir_with_agent_stderr(base.path(), false);
    provider.install_workdir_hand(
        Arc::new(FakeHandExecutorFactory),
        base.path()
            .join("missing-hand")
            .to_string_lossy()
            .into_owned(),
        std::time::Duration::ZERO,
    );
    let provider = provider.into_session_provider();
    let create_fence =
        pc::SandboxEffectFence::new("create-hand", "owner", "runtime", 1, u64::MAX).unwrap();
    // C5 fenced creation has no source/denied source/current source. The first
    // two must reject before provider filesystem effects; only current proceeds
    // to the existing lazy-Hand checks. An immutable fence is not a grant.
    let denied = pc::test_support::EffectAuthority::denied();
    // A fence without a source cannot even be projected into a provider
    // effect; the denied source reaches the provider and is refused there.
    assert!(
        awaken_provisioning_contract::SandboxEffect::from_parts(Some(&create_fence), None).is_err()
    );
    assert!(
        provider
            .realizer()
            .create_effective_for_effect(
                &spec(),
                awaken_provisioning_contract::SandboxEffect::Fenced {
                    fence: &create_fence,
                    authority: &denied,
                },
                None,
                awaken_session_environment_contract::EnvironmentRealizationKind::Create
            )
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(base.path()).unwrap().count(), 0);
    let environment = provider
        .realizer()
        .create_effective_for_effect(
            &spec(),
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &create_fence,
                authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    &create_fence,
                ),
            },
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
        .unwrap();
    let executor = environment.agent_tools().tool_executor();
    let call = ToolCall {
        call_id: "no-fallback".into(),
        tool_id: "write".into(),
        arguments: serde_json::json!({"path": "/workspace/must-not-exist", "content": "forbidden"}),
    };
    let (first, second) = tokio::join!(executor.invoke(&call), executor.invoke(&call));
    assert!(first.is_err() && second.is_err());
    let SandboxEnvironment::Workdir {
        sandbox,
        hand: Some(hand),
    } = environment.sandbox_environment()
    else {
        panic!("installed Hand")
    };
    assert!(
        !sandbox
            .workspace_path()
            .join("workspace/must-not-exist")
            .exists()
    );
    assert!(!hand.has_tracked_binding().await);
    // Hand commands always start at /workspace; an ACP-only configured cwd
    // cannot become a different Native prompt path by accident.
    assert_eq!(
        environment
            .agent_tools()
            .agent_filesystem_view(Some("repo/src"), AgentFilesystemClient::NativeTools)
            .prompt_paths,
        awaken_session_environment_contract::AgentPathView::workdir_relative("/workspace")
    );
    let handle = environment.lifecycle().handle();
    // H5 retained TrustedHost + installed Hand + revoked old wrapper => reuse
    // the exact physical Sandbox, create a fresh lazy Hand, and retain the
    // mandatory launch failure (never fall back to direct filesystem access).
    // H5 shares H2's missing executable so no platform-specific helper is needed.
    environment
        .lifecycle()
        .stop_bound_processes()
        .await
        .unwrap();
    let rebound = provider
        .adoption()
        .rebind_retained(
            &spec(),
            &handle,
            &environment,
            awaken_provisioning_contract::SandboxEffect::Unattached,
        )
        .await
        .unwrap();
    let (
        SandboxEnvironment::Workdir {
            sandbox: before,
            hand: Some(old_hand),
        },
        SandboxEnvironment::Workdir {
            sandbox: after,
            hand: Some(new_hand),
        },
    ) = (
        environment.sandbox_environment(),
        rebound.sandbox_environment(),
    )
    else {
        panic!("H5 mandatory Hand survives retained rebind");
    };
    assert!(Arc::ptr_eq(before, after), "H5 same physical workspace");
    assert!(!Arc::ptr_eq(old_hand, new_hand), "H5 fresh Hand wrapper");
    assert!(
        rebound
            .agent_tools()
            .tool_executor()
            .invoke(&call)
            .await
            .is_err()
    );
    assert!(
        !before
            .workspace_path()
            .join("workspace/must-not-exist")
            .exists()
    );
    rebound.lifecycle().stop_bound_processes().await.unwrap();
    drop(rebound);
    drop(executor);
    drop(environment);
    let adopted = provider
        .adoption()
        .adopt_effective_for_effect(
            &spec(),
            &handle,
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &create_fence,
                authority: &pc::test_support::EffectAuthority::new(&create_fence),
            },
        )
        .await
        .unwrap();
    assert!(
        adopted
            .agent_tools()
            .tool_executor()
            .invoke(&call)
            .await
            .is_err()
    );
    adopted.lifecycle().stop_bound_processes().await.unwrap();
    let terminal =
        pc::SandboxEffectFence::new("terminal-hand", "owner", "runtime", 1, u64::MAX).unwrap();
    let prepared = adopted
        .lifecycle()
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .unwrap();
    let authorization = restart_disposal_authorization(prepared, "hand-adoption");
    adopted
        .lifecycle()
        .dispose_for_effect(
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .unwrap();
    let relocated =
        crate::test_support::relocated_provider(&provider, base.path().join("relocated"));
    let environment = relocated.create(&spec()).await.unwrap();
    assert!(
        environment
            .agent_tools()
            .tool_executor()
            .invoke(&call)
            .await
            .is_err()
    );
    environment.lifecycle().dispose().await.unwrap();
    let plain = SandboxEnvironmentProvider::workdir(base.path().join("plain"))
        .into_session_provider()
        .create(&spec())
        .await
        .unwrap();
    assert!(
        !plain
            .agent_tools()
            .tool_executor()
            .invoke(&call)
            .await
            .unwrap()
            .is_error
    );
    plain.lifecycle().dispose().await.unwrap();
}

#[tokio::test]
async fn trusted_host_live_resource_admission_keeps_readonly_advisory() {
    // Cause/effect graph: C1 resident boundary={ordinary Workdir,TrustedHost};
    // C2 late managed mount=read-only. The ordinary rejection rule is exercised
    // by the Host resource-transition test; this complementary rule requires
    // TrustedHost+C2 to admit the same typed resource without claiming an
    // enforced-readonly capability or creating another mount path.
    let base = tempfile::tempdir().unwrap();
    let provider =
        SandboxEnvironmentProvider::trusted_host_workdir_with_agent_stderr(base.path(), false)
            .into_session_provider();
    let environment = provider.create(&spec()).await.unwrap();
    let readonly = pc::MountRequirement {
        mount_id: "late-file".into(),
        source: pc::MountSource::Inline {
            contents: "late".into(),
        },
        mount_path: "/mnt/session/uploads/late.txt".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    };
    environment
        .projection()
        .validate_live_mount_replacement(&[], std::slice::from_ref(&readonly))
        .expect("TrustedHost admits advisory read-only managed resources");
    assert!(!environment.projection().capabilities().enforced_readonly);
    // L1 trusted + inline RO => attach exact bytes and Copy receipt;
    // L2 repeat => idempotent bytes; L3 invalid hash => preserve previous bytes;
    // L4 escape => reject; L5 unresolved source => reject. Admission alone is
    // insufficient: each rule crosses the actual provider attachment port.
    for _ in 0..2 {
        let realized = environment
            .projection()
            .attach_mount(readonly.clone())
            .await
            .unwrap();
        assert_eq!(realized.realization, pc::Realization::Copy);
    }
    let mut invalid = readonly.clone();
    invalid.source = pc::MountSource::InlineBytes {
        contents: b"wrong".to_vec(),
        content_hash: Some("sha256:wrong".into()),
    };
    assert!(
        environment
            .projection()
            .attach_mount(invalid)
            .await
            .is_err(),
        "L3"
    );
    let mut escape = readonly.clone();
    escape.mount_path = "/../escape.txt".into();
    assert!(
        environment.projection().attach_mount(escape).await.is_err(),
        "L4"
    );
    let mut unresolved = readonly;
    unresolved.source = pc::MountSource::MemoryStore {
        store_id: "late-store".into(),
        materialization_reference: None,
        write_consistency: Default::default(),
    };
    assert!(
        environment
            .projection()
            .attach_mount(unresolved)
            .await
            .is_err(),
        "L5"
    );
    let files = environment
        .harvest()
        .list_frozen_mount_files("/mnt/session/uploads")
        .await
        .unwrap();
    assert!(
        files.iter().any(|(_, bytes)| bytes == b"late"),
        "L1/L3 preserved bytes"
    );
    environment.lifecycle().dispose().await.unwrap();
}

#[tokio::test]
async fn trusted_host_filesystem_view_maps_workspace_relative_cwd_once() {
    // Cause/effect graph: C1 cwd={default,repo/src}; C2 client={Native Hand,
    // ACP process}; C3 host separator={Windows,POSIX}. E1 default selects the
    // canonical workspace while relative /mnt projections remain reachable; E2 an
    // authored cwd is based on the canonical logical /workspace; E3 ACP gets
    // the platform-native physical path; E4 process gets the provider-neutral
    // logical path; E5 prompts contain no host prefix. Decision rules:
    // T1 ACP+default=>E1+E5; T2 ACP+configured=>E2+E3+E4+E5;
    // T3 embedded Native=>paths relative to its actual sandbox-root cwd;
    // T4 Workdir+mixed=>reject. A selected mount alias is not a general root alias.
    let base = tempfile::tempdir().unwrap();
    let provider =
        SandboxEnvironmentProvider::trusted_host_workdir_with_agent_stderr(base.path(), false)
            .into_session_provider();
    let environment = provider.create(&spec()).await.unwrap();
    let root = environment.agent_process().workspace_cwd();

    let default = environment
        .agent_tools()
        .agent_filesystem_view(None, AgentFilesystemClient::AcpProcess);
    assert_eq!(
        default.protocol_cwd,
        std::path::Path::new(&root)
            .join("workspace")
            .to_string_lossy(),
        "T1/E1"
    );
    assert_eq!(default.command_cwd, "/workspace", "T1/E1");
    assert_eq!(
        default.prompt_paths,
        awaken_session_environment_contract::AgentPathView::workdir_relative("/workspace"),
        "T1/E5"
    );

    let configured = environment
        .agent_tools()
        .agent_filesystem_view(Some("repo/src"), AgentFilesystemClient::AcpProcess);
    assert_eq!(
        configured.protocol_cwd,
        std::path::Path::new(&root)
            .join("workspace")
            .join("repo")
            .join("src")
            .to_string_lossy(),
        "T2/E2+E3"
    );
    assert_eq!(configured.command_cwd, "/workspace/repo/src", "T2/E4");
    assert_eq!(
        configured.prompt_paths,
        awaken_session_environment_contract::AgentPathView::workdir_relative("/workspace/repo/src"),
        "T2/E5"
    );
    let native = environment
        .agent_tools()
        .agent_filesystem_view(None, AgentFilesystemClient::NativeTools);
    assert_eq!(
        native.prompt_paths,
        awaken_session_environment_contract::AgentPathView::workdir_relative("/"),
        "Embedded Native paths use the actual rooted-tools cwd"
    );
    assert_eq!(native.protocol_cwd, root);
    assert_eq!(native.command_cwd, "/");
    assert!(
        environment
            .agent_tools()
            .agent_prompt_path_view(None, true, true)
            .is_err(),
        "T4 mixed Workdir path consumers"
    );
    environment.lifecycle().dispose().await.unwrap();
}

#[tokio::test]
async fn namespace_native_process_and_agent_channel_share_one_live_environment() {
    // Namespace Workspace-path cause/effect graph: C0 the caller is an ordinary
    // developer run or the required substrate profile; C1 the admitted provider
    // offers tool transparency and path fidelity; C2 a sandbox process writes
    // through the canonical absolute `/workspace`; C3 the typed file surface
    // reads/writes that Session-owned tree; C4 an ACP process reads the same
    // canonical absolute paths. Effects: E1 the typed read sees C2's bytes and
    // E2 the ACP process sees both C2 and C3 without an alias or path rewrite;
    // E3 an unavailable optional substrate is reported and skipped; E4 the
    // required profile fails rather than converting a skipped proof to green.
    //
    // | Rule | required profile | provider admitted | process write | typed file I/O | ACP read | Effect |
    // |---|---|---|---|---|---|---|
    // | N0 | no | no | no | no | no | E3 plus fail-closed provider-selection assertion |
    // | NR | yes | no | no | no | no | E4 |
    // | N1 | either | yes | yes | read process bytes | no | E1 |
    // | N2 | either | yes | yes | write typed bytes | yes | E1+E2 |
    //
    // Constraint: WorkspaceLayout plus Sandbox capability admission remain the
    // sole path authority. This test must not add a host-path alias, symlink, or
    // shell-command classifier to make split paths appear consistent.
    let base = tempfile::tempdir().unwrap();
    let mut namespace_spec = spec();
    namespace_spec.scope = "session-namespace".into();
    namespace_spec.isolation = IsolationClass::Namespace;
    let provider = NamespaceProvider::new(base.path());
    let namespace_workspace_required =
        std::env::var("AWAKEN_TEST_REQUIRE_NAMESPACE_WORKSPACE").as_deref() == Ok("1");
    if let Err(probe_error) = provider.probe_ready().await {
        assert!(
            !namespace_workspace_required,
            "NR/E4 required Namespace /workspace substrate is unavailable: {probe_error}"
        );
        // Startup tier selection (`awaken-execution-composition`) owns the
        // fail-closed refusal of an unavailable Namespace; this row has no
        // substrate to observe.
        let _ = probe_error;
        return;
    }
    let capabilities = provider.capabilities();
    if !capabilities.path_fidelity {
        assert!(
            !namespace_workspace_required,
            "NR/E4 required Namespace provider lacks /workspace path fidelity"
        );
        let requirements = pc::SandboxRequirements::from_spec(&namespace_spec, true);
        assert!(requirements.path_fidelity, "N0 path requirement");
        assert!(
            !capabilities.satisfies_requirements(&requirements),
            "N0 a split-path Namespace must fail closed before environment creation"
        );
        return;
    }
    let namespace = provider.create_sandbox(&namespace_spec).await.unwrap();
    let environment = SandboxEnvironment::namespace(
        namespace,
        Arc::new(FakeHandExecutorFactory),
        "/bin/sh",
        std::time::Duration::ZERO,
    )
    .into_session_environment();
    // N3: either native platform must retain the mechanism selected by its
    // sole Namespace provider in the Session lifecycle handle; a mismatched
    // provider discriminator fails before Native or Hand claims path fidelity.
    assert_eq!(
        environment.lifecycle().handle().provider_kind(),
        NamespaceProvider::provider_kind().schema_provider()
    );

    let mut native_command = pc::Command::new([
        "/bin/sh",
        "-c",
        "printf namespace-state > /workspace/process-marker",
    ]);
    native_command.cwd = pc::WorkspaceLayout::ROOT.into();
    let native = environment
        .sandbox_environment()
        .sandbox()
        .spawn(native_command)
        .await
        .unwrap();
    assert_eq!(native.wait().await.unwrap().code, Some(0));

    let typed_files = environment
        .sandbox_environment()
        .list_workspace_files("")
        .await
        .unwrap();
    assert!(
        typed_files.iter().any(|(path, bytes)| {
            path.ends_with("process-marker") && bytes.as_slice() == b"namespace-state"
        }),
        "N1/E1 typed file reads the process-authored /workspace bytes: {typed_files:?}"
    );
    environment
        .projection()
        .write_workspace_file("typed-marker", b"typed-state")
        .await
        .unwrap();

    let hand_output = environment
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "namespace-hand".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({"path": "marker"}),
        })
        .await
        .unwrap();
    assert_eq!(hand_output.text(), "bound-hand-ok", "N1 Hand binding");

    let mut agent_command = pc::Command::new([
        "/bin/sh",
        "-c",
        "cat /workspace/process-marker; printf '|'; cat /workspace/typed-marker",
    ]);
    agent_command.cwd = pc::WorkspaceLayout::ROOT.into();
    let (agent, mut channel) = environment
        .agent_process()
        .spawn_agent(agent_command)
        .await
        .unwrap();
    let mut output = String::new();
    channel.read_to_string(&mut output).await.unwrap();
    assert_eq!(agent.wait().await.unwrap().code, Some(0));
    assert_eq!(output, "namespace-state|typed-state", "N2/E2");

    // N3 a revoked Namespace wrapper -> same physical Sandbox Arc and root,
    // fresh Hand, and successful agent I/O. The closed prior Hand stays closed.
    environment
        .lifecycle()
        .stop_bound_processes()
        .await
        .unwrap();
    let selected = SandboxEnvironmentProvider::Namespace {
        provider,
        hand_factory: Arc::new(FakeHandExecutorFactory),
        hand_bin: "/bin/sh".into(),
        hand_idle_after: std::time::Duration::ZERO,
    };
    let rebound = selected
        .rebind_retained(
            &crate::test_support::spec(),
            &environment.lifecycle().handle(),
            &environment,
            awaken_provisioning_contract::SandboxEffect::Unattached,
        )
        .await
        .unwrap();
    let (
        SandboxEnvironment::Namespace {
            sandbox: before, ..
        },
        SandboxEnvironment::Namespace { sandbox: after, .. },
    ) = (
        environment.sandbox_environment(),
        rebound.sandbox_environment(),
    )
    else {
        unreachable!()
    };
    assert!(Arc::ptr_eq(before, after), "N3 physical owner");
    assert!(
        !Arc::ptr_eq(
            &environment.agent_tools().tool_executor(),
            &rebound.agent_tools().tool_executor()
        ),
        "N3 fresh Hand"
    );
    let (agent, mut channel) = rebound
        .agent_process()
        .spawn_agent(pc::Command::new([
            "/bin/sh",
            "-c",
            "cat /workspace/typed-marker",
        ]))
        .await
        .unwrap();
    let mut bytes = String::new();
    channel.read_to_string(&mut bytes).await.unwrap();
    assert_eq!(agent.wait().await.unwrap().code, Some(0));
    assert_eq!(
        bytes, "typed-state",
        "N3 new Hand still owns the original root"
    );
    rebound.lifecycle().dispose().await.unwrap();
}

#[tokio::test]
async fn skill_paths_follow_the_materialization_layout_and_attempt_client() {
    // P4 decision table: provider={Workdir rooted tools, Workdir Hand,
    // Namespace, Container} x client={Native,ACP} x cwd={default,repo/src}
    // x source={attached relative,repository relative,repository absolute}.
    // Isolated providers expose exact /workspace paths regardless of cwd;
    // local ACP/Hand paths are relative to the actual client cwd and expose no
    // host prefix. Native ignores the ACP-only cwd option. No materialization
    // alias is created. P5 unsafe metadata fails before prompt publication.
    // This is adapter/path evidence; the Workdir read test proves actual bytes,
    // and real Cowork Native/ACP E2E owns isolated tool execution evidence.
    for tier in ["rooted", "hand", "namespace", "container"] {
        let root = tempfile::tempdir().unwrap();
        let provider = match tier {
            "namespace" => SandboxEnvironmentProvider::namespace_with_agent_stderr(
                root.path(),
                false,
                Arc::new(FakeHandExecutorFactory),
                "/bin/sh",
                std::time::Duration::ZERO,
            )
            .into_session_provider(),
            "container" => SandboxEnvironmentProvider::container(
                Arc::new(FakeContainerProvider::default()),
                Vec::new(),
                Arc::new(FakeHandExecutorFactory),
                "/bin/sh",
            )
            .into_session_provider(),
            _ => {
                let mut provider = SandboxEnvironmentProvider::workdir(root.path());
                if tier == "hand" {
                    provider.install_workdir_hand(
                        Arc::new(FakeHandExecutorFactory),
                        "/bin/sh".into(),
                        std::time::Duration::ZERO,
                    );
                }
                provider.into_session_provider()
            }
        };
        let environment = provider.create(&spec()).await.unwrap();
        for client in [
            AgentFilesystemClient::NativeTools,
            AgentFilesystemClient::AcpProcess,
        ] {
            for cwd in [None, Some("repo/src")] {
                let view = environment.agent_tools().agent_filesystem_view(cwd, client);
                for directory in [
                    ".skills/s",
                    "workspace/repo/.claude/skills/s",
                    "/workspace/repo/.claude/skills/s",
                ] {
                    let attached = directory == ".skills/s";
                    let expected = if matches!(tier, "namespace" | "container") {
                        if attached {
                            "/workspace/.skills/s"
                        } else {
                            "/workspace/repo/.claude/skills/s"
                        }
                    } else if tier == "rooted" && client == AgentFilesystemClient::NativeTools {
                        if attached {
                            ".skills/s"
                        } else {
                            "workspace/repo/.claude/skills/s"
                        }
                    } else if client == AgentFilesystemClient::AcpProcess && cwd.is_some() {
                        if attached {
                            "../../../.skills/s"
                        } else {
                            "../.claude/skills/s"
                        }
                    } else if attached {
                        "../.skills/s"
                    } else {
                        "repo/.claude/skills/s"
                    };
                    let logical = environment
                        .skills()
                        .skill_directory_path(directory)
                        .unwrap();
                    assert_eq!(
                        view.prompt_paths.render(&logical),
                        if expected.starts_with('/') {
                            expected.to_owned()
                        } else {
                            expected.replace('/', std::path::MAIN_SEPARATOR_STR)
                        },
                        "P4 {tier} {client:?} {cwd:?} {directory}"
                    );
                }
            }
        }
        for bad in [
            "",
            "../escape",
            "skill/../escape",
            "skill\\escape",
            "skill\0escape",
        ] {
            assert!(
                environment.skills().skill_directory_path(bad).is_err(),
                "P5 {tier} {bad:?}"
            );
        }
        environment.lifecycle().dispose().await.unwrap();
    }
}

#[tokio::test]
async fn container_native_tools_and_acp_share_one_environment_and_bound_hand() {
    let provider = Arc::new(FakeContainerProvider::default());
    let environment = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    assert_eq!(
        provider.creates.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        environment.lifecycle().handle().provider_kind(),
        "container"
    );
    assert!(environment.agent_process().is_container());
    // Config-home placement decision table:
    // C1=environment already exists; C2=root may be read-only; C3=workspace
    // is the provider's writable Session boundary. C1+C2+C3 requires both
    // the exposed and materialization paths to stay under /workspace.
    assert_eq!(
        environment.agent_process().config_home(),
        "/workspace/.acp-config"
    );
    assert_eq!(
        environment.agent_process().config_home_logical(),
        "/workspace/.acp-config"
    );

    environment
        .agent_process()
        .materialize_inline("/workspace/direct.bin", b"direct")
        .await
        .unwrap();
    environment
        .projection()
        .write_workspace_file("projected.bin", b"projected")
        .await
        .unwrap();
    environment
        .skills()
        .materialize_read_only_tree(
            "generated-skills",
            &[("skill/SKILL.md".into(), b"generated".to_vec(), false)],
        )
        .await
        .unwrap();
    environment
        .projection()
        .remove_projection_path("projected.bin")
        .await
        .unwrap();

    let native = environment
        .sandbox_environment()
        .sandbox()
        .spawn(pc::Command::new(["sh", "-c", "write marker"]))
        .await
        .unwrap();
    assert_eq!(native.wait().await.unwrap().code, Some(0));
    let (agent, mut channel) = environment
        .agent_process()
        .spawn_agent(pc::Command::new(["sh", "-c", "cat marker"]))
        .await
        .unwrap();
    let mut output = String::new();
    channel.read_to_string(&mut output).await.unwrap();
    assert_eq!(agent.wait().await.unwrap().code, Some(0));
    assert_eq!(output, "shared-container-state");

    let hand = environment.agent_tools().tool_executor();
    environment.skills().register_skill_dir("skills");
    provider.shared.lock().unwrap().insert(
        "authored/SKILL.md".into(),
        b"---\ndescription: authored\n---\nbody".to_vec(),
    );
    let result = hand
        .invoke(&ToolCall {
            call_id: "bound-hand".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({"command": "printf bound-hand-ok"}),
        })
        .await
        .unwrap();
    assert!(result.text().contains("bound-hand-ok"));
    let skills = environment.skills().scan_skill_dir("skills").unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "authored");
    assert_eq!(skills[0].dir, "skills/authored");
    environment.skills().refresh_skills().await.unwrap();
    environment
        .lifecycle()
        .stop_bound_processes()
        .await
        .unwrap();
    environment.lifecycle().dispose().await.unwrap();
}

#[tokio::test]
async fn controlled_modification_fails_when_post_tool_skill_refresh_fails() {
    /* Cause/effect graph: C1 the tool changes workspace bytes (Bash/Write/Edit)
     * or is read-only perception; C2 the post-dispatch catalog refresh succeeds
     * or fails. Effects: E1 perception keeps its result without refreshing; E2
     * mutation+refresh failure clears the last-good catalog,
     * closes the Hand, and returns the Runtime-only fatal signal; E3 neither a
     * stale catalog nor a second dispatch is admitted. Decision table:
     * PT1=content-mutation+refresh-ok=>normal result (neighboring bound-hand
     * test); PT2=perception+refresh-fails-if-run=>normal result here;
     * PT3=content-mutation+refresh-error=>E2+E3 here. */
    let provider = Arc::new(FakeContainerProvider::default());
    let environment = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    environment.skills().register_skill_dir("skills");
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__fail_skill_read".into(), Vec::new());

    let perception = environment
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "read-only".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({"path": "SKILL.md"}),
        })
        .await
        .expect("PT2 perception does not mutate Skill files");
    assert_eq!(perception.text(), "bound-hand-ok", "PT2");

    let error = environment
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "controlled-refresh".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({"command": "true"}),
        })
        .await
        .expect_err("PT2 stale post-modification Skill cache must fail closed");
    assert!(
        matches!(
            &error,
            awaken_tool_contract::tool::ToolError::StateInvalidatedAfterDispatch(detail)
                if detail.contains("scripted Skill read outage")
        ),
        "PT3 original refresh diagnostic: {error}"
    );
    let stale = environment
        .skills()
        .scan_skill_dir("skills")
        .expect_err("PT3 invalidation must not expose the last-good catalog");
    assert!(stale.to_string().contains("unavailable"), "PT3: {stale}");
    assert!(
        matches!(
            environment
                .agent_tools()
                .tool_executor()
                .invoke(&ToolCall {
                    call_id: "no-replay".into(),
                    tool_id: "bash".into(),
                    arguments: serde_json::json!({"command": "true"}),
                })
                .await,
            Err(awaken_tool_contract::tool::ToolError::UnavailableBeforeDispatch(_))
        ),
        "PT3 closed Hand rejects every later dispatch"
    );
    environment.lifecycle().dispose().await.unwrap();
}

#[tokio::test]
async fn post_dispatch_refresh_failure_cannot_hide_an_unknown_hand_effect() {
    // PT4 typed unknown effect + failed catalog refresh => retain Indeterminate
    // and close the one Hand binding. Neither StateInvalidated nor reacquisition
    // may hide uncertainty; PT3 above owns definitive effect + refresh failure.
    use awaken_tool_contract::tool::ToolError;
    let provider = Arc::new(FakeContainerProvider::default());
    let factory = ScriptedHandFactory::new([ScriptedHandOutcome::Indeterminate]);
    let environment = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        factory.clone(),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    environment.skills().register_skill_dir("skills");
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__fail_skill_read".into(), Vec::new());
    let call = ToolCall {
        call_id: "unknown-refresh".into(),
        tool_id: "write".into(),
        arguments: serde_json::json!({}),
    };
    assert!(matches!(
        environment
            .agent_tools()
            .tool_executor()
            .invoke(&call)
            .await,
        Err(ToolError::Indeterminate(_))
    ));
    assert!(matches!(
        environment
            .agent_tools()
            .tool_executor()
            .invoke(&call)
            .await,
        Err(ToolError::UnavailableBeforeDispatch(_))
    ));
    assert!(environment.skills().scan_skill_dir("skills").is_err());
    assert_eq!(factory.binds.load(std::sync::atomic::Ordering::SeqCst), 1);
    environment.lifecycle().dispose().await.unwrap();
}
