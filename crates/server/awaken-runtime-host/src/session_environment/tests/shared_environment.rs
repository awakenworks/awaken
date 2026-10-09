//! One live Session environment shared by native tools, processes, ACP, and Skill paths,
//! under the Host's prompt rendering and Skill delivery.

use super::*;

#[tokio::test]
async fn native_process_and_agent_channel_share_one_live_environment() {
    // Cause/effect rules: W1 TrustedHost+default cwd; W2 nested Unicode/space cwd.
    // Both rules require Native Hand write -> real ACP child read/write -> Hand
    // read on the SAME mount. E1 actual process cwd equals protocol cwd;
    // E2 rendered File paths resolve to exact bytes; E3 child writes are visible
    // through the Native logical path. A file used as cwd fails without starting the
    // peer (W3). Reuse the Session owner and prompt renderer on all three OSes.
    use crate::managed_input_layout::ResourcePromptProjection;
    for cwd in [None, Some("项目 repo/src")] {
        let base = tempfile::tempdir().unwrap();
        let provider =
            SandboxEnvironmentProvider::trusted_host_workdir_with_agent_stderr(base.path(), false)
                .into_session_provider();
        let environment = provider.create(&spec()).await.unwrap();
        environment
            .agent_process()
            .materialize_inline("/workspace/项目 repo/src/.keep", b"")
            .await
            .unwrap();
        let view = environment
            .agent_tools()
            .agent_filesystem_view(cwd, AgentFilesystemClient::AcpProcess);
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
            .agent_tools()
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
                crate::frozen_inputs::ManagedContentDelivery::ManagedFilesystem,
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
        let (agent, mut channel) = environment
            .agent_process()
            .spawn_agent(command.clone())
            .await
            .unwrap();
        let mut output = String::new();
        channel.read_to_string(&mut output).await.unwrap();
        assert_eq!(agent.wait().await.unwrap().code, Some(0), "{output}");
        assert!(output.contains("FILESYSTEM_CHILD_OK"));
        let read = environment
            .agent_tools()
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
        assert!(
            environment
                .agent_process()
                .spawn_agent(command)
                .await
                .is_err(),
            "W3"
        );
        // W4 retained Workdir + either cwd -> same physical owner and exact
        // ACP-written bytes; rebind must not adopt or replace the workspace.
        let rebound = provider
            .adoption()
            .rebind_retained(
                &spec(),
                &environment.lifecycle().handle(),
                &environment,
                crate::session_environment::ProviderEffect::Unattached,
            )
            .await
            .unwrap();
        let (
            SandboxEnvironment::Workdir {
                sandbox: before, ..
            },
            SandboxEnvironment::Workdir { sandbox: after, .. },
        ) = (
            environment.sandbox_environment(),
            rebound.sandbox_environment(),
        )
        else {
            unreachable!()
        };
        assert!(Arc::ptr_eq(before, after), "W4 owner");
        let retained = rebound
            .agent_tools()
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "native-after-rebind".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": logical}),
            })
            .await
            .unwrap();
        assert!(retained.text().contains("acp-bytes"), "W4 bytes");
        rebound.lifecycle().dispose().await.unwrap();
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
        SandboxEnvironmentProvider::container(
            provider,
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        )
        .into_session_provider()
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
    environment.lifecycle().dispose().await.unwrap();
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
        SandboxEnvironmentProvider::container(
            provider,
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        )
        .into_session_provider()
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
    environment.lifecycle().dispose().await.unwrap();
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
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        }
    }

    let provider = Arc::new(FakeContainerProvider::default());
    let environments = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider();
    let unprepared =
        crate::provisioning::environment_capacity_projection(&snapshot(None), true).spec;
    let environment = environments.create(&unprepared).await.unwrap();
    environment.lifecycle().dispose().await.unwrap();

    let digest = "registry.example/awaken@sha256:0123456789abcdef";
    let prepared =
        crate::provisioning::environment_capacity_projection(&snapshot(Some(digest.into())), true)
            .spec;
    let environment = environments.create(&prepared).await.unwrap();
    environment.lifecycle().dispose().await.unwrap();

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
