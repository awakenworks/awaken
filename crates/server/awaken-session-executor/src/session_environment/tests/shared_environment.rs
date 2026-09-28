//! One live Session environment shared by native tools, processes, ACP, and Skill paths.

use super::*;

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
        SessionEnvironmentProvider::trusted_host_workdir_with_agent_stderr(base.path(), false);
    provider.install_workdir_hand(
        Arc::new(FakeHandExecutorFactory),
        base.path()
            .join("missing-hand")
            .to_string_lossy()
            .into_owned(),
        std::time::Duration::ZERO,
    );
    let create_fence =
        pc::SandboxEffectFence::new("create-hand", "owner", "runtime", 1, u64::MAX).unwrap();
    // C5 fenced creation has no source/denied source/current source. The first
    // two must reject before provider filesystem effects; only current proceeds
    // to the existing lazy-Hand checks. An immutable fence is not a grant.
    let denied = pc::test_support::EffectAuthority::denied();
    // A fence without a source cannot even be projected into a provider
    // effect; the denied source reaches the provider and is refused there.
    assert!(
        crate::session_environment::ProviderEffect::from_parts(Some(&create_fence), None).is_err()
    );
    assert!(
        provider
            .create_effective_for_effect(
                &spec(),
                crate::session_environment::ProviderEffect::Fenced {
                    fence: &create_fence,
                    authority: &denied,
                },
                None,
                awaken_container_contract::EnvironmentRealizationIntent::Create
            )
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(base.path()).unwrap().count(), 0);
    let environment = provider
        .create_effective_for_effect(
            &spec(),
            crate::session_environment::ProviderEffect::Fenced {
                fence: &create_fence,
                authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    &create_fence,
                ),
            },
            None,
            awaken_container_contract::EnvironmentRealizationIntent::Create,
        )
        .await
        .unwrap();
    let executor = environment.tool_executor();
    let call = ToolCall {
        call_id: "no-fallback".into(),
        tool_id: "write".into(),
        arguments: serde_json::json!({"path": "/workspace/must-not-exist", "content": "forbidden"}),
    };
    let (first, second) = tokio::join!(executor.invoke(&call), executor.invoke(&call));
    assert!(first.is_err() && second.is_err());
    let SessionEnvironment::Workdir {
        sandbox,
        hand: Some(hand),
    } = &environment
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
            .agent_filesystem_view(Some("repo/src"), AgentFilesystemClient::NativeTools)
            .prompt_paths,
        crate::managed_resource_projection::AgentPathView::workdir_relative("/workspace")
    );
    let handle = environment.handle();
    // H5 retained TrustedHost + installed Hand + revoked old wrapper => reuse
    // the exact physical Sandbox, create a fresh lazy Hand, and retain the
    // mandatory launch failure (never fall back to direct filesystem access).
    // H5 shares H2's missing executable so no platform-specific helper is needed.
    environment.stop_bound_processes().await.unwrap();
    let rebound = provider
        .rebind_retained(
            &spec(),
            &handle,
            &environment,
            crate::session_environment::ProviderEffect::Unattached,
        )
        .await
        .unwrap();
    let (
        SessionEnvironment::Workdir {
            sandbox: before,
            hand: Some(old_hand),
        },
        SessionEnvironment::Workdir {
            sandbox: after,
            hand: Some(new_hand),
        },
    ) = (&environment, &rebound)
    else {
        panic!("H5 mandatory Hand survives retained rebind");
    };
    assert!(Arc::ptr_eq(before, after), "H5 same physical workspace");
    assert!(!Arc::ptr_eq(old_hand, new_hand), "H5 fresh Hand wrapper");
    assert!(rebound.tool_executor().invoke(&call).await.is_err());
    assert!(
        !before
            .workspace_path()
            .join("workspace/must-not-exist")
            .exists()
    );
    rebound.stop_bound_processes().await.unwrap();
    drop(rebound);
    drop(executor);
    drop(environment);
    let adopted = provider
        .adopt_effective_for_effect(
            &spec(),
            &handle,
            crate::session_environment::ProviderEffect::Fenced {
                fence: &create_fence,
                authority: &pc::test_support::EffectAuthority::new(&create_fence),
            },
        )
        .await
        .unwrap();
    assert!(adopted.tool_executor().invoke(&call).await.is_err());
    adopted.stop_bound_processes().await.unwrap();
    let terminal =
        pc::SandboxEffectFence::new("terminal-hand", "owner", "runtime", 1, u64::MAX).unwrap();
    let prepared = adopted
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .unwrap();
    let authorization = restart_disposal_authorization(prepared, "hand-adoption");
    adopted
        .dispose_for_effect(
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .unwrap();
    let relocated = provider.at_root(base.path().join("relocated"));
    let environment = relocated.create(&spec()).await.unwrap();
    assert!(environment.tool_executor().invoke(&call).await.is_err());
    environment.dispose().await.unwrap();
    let plain = SessionEnvironmentProvider::workdir(base.path().join("plain"))
        .create(&spec())
        .await
        .unwrap();
    assert!(!plain.tool_executor().invoke(&call).await.unwrap().is_error);
    plain.dispose().await.unwrap();
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
        SessionEnvironmentProvider::trusted_host_workdir_with_agent_stderr(base.path(), false);
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
        .validate_live_mount_replacement(&[], std::slice::from_ref(&readonly))
        .expect("TrustedHost admits advisory read-only managed resources");
    assert!(!environment.capabilities().enforced_readonly);
    // L1 trusted + inline RO => attach exact bytes and Copy receipt;
    // L2 repeat => idempotent bytes; L3 invalid hash => preserve previous bytes;
    // L4 escape => reject; L5 unresolved source => reject. Admission alone is
    // insufficient: each rule crosses the actual provider attachment port.
    for _ in 0..2 {
        let realized = environment.attach_mount(readonly.clone()).await.unwrap();
        assert_eq!(realized.realization, pc::Realization::Copy);
    }
    let mut invalid = readonly.clone();
    invalid.source = pc::MountSource::InlineBytes {
        contents: b"wrong".to_vec(),
        content_hash: Some("sha256:wrong".into()),
    };
    assert!(environment.attach_mount(invalid).await.is_err(), "L3");
    let mut escape = readonly.clone();
    escape.mount_path = "/../escape.txt".into();
    assert!(environment.attach_mount(escape).await.is_err(), "L4");
    let mut unresolved = readonly;
    unresolved.source = pc::MountSource::MemoryStore {
        store_id: "late-store".into(),
        materialization_reference: None,
        write_consistency: Default::default(),
    };
    assert!(environment.attach_mount(unresolved).await.is_err(), "L5");
    let files = environment
        .list_frozen_mount_files("/mnt/session/uploads")
        .await
        .unwrap();
    assert!(
        files.iter().any(|(_, bytes)| bytes == b"late"),
        "L1/L3 preserved bytes"
    );
    environment.dispose().await.unwrap();
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
        SessionEnvironmentProvider::trusted_host_workdir_with_agent_stderr(base.path(), false);
    let environment = provider.create(&spec()).await.unwrap();
    let root = AgentSandbox::workspace_cwd(&environment);

    let default = environment.agent_filesystem_view(None, AgentFilesystemClient::AcpProcess);
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
        crate::managed_resource_projection::AgentPathView::workdir_relative("/workspace"),
        "T1/E5"
    );

    let configured =
        environment.agent_filesystem_view(Some("repo/src"), AgentFilesystemClient::AcpProcess);
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
        crate::managed_resource_projection::AgentPathView::workdir_relative("/workspace/repo/src"),
        "T2/E5"
    );
    let native = environment.agent_filesystem_view(None, AgentFilesystemClient::NativeTools);
    assert_eq!(
        native.prompt_paths,
        crate::managed_resource_projection::AgentPathView::workdir_relative("/"),
        "Embedded Native paths use the actual rooted-tools cwd"
    );
    assert_eq!(native.protocol_cwd, root);
    assert_eq!(native.command_cwd, "/");
    assert!(
        environment
            .agent_prompt_path_view(None, true, true)
            .is_err(),
        "T4 mixed Workdir path consumers"
    );
    environment.dispose().await.unwrap();
}

