//! Managed filesystem Skill discovery and delivery through the Session environment.

use super::*;

struct BrainSkillModel;

#[async_trait::async_trait]
impl LlmExecutor for BrainSkillModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            AssistantOutput::text("done")
        } else {
            AssistantOutput::from_tool_calls(vec![awaken_tool_contract::tool::ToolCall {
                call_id: "skills-1".into(),
                tool_id: awaken_ext_skills::SKILL_LIST_TOOL_ID.into(),
                arguments: serde_json::json!({}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// Published-Skill Managed-filesystem cause/effect graph. C1 the immutable
/// Agent publication selects a Skill; C2 the installed Session contains its exact
/// verified bundle; C3 Runtime owns a Sandbox. Effects: E1 Skill metadata/path
/// is model-visible without its full body; E2 `SKILL.md` is materialized; E3 no
/// semantic Skill tools create a parallel activation path.
///
/// | Rule | C1 selected | C2 delivered | Effect |
/// |---|---|---|---|
/// | S1 | yes | yes | E1 + E2 + E3 |
/// | S2 | yes | no | fail closed before inference/materialization/read |
/// | S3 | no | yes | no unselected Skill projection/read |
/// S3 is owned by `managed_session_ignores_unbound_host_skill_sources`; this
/// test supplies the exact Resource pin for S1 and withholds its bytes for S2.
#[tokio::test]
async fn published_agent_receives_managed_filesystem_skill_discovery() {
    use awaken_runtime_contract::StaticPublishedAgentSnapshots;
    use awaken_runtime_contract::agent_bindings::AgentBindings;

    // S1/E1: the model-visible Workdir path uses this host's separator.
    fn skill_path() -> String {
        std::path::Path::new(".skills")
            .join("release-signal")
            .join("SKILL.md")
            .to_string_lossy()
            .into_owned()
    }

    #[derive(Clone, Default)]
    struct ToolFaceRecorder(Arc<Mutex<Vec<ChatRequest>>>);

    #[async_trait::async_trait]
    impl LlmExecutor for ToolFaceRecorder {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            self.0.lock().unwrap().push(request.clone());
            let system = request
                .messages
                .iter()
                .filter(|message| message.role == Role::System)
                .map(|message| block_text(&message.content))
                .collect::<Vec<_>>()
                .join("\n");
            let output = if request
                .messages
                .last()
                .is_some_and(|message| message.role == Role::Tool)
            {
                AssistantOutput::text("used managed filesystem Skill")
            } else if system.contains(&skill_path()) {
                AssistantOutput::from_tool_calls(vec![awaken_tool_contract::tool::ToolCall {
                    call_id: "read-managed-skill".into(),
                    tool_id: "read".into(),
                    arguments: serde_json::json!({
                        "path": skill_path()
                    }),
                }])
            } else {
                AssistantOutput::text("no selected managed Skill")
            };
            Ok(ChatResponse {
                output,
                usage: None,
                stop_reason: None,
            })
        }
    }

    let selected = "release-signal";
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("published-skill")
        .model(test_model_binding())
        .tools(crate::config::advertised_tools(
            &HashSet::new(),
            &HashSet::new(),
            &[],
        ))
        .agent_bindings(AgentBindings {
            skills: vec![awaken_agent_contract::AgentSkillBinding::custom(selected)],
            toolsets: vec![awaken_agent_contract::ToolsetPolicy {
                source: awaken_agent_contract::ToolsetSource::Agent,
                default: awaken_agent_contract::ToolExecutionPolicy::default(),
                overrides: Vec::new(),
            }],
            ..Default::default()
        })
        .build();
    let publications = Arc::new(
        StaticPublishedAgentSnapshots::try_new([snapshot.clone()])
            .expect("one immutable published Agent"),
    );
    let recorder = ToolFaceRecorder::default();
    let observed = recorder.0.clone();
    let storage = tempfile::tempdir().expect("selected Skill fixture storage");
    let host = (crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(recorder), "stub")
            .await
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path())
            .await
            .with_agent_publications(publications.clone()),
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let version = frozen_skill_version(selected, selected, "release", "Say READY.", &[]);
    install_pinned_managed_skill_session(
        &host,
        &managed,
        "published-skill-thread",
        "published-skill",
        session_environment(
            awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
        &version,
        true,
    )
    .await
    .expect("S1 exact Resource pin and Skill bytes");

    run_prepared_session_messages(
        &managed,
        "published-skill",
        "published-skill-thread",
        vec![Message::text(
            MessageId("published-skill-user".into()),
            Role::User,
            "Release signal",
        )],
    )
    .await
    .expect("published Skill run");

    {
        let requests = observed.lock().unwrap();
        let request = requests.first().expect("initial model request");
        let tools = &request.tools;
        assert!(
            !tools
                .iter()
                .any(|tool| tool.id == awaken_ext_skills::SKILL_LIST_TOOL_ID),
            "S1/E3 list_skills is absent"
        );
        assert!(
            !tools
                .iter()
                .any(|tool| tool.id == awaken_ext_skills::SKILL_TOOL_ID),
            "S1/E3 Skill is absent"
        );
        let system = request
            .messages
            .iter()
            .filter(|message| message.role == Role::System)
            .map(|message| block_text(&message.content))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(system.contains("release-signal"), "S1/E1 name");
        assert!(system.contains(&skill_path()), "S1/E1 path");
        assert!(
            !system.contains("Say READY"),
            "S1/E1 body remains on demand"
        );
        assert!(
            requests
                .last()
                .expect("post-read model request")
                .messages
                .iter()
                .filter(|message| message.role == Role::Tool)
                .map(|message| block_text(&message.content))
                .any(|content| content.contains("Say READY")),
            "S1/E1 the advertised path is readable through the real Session executor"
        );
    }
    assert!(
        host.realization_executor
            .environments()
            .session_environment("published-skill-thread")
            .await
            .expect("installed Sessions")
            .expect("S1 sandbox")
            .skills()
            .scan_skill_dir(
                awaken_session_configuration_contract::skill_delivery::DELIVERED_SKILLS_SUBDIR
            )
            .unwrap()
            .iter()
            .any(|file| file.id == selected),
        "S1/E2"
    );

    let missing_recorder = ToolFaceRecorder::default();
    let missing_observed = missing_recorder.0.clone();
    let missing_storage = tempfile::tempdir().expect("missing Skill fixture storage");
    let missing_host = (crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(missing_recorder), "stub")
            .await
            .with_skill_store(missing_storage.path().join("skills"))
            .with_store_dir(missing_storage.path())
            .await
            .with_agent_publications(publications),
    ))
    .build()
    .expect("validated Host dependencies");
    let missing_managed = managed_with_resource_source(missing_host.clone());
    let missing = install_pinned_managed_skill_session(
        &missing_host,
        &missing_managed,
        "selected-without-delivery",
        "published-skill",
        session_environment(
            awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
        &version,
        false,
    )
    .await
    .expect_err("S2 bound Skill without frozen bytes fails closed before execution");
    assert!(
        missing.message.contains("not found in this Workspace"),
        "S2: {missing}"
    );
    let missing_system = missing_observed
        .lock()
        .unwrap()
        .iter()
        .flat_map(|request| request.messages.iter())
        .filter(|message| message.role == Role::System)
        .map(|message| block_text(&message.content))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!missing_system.contains("release-signal"), "S2");
    assert!(
        missing_host
            .realization_executor
            .environments()
            .session_environment("selected-without-delivery")
            .await
            .expect("installed Sessions")
            .is_none(),
        "S2 fails before exposing a partial Environment"
    );
}

struct HandReadModel;

#[async_trait::async_trait]
impl LlmExecutor for HandReadModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            AssistantOutput::text("done")
        } else {
            AssistantOutput::from_tool_calls(vec![awaken_tool_contract::tool::ToolCall {
                call_id: "read-1".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": "missing.txt"}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// L2: the direct-session compatibility adapter executes an instruction-only
/// Skill in the Brain without awakening Hand.
#[tokio::test]
async fn direct_on_tool_use_brain_skill_adapter_keeps_the_environment_absent() {
    // Test design. Causes: C1 a direct (non-Managed) caller selects a Brain-owned
    // Skill; C2 it disables every filesystem tool; C3 Environment provisioning
    // is OnToolUse. Effects: E1 the compatibility `list_skills` adapter reaches
    // the canonical registry; E2 it returns the catalog; E3 Hand remains absent.
    // Rule L2: !Managed+C1+C2+C3 => E1+E2+E3. Constraint: the adapter consumes
    // the one registry; it neither creates a second catalog nor grants Hand.
    // Assembly rule S1: absent Environment passes no fork tool to this same
    // adapter; narrowing its inputs must retain the Brain-only result.
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(BrainSkillModel), "stub")
        .await
        .with_skills(vec![awaken_ext_skills::SkillSpec::new(
            "think",
            "Think",
            "reason",
            "Think carefully.",
        )]))
    .build()
    .expect("validated Host dependencies");
    let thread = "direct-deferred-brain";
    host.run_assembly
        .install_environment_projection(thread, &on_tool_use_environment())
        .unwrap();
    let deferred: Arc<dyn awaken_tool_contract::tool::ToolExecutor> = Arc::new(
        crate::lazy_sandbox::DeferredSandboxExecutor::new(&host.realization_executor, thread),
    );
    host.run_assembly
        .installed_sessions
        .install_with(thread, |installed| {
            installed.replace_frozen_with(|next| {
                next.tools = Some(
                    awaken_session_configuration_contract::SessionToolConfiguration {
                        toolsets: vec![ToolsetPolicy {
                            source: ToolsetSource::Agent,
                            default: ToolExecutionPolicy {
                                enabled: false,
                                permission: ToolPermissionRequirement::AlwaysAllow,
                            },
                            overrides: vec![ToolPolicyOverride::new(
                                "web_fetch",
                                ToolExecutionPolicy {
                                    enabled: true,
                                    permission: ToolPermissionRequirement::AlwaysAllow,
                                },
                            )],
                        }],
                        client_tools: Vec::new(),
                    },
                )
            });
            installed.run.deferred_executor = Some(deferred);
        })
        .expect("installed Sessions");

    let result = host
        .session_runs
        .run(
            None,
            thread,
            vec![Message::text(MessageId("u2".into()), Role::User, "skills")],
        )
        .await
        .unwrap();
    let tool_results = result
        .new_messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| block_text(&message.content))
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 1, "L2/E1 one semantic Skill result");
    assert!(
        tool_results[0].contains("\"skills\"") && !tool_results[0].contains("unknown tool"),
        "L2/E2 canonical Skill catalog: {tool_results:?}"
    );
    assert!(
        host.realization_executor
            .environments()
            .session_environment(thread)
            .await
            .expect("installed Sessions")
            .is_none(),
        "L2/E3"
    );
}

#[tokio::test]
async fn managed_session_ignores_unbound_host_skill_sources() {
    // Managed repository Skill rule R7. C1 the caller is Managed; C2 its frozen
    // Resource manifest selects no Skill; C3 a filesystem-requiring host-static Skill
    // exists; C4 a durable catalog cache also contains an unbound Skill; C5 all
    // filesystem tools are disabled. Effect E1 no Skill descriptor/executor or
    // prompt is projected; E2 Hand stays absent.
    // R7=C1+C2+C3+C4+C5=>E1+E2.
    // Counter-rule A2=!C1+instruction-only static Skill+C4=>the direct semantic
    // adapter is covered by L2. This distinguishes a compatibility input from the Managed
    // Binding/version/bytes authority instead of synchronizing both catalogs.
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolsetPolicy, ToolsetSource,
    };
    use awaken_session_configuration_contract::SessionInit;

    let host_static = awaken_ext_skills::SkillSpec {
        environment: awaken_ext_skills::SkillEnvironment::Filesystem,
        dir: Some("skills/think".into()),
        ..awaken_ext_skills::SkillSpec::new("think", "Think", "reason", "Think carefully.")
    };
    let storage = tempfile::tempdir().expect("Managed Skill authority test storage");
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_skills(vec![host_static])
        .with_skill_store(storage.path().join("skills")))
    .build()
    .expect("validated Host dependencies");
    let durable = frozen_skill_version(
        "cached",
        "Cached",
        "unbound durable catalog entry",
        "Never project without a binding.",
        &[],
    );
    host.capabilities
        .tools
        .skills
        .create(
            awaken_skill_store::SkillDefinition {
                id: durable.skill_id.clone(),
                workspace_id: host.run_assembly.local_workspace().into(),
                display_title: None,
                latest_version: durable.version,
                last_version: durable.version,
                timestamps: Default::default(),
            },
            durable,
        )
        .await
        .expect("durable Skill store configured")
        .expect("cache unbound Skill version");
    assert_eq!(
        host.capabilities
            .tools
            .skills
            .managed_ids_in(host.run_assembly.local_workspace())
            .expect("healthy registry"),
        vec!["cached".to_string()],
        "A1/C4 proves the live catalog contains an otherwise advertisable Skill"
    );
    install_test_session_application(&host);
    host.clone()
        .run_assembly
        .install_test_session_init(
            "managed-no-frozen-skill",
            SessionInit {
                workspace_id: host.run_assembly.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: Some(
                    awaken_session_configuration_contract::SessionToolConfiguration {
                        toolsets: vec![ToolsetPolicy {
                            source: ToolsetSource::Agent,
                            default: ToolExecutionPolicy {
                                enabled: false,
                                permission: ToolPermissionRequirement::AlwaysAllow,
                            },
                            overrides: Vec::new(),
                        }],
                        client_tools: Vec::new(),
                    },
                ),
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();

    let context = host
        .run_assembly
        .ctx_for("managed-no-frozen-skill", Some("assistant"))
        .await
        .expect("A1 builds from the empty frozen Skill projection");
    assert!(
        context
            .config
            .resolved_spec
            .tool_descriptors
            .iter()
            .all(|descriptor| {
                descriptor.id != awaken_ext_skills::SKILL_LIST_TOOL_ID
                    && descriptor.id != awaken_ext_skills::SKILL_TOOL_ID
            }),
        "A1/E1 no semantic Skill compatibility surface"
    );
    assert_eq!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization("managed-no-frozen-skill", |r| r
                .staging()
                .skill_prompt
                .clone())
            .expect("installed Sessions")
            .flatten(),
        None,
        "A1/E1 no unbound filesystem Skill prompt"
    );
    assert!(
        host.realization_executor
            .environments()
            .session_environment("managed-no-frozen-skill")
            .await
            .expect("installed Sessions")
            .is_none(),
        "A1/E2"
    );
}

