//! Frozen Session projection: model/tool/web/skill overlays and cache warmup.

use super::*;
use awaken_session_realization::peers::EnvironmentPeer as _;

#[tokio::test]
async fn host_accepts_only_backend_projections_that_match_the_publication() {
    // Cause graph:
    // C1 immutable publication -> E1 execution backend authority.
    // C2 non-default frozen baseline projection -> E2 equality check only;
    // canonical `default` is Native absence and is not stored redundantly.
    // C3 projection without publication -> E3 reject; a cache cannot become an
    // authoring source merely because the publication is unavailable.
    //
    // | Rule | Publication | Projection | Result |
    // | H1 | default | default (canonical absence) | accept native |
    // | H2 | default | acp:claude | reject mismatch |
    // | H3 | default | absent | accept publication |
    // | H4 | absent | acp:claude | reject missing authority |
    let published_host = || async {
        let snapshot = crate::config::server_config(
            "assistant",
            "stub",
            &HashSet::new(),
            &HashSet::new(),
            &[],
            &Default::default(),
            &[],
            awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        )
        .expect("literal model reference");
        let publications =
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
                .expect("valid publication");
        dispatch_test_host(
            (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
                .await
                .with_agent_publications(Arc::new(publications)))
            .build()
            .expect("validated Host dependencies"),
        )
    };

    let (matching, _matching_managed) = published_host().await;
    matching
        .run_assembly
        .register_thread_backend_projection("backend-h1", "default");
    assert!(
        matching
            .installed_sessions
            .read("backend-h1", |slot| slot.inputs.backend_ref.is_none())
            .unwrap_or(false),
        "H1 default has no redundant projection"
    );
    matching
        .run_assembly
        .ctx_for("backend-h1", Some("assistant"))
        .await
        .expect("H1 matching projection");

    let (mismatch, _mismatch_managed) = published_host().await;
    mismatch
        .run_assembly
        .register_thread_backend_projection("backend-h2", "acp:claude");
    let error = match mismatch
        .run_assembly
        .ctx_for("backend-h2", Some("assistant"))
        .await
    {
        Ok(_) => panic!("H2 accepted a mismatched backend projection"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("does not match publication"),
        "H2"
    );

    let (publication_only, _publication_only_managed) = published_host().await;
    publication_only
        .run_assembly
        .ctx_for("backend-h3", Some("assistant"))
        .await
        .expect("H3 publication without redundant projection");

    let (orphan, _orphan_managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
            .build()
            .expect("validated Host dependencies"),
    );
    orphan
        .run_assembly
        .register_thread_backend_projection("backend-h4", "acp:claude");
    let error = match orphan
        .run_assembly
        .ctx_for("backend-h4", Some("assistant"))
        .await
    {
        Ok(_) => panic!("H4 accepted a backend projection without a publication"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("no immutable model publication"),
        "H4: {error}"
    );
}

#[test]
fn frozen_session_model_override_replaces_the_complete_route_exactly_once() {
    use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
    use awaken_session_configuration_contract::{SessionModelOverride, SessionModelPublication};

    let base = awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
        .resolved_model(ResolvedModelCandidate::host(ModelBinding::new(
            "agent-provider",
            "base",
            "genai",
        )))
        .model_candidates([ModelBinding::new(
            "agent-provider",
            "base-fallback",
            "genai",
        )])
        .build();
    let primary =
        ResolvedModelCandidate::host(ModelBinding::new("third-party/gateway", "fast", "genai"));
    let fallback =
        ResolvedModelCandidate::host(ModelBinding::new("third-party/gateway", "slow", "genai"));
    let frozen = SessionModelOverride {
        publication: Some(Box::new(SessionModelPublication {
            primary: primary.clone(),
            candidates: vec![fallback.clone()],
        })),
        inference: Default::default(),
    };

    // Cause/effect table:
    // O1 complete override -> only its exact primary + roster survive;
    // O2 replay -> byte-identical fingerprint (idempotent);
    // O3 equal-id inference-only override -> Agent roster stays authoritative;
    // O4 malformed frozen roster -> fail before executor materialization.
    let inherit = awaken_session_configuration_contract::SessionSystemPromptSelection::Inherit;
    let projected = awaken_session_realization_contract::project_effective_agent_publication(
        Some(&frozen),
        &inherit,
        None,
        "workspace",
        base.clone(),
    )
    .expect("O1 exact projection");
    assert_eq!(projected.resolved_spec.model_binding, primary, "O1");
    assert_eq!(projected.resolved_spec.model_candidates, [fallback], "O1");
    assert!(
        projected
            .resolved_spec
            .candidate_for_model("base")
            .is_none(),
        "O1 superseded Agent route must not leak"
    );
    let replayed = awaken_session_realization_contract::project_effective_agent_publication(
        Some(&frozen),
        &inherit,
        None,
        "workspace",
        projected.clone(),
    )
    .expect("O2 idempotent replay");
    assert_eq!(replayed.fingerprint, projected.fingerprint, "O2");

    let inference_only = SessionModelOverride {
        publication: None,
        inference: Default::default(),
    };
    let reused = awaken_session_realization_contract::project_effective_agent_publication(
        Some(&inference_only),
        &inherit,
        None,
        "workspace",
        base.clone(),
    )
    .expect("O3 Agent publication reuse");
    assert_eq!(
        reused.resolved_spec.candidate_bindings(),
        base.resolved_spec.candidate_bindings(),
        "O3"
    );

    let malformed = SessionModelOverride {
        publication: Some(Box::new(SessionModelPublication {
            primary: primary.clone(),
            candidates: vec![primary],
        })),
        inference: Default::default(),
    };
    assert!(
        awaken_session_realization_contract::project_effective_agent_publication(
            Some(&malformed),
            &inherit,
            None,
            "workspace",
            base,
        )
        .is_err(),
        "O4 duplicate route must fail closed"
    );
}

#[tokio::test]
async fn session_tool_policy_preserves_the_published_snapshot_identity() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use awaken_runtime_contract::agent_bindings::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };

    // Cause/effect decision table:
    // | publication | frozen Session policy | effect |
    // | present     | present               | Runtime gate reads Session policy separately; snapshot and fingerprint unchanged |
    // | generated   | present               | generated execution config receives policy |
    // The second rule is owned by the fallback construction path. This test owns
    // the immutable-publication and distributed-continuation boundary: another
    // Worker must reconstruct the exact publication fingerprint. The executable
    // gate behavior of the same `effective_tool_authorization` owner is covered by
    // `config::tests::toolset_policy_controls_execution_gate_behavior`; copying
    // policy into this snapshot would create a second authority and fingerprint.
    let publication = crate::config::server_config(
        "assistant",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
    .expect("literal model reference");
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([publication.clone()])
            .expect("valid publication");
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_agent_publications(Arc::new(publications)))
        .build()
        .expect("validated Host dependencies"),
    );
    host.installed_sessions
        .replace_frozen_with("policy-session", |next| {
            next.tools = Some(
                awaken_session_configuration_contract::SessionToolConfiguration {
                    toolsets: vec![ToolsetPolicy {
                        source: ToolsetSource::Mcp {
                            server_name: "flow".into(),
                        },
                        default: ToolExecutionPolicy::default(),
                        overrides: vec![ToolPolicyOverride::new(
                            "write",
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

    let context = host
        .run_assembly
        .ctx_for("policy-session", Some("assistant"))
        .await
        .expect("build Session from immutable publication");
    assert!(
        context
            .config
            .resolved_spec
            .plugin_config
            .agent
            .toolsets
            .is_empty(),
        "the attempt clone preserves the immutable publication; Session policy is consumed by the one authorization owner"
    );
    assert!(
        publication
            .resolved_spec
            .plugin_config
            .agent
            .toolsets
            .is_empty()
    );
    assert_eq!(context.config.fingerprint, publication.fingerprint);
}

#[tokio::test]
async fn session_web_policy_overlay_reaches_the_configured_plugin_before_inference() {
    // Cause/effect graph: C1 an immutable root publication selects the
    // provider-server WebFetch realization; C2 the final Session tool overlay
    // adds one content restriction. E1 root Session composition passes C2 to the
    // same configured WebFetch plugin; E2 that plugin rejects C1+C2 before the
    // first model request, so a provider-internal fetch is also impossible.
    // The extension-level matrix owns the individual Web policy fields; this
    // composition rule owns only the Session overlay wiring.
    //
    // | Rule | realization | final Session policy | Effect |
    // | S1 | provider-server | max_content_tokens | capability-bound terminal; inference=0 |
    use awaken_runtime_contract::agent_bindings::{
        ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
    };

    struct SessionInferenceProbe(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl LlmExecutor for SessionInferenceProbe {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                output: AssistantOutput::text("unexpected inference"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    let mut publication = crate::config::server_config(
        "assistant",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.to_string()],
        &BTreeMap::from([(
            awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.to_string(),
            serde_json::json!({"provider_id": "openrouter", "options": {}}),
        )]),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
    .expect("literal model reference");
    publication
        .recompute_fingerprint()
        .expect("provider-server root publication remains coherent");
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([publication])
            .expect("valid provider-server publication");
    let inferences = Arc::new(AtomicUsize::new(0));
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(
            Arc::new(SessionInferenceProbe(inferences.clone())),
            "stub",
        )
        .await
        .with_agent_publications(Arc::new(publications)))
        .build()
        .expect("validated Host dependencies"),
    );
    host.installed_sessions
        .replace_frozen_with("provider-server-policy", |next| {
            next.tools = Some(
                awaken_session_configuration_contract::SessionToolConfiguration {
                    toolsets: vec![ToolsetPolicy {
                        source: ToolsetSource::Agent,
                        default: ToolExecutionPolicy::default(),
                        overrides: vec![ToolPolicyOverride::with_optional_configuration(
                            awaken_ext_builtin_tools::WEB_FETCH_TOOL_ID,
                            ToolExecutionPolicy::default(),
                            Some(serde_json::json!({
                                "type": "web_fetch",
                                "max_content_tokens": 512
                            })),
                        )],
                    }],
                    client_tools: Vec::new(),
                },
            );
        });

    let receipt = host
        .run(
            Some("assistant"),
            "provider-server-policy",
            user("fetch docs"),
        )
        .await
        .expect("S1 capability violations are committed as terminal Run truth");
    assert!(
        matches!(
            receipt.state,
            RunState::Ended(EndCause::Error(
                awaken_agent_contract::agent::run::Failure::CapabilityBound
            ))
        ),
        "S1/E1"
    );
    assert_eq!(inferences.load(Ordering::SeqCst), 0, "S1/E2");
}

#[tokio::test]
async fn repository_skill_discovery_requires_read_not_merely_a_filesystem_tool() {
    // Managed repository Skill cause/effect rules R2/R3. C1 a Managed Agent
    // toolset defaults disabled; C2 `bash` is explicitly enabled; C3 exact
    // `read` is disabled or enabled; C4 workspace-qualified and legacy-relative
    // resolved Repository mount plans exist; C5 a cold Managed recovery has no
    // publication snapshot but retains the same slot tool policy.
    // E1 general filesystem delivery remains possible from C2; E2 repository
    // roots are excluded for C3=false and admitted for C3=true.
    //
    // | Rule | Managed | repo | bash | read | admitted roots |
    // | R3   | yes     | yes  | on   | off  | none           |
    // | R2   | yes     | yes  | on   | on   | fixed repo path|
    // | R2c  | yes/cold| yes  | on   | on   | fixed repo path|
    // Constraint: another filesystem capability cannot widen `read` and
    // `plugin_config.skills_dir` cannot alter the Managed repository path.
    // Direct compatibility rows remain unchanged: a published/read-enabled
    // caller includes its authored root plus repository roots; no publication
    // retains `Some([])` as the live authored-source marker.
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };

    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let enabled = ToolExecutionPolicy {
        enabled: true,
        permission: ToolPermissionRequirement::AlwaysAllow,
    };
    let repository =
        |repository_id: &str, mount_path: &str| crate::staged_resources::RepositoryActivation {
            plan: awaken_provisioning_contract::RepositoryRealizationPlan {
                repository_id: repository_id.into(),
                mount_path: mount_path.into(),
                source_remote_url: format!("https://example.invalid/{repository_id}.git"),
                transport_url: format!("https://example.invalid/{repository_id}.git"),
                initial_branch: None,
                initial_commit: None,
                access: awaken_provisioning_contract::MountAccess::ReadWrite,
            },
            credential_pin: None,
        };
    host.installed_sessions.update("repository-policy", |slot| {
        slot.run.session_dispatch = true;
        slot.replace_frozen_with(|next| {
            next.tools = Some(
                awaken_session_configuration_contract::SessionToolConfiguration {
                    toolsets: vec![ToolsetPolicy {
                        source: ToolsetSource::Agent,
                        default: ToolExecutionPolicy {
                            enabled: false,
                            permission: ToolPermissionRequirement::AlwaysAllow,
                        },
                        overrides: vec![ToolPolicyOverride::new("bash", enabled)],
                    }],
                    client_tools: Vec::new(),
                },
            )
        });
        slot.realization
            .staging_mut()
            .resources
            .repositories
            .extend([
                repository("repo-a", "/workspace/repo-a"),
                repository("legacy-repo", "legacy-repo"),
            ]);
    });
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("repository-agent")
        .model(test_model_binding())
        .build();

    assert!(
        host.run_assembly
            .session_allows_filesystem_tools("repository-policy", None),
        "R3 filesystem delivery"
    );
    assert!(
        !host
            .run_assembly
            .session_allows_repository_skill_discovery("repository-policy", Some(&snapshot)),
        "R3 exact read denial"
    );
    assert_eq!(
        host.run_assembly.session_skill_source_roots(
            "repository-policy",
            Some(&snapshot),
            "custom-authored-skills",
        ),
        None,
        "R3 no Managed repository source"
    );

    host.installed_sessions.update("repository-policy", |slot| {
        let mut tools = slot.inputs.tools.clone().expect("Session tools");
        tools.toolsets[0]
            .overrides
            .push(ToolPolicyOverride::new("read", enabled));
        slot.replace_frozen_with(|next| next.tools = Some(tools));
    });
    assert!(
        host.run_assembly
            .session_allows_repository_skill_discovery("repository-policy", Some(&snapshot)),
        "R2 exact read admission"
    );
    assert_eq!(
        host.run_assembly.session_skill_source_roots(
            "repository-policy",
            Some(&snapshot),
            "custom-authored-skills",
        ),
        Some(vec![
            "legacy-repo/.claude/skills".to_string(),
            "workspace/repo-a/.claude/skills".to_string(),
        ]),
        "R2 fixed relative path ignores the direct authored setting"
    );
    assert_eq!(
        host.run_assembly.session_skill_source_roots(
            "repository-policy",
            None,
            "custom-authored-skills",
        ),
        Some(vec![
            "legacy-repo/.claude/skills".to_string(),
            "workspace/repo-a/.claude/skills".to_string(),
        ]),
        "R2c cold Managed recovery uses the durable slot read policy"
    );

    host.installed_sessions
        .update_run("repository-policy", |run| run.session_dispatch = false);
    assert_eq!(
        host.run_assembly.session_skill_source_roots(
            "repository-policy",
            Some(&snapshot),
            "custom-authored-skills",
        ),
        Some(vec![
            "custom-authored-skills".to_string(),
            "legacy-repo/.claude/skills".to_string(),
            "workspace/repo-a/.claude/skills".to_string(),
        ]),
        "direct published compatibility keeps authored and repository roots"
    );
    assert_eq!(
        host.run_assembly
            .session_skill_source_roots("unpublished-direct", None, "skills"),
        Some(Vec::new()),
        "direct unpublished compatibility keeps the live authored marker"
    );
}

#[tokio::test]
async fn skill_delivery_profile_uses_one_managed_filesystem_projection() {
    // Cause/effect graph: C1 profile is Managed; C2 an exact frozen Skill is
    // selected; C3 at least one filesystem tool is enabled; C4 the frozen
    // projection is present. Effects: E1 ManagedFilesystem is selected; E2 the
    // direct compatibility SemanticTools projection is selected; E3 admission
    // fails before inference; E4 a Skill-free Managed Session may retain
    // SemanticTools for non-Skill content such as Memory.
    //
    // | Rule | C1 Managed | C2 Skill | C3 FS | C4 frozen | Effect |
    // | M1 | no  | yes | no  | n/a | E2 direct compatibility adapter |
    // | M2 | yes | yes | yes | yes | E1 filesystem-only Skill projection |
    // | M3 | yes | yes | no  | yes | E3 reject; no semantic fallback |
    // | M4 | yes | no  | no  | yes | E4 semantic non-Skill content allowed |
    // | M5 | yes | bound | any | no | E3 missing selected evidence rejected |
    // | M6 | yes | bound | yes | deferred binding | E1 metadata-only reservation |
    // | M7 | yes | other | any | deferred binding | E3 mismatched pin rejected |
    // | M8 | yes | no | no | Memory path variable | E3 physical path rejected |
    use crate::frozen_inputs::ManagedContentDelivery;
    use crate::host::session::SkillDeliveryEvidence;
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolsetPolicy, ToolsetSource,
    };

    let deny_filesystem = || awaken_session_configuration_contract::SessionToolConfiguration {
        toolsets: vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy {
                enabled: false,
                permission: ToolPermissionRequirement::AlwaysAllow,
            },
            overrides: Vec::new(),
        }],
        client_tools: Vec::new(),
    };
    let frozen = vec![frozen_skill_version(
        "think",
        "Think",
        "reason",
        "Think carefully.",
        &[],
    )];
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_skills(vec![awaken_ext_skills::SkillSpec::new(
            "think",
            "Think",
            "reason",
            "Think carefully.",
        )]))
    .build()
    .expect("validated Host dependencies");

    host.installed_sessions
        .replace_frozen_with("direct-semantic", |next| {
            next.tools = Some(deny_filesystem())
        });
    assert_eq!(
        host.run_assembly
            .select_content_delivery(
                "direct-semantic",
                None,
                SkillDeliveryEvidence::Materialized(None),
            )
            .expect("M1 direct compatibility projection"),
        ManagedContentDelivery::SemanticTools,
        "M1/E2"
    );

    host.installed_sessions
        .update_run("managed-filesystem", |run| {
            run.session_dispatch = true;
        });
    assert_eq!(
        host.run_assembly
            .select_content_delivery(
                "managed-filesystem",
                None,
                SkillDeliveryEvidence::Materialized(Some(&frozen)),
            )
            .expect("M2 Managed filesystem projection"),
        ManagedContentDelivery::ManagedFilesystem,
        "M2/E1"
    );

    host.installed_sessions.update("managed-denied", |slot| {
        slot.run.session_dispatch = true;
        slot.replace_frozen_with(|next| next.tools = Some(deny_filesystem()));
    });
    let denied = host
        .run_assembly
        .select_content_delivery(
            "managed-denied",
            None,
            SkillDeliveryEvidence::Materialized(Some(&frozen)),
        )
        .expect_err("M3 must not construct the semantic Skill adapter");
    assert!(
        denied
            .to_string()
            .contains("require filesystem progressive disclosure"),
        "M3/E3: {denied}"
    );

    let empty_host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    empty_host
        .installed_sessions
        .update("managed-no-skill", |slot| {
            slot.run.session_dispatch = true;
            slot.replace_frozen_with(|next| next.tools = Some(deny_filesystem()));
        });
    assert_eq!(
        empty_host
            .run_assembly
            .select_content_delivery(
                "managed-no-skill",
                None,
                SkillDeliveryEvidence::Materialized(Some(&[])),
            )
            .expect("M4 keeps semantic delivery available to non-Skill content"),
        ManagedContentDelivery::SemanticTools,
        "M4/E4"
    );

    let memory_with_path = crate::session_resource::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::from("memory"),
        source: crate::session_resource::ResolvedInputSource::MemoryStore {
            memory_store_id: awaken_resource_contract::MemoryStoreId::from("store"),
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from("store"),
                version: awaken_resource_contract::ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        },
        mount_path: "/mnt/memory".into(),
        access: awaken_resource_contract::ResourceAccess::ReadOnly,
        instructions: Some("Read {{awaken.resource.path}}".into()),
    };
    empty_host
        .installed_sessions
        .update("managed-memory-path", |slot| {
            slot.run.session_dispatch = true;
            slot.replace_frozen_with(|next| next.tools = Some(deny_filesystem()));
            slot.realization
                .staging_mut()
                .resources
                .resource_prompts
                .push(
                    crate::managed_input_layout::ResourcePromptProjection::try_from_input(
                        &memory_with_path,
                    )
                    .unwrap(),
                );
        });
    let path_error = empty_host
        .run_assembly
        .select_content_delivery(
            "managed-memory-path",
            None,
            SkillDeliveryEvidence::Materialized(Some(&[])),
        )
        .expect_err("M8 rejects an unusable physical path instruction");
    assert!(
        path_error
            .to_string()
            .contains("instructions reference a filesystem path"),
        "M8/E3: {path_error}"
    );

    empty_host
        .installed_sessions
        .update_run("managed-missing-freeze", |run| {
            run.session_dispatch = true;
        });
    let selected_snapshot =
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("managed-selected-skill")
            .model(test_model_binding())
            .agent_bindings(awaken_runtime_contract::agent_bindings::AgentBindings {
                skills: vec![awaken_agent_contract::AgentSkillBinding::custom("think")],
                toolsets: vec![ToolsetPolicy {
                    source: ToolsetSource::Agent,
                    default: ToolExecutionPolicy::default(),
                    overrides: Vec::new(),
                }],
                ..Default::default()
            })
            .build();
    let missing = empty_host
        .run_assembly
        .select_content_delivery(
            "managed-missing-freeze",
            Some(&selected_snapshot),
            SkillDeliveryEvidence::Materialized(None),
        )
        .expect_err("M5 rejects an incomplete Managed projection");
    assert!(
        missing
            .to_string()
            .contains("has neither frozen version bytes nor a deferred aggregate binding"),
        "M5/E3: {missing}"
    );
    // M5a: the same published Agent with an explicit empty Resource manifest
    // selects no Skill. This must admit a Skill-free Managed Session while M5
    // still rejects a published Skill whose frozen bytes are missing.
    let cleared = awaken_session_realization_contract::project_effective_agent_publication(
        None,
        &awaken_session_configuration_contract::SessionSystemPromptSelection::Inherit,
        Some(&[]),
        "workspace",
        selected_snapshot.clone(),
    )
    .expect("M5a canonical Resource projection");
    assert!(
        empty_host
            .run_assembly
            .select_content_delivery(
                "managed-missing-freeze",
                Some(&cleared),
                SkillDeliveryEvidence::Materialized(Some(&[])),
            )
            .is_ok(),
        "M5a empty Skill manifest clears the published selection"
    );

    let binding = crate::session_resource::ResolvedSkillBinding {
        kind: awaken_agent_contract::AgentSkillKind::Custom,
        skill_id: "think".into(),
        version: 1,
        bundle_sha256: "sha256:think".into(),
    };
    empty_host
        .installed_sessions
        .update_run("managed-deferred-freeze", |run| run.session_dispatch = true);
    assert_eq!(
        empty_host
            .run_assembly
            .select_content_delivery(
                "managed-deferred-freeze",
                Some(&selected_snapshot),
                SkillDeliveryEvidence::DeferredBindings(std::slice::from_ref(&binding)),
            )
            .expect("M6 accepts the aggregate pin without loading bytes"),
        ManagedContentDelivery::ManagedFilesystem,
        "M6/E1"
    );

    let other_snapshot =
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("managed-other-skill")
            .model(test_model_binding())
            .agent_bindings(awaken_runtime_contract::agent_bindings::AgentBindings {
                skills: vec![awaken_agent_contract::AgentSkillBinding::custom("other")],
                toolsets: vec![ToolsetPolicy {
                    source: ToolsetSource::Agent,
                    default: ToolExecutionPolicy::default(),
                    overrides: Vec::new(),
                }],
                ..Default::default()
            })
            .build();
    empty_host
        .installed_sessions
        .update_run("managed-deferred-mismatch", |run| {
            run.session_dispatch = true
        });
    let mismatch = empty_host
        .run_assembly
        .select_content_delivery(
            "managed-deferred-mismatch",
            Some(&other_snapshot),
            SkillDeliveryEvidence::DeferredBindings(std::slice::from_ref(&binding)),
        )
        .expect_err("M7 rejects an unrelated aggregate pin");
    assert!(
        mismatch
            .to_string()
            .contains("has neither frozen version bytes nor a deferred aggregate binding"),
        "M7/E3: {mismatch}"
    );
}

#[tokio::test]
async fn session_content_delivery_is_frozen_across_auxiliary_agent_snapshots() {
    // Content-delivery cause/effect graph: C1 the physical Session has no prior
    // choice; C2 it already chose filesystem and a deny-all Outcome grader is
    // projected; C3 it already chose semantic tools and a later Agent snapshot
    // exposes filesystem capability. Effects: E1 C1 derives exactly once; E2
    // C2/C3 reuse the existing Session choice without mutation or rejection.
    // Constraint K1 Agent tool capability still gates what that Agent may call;
    // reusing delivery neither widens tools nor creates a child-owned mount
    // authority. Rules CD1=C1=>E1; CD2=C2=>E2; CD3=C3=>E2.
    use crate::frozen_inputs::ManagedContentDelivery;
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };

    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let restricted = awaken_runtime_contract::ExecutableAgentSnapshot::builder("grader")
        .model(test_model_binding())
        .build();
    assert!(
        !host
            .run_assembly
            .session_allows_filesystem_tools("filesystem-session", Some(&restricted)),
        "CD2 auxiliary snapshot would independently derive semantic delivery"
    );
    host.installed_sessions
        .install_realization("filesystem-session", |r| {
            r.staging_mut().content_delivery = Some(ManagedContentDelivery::ManagedFilesystem)
        });
    assert_eq!(
        host.run_assembly
            .select_content_delivery(
                "filesystem-session",
                Some(&restricted),
                crate::host::session::SkillDeliveryEvidence::Materialized(None),
            )
            .expect("CD2 reuses physical Session delivery"),
        ManagedContentDelivery::ManagedFilesystem,
        "CD2/E2"
    );

    host.installed_sessions.update("semantic-session", |slot| {
        slot.realization.staging_mut().content_delivery =
            Some(ManagedContentDelivery::SemanticTools);
        slot.replace_frozen_with(|next| {
            next.tools = Some(
                awaken_session_configuration_contract::SessionToolConfiguration {
                    toolsets: vec![ToolsetPolicy {
                        source: ToolsetSource::Agent,
                        default: ToolExecutionPolicy {
                            enabled: false,
                            permission: ToolPermissionRequirement::AlwaysAllow,
                        },
                        overrides: vec![ToolPolicyOverride::new(
                            "write",
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
    });
    assert!(
        host.run_assembly
            .session_allows_filesystem_tools("semantic-session", None),
        "CD3 later snapshot would independently derive filesystem delivery"
    );
    assert_eq!(
        host.run_assembly
            .select_content_delivery(
                "semantic-session",
                None,
                crate::host::session::SkillDeliveryEvidence::Materialized(None),
            )
            .expect("CD3 reuses physical Session delivery"),
        ManagedContentDelivery::SemanticTools,
        "CD3/E2"
    );
}

#[test]
fn managed_tool_projection_has_one_role_and_session_override_decision_table() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use crate::host::session::{
        ManagedCoordinationRole, SessionToolsetProjection, project_managed_coordination_surface,
        project_session_tool_override,
    };
    use awaken_agent_contract::{
        ClientToolDescriptor, ToolExecutionPolicy, ToolsetPolicy, ToolsetSource,
    };
    use awaken_runtime_contract::agent_bindings::{AgentAdvisorBinding, AgentDelegateBinding};
    use awaken_runtime_contract::resolved::{
        ADVISOR_TOOL_ID, ModelBinding, ResolvedModelCandidate, ToolDescriptor, ToolKind,
    };

    // Cause/effect graph: C1 native vs Managed root/child role selects exactly
    // one coordination surface; C2 generated/inherited vs immutable publication
    // selects whether Session toolsets enter the executable clone; C3 an explicit
    // Session client descriptor replaces published client ownership. Effects:
    // E1 Managed primary replaces native `agent_run` with fixed list/send;
    // E2 every Managed child loses nested delegation/advisor and receives no
    // coordination command; E3 generated/self-child embeds Session toolsets;
    // E4 published/non-self retains its publication toolsets; E5 explicit client
    // tools exact-replace. No obsolete Task descriptor participates in projection.
    //
    // | Rule | role | snapshot owner | Session overlay | Effects |
    // | M0 | native | published | preserve | agent_run only; no Managed/Task commands |
    // | M1 | primary | generated | project | E1,E3,E5 |
    // | M2 | primary | published | preserve | E1,E4,E5 |
    // | M3 | child | self/inherited | project | E2,E3,E5 |
    // | M4 | child | other Agent | absent | E2,E4 |
    let published_policy = ToolsetPolicy {
        source: ToolsetSource::Agent,
        default: ToolExecutionPolicy::default(),
        overrides: Vec::new(),
    };
    let session_policy = ToolsetPolicy {
        source: ToolsetSource::Mcp {
            server_name: "session-policy".into(),
        },
        default: ToolExecutionPolicy::default(),
        overrides: Vec::new(),
    };
    let published_client = ClientToolDescriptor {
        name: "published_client".into(),
        description: "published".into(),
        input_schema: serde_json::json!({"type": "object"}),
    };
    let session_client = ClientToolDescriptor {
        name: "session_client".into(),
        description: "session".into(),
        input_schema: serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}}),
    };
    let tools = awaken_session_configuration_contract::SessionToolConfiguration {
        toolsets: vec![session_policy.clone()],
        client_tools: vec![session_client.clone()],
    };
    let builtin = |id: &str| {
        awaken_ext_builtin_tools::builtin_tools()
            .into_iter()
            .find(|tool| tool.descriptor().id == id)
            .expect("canonical builtin descriptor")
            .into_descriptor()
    };
    let native_ids = crate::config::advertised_tools(
        &HashSet::new(),
        &HashSet::from(["worker".to_string()]),
        &[],
    )
    .into_iter()
    .map(|descriptor| descriptor.id)
    .collect::<std::collections::BTreeSet<_>>();
    assert!(
        native_ids.contains(awaken_ext_builtin_tools::AGENT_RUN),
        "M0 native delegation"
    );
    for forbidden in [
        awaken_ext_builtin_tools::LIST_AGENTS,
        awaken_ext_builtin_tools::SEND_MESSAGE,
        "cancel_task",
        "recover_failed_messages",
    ] {
        assert!(
            !native_ids.contains(forbidden),
            "M0 forbids parallel command {forbidden}"
        );
    }
    let mut base = awaken_runtime_contract::ExecutableAgentSnapshot::builder("coordinator")
        .model(test_model_binding())
        .build();
    base.resolved_spec.tool_descriptors = vec![
        ToolDescriptor::pinned(
            "test",
            "ordinary",
            "ordinary",
            serde_json::json!({"type": "object"}),
        ),
        crate::config::session_client_tool_descriptor(&published_client),
        builtin(awaken_ext_builtin_tools::AGENT_RUN),
        builtin(awaken_ext_builtin_tools::LIST_AGENTS),
        builtin(awaken_ext_builtin_tools::SEND_MESSAGE),
        ToolDescriptor::pinned(
            "test",
            ADVISOR_TOOL_ID,
            "advisor",
            serde_json::json!({"type": "object"}),
        )
        .with_kind(ToolKind::Advisor),
    ];
    base.resolved_spec.plugin_config.agent.delegates = vec![AgentDelegateBinding {
        agent_id: awaken_runtime_contract::snapshot::AgentId("worker".into()),
        source_revision: Some(1),
        recursive_self: false,
    }];
    base.resolved_spec.plugin_config.agent.advisor = Some(AgentAdvisorBinding {
        model: "advisor-model".into(),
        candidate: ResolvedModelCandidate::host(ModelBinding::new(
            "provider",
            "advisor-model",
            "backend",
        )),
    });
    base.resolved_spec.plugin_config.agent.toolsets = vec![published_policy.clone()];

    let descriptor_ids = |snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot| {
        snapshot
            .resolved_spec
            .tool_descriptors
            .iter()
            .map(|descriptor| descriptor.id.clone())
            .collect::<std::collections::BTreeSet<_>>()
    };
    let assert_primary = |snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
                          rule: &str| {
        let ids = descriptor_ids(snapshot);
        assert!(
            ids.contains(awaken_ext_builtin_tools::LIST_AGENTS),
            "{rule}/E1"
        );
        assert!(
            ids.contains(awaken_ext_builtin_tools::SEND_MESSAGE),
            "{rule}/E1"
        );
        assert_eq!(
            snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .filter(|descriptor| descriptor.id == awaken_ext_builtin_tools::SEND_MESSAGE)
                .count(),
            1,
            "{rule}/E1 one Agent-message implementation"
        );
        assert!(
            !ids.contains(awaken_ext_builtin_tools::AGENT_RUN),
            "{rule}/E1"
        );
        assert!(
            ids.contains(ADVISOR_TOOL_ID),
            "{rule}/E1 advisor remains internal"
        );
    };
    let assert_child = |snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot, rule: &str| {
        let ids = descriptor_ids(snapshot);
        for forbidden in [
            awaken_ext_builtin_tools::AGENT_RUN,
            awaken_ext_builtin_tools::LIST_AGENTS,
            awaken_ext_builtin_tools::SEND_MESSAGE,
            ADVISOR_TOOL_ID,
        ] {
            assert!(!ids.contains(forbidden), "{rule}/E2 forbids {forbidden}");
        }
        assert!(
            snapshot
                .resolved_spec
                .plugin_config
                .agent
                .delegates
                .is_empty(),
            "{rule}/E2"
        );
        assert!(
            snapshot.resolved_spec.plugin_config.agent.advisor.is_none(),
            "{rule}/E2"
        );
    };

    let mut generated_primary = base.clone();
    project_session_tool_override(
        &mut generated_primary,
        &tools,
        SessionToolsetProjection::ProjectIntoSnapshot,
    );
    project_managed_coordination_surface(&mut generated_primary, ManagedCoordinationRole::Primary);
    assert_primary(&generated_primary, "M1");
    assert_eq!(
        generated_primary
            .resolved_spec
            .plugin_config
            .agent
            .toolsets
            .as_slice(),
        std::slice::from_ref(&session_policy),
        "M1/E3"
    );

    let mut published_primary = base.clone();
    project_session_tool_override(
        &mut published_primary,
        &tools,
        SessionToolsetProjection::PreservePublished,
    );
    project_managed_coordination_surface(&mut published_primary, ManagedCoordinationRole::Primary);
    assert_primary(&published_primary, "M2");
    assert_eq!(
        published_primary
            .resolved_spec
            .plugin_config
            .agent
            .toolsets
            .as_slice(),
        std::slice::from_ref(&published_policy),
        "M2/E4"
    );

    for (rule, snapshot) in [("M1", &generated_primary), ("M2", &published_primary)] {
        let clients = snapshot
            .resolved_spec
            .tool_descriptors
            .iter()
            .filter(|descriptor| descriptor.kind == ToolKind::ClientExecuted)
            .collect::<Vec<_>>();
        assert_eq!(clients.len(), 1, "{rule}/E5");
        assert_eq!(clients[0].id, session_client.name, "{rule}/E5");
    }

    let mut inherited_child = base.clone();
    project_session_tool_override(
        &mut inherited_child,
        &tools,
        SessionToolsetProjection::ProjectIntoSnapshot,
    );
    project_managed_coordination_surface(&mut inherited_child, ManagedCoordinationRole::Child);
    assert_child(&inherited_child, "M3");
    assert_eq!(
        inherited_child.resolved_spec.plugin_config.agent.toolsets,
        [session_policy],
        "M3/E3"
    );
    assert_eq!(
        inherited_child
            .resolved_spec
            .tool_descriptors
            .iter()
            .filter(|descriptor| descriptor.kind == ToolKind::ClientExecuted)
            .map(|descriptor| descriptor.id.as_str())
            .collect::<Vec<_>>(),
        [session_client.name.as_str()],
        "M3/E5"
    );

    let mut other_child = base;
    project_managed_coordination_surface(&mut other_child, ManagedCoordinationRole::Child);
    assert_child(&other_child, "M4");
    assert_eq!(
        other_child.resolved_spec.plugin_config.agent.toolsets,
        [published_policy],
        "M4/E4"
    );
    assert_eq!(
        other_child
            .resolved_spec
            .tool_descriptors
            .iter()
            .filter(|descriptor| descriptor.kind == ToolKind::ClientExecuted)
            .map(|descriptor| descriptor.id.as_str())
            .collect::<Vec<_>>(),
        [published_client.name.as_str()],
        "M4/E4"
    );
}

#[tokio::test]
async fn session_client_tools_replace_the_published_surface_with_exact_ownership() {
    // Cause/effect graph: C1 no Session override inherits the immutable
    // publication; C2 an explicit empty override clears published client tools;
    // C3 a non-empty override supplies an exact descriptor. Effects: E1 the
    // publication object remains unchanged; E2 the execution clone exposes only
    // the Session descriptor with its exact schema; E3 a model call awaits a
    // client result rather than built-in approval.
    //
    // | Rule | Session client tools | execution surface | pending owner |
    // | R1 | None | published tools | published owner |
    // | R2 | Some([]) | no client tools | none |
    // | R3 | Some([lookup]) | exact lookup schema | protocol client |
    // R1 is covered by ordinary published-Agent tests; this regression owns R2
    // and R3, including the real classification boundary that Managed events use.
    let publication = crate::config::server_config(
        "assistant",
        "stub",
        &HashSet::from(["published_lookup".to_string()]),
        &HashSet::new(),
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
    .expect("literal model reference");
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([publication.clone()])
            .expect("valid publication");
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(ClientLookupModel), "stub")
            .await
            .with_agent_publications(Arc::new(publications)))
        .build()
        .expect("validated Host dependencies"),
    );

    host.installed_sessions
        .replace_frozen_with("cleared-client-tools", |next| {
            next.tools =
                Some(awaken_session_configuration_contract::SessionToolConfiguration::default());
        });
    let cleared = host
        .run_assembly
        .ctx_for("cleared-client-tools", Some("assistant"))
        .await
        .expect("R2 build explicit empty Session tool surface");
    assert!(
        cleared
            .config
            .resolved_spec
            .tool_descriptors
            .iter()
            .all(|tool| tool.kind != awaken_runtime_contract::resolved::ToolKind::ClientExecuted),
        "R2/E2"
    );

    let lookup = awaken_agent_contract::ClientToolDescriptor {
        name: "lookup".into(),
        description: "Look up the requested city".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
            "additionalProperties": false
        }),
    };
    host.installed_sessions
        .replace_frozen_with("session-client-tools", |next| {
            next.tools = Some(
                awaken_session_configuration_contract::SessionToolConfiguration {
                    toolsets: Vec::new(),
                    client_tools: vec![lookup.clone()],
                },
            )
        });
    let context = host
        .run_assembly
        .ctx_for("session-client-tools", Some("assistant"))
        .await
        .expect("R3 build exact Session tool surface");
    let client_tools = context
        .config
        .resolved_spec
        .tool_descriptors
        .iter()
        .filter(|tool| tool.kind == awaken_runtime_contract::resolved::ToolKind::ClientExecuted)
        .collect::<Vec<_>>();
    assert_eq!(client_tools.len(), 1, "R3/E2");
    assert_eq!(client_tools[0].id, lookup.name, "R3/E2");
    assert_eq!(client_tools[0].description, lookup.description, "R3/E2");
    assert_eq!(
        client_tools[0].model_parameters(),
        lookup.input_schema,
        "R3/E2"
    );
    assert!(
        publication
            .resolved_spec
            .tool_descriptors
            .iter()
            .any(|tool| {
                tool.id == "published_lookup"
                    && tool.kind == awaken_runtime_contract::resolved::ToolKind::ClientExecuted
            }),
        "R2+R3/E1"
    );

    let result = host
        .run(
            Some("assistant"),
            "session-client-tools",
            user("look it up"),
        )
        .await
        .expect("R3 model calls Session client tool");
    let pending = result.pending.expect("R3 awaits lookup result");
    assert_eq!(pending.name, "lookup", "R3/E3");
    assert!(pending.client_executed, "R3/E3");
}

#[tokio::test]
async fn cold_session_uses_its_frozen_agent_projection_for_internal_history_reads() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Causes: C1 a cold Session has a frozen non-default Agent projection and an
    // internal history read carries no repeated Agent argument; C2 the caller
    // repeats the same Agent; C3 it asserts a different Agent. Effects: E1/E2
    // resolve the exact publication; C4 the frozen Agent has no optional
    // publication; E3 fail closed before constructing Runtime; E4 generate the
    // fallback snapshot with the frozen Agent identity rather than `assistant`;
    // C5 a provider-driven re-entry supplies only its already delivered exact
    // publication; E5 derive the selected Agent from that publication instead
    // of manufacturing a parallel default identity.
    //
    // Decision table:
    // | rule | projected Agent | requested Agent | result                  |
    // | R1   | agent-a         | absent          | exact agent-a snapshot  |
    // | R2   | agent-a         | agent-a         | exact agent-a snapshot  |
    // | R3   | agent-a         | assistant       | projection mismatch     |
    // | R4   | built-in        | absent          | fallback id is built-in |
    // | R5   | absent          | absent+delivered| delivered root identity |
    let snapshot = crate::config::server_config(
        "agent-a",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
    .expect("literal model reference");
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("valid publication");
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_agent_publications(Arc::new(publications)))
    .build()
    .expect("validated Host dependencies");
    install_test_session_application(&host);
    install_test_dispatch_runtime(&host)
        .host
        .run_assembly
        .install_test_session_init(
            "cold-agent",
            awaken_session_configuration_contract::SessionInit {
                workspace_id: host.run_assembly.local_workspace().into(),
                agent_id: "agent-a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: Some("stub".into()),
                runtime: Some("default".into()),
                environment: on_tool_use_environment(),
            },
        )
        .await
        .expect("project the frozen Session baseline");

    let recovered = host
        .run_assembly
        .ctx_for("cold-agent", None)
        .await
        .expect("R1 internal history read resolves the frozen Agent");
    assert_eq!(recovered.config.root_agent_id.0, "agent-a", "R1");

    host.run_assembly
        .register_thread_agent_projection("same-agent", "agent-a");
    host.run_assembly
        .ctx_for("same-agent", Some("agent-a"))
        .await
        .expect("R2 repeated exact Agent");

    host.run_assembly
        .register_thread_agent_projection("mismatch-agent", "agent-a");
    let error = match host
        .run_assembly
        .ctx_for("mismatch-agent", Some("assistant"))
        .await
    {
        Ok(_) => panic!("R3 accepted a different requested Agent"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("projection"), "R3");

    let fallback_host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    install_test_session_application(&fallback_host);
    install_test_dispatch_runtime(&fallback_host)
        .host
        .run_assembly
        .install_test_session_init(
            "cold-built-in",
            awaken_session_configuration_contract::SessionInit {
                workspace_id: fallback_host.local_workspace().into(),
                agent_id: "built-in-agent".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: Some("stub".into()),
                runtime: Some("default".into()),
                environment: on_tool_use_environment(),
            },
        )
        .await
        .expect("R4 project optional unpublished Agent baseline");
    let fallback = fallback_host
        .run_assembly
        .ctx_for_session_coordination("cold-built-in", None)
        .await
        .expect("R4 build the generated fallback snapshot");
    assert_eq!(fallback.config.root_agent_id.0, "built-in-agent", "R4/E4");

    let delivered = crate::config::server_config(
        "delivered-agent",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
    .expect("literal model reference");
    let (_, selected_agent, selected, _) = fallback_host
        .realization_executor
        .environments()
        .publication()
        .resolve_session_publication("delivered-agent-thread", None, Some(delivered.clone()))
        .expect("R5 use the delivered publication as the retained identity authority");
    assert_eq!(selected_agent, "delivered-agent", "R5/E5");
    assert_eq!(selected, Some(delivered), "R5/E5");
}

#[tokio::test]
async fn frozen_session_resolves_its_exact_agent_revision_instead_of_current() {
    // Cause/effect graph: C1 the publication catalog retains exact revision 2;
    // C2 revision 3 is current; C3 the Session baseline is frozen at revision 2.
    // The Run activation must use revision 2. Resolving current would create a
    // second publication authority and make the Worker reject the Session.
    //
    // | Rule | frozen revision | current revision | effect |
    // |---|---:|---:|---|
    // | R1 | 2 | 3 | resolve exact revision 2 |
    // | R2 | absent | 3 | retain legacy current lookup |
    fn publication(revision: u64, instructions: &str) -> ExecutableAgentSnapshot {
        let mut snapshot = crate::config::server_config(
            "agent-a",
            "stub",
            &HashSet::new(),
            &HashSet::new(),
            &[],
            &Default::default(),
            &[],
            awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        )
        .expect("literal model reference");
        snapshot.metadata.source.agent_id = snapshot.root_agent_id.clone();
        snapshot.metadata.source.revision = revision;
        snapshot.resolved_spec.instructions = instructions.into();
        snapshot.recompute_fingerprint().unwrap();
        snapshot
    }

    let frozen = publication(2, "frozen");
    let current = publication(3, "current");
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([
        frozen.clone(),
        current.clone(),
    ])
    .expect("valid revisioned publications");
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_agent_publications(Arc::new(publications)))
    .build()
    .expect("validated Host dependencies");
    let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
        awaken_session_configuration_contract::SessionBaselineInputs {
            environment: on_tool_use_environment(),
            runtime_placement: awaken_session_types::SessionRuntimePlacement::Local,
            mcp_authoring: Default::default(),
            agent_id: "agent-a".into(),
            agent_revision: Some(2),
            model: "stub".into(),
            model_override: None,
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    host.run_assembly
        .install_frozen_session_projection(
            "frozen-revision",
            awaken_session_realization_contract::FrozenSessionProjection {
                workspace_id: host.run_assembly.local_workspace().to_string(),
                revision: awaken_session_types::SessionRevision(1),
                environment: Default::default(),
                resource_revision: 0,
                resources: Default::default(),
                previous_resource_manifest: Some(
                    crate::session_resource::SessionResourceManifest::new(
                        host.run_assembly.local_workspace(),
                        crate::session_resource::ResolvedSessionResources::default(),
                    ),
                ),
                tools: Default::default(),
                mcp: Vec::new(),
                request_context: Vec::new(),
                agent_publication: None,
                environment_materialization: None,
                baseline,
            },
            None,
            true,
            None,
        )
        .await
        .expect("frozen baseline");

    let (_, _, selected, _) = host
        .realization_executor
        .environments()
        .publication()
        .resolve_session_publication("frozen-revision", None, None)
        .expect("R1 exact frozen publication");
    assert_eq!(selected, Some(frozen), "R1");

    let (_, _, selected, _) = host
        .realization_executor
        .environments()
        .publication()
        .resolve_session_publication("legacy-current", Some("agent-a"), None)
        .expect("R2 legacy current publication");
    assert_eq!(selected, Some(current), "R2");
}

#[tokio::test]
async fn frozen_projection_replaces_an_inactive_default_runtime_context() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a durable-thread operation may open a context before
    // the frozen projection is installed; C2 that context is inactive or active;
    // C3 the later projection selects the default or a published non-default
    // Agent. Effects: E1 inactive cached defaults are discarded and rebuilt from
    // the frozen Agent; E2 an active activation is never rebound; E3 an already
    // prepared context remains rebuildable from the same immutable facts.
    //
    // | Rule | resident context | active run | frozen Agent | effect |
    // |---|---|---|---|---|
    // | P1 | default | no | published agent-a | evict; rebuild agent-a |
    // | P2 | default | yes | agent-a | reject projection install |
    // | P3 | absent/matching | no | same baseline | install/rebuild safely |
    // | P4 | frozen matching | yes | same baseline | reuse; successor queues |
    //
    // P1 and P2 are the distributed authority-transition regressions exercised
    // here. P3 is covered by
    // `cold_session_uses_its_frozen_agent_projection_for_internal_history_reads`
    // and the idempotent projection tests above. P4 prevents an approval event
    // racing the preceding Run's terminal cleanup from rebinding or rejecting
    // the already-frozen Runtime. FMECA: rejecting P4 loses the approved input;
    // rebuilding it risks changing Hand/MCP effects under an active run.
    let snapshot = crate::config::server_config(
        "agent-a",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
    .expect("literal model reference");
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("valid publication");
    let mut host = crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_agent_publications(Arc::new(publications));
    host.execution.local = crate::LocalExecution::RemoteWorkersOnly;
    let host = (host).build().expect("validated Host dependencies");
    install_test_session_application(&host);

    let stale = host
        .run_assembly
        .ctx_for("late-projection", None)
        .await
        .expect("pre-projection durable operation can open a default context");
    assert_eq!(stale.config.root_agent_id.0, "assistant", "P1 precondition");

    (host.clone())
        .run_assembly
        .install_test_session_init(
            "late-projection",
            awaken_session_configuration_contract::SessionInit {
                workspace_id: host.run_assembly.local_workspace().into(),
                agent_id: "agent-a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: Some("stub".into()),
                runtime: Some("default".into()),
                environment: on_tool_use_environment(),
            },
        )
        .await
        .expect("P1 installs the frozen projection");
    assert!(
        host.installed_sessions
            .read_run("late-projection", |run| run.context.is_none())
            .unwrap_or(false),
        "P1 stale context must not survive the authority transition"
    );

    let rebuilt = host
        .run_assembly
        .ctx_for("late-projection", None)
        .await
        .expect("P1 rebuilds from the frozen publication");
    assert_eq!(rebuilt.config.root_agent_id.0, "agent-a", "P1/E1");

    let active = host
        .run_assembly
        .ctx_for("active-projection", None)
        .await
        .expect("P2 pre-projection context");
    active
        .foreground
        .install_for_test(Some(RunId("active-run".into())));
    let error = (host.clone())
        .run_assembly
        .install_test_session_init(
            "active-projection",
            awaken_session_configuration_contract::SessionInit {
                workspace_id: host.run_assembly.local_workspace().into(),
                agent_id: "agent-a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: Some("stub".into()),
                runtime: Some("default".into()),
                environment: on_tool_use_environment(),
            },
        )
        .await
        .expect_err("P2 must not rebind an active Runtime");
    assert!(
        error.to_string().contains("while its Runtime is active"),
        "P2/E2"
    );
    assert!(
        host.run_assembly
            .thread_agent_projection("active-projection")
            .is_none(),
        "P2 rejection precedes every projection mutation"
    );

    let frozen_init = awaken_session_configuration_contract::SessionInit {
        workspace_id: host.run_assembly.local_workspace().into(),
        agent_id: "agent-a".into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: Default::default(),
        model: Some("stub".into()),
        runtime: Some("default".into()),
        environment: on_tool_use_environment(),
    };
    (host.clone())
        .run_assembly
        .install_test_session_init("active-frozen-projection", frozen_init.clone())
        .await
        .expect("P4 initial frozen coordinates");
    let frozen_active = host
        .run_assembly
        .ctx_for("active-frozen-projection", None)
        .await
        .expect("P4 context built after frozen authority");
    frozen_active
        .foreground
        .install_for_test(Some(RunId("frozen-active-run".into())));
    (host.clone())
        .run_assembly
        .install_test_session_init("active-frozen-projection", frozen_init)
        .await
        .expect("P4 reuses the immutable active projection");
    let retained = host
        .installed_sessions
        .read_run("active-frozen-projection", |run| {
            Arc::ptr_eq(run.context.as_ref().expect("P4 runtime"), &frozen_active)
        })
        .unwrap_or(false);
    assert!(retained, "P4 keeps the one resident Runtime");
}

#[tokio::test]
async fn live_inbox_is_advertised_only_for_a_locally_reachable_active_attempt() {
    // FMECA cause/effect graph: C1 a direct or durable-local physical attempt has
    // opened its exact scope; C2 the scope supports safe-boundary live input; C3
    // the exact scope has settled; C4 a Coordinator-only context exists. Effects:
    // E1 C1+C2 exposes that scope's process-local inbox; E2 no current scope stays
    // inactive; E3 settled and remote attempts fail closed.
    // Constraint: Runtime's generation/ownership-fenced active-attempt registry is
    // the sole discovery and lifecycle authority; no Session/ingress queue exists.
    //
    // | Rule | topology | attempt scope | effect |
    // |---|---|---|---|
    // | L1 | direct idle | absent | E2 inactive |
    // | L2 | direct Runtime | current | E1 exact inbox |
    // | L3 | direct settled | removed/closed | E2+E3 inactive |
    // | L4 | pool-owned Event | current | E1 exact inbox |
    // | L5 | Event settled | removed/closed | E3 inactive |
    // | L6 | durable local Worker | current | E1 exact inbox |
    // | L7 | Coordinator-only | absent | E3 inactive |
    // Decision rule: execute L1-L7; only a current registry entry may produce E1.
    let direct = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let _direct_managed = install_test_dispatch_runtime(&direct);
    let direct_ctx = direct
        .run_assembly
        .ctx_for("direct-live", None)
        .await
        .expect("L1 direct context");
    assert!(
        !direct_ctx.delivery.is_durable(),
        "L1 direct foreground precondition"
    );
    assert!(
        direct
            .live_inboxes
            .live_inbox("direct-live")
            .await
            .is_none(),
        "L1/E2"
    );

    let direct_tracking = direct_ctx.runtime.begin_active_attempt(
        &RunId("run-direct".into()),
        &direct_ctx.thread_id,
        awaken_runtime_contract::RuntimeRunContext::new(),
        awaken_runtime_contract::execution::LiveInput::SafeBoundary,
    );
    assert!(
        direct
            .live_inboxes
            .live_inbox("direct-live")
            .await
            .is_some(),
        "L2/E1"
    );
    drop(direct_tracking);
    assert!(
        direct
            .live_inboxes
            .live_inbox("direct-live")
            .await
            .is_none(),
        "L3/E2+E3 a settled scope is not a fallback authority"
    );

    let event_tracking = direct_ctx.runtime.begin_active_attempt(
        &RunId("run-session-event".into()),
        &direct_ctx.thread_id,
        awaken_runtime_contract::RuntimeRunContext::new(),
        awaken_runtime_contract::execution::LiveInput::SafeBoundary,
    );
    let event_inbox = event_tracking
        .context()
        .live_inbox
        .clone()
        .expect("L4 event attempt inbox");
    let discovered = direct
        .live_inboxes
        .live_inbox("direct-live")
        .await
        .expect("L4/E1 pool-owned Session Event inbox");
    let _ = discovered.offer(Message::text(
        MessageId("event-live-message".into()),
        Role::User,
        "event",
    ));
    assert_eq!(event_inbox.list().len(), 1, "L4/E1 exact Event inbox");
    drop(event_tracking);
    assert!(
        direct
            .live_inboxes
            .live_inbox("direct-live")
            .await
            .is_none(),
        "L5/E3"
    );

    let mut local_deployment = crate::DeploymentConfig::ephemeral();
    local_deployment.durable = true;
    let local = (crate::HostDependencies::ephemeral_with_deployment(
        Arc::new(OkModel),
        "stub",
        local_deployment,
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let _local_managed = install_test_dispatch_runtime(&local);
    let local_ctx = local
        .run_assembly
        .ctx_for("local-live", None)
        .await
        .expect("L6 context");
    local_ctx
        .foreground
        .install_for_test(Some(RunId("run-local".into())));
    let tracking = local_ctx.runtime.begin_active_attempt(
        &RunId("run-local".into()),
        &local_ctx.thread_id,
        awaken_runtime_contract::RuntimeRunContext::new(),
        awaken_runtime_contract::execution::LiveInput::SafeBoundary,
    );
    assert!(
        local.live_inboxes.live_inbox("local-live").await.is_some(),
        "L6/E1"
    );
    drop(tracking);
    assert!(
        local.live_inboxes.live_inbox("local-live").await.is_none(),
        "L6/E3 settled"
    );

    let mut remote_deployment = crate::DeploymentConfig::ephemeral();
    remote_deployment.durable = true;
    remote_deployment.disable_local_pool = true;
    let remote = (crate::HostDependencies::ephemeral_with_deployment(
        Arc::new(OkModel),
        "stub",
        remote_deployment,
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let remote_ctx = remote
        .run_assembly
        .ctx_for("remote-live", None)
        .await
        .expect("L7 context");
    remote_ctx
        .foreground
        .install_for_test(Some(RunId("run-remote".into())));
    assert!(
        remote
            .live_inboxes
            .live_inbox("remote-live")
            .await
            .is_none(),
        "L7/E3"
    );
}

/// FMECA: FM1 explicit warmup and Session creation use separate preparation
/// registries, causing duplicate work or different readiness truth; FM2 Session
/// creation reaches the provider before cache preparation succeeds.
/// Cause/effect graph:
/// C1=Session spec declares CacheVolume, C2=no eager preparation exists,
/// C3=initializer succeeds, C4=the same identity is explicitly prewarmed later.
/// E1=initialization precedes provider creation, E2=Session creation succeeds,
/// E3=explicit and implicit entry points reuse one successful preparation.
/// Decision rules: (C1,C2,C3)->(E1,E2); (C1,C3,C4)->E3.
#[tokio::test]
async fn session_creation_and_explicit_cache_warmup_share_one_preparation_path() {
    struct RecordingInitializer(AtomicUsize);

    #[async_trait::async_trait]
    impl crate::CacheVolumeInitializer for RecordingInitializer {
        async fn initialize(
            &self,
            _volume: &crate::CacheVolumeWarmup,
        ) -> Result<(), crate::cache_volume::CacheVolumeError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let initializer = Arc::new(RecordingInitializer(AtomicUsize::new(0)));
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_cache_volume_initializer(initializer.clone()))
    .build()
    .expect("validated Host dependencies");
    let mut spec = crate::sandbox_projection::agent_run_sandbox_spec("cache-wiring");
    spec.mounts
        .push(awaken_provisioning_contract::MountRequirement {
            mount_id: "build-cache".into(),
            source: awaken_provisioning_contract::MountSource::CacheVolume {
                location: awaken_provisioning_contract::CacheVolumeLocation::HostPath {
                    path: "/tmp/awaken-cache-volume-wiring".into(),
                },
                key: "build-cache-v1".into(),
            },
            mount_path: "cache-placeholder".into(),
            access: awaken_provisioning_contract::MountAccess::ReadWrite,
            lifetime: awaken_provisioning_contract::MountLifetime::Durable,
            // Workdir cannot bind a host directory; the wiring fixture keeps the
            // provider effect optional. Real bind behavior is covered by the
            // container/namespace substrate tests.
            required: false,
        });

    let environment = host
        .realization_executor
        .environments()
        .create_session_environment(
            host.realization_executor.peers(),
            &host
                .realization_executor
                .environments()
                .providers()
                .session_provider,
            &spec,
        )
        .await
        .expect("cache preparation precedes Session environment creation");
    assert_eq!(initializer.0.load(Ordering::SeqCst), 1, "E1/E2");

    host.environment_capacity()
        .prewarm_cache_volume(crate::CacheVolumeWarmup::host_path(
            "build-cache-v1",
            "/tmp/awaken-cache-volume-wiring",
        ))
        .await
        .expect("same identity is already prepared");
    assert_eq!(initializer.0.load(Ordering::SeqCst), 1, "E3");
    environment
        .lifecycle()
        .dispose()
        .await
        .expect("dispose fixture environment");
}

/// Final-layout/prewarm decision rule: P1 a staged Repository owns
/// `/workspace/repo`; P2 the effective SandboxSpec adds a CacheVolume below that
/// tree. P1+P2 => reject before the shared CacheVolume initializer or provider
/// create edge. This proves the call ordering in addition to the contract's pure
/// overlap matrix.
#[tokio::test]
async fn repository_layout_conflict_fails_before_cache_prewarm() {
    struct CountingInitializer(AtomicUsize);

    #[async_trait::async_trait]
    impl crate::CacheVolumeInitializer for CountingInitializer {
        async fn initialize(
            &self,
            _volume: &crate::CacheVolumeWarmup,
        ) -> Result<(), crate::cache_volume::CacheVolumeError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let initializer = Arc::new(CountingInitializer(AtomicUsize::new(0)));
    let host = (crate::host::with_test_resource_validation(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_cache_volume_initializer(initializer.clone()),
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let mut init = bare_session("agent", host.run_assembly.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "github_repository".into(),
        id: "https://github.com/awaken/prewarm-fence.git".into(),
        mount_path: "/workspace/repo".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    managed
        .host
        .run_assembly
        .install_test_session_init("repository-prewarm-fence", init)
        .await
        .expect("P1 stage Repository without opening an Environment");

    let mut spec = host
        .realization_executor
        .sandbox_spec("repository-prewarm-fence");
    spec.mounts
        .push(awaken_provisioning_contract::MountRequirement {
            mount_id: "conflicting-cache".into(),
            source: awaken_provisioning_contract::MountSource::CacheVolume {
                location: awaken_provisioning_contract::CacheVolumeLocation::HostPath {
                    path: "/tmp/awaken-conflicting-cache".into(),
                },
                key: "conflicting-cache-v1".into(),
            },
            mount_path: "/workspace/repo/cache".into(),
            access: awaken_provisioning_contract::MountAccess::ReadWrite,
            lifetime: awaken_provisioning_contract::MountLifetime::Durable,
            required: false,
        });
    let error = match host
        .realization_executor
        .environments()
        .create_session_environment(
            host.realization_executor.peers(),
            &host
                .realization_executor
                .environments()
                .providers()
                .session_provider,
            &spec,
        )
        .await
    {
        Ok(_) => panic!("P2 must fail before cache preparation"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("overlaps"), "P1/P2: {error}");
    assert_eq!(
        initializer.0.load(Ordering::SeqCst),
        0,
        "P1/P2 no prewarm effect"
    );
}

/// P4 — a Platform-held MCP credential is performed by the deployment's egress
/// mediator, and the Runtime Host never holds the material.
///
/// | Rule | installed mediator | effect |
/// |---|---|---|
/// | G1 | none | refused; no projection |
/// | G2 | one | staged on the mediated route, with no bearer in the Host |
///
/// The access below names a credential that was never stored, so any attempt
/// to resolve material would fail G2 instead of producing a route. That is the
/// property under test: mediation resolves nothing into this process.
#[tokio::test]
async fn platform_held_mcp_is_mediated_without_resolving_material_into_the_host() {
    use awaken_credential_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };

    struct StubMediator {
        url: String,
    }

    #[async_trait::async_trait]
    impl awaken_credential_materializer::PlatformMcpEgressProvisioner for StubMediator {
        async fn provision(
            &self,
            request: awaken_credential_materializer::PlatformMcpEgressRequest<'_>,
        ) -> Result<
            awaken_credential_materializer::MediatedMcpRoute,
            awaken_credential_contract::CredentialMaterialError,
        > {
            assert_eq!(request.holder.boundary, PlaintextBoundary::Platform, "G2");
            assert_eq!(request.session_id, "mcp-platform", "G2");
            assert_eq!(request.runtime_incarnation, "runtime-1", "G2");
            Ok(awaken_credential_materializer::MediatedMcpRoute {
                url: self.url.clone(),
                expires_at_unix_ms: u64::MAX,
            })
        }
    }

    let dependencies = crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await;
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let host = (dependencies.with_credentials(credentials.clone(), secrets.clone()))
        .build()
        .unwrap();
    // The real mediator injects credential material upstream. To this process a
    // mediated route is simply a URL it may call without holding anything, so
    // the stand-in requires no bearer.
    let (mediated_url, _seen) = crate::test_mcp::start(None).await;

    let holder = PlaintextHolder::new(
        PlaintextBoundary::Platform,
        awaken_credential_contract::PLATFORM_EGRESS_GATEWAY_TRUST_DOMAIN,
    );
    let upstream = "https://upstream.invalid/mcp";
    let audience = awaken_agent_contract::McpTarget::identity(upstream)
        .unwrap()
        .canonical_url();
    let generation = crate::session_mcp::McpGenerationRef::from_persisted(
        "mcp-platform".into(),
        crate::session_mcp::McpGeneration::of_bare_target(
            crate::session_mcp::McpAttachmentId("mcp-docs".into()),
            1,
            &awaken_agent_contract::McpTarget::parse_http(upstream).unwrap(),
        ),
        "runtime-1".into(),
        1,
        u64::MAX - 1,
    )
    .expect("MCP generation fence epoch is nonzero");
    let request = crate::session_mcp::StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: generation.clone(),
        realization_id: "realize-platform".into(),
        stage_idempotency_key: "stage-platform".into(),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http(upstream).unwrap(),
        prompts_as_skills: false,
        credential: Some(
            CredentialAccess::new(
                CredentialRef {
                    id: "credential-never-stored".into(),
                    revision: 7,
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::VirtualOnly),
            )
            .with_target(awaken_runtime_contract::CredentialTarget::new(
                awaken_credential_contract::CredentialPurpose::McpAuthorization,
                audience,
            )),
        ),
        selected_plaintext_holder: Some(holder),
    };

    let bare = crate::ManagedHost::new(host.clone());
    assert_eq!(
        bare.host
            .run_assembly
            .stage_direct_mcp_attachment(request.clone())
            .await
            .unwrap_err()
            .code,
        "mcp_gateway_provisioner_required",
        "G1"
    );
    assert!(
        host.run_assembly.mcp_projection(&generation).is_none(),
        "G1"
    );

    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_credentials(credentials, secrets)
        .with_session_execution(
            crate::SessionExecutionDependencies::default().with_platform_mcp_egress_provisioner(
                Arc::new(StubMediator {
                    url: mediated_url.clone(),
                }),
            ),
        ))
    .build()
    .unwrap();
    let managed = crate::ManagedHost::new(host.clone());
    let receipt = managed
        .host
        .run_assembly
        .stage_direct_mcp_attachment(request)
        .await
        .expect("G2 mediated staging resolves no material");
    assert_eq!(
        receipt.actual_realization_kind,
        Some(awaken_credential_contract::CredentialRealizationKind::PlatformRelay),
        "G2"
    );
    let server = host
        .run_assembly
        .mcp_projection(&generation)
        .expect("G2 projection")
        .server
        .expect("G2 transport");
    assert_eq!(server.http_url(), Some(mediated_url.as_str()), "G2");
    assert!(
        server.bearer().is_none(),
        "G2 the Host must hold no upstream material on a mediated route"
    );
}

// The executor components must supply every required role at construction. A
// missing role is a type error before a Coordinator can accept this executor;
// each component-owned role is the component itself, not a forwarding adapter.
#[tokio::test]
async fn executor_components_supply_every_session_role() {
    let shared = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let host = crate::ManagedHost::new(shared.clone());
    let roles = crate::session_execution_roles(&host);
    assert!(Arc::ptr_eq(
        &(shared.thread_reads().clone()
            as Arc<dyn awaken_session_execution_contract::ThreadReadRuntime>),
        &roles.thread_reads
    ));
    assert!(Arc::ptr_eq(
        &(shared.live_inboxes().clone()
            as Arc<dyn awaken_session_execution_contract::LiveInboxRuntime>),
        &roles.live_inbox
    ));
    let bindings = awaken_session_execution_contract::SessionExecutor::from(roles);
    assert_eq!(bindings.core().model(), host.host.run_assembly.model());
}