#[tokio::test]
async fn native_process_and_agent_channel_share_one_live_environment() {
    // Cause/effect rules: W1 TrustedHost+default cwd; W2 nested Unicode/space cwd.
    // Both rules require Native Hand write -> real ACP child read/write -> Hand
    // read on the SAME mount. E1 actual process cwd equals protocol cwd;
    // E2 rendered File paths resolve to exact bytes; E3 child writes are visible
    // through the Native logical path. A file used as cwd fails without starting the
    // peer (W3). Reuse the Session owner and prompt renderer on all three OSes.
    use crate::managed_resource_projection::ResourcePromptProjection;
    for cwd in [None, Some("项目 repo/src")] {
        let base = tempfile::tempdir().unwrap();
        let provider =
            SessionEnvironmentProvider::trusted_host_workdir_with_agent_stderr(base.path(), false);
        let environment = provider.create(&spec()).await.unwrap();
        environment
            .materialize_inline("/workspace/项目 repo/src/.keep", b"")
            .await
            .unwrap();
        let view = environment.agent_filesystem_view(cwd, AgentFilesystemClient::AcpProcess);
        let logical = "/mnt/session/uploads/资料 brief.txt";
        let input = awaken_session_resource_contract::ResolvedInput {
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            binding_id: "file".into(),
            source: awaken_session_resource_contract::ResolvedInputSource::File {
                file_id: "file".into(),
            },
            mount_path: logical.into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: Some("{{awaken.resource.path}}".into()),
        };
        environment
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "native-before-acp".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({"path": logical, "content": "native-bytes"}),
            })
            .await
            .unwrap();
        let prompt = ResourcePromptProjection::try_from_input(&input)
            .unwrap()
            .render(
                crate::session_slot::ManagedContentDelivery::ManagedFilesystem,
                view.prompt_paths,
            );
        let relative = prompt.split_once('\n').unwrap().1;
        let exe = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut command = pc::Command::new([
            exe.as_str(),
            "--ignored",
            "--exact",
            "session_environment::tests::shared_environment::shared_filesystem_child",
            "--nocapture",
        ]);
        command.cwd = view.command_cwd;
        command.env = [
            ("AWAKEN_TEST_RESOURCE_PATH", relative),
            ("AWAKEN_TEST_PROTOCOL_CWD", &view.protocol_cwd),
        ]
        .into_iter()
        .map(|(name, value)| pc::EnvVar {
            name: name.into(),
            value: pc::EnvValue::Inline {
                value: value.into(),
            },
            visibility: pc::EnvVisibility::Process,
        })
        .collect();
        let (agent, mut channel) = environment.spawn_agent(command.clone()).await.unwrap();
        let mut output = String::new();
        channel.read_to_string(&mut output).await.unwrap();
        assert_eq!(agent.wait().await.unwrap().code, Some(0), "{output}");
        assert!(output.contains("FILESYSTEM_CHILD_OK"));
        let read = environment
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "native-after-acp".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": logical}),
            })
            .await
            .unwrap();
        assert!(read.text().contains("acp-bytes"), "E3");
        command.cwd = logical.into();
        assert!(environment.spawn_agent(command).await.is_err(), "W3");
        // W4 retained Workdir + either cwd -> same physical owner and exact
        // ACP-written bytes; rebind must not adopt or replace the workspace.
        let rebound = provider
            .rebind_retained(
                &spec(),
                &environment.handle(),
                &environment,
                crate::session_environment::ProviderEffect::Unattached,
            )
            .await
            .unwrap();
        let (
            SessionEnvironment::Workdir {
                sandbox: before, ..
            },
            SessionEnvironment::Workdir { sandbox: after, .. },
        ) = (&environment, &rebound)
        else {
            unreachable!()
        };
        assert!(Arc::ptr_eq(before, after), "W4 owner");
        let retained = rebound
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "native-after-rebind".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": logical}),
            })
            .await
            .unwrap();
        assert!(retained.text().contains("acp-bytes"), "W4 bytes");
        rebound.dispose().await.unwrap();
    }
}