/// L3: the Runtime's per-tool target routing sends a Sandbox tool through the
/// deferred Hand; the invoking Run blocks until materialization completes.
#[tokio::test]
async fn on_tool_use_runtime_hand_call_materializes_before_tool_execution() {
    // This is the Managed L3 row: a real fresh Root/controlled claim/production
    // sink owns first Create CAS before the Runtime tool. Direct L2 remains
    // non-Managed and environment-free; no synthesized grant covers this row.
    // Test design. Causes: C1 a Runtime Hand tool is selected on a lazy Session.
    // Effects: E1 Environment materialization completes before the tool effect;
    // E2 exactly one Environment is persisted. Constraint/Invariant: placement
    // precedes Hand execution. Decision rule: exercise C1 and assert E1 ordering/E2.
    use awaken_session_configuration_contract::SessionInit;
    let roots = ManagedSessionRootFixture::new().await;
    let host = roots
        .configure(crate::SessionExecutorBuilder::ephemeral(Arc::new(HandReadModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    install_test_session_application(&host);
    let managed = host.clone();
    roots
        .install_complete_test_session(
            &managed,
            "deferred-runtime-hand",
            SessionInit {
                workspace_id: host.run_assembly.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();

    run_prepared_session_messages(
        &managed,
        "assistant",
        "deferred-runtime-hand",
        vec![Message::text(MessageId("u3".into()), Role::User, "read")],
    )
    .await
    .unwrap();
    assert!(
        host.realization_executor
            .environments()
            .session_environment("deferred-runtime-hand")
            .await
            .expect("installed Sessions")
            .is_some()
    );
}

/// Managed-filesystem reservation cause/effect graph: C1 the Session permits
/// `read`; C2 its Resource projection contains an exact frozen bundle with a script;
/// Skill version; C3 reservation must remain side-effect free; C4 the
/// claimed/local execution context is built afterward; C5 an ordinary Hand
/// command follows the pinned read; C6 a Resource generation advances with
/// identical Skill pins.
/// Effects: E1 C1+C2+C3 projects the stable `SKILL.md` path without creating an
/// Environment; E2 C1+C2+C4 rejects the resident coordination envelope as an
/// execution cache hit and reads that exact path without an Environment; E3 C5
/// creates the Environment and executes its pinned script. Constraint:
/// reservation and execution share one Skill projection;
/// semantic tools are never a fallback for a missing provisional directory.
///
/// | Rule | read | selected Skill | phase | Environment | path/body |
/// |---|---|---|---|---|---|
/// | L9 | yes | yes | reservation | absent | path only |
/// | L10 | yes | yes | execution/read | absent | same path + exact body |
/// | L11 | yes | yes | ordinary command | present | identical materialized body |
/// | L12 | yes | same pins | resource revision | same | no Skill tree rewrite |
///
/// M3 in `skill_delivery_profile_uses_one_managed_filesystem_projection` covers
/// the Managed read-disabled rejection. L2 is direct compatibility only.
#[tokio::test]
async fn managed_filesystem_skill_path_survives_deferred_reservation() {
    let storage = tempfile::tempdir().expect("deferred Skill fixture storage");
    let host = (crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path())
            .await,
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let version = frozen_skill_version(
        "release-signal",
        "Release signal",
        "release safely",
        "RESERVATION-SKILL-BODY",
        &[("scripts/check.sh", "#!/bin/sh\nprintf PINNED-RUN")],
    );
    install_pinned_managed_skill_session(
        &host,
        &managed,
        "deferred-managed-filesystem-skill",
        "assistant",
        on_tool_use_environment(),
        &version,
        true,
    )
    .await
    .expect("prepare exact bound Skill on lazy Session");

    let provisional = host
        .run_assembly
        .ctx_for_session_coordination("deferred-managed-filesystem-skill", Some("assistant"), None)
        .await
        .expect("L9 provisional reservation context");
    assert!(
        provisional.env.is_none(),
        "L9/E1 no reservation side effect"
    );
    let provisional_metadata = host
        .run_assembly
        .installed_sessions
        .realizations()
        .read_realization("deferred-managed-filesystem-skill", |r| {
            r.staging().skill_prompt.clone()
        })
        .expect("installed Sessions")
        .flatten()
        .expect("L9/E1 projected Skill metadata");
    let provisional_prompt = provisional_metadata
        .render(&Default::default(), None)
        .unwrap();
    assert!(
        provisional_prompt.contains(".skills/release-signal/SKILL.md"),
        "L9/E1 stable path: {provisional_prompt}"
    );
    assert_eq!(
        provisional
            .config
            .resolved_spec
            .plugin_config
            .agent
            .skills
            .iter()
            .map(|skill| skill.skill_id.as_str())
            .collect::<Vec<_>>(),
        vec!["release-signal"],
        "L9/E1 generated dispatch snapshot freezes the selected catalog"
    );

    let executable = host
        .run_assembly
        .ctx_for_snapshot(
            "deferred-managed-filesystem-skill",
            Some("assistant"),
            Some(provisional.config.clone()),
        )
        .await
        .expect("L10 claimed-style executable context");
    assert!(
        !Arc::ptr_eq(&provisional, &executable),
        "L10/E2 execution must replace the resident coordination envelope"
    );
    assert!(
        executable.env.is_none(),
        "L10/E2 Skill reads remain sandbox-free"
    );
    let deferred = host
        .run_assembly
        .installed_sessions
        .read_run("deferred-managed-filesystem-skill", |run| {
            run.deferred_executor.clone()
        })
        .expect("installed Sessions")
        .flatten()
        .unwrap();
    let read = deferred
        .invoke(&awaken_tool_contract::tool::ToolCall {
            call_id: "read-pinned".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({"file_path": ".skills/release-signal/SKILL.md"}),
        })
        .await
        .expect("L10/E2 exact pinned read");
    assert_eq!(read.text().as_bytes(), version.skill_md().unwrap());
    assert!(
        host.realization_executor
            .environments()
            .session_environment("deferred-managed-filesystem-skill")
            .await
            .expect("installed Sessions")
            .is_none()
    );
    let script = deferred
        .invoke(&awaken_tool_contract::tool::ToolCall {
            call_id: "first-hand".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({"command": "sh .skills/release-signal/scripts/check.sh"}),
        })
        .await
        .expect("L11/E3 ordinary command materializes Hand");
    assert!(
        script.text().contains("PINNED-RUN"),
        "L11 executes the pinned script after materialization"
    );
    let environment = host
        .realization_executor
        .environments()
        .session_environment("deferred-managed-filesystem-skill")
        .await
        .expect("installed Sessions")
        .expect("L11/E3 materialized Environment");
    let files = environment
        .skills()
        .scan_skill_dir(
            awaken_session_configuration_contract::skill_delivery::DELIVERED_SKILLS_SUBDIR,
        )
        .unwrap();
    let materialized = files
        .iter()
        .find(|file| file.id == "release-signal")
        .expect("L10/E2 exact Skill path materialized");
    assert_eq!(
        materialized.content,
        read.text(),
        "L11 exact pre/post materialization bytes"
    );
    assert_eq!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization("deferred-managed-filesystem-skill", |r| {
                r.staging().skill_prompt.clone()
            })
            .expect("installed Sessions")
            .flatten(),
        Some(provisional_metadata),
        "L10/E2 execution retains metadata while projecting its realized path"
    );
    // L12 measures the real file independently of the projection helper. A
    // fixed mtime is an observation marker, not a delay or timing assumption.
    let skill_path = storage
        .path()
        .join("sandboxes/deferred-managed-filesystem-skill/.skills/release-signal/SKILL.md");
    let marker = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt as _;
        // FILE_WRITE_ATTRIBUTES grants only the metadata operation used by
        // L12; the pinned Skill remains read-only for file content.
        std::fs::OpenOptions::new()
            .access_mode(0x0100)
            .open(&skill_path)
            .unwrap()
    };
    #[cfg(not(windows))]
    let file = std::fs::File::open(&skill_path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(marker))
        .unwrap();
    #[cfg(windows)]
    assert!(
        std::fs::metadata(&skill_path)
            .unwrap()
            .permissions()
            .readonly(),
        "L12 marker must leave the pinned Skill read-only"
    );
    let active = host
        .realization_executor
        .staging()
        .thread_resource_manifest("deferred-managed-filesystem-skill")
        .expect("installed Sessions")
        .unwrap();
    managed
        .realization_executor
        .apply_session_inputs(
            "deferred-managed-filesystem-skill",
            &resource_transition(
                &active.workspace_id,
                active.revision,
                active.resources.clone(),
                active.revision + 1,
                active.resources,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(skill_path).unwrap().modified().unwrap(),
        marker,
        "L12 no redundant Skill installation"
    );
}

/// Legacy delivered-Skill deferral cause/effect decision table. Causes: C1 the
/// Environment is `on_tool_use`; C2 the legacy resource manifest has no frozen
/// Skill selection; C3 a cold-process durable catalog contains a Skill
/// with a support file. The cold catalog is not a frozen Session selection:
/// C1+C2+C3 therefore retains deferral, creates no Environment, and materializes
/// no ambient Skill bytes. L1/L2 above cover the selected rows.
#[tokio::test]
async fn on_tool_use_legacy_delivered_filesystem_skill_forces_an_eager_environment() {
    use awaken_skill_store::{SkillBundleFile, SkillDefinition, SkillVersion, bundle_sha256};

    let storage = tempfile::tempdir().expect("storage");
    let skill_store = storage.path().join("skills");
    let catalog_host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_skill_store(skill_store.clone()))
    .build()
    .expect("validated Host dependencies");
    let workspace = catalog_host.local_workspace().to_string();
    let files = vec![
        SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\nname: files\ndescription: inspect files\n---\nRead the bundled guide."
                .to_vec(),
            executable: false,
        },
        SkillBundleFile {
            path: "references/guide.md".into(),
            content: b"guide".to_vec(),
            executable: false,
        },
    ];
    let hash = bundle_sha256(&files);
    catalog_host
        .capabilities
        .tools
        .skills
        .create(
            SkillDefinition {
                id: "files".into(),
                workspace_id: workspace.clone(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
                timestamps: Default::default(),
            },
            SkillVersion {
                id: "skver_files_1".into(),
                skill_id: "files".into(),
                version: 1,
                name: "files".into(),
                description: "inspect files".into(),
                directory: "/skills/files".into(),
                bundle_sha256: hash,
                files,
                created_unix_nanos: 0,
            },
        )
        .await
        .expect("durable SkillStore")
        .expect("create Skill");
    drop(catalog_host);

    let host = (crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_skill_store(skill_store)
            .with_store_dir(storage.path())
            .await,
    ))
    .build()
    .expect("validated Host dependencies");
    assert!(
        host.capabilities
            .tools
            .skills
            .cache_snapshot_in(&workspace)
            .expect("healthy registry")
            .is_empty(),
        "L8 starts from a cold process cache"
    );
    let managed = managed_with_resource_source(host.clone());
    managed
        .install_complete_test_session(
            "deferred-legacy-delivered-skill",
            awaken_session_configuration_contract::SessionInit {
                workspace_id: workspace,
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .expect("L8 cold legacy projection");

    let context = host
        .run_assembly
        .ctx_for("deferred-legacy-delivered-skill", Some("assistant"))
        .await
        .expect("L8 context");
    assert!(
        context.env.is_none(),
        "L8 unselected catalog remains deferred"
    );
    assert!(
        host.realization_executor
            .environments()
            .session_environment("deferred-legacy-delivered-skill")
            .await
            .expect("installed Sessions")
            .is_none(),
        "L8 no ambient Skill materialization"
    );
}