#[test]
#[ignore = "real child of the portable Session filesystem contract"]
fn shared_filesystem_child() {
    // The child receives only the production-derived cwd and prompt path.
    // F1 wrong cwd/path => hard failure; F2 exact view => read/write exact bytes.
    let expected = std::env::var("AWAKEN_TEST_PROTOCOL_CWD").unwrap();
    assert_eq!(
        std::fs::canonicalize(expected).unwrap(),
        std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap()
    );
    let path = std::env::var("AWAKEN_TEST_RESOURCE_PATH").unwrap();
    assert!(!std::path::Path::new(&path).is_absolute());
    assert_eq!(std::fs::read(&path).unwrap(), b"native-bytes");
    std::fs::write(&path, b"acp-bytes").unwrap();
    println!("FILESYSTEM_CHILD_OK");
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
        let rejection = crate::sandbox_source::resolve_sandbox_tier(
            crate::deployment_config::SandboxTier::Namespace,
            false,
            base.path(),
        )
        .await
        .expect_err("an unavailable namespace must not silently degrade");
        assert!(rejection.contains("OS-native sandbox unavailable"));
        assert!(rejection.contains(&probe_error.to_string()));
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
    let environment = SessionEnvironment::namespace(
        namespace,
        Arc::new(FakeHandExecutorFactory),
        "/bin/sh",
        std::time::Duration::ZERO,
    );
    #[cfg(target_os = "macos")]
    assert_eq!(environment.handle().provider_kind(), "seatbelt");
    #[cfg(not(target_os = "macos"))]
    assert_eq!(environment.handle().provider_kind(), "bubblewrap");

    let mut native_command = pc::Command::new([
        "/bin/sh",
        "-c",
        "printf namespace-state > /workspace/process-marker",
    ]);
    native_command.cwd = pc::WorkspaceLayout::ROOT.into();
    let native = environment.sandbox().spawn(native_command).await.unwrap();
    assert_eq!(native.wait().await.unwrap().code, Some(0));

    let typed_files = environment.list_workspace_files("").await.unwrap();
    assert!(
        typed_files.iter().any(|(path, bytes)| {
            path.ends_with("process-marker") && bytes.as_slice() == b"namespace-state"
        }),
        "N1/E1 typed file reads the process-authored /workspace bytes: {typed_files:?}"
    );
    environment
        .write_workspace_file("typed-marker", b"typed-state")
        .await
        .unwrap();

    let hand_output = environment
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
    let (agent, mut channel) = environment.spawn_agent(agent_command).await.unwrap();
    let mut output = String::new();
    channel.read_to_string(&mut output).await.unwrap();
    assert_eq!(agent.wait().await.unwrap().code, Some(0));
    assert_eq!(output, "namespace-state|typed-state", "N2/E2");

    // N3 a revoked Namespace wrapper -> same physical Sandbox Arc and root,
    // fresh Hand, and successful agent I/O. The closed prior Hand stays closed.
    environment.stop_bound_processes().await.unwrap();
    let selected = SessionEnvironmentProvider::Namespace {
        provider,
        hand_factory: Arc::new(FakeHandExecutorFactory),
        hand_bin: "/bin/sh".into(),
        hand_idle_after: std::time::Duration::ZERO,
    };
    let rebound = selected
        .rebind_retained(
            &crate::session_environment::environment_test_spec(),
            &environment.handle(),
            &environment,
            crate::session_environment::ProviderEffect::Unattached,
        )
        .await
        .unwrap();
    let (
        SessionEnvironment::Namespace {
            sandbox: before, ..
        },
        SessionEnvironment::Namespace { sandbox: after, .. },
    ) = (&environment, &rebound)
    else {
        unreachable!()
    };
    assert!(Arc::ptr_eq(before, after), "N3 physical owner");
    assert!(
        !Arc::ptr_eq(&environment.tool_executor(), &rebound.tool_executor()),
        "N3 fresh Hand"
    );
    let (agent, mut channel) = rebound
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
    rebound.dispose().await.unwrap();
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
            "namespace" => SessionEnvironmentProvider::namespace_with_agent_stderr(
                root.path(),
                false,
                Arc::new(FakeHandExecutorFactory),
                "/bin/sh",
                std::time::Duration::ZERO,
            ),
            "container" => SessionEnvironmentProvider::container(
                Arc::new(FakeContainerProvider::default()),
                Vec::new(),
                Arc::new(FakeHandExecutorFactory),
                "/bin/sh",
            ),
            _ => {
                let mut provider = SessionEnvironmentProvider::workdir(root.path());
                if tier == "hand" {
                    provider.install_workdir_hand(
                        Arc::new(FakeHandExecutorFactory),
                        "/bin/sh".into(),
                        std::time::Duration::ZERO,
                    );
                }
                provider
            }
        };
        let environment = provider.create(&spec()).await.unwrap();
        for client in [
            AgentFilesystemClient::NativeTools,
            AgentFilesystemClient::AcpProcess,
        ] {
            for cwd in [None, Some("repo/src")] {
                let view = environment.agent_filesystem_view(cwd, client);
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
                    let logical = environment.skill_directory_path(directory).unwrap();
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
                environment.skill_directory_path(bad).is_err(),
                "P5 {tier} {bad:?}"
            );
        }
        environment.dispose().await.unwrap();
    }
}

#[tokio::test]
async fn container_native_tools_and_acp_share_one_environment_and_bound_hand() {
    let provider = Arc::new(FakeContainerProvider::default());
    let environment = SessionEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .create(&spec())
    .await
    .unwrap();
    assert_eq!(
        provider.creates.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(environment.handle().provider_kind(), "container");
    assert!(AgentSandbox::is_container(&environment));
    // Config-home placement decision table:
    // C1=environment already exists; C2=root may be read-only; C3=workspace
    // is the provider's writable Session boundary. C1+C2+C3 requires both
    // the exposed and materialization paths to stay under /workspace.
    assert_eq!(
        AgentSandbox::config_home(&environment),
        "/workspace/.acp-config"
    );
    assert_eq!(
        AgentSandbox::config_home_logical(&environment),
        "/workspace/.acp-config"
    );

    AgentSandbox::materialize_inline(&environment, "/workspace/direct.bin", b"direct")
        .await
        .unwrap();
    environment
        .write_workspace_file("projected.bin", b"projected")
        .await
        .unwrap();
    environment
        .materialize_read_only_tree(
            "generated-skills",
            &[("skill/SKILL.md".into(), b"generated".to_vec(), false)],
        )
        .await
        .unwrap();
    environment
        .remove_projection_path("projected.bin")
        .await
        .unwrap();

    let native = environment
        .sandbox()
        .spawn(pc::Command::new(["sh", "-c", "write marker"]))
        .await
        .unwrap();
    assert_eq!(native.wait().await.unwrap().code, Some(0));
    let (agent, mut channel) = environment
        .spawn_agent(pc::Command::new(["sh", "-c", "cat marker"]))
        .await
        .unwrap();
    let mut output = String::new();
    channel.read_to_string(&mut output).await.unwrap();
    assert_eq!(agent.wait().await.unwrap().code, Some(0));
    assert_eq!(output, "shared-container-state");

    let hand = environment.tool_executor();
    environment.register_skill_dir("skills");
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
    let skills = environment.scan_skill_dir("skills").unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "authored");
    assert_eq!(skills[0].dir, "skills/authored");
    environment.refresh_skills().await.unwrap();
    environment.stop_bound_processes().await.unwrap();
    environment.dispose().await.unwrap();
}

#[tokio::test]
async fn managed_skill_refresh_propagates_container_read_failure() {
    /* Cause/effect graph: C1 a Managed repository Skill root is admitted;
     * C2 the container read backing the registered-root refresh succeeds or
     * fails. Effects: E1 success may build the one repository registry (the
     * neighboring container test owns that row); E2 failure aborts registry
     * construction with the original diagnostic; E3 no empty catalog/prompt
     * is published as if the root legitimately contained no Skills.
     * Decision table: SR1 C1+read-ok => E1; SR2 C1+read-error => E2+E3.
     * This case owns SR2 through the existing `SessionEnvironment` refresh and
     * `build_skill_registry(Result)` boundaries; it adds no fake registry or
     * alternate discovery path. */
    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__fail_skill_read".into(), Vec::new());
    let environment = Arc::new(
        SessionEnvironmentProvider::container(
            provider,
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap(),
    );
    let roots = vec!["workspace/repository/.claude/skills".to_owned()];

    let error = match crate::skills::build_skill_registry(
        &[],
        Vec::new(),
        None,
        Some(environment.clone()),
        crate::skills::MANAGED_SKILLS_SUBDIR,
        Some(&roots),
        true,
        true,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => {
            panic!("a container Skill read failure must not become an empty registry")
        }
    };
    assert!(error.contains("scripted Skill read outage"), "SR2: {error}");
    environment.dispose().await.unwrap();
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
    let environment = SessionEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .create(&spec())
    .await
    .unwrap();
    environment.register_skill_dir("skills");
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__fail_skill_read".into(), Vec::new());

    let perception = environment
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
            awaken_runtime_contract::tool::ToolError::StateInvalidatedAfterDispatch(detail)
                if detail.contains("scripted Skill read outage")
        ),
        "PT3 original refresh diagnostic: {error}"
    );
    let stale = environment
        .scan_skill_dir("skills")
        .expect_err("PT3 invalidation must not expose the last-good catalog");
    assert!(stale.to_string().contains("unavailable"), "PT3: {stale}");
    assert!(
        matches!(
            environment
                .tool_executor()
                .invoke(&ToolCall {
                    call_id: "no-replay".into(),
                    tool_id: "bash".into(),
                    arguments: serde_json::json!({"command": "true"}),
                })
                .await,
            Err(awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_))
        ),
        "PT3 closed Hand rejects every later dispatch"
    );
    environment.dispose().await.unwrap();
}

#[tokio::test]
async fn post_dispatch_refresh_failure_cannot_hide_an_unknown_hand_effect() {
    // PT4 typed unknown effect + failed catalog refresh => retain Indeterminate
    // and close the one Hand binding. Neither StateInvalidated nor reacquisition
    // may hide uncertainty; PT3 above owns definitive effect + refresh failure.
    use awaken_runtime_contract::tool::ToolError;
    let provider = Arc::new(FakeContainerProvider::default());
    let factory = ScriptedHandFactory::new([ScriptedHandOutcome::Indeterminate]);
    let environment = SessionEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        factory.clone(),
        "/usr/local/bin/awaken-sandbox",
    )
    .create(&spec())
    .await
    .unwrap();
    environment.register_skill_dir("skills");
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
        environment.tool_executor().invoke(&call).await,
        Err(ToolError::Indeterminate(_))
    ));
    assert!(matches!(
        environment.tool_executor().invoke(&call).await,
        Err(ToolError::UnavailableBeforeDispatch(_))
    ));
    assert!(environment.scan_skill_dir("skills").is_err());
    assert_eq!(factory.binds.load(std::sync::atomic::Ordering::SeqCst), 1);
    environment.dispose().await.unwrap();
}

#[tokio::test]
async fn managed_skill_refresh_rejects_invalid_container_skill_bytes() {
    /* Cause/effect graph: C1 the container refresh returns an exact
     * `<id>/SKILL.md`; C2 its bytes are valid or invalid UTF-8. Effects: E1
     * valid bytes enter the one repository snapshot; E2 invalid bytes surface
     * a construction error; E3 invalid bytes never disappear into a healthy
     * empty registry. Decision table: SU1 C1+UTF8 => E1 (owned by
     * `container_native_tools_and_acp_share_one_environment_and_bound_hand`);
     * SU2 C1+!UTF8 => E2+E3. */
    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .shared
        .lock()
        .unwrap()
        .insert("broken/SKILL.md".into(), vec![0xff, 0xfe]);
    let environment = Arc::new(
        SessionEnvironmentProvider::container(
            provider,
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap(),
    );
    let roots = vec!["workspace/repository/.claude/skills".to_owned()];

    let error = match crate::skills::build_skill_registry(
        &[],
        Vec::new(),
        None,
        Some(environment.clone()),
        crate::skills::MANAGED_SKILLS_SUBDIR,
        Some(&roots),
        true,
        true,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => {
            panic!("invalid UTF-8 from Skill refresh must not become an empty registry")
        }
    };
    assert!(error.contains("UTF-8"), "SU2: {error}");
    environment.dispose().await.unwrap();
}

#[tokio::test]
async fn prepared_image_is_the_only_environment_to_container_image_conversion() {
    // FMECA: F1 every authored Environment is implicitly converted to an
    // image (S6/O5/D4, RPN120) -> self-hosted/package-free paths become
    // dependent on a builder; F2 a ready immutable image is omitted from the
    // provider request (S9/O3/D5, RPN135) -> packages are resolved again at
    // runtime; F3 both prepared image and mutable packages reach the provider
    // (S8/O3/D4, RPN96) -> two realization tracks can diverge. Mitigation is
    // the canonical Snapshot projection: `prepared_image=Some` selects one
    // Image environment and clears packages; `None` retains package inputs.
    //
    // Cause/effect decision table:
    // | Rule | prepared image | packages | final provider SandboxSpec |
    // | I1 | none | non-empty | no image; exact packages retained |
    // | I2 | ready digest | non-empty | Image(digest); packages empty |
    // This test crosses the final `SessionEnvironmentProvider::create`
    // boundary, so it verifies the actual container adapter input rather
    // than only an intermediate projection helper.
    fn snapshot(
        prepared_image: Option<String>,
    ) -> awaken_session_configuration_contract::EnvironmentSnapshot {
        awaken_session_configuration_contract::EnvironmentSnapshot {
            environment_id: "image-flow".into(),
            revision: awaken_environment_contract::EnvironmentRevision(7),
            self_hosted: false,
            config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                "image-flow-v7".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: awaken_environment_contract::EnvironmentPackages {
                npm: vec!["tsx@4".into()],
                ..Default::default()
            },
            prepared_image,
            network: awaken_session_configuration_contract::SessionNetworkPolicy::None,
            credential_realization:
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
        }
    }

    let provider = Arc::new(FakeContainerProvider::default());
    let environments = SessionEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    );
    let unprepared =
        crate::provisioning::environment_capacity_projection(&snapshot(None), true).spec;
    let environment = environments.create(&unprepared).await.unwrap();
    environment.dispose().await.unwrap();

    let digest = "registry.example/awaken@sha256:0123456789abcdef";
    let prepared =
        crate::provisioning::environment_capacity_projection(&snapshot(Some(digest.into())), true)
            .spec;
    let environment = environments.create(&prepared).await.unwrap();
    environment.dispose().await.unwrap();

    let specs = provider.specs.lock().unwrap();
    assert_eq!(specs.len(), 2, "I1/I2 provider boundary");
    assert_eq!(
        specs[0].packages.managers.get("npm"),
        Some(&vec!["tsx@4".to_string()]),
        "I1"
    );
    assert!(specs[0].environment.is_none(), "I1");
    assert!(specs[1].packages.managers.is_empty(), "I2/F3");
    assert_eq!(
        specs[1].environment,
        Some(awaken_provisioning_contract::EnvironmentKind::Image {
            reference: digest.into()
        }),
        "I2"
    );
}
