use super::*;

struct AdmissionEnvironment;

struct ProfiledAgent {
    unavailable: bool,
}

struct SubstitutingProfileSource;

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for SubstitutingProfileSource {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        _agent_id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        None
    }

    fn session_profile_at_revision_in(
        &self,
        _workspace_id: &str,
        _agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        Some(
            awaken_executable_agent_contract::ExecutableAgentSessionProfile {
                source_revision: source_revision.saturating_add(1),
                model: Some("substituted-current-model".into()),
                ..Default::default()
            },
        )
    }
}

struct FixedModelPublication {
    publication: awaken_session_contract::SessionModelPublication,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionModelPublicationResolver for FixedModelPublication {
    async fn resolve_session_model(
        &self,
        _workspace_id: &str,
        _model_reference: &str,
    ) -> Result<
        awaken_session_contract::SessionModelPublication,
        awaken_session_contract::SessionModelResolutionError,
    > {
        Ok(self.publication.clone())
    }
}

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for ProfiledAgent {
    fn session_profile_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        (workspace_id == "workspace" && agent_id == "profiled").then(|| {
            awaken_executable_agent_contract::ExecutableAgentSessionProfile {
                model: Some("published-model".into()),
                execution_model_ref: Some("execution-model".into()),
                mcp_servers: vec![
                    awaken_executable_agent_contract::ExecutableAgentMcpServer {
                        name: "agent-only".into(),
                        target: awaken_session_contract::McpTarget::parse_http(
                            "https://agent-only.example.test/mcp",
                        )
                        .unwrap(),
                        prompts_as_skills: false,
                        credential_source_id: None,
                        credential_revision: None,
                    },
                    awaken_executable_agent_contract::ExecutableAgentMcpServer {
                        name: "shared".into(),
                        target: awaken_session_contract::McpTarget::parse_http(
                            "https://agent-shared.example.test/mcp",
                        )
                        .unwrap(),
                        prompts_as_skills: false,
                        credential_source_id: None,
                        credential_revision: None,
                    },
                ],
                ..Default::default()
            }
        })
    }

    fn session_profile_at_revision_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        (workspace_id == "workspace" && agent_id == "profiled" && matches!(source_revision, 7 | 9))
            .then(
                || awaken_executable_agent_contract::ExecutableAgentSessionProfile {
                    source_revision,
                    model: Some("historical-model".into()),
                    execution_model_ref: Some("historical-execution-model".into()),
                    ..Default::default()
                },
            )
    }

    fn executable_snapshot_at_revision_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        (workspace_id == "workspace" && agent_id == "profiled" && source_revision == 7).then(|| {
            let mut snapshot =
                awaken_runtime_contract::ExecutableAgentSnapshot::builder("profiled")
                    .fingerprint("profiled-revision-7")
                    .build();
            snapshot.metadata.source.revision = 7;
            snapshot
        })
    }

    fn agent_unavailable_in(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.unavailable && workspace_id == "workspace" && agent_id == "profiled"
    }
}

#[async_trait::async_trait]
impl SessionEnvironmentSource for AdmissionEnvironment {
    async fn get(
        &self,
        _environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError> {
        Ok(None)
    }

    async fn resolve_current_for_session(
        &self,
        environment_id: &str,
        _runtime: Option<&str>,
        _mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
        let mut snapshot = persisted("environment-template", false, "idle")
            .frozen_baseline()
            .expect("fixture baseline")
            .environment
            .clone();
        snapshot.environment_id = if environment_id == "substitute-me" {
            "another-environment".into()
        } else {
            environment_id.to_owned()
        };
        Ok(Some(ResolvedSessionEnvironment { snapshot }))
    }

    async fn resolve_exact_for_session(
        &self,
        environment_id: &str,
        _revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
        self.resolve_current_for_session(environment_id, runtime, mcp_targets)
            .await
    }

    async fn enqueue_session_work(
        &self,
        _environment_id: &str,
        _session_id: &str,
    ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError> {
        unreachable!("local admission does not dispatch WorkQueue work")
    }

    async fn wake_session_work(
        &self,
        _environment_id: &str,
        _session_id: &str,
    ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError> {
        unreachable!("local admission does not wake WorkQueue work")
    }

    async fn retire_session_work(
        &self,
        _environment_id: &str,
        _session_id: &str,
    ) -> Result<
        Option<awaken_session_contract::work_queue::WorkItem>,
        awaken_session_contract::work_queue::WorkQueueError,
    > {
        unreachable!("local admission does not retire WorkQueue work")
    }

    async fn acquire_session_work(
        &self,
        _environment_id: &str,
        _session_id: &str,
        _worker_owner: &str,
        _now_ms: u64,
    ) -> Result<
        Option<awaken_session_contract::work_queue::SessionWorkLease>,
        awaken_session_contract::work_queue::WorkQueueError,
    > {
        Ok(None)
    }
}

fn creation_command(session_id: &str) -> CreateSessionCommand {
    let environment = persisted("environment-template", true, "idle")
        .frozen_baseline()
        .expect("fixture baseline")
        .environment
        .clone();
    CreateSessionCommand {
        owner_scope: "workspace".into(),
        session_id: session_id.into(),
        intent: awaken_session_contract::SessionCreationIntent {
            control: awaken_session_contract::ControlSessionCreationInputs {
                environment,
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                agent_id: "agent".into(),
                agent_revision: None,
                model_override: None,
                model: "model".into(),
                execution_model_ref: "model".into(),
                runtime: None,
                mcp_authoring: Default::default(),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
                resources: Default::default(),
                initial_mcp: Vec::new(),
            },
        },
        title: None,
        metadata: Default::default(),
        tools: Default::default(),
        budget: Default::default(),
    }
}

#[tokio::test]
async fn creation_driver_owns_finalize_realize_and_activation_order() {
    // Cause/effect graph: C1 complete creation inputs are present; C2 the frozen
    // placement is local; C3 durable insertion succeeds; C4 realization is
    // acknowledged. Effects: E1 finalization precedes realization, E2 the
    // Session becomes Idle, E3 one repository owner exists, and E4 one durable
    // initial-idle fact is emitted. Decision rule R1 C1+C2+C3+C4 => E1-E4.
    // FMECA: emitting E4 at intent insertion
    // creates a false-ready fact if realization later fails, severity 9,
    // occurrence 4, detection 7; the acknowledgement boundary removes that
    // failure mode. This proves protocols cannot reorder the shared sequence.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let app = application(
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let active = app
        .create_session(creation_command("active"))
        .await
        .expect("R1");
    assert!(active.frozen_baseline().is_some(), "R1/E1");
    assert_eq!(
        active.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "R1/E1"
    );

    assert_eq!(
        repository.owner("active").await.unwrap(),
        "workspace",
        "R1/E3"
    );
    let pending = repository.pending_lifecycle().await.unwrap();
    assert_eq!(pending.len(), 1, "R1/E4");
    assert_eq!(pending[0].object_id, "active", "R1/E4");
    assert_eq!(pending[0].event_type, "session.status_idled", "R1/E4");
}

#[tokio::test]
async fn self_hosted_work_dispatch_is_durable_but_not_a_fabricated_readiness_ack() {
    // Environment WorkQueue FMECA/cause-effect graph. C1 the frozen Environment
    // is self-hosted; C2 Runtime placement is a registered Worker; C3 the
    // idempotent WorkQueue projection succeeds; C4 no external worker has yet
    // acknowledged physical realization. Effects: E1 create returns the
    // durable Preparing projection without waiting for an acknowledgement that
    // this protocol does not carry; E2 exactly one stable Session work item is
    // queued; E3 no false Idle lifecycle fact is committed. Treating C3 as C4
    // previously caused every split-process create to time out (S9/O10/D2).
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | W1   | T  | T  | T  | F  | E1+E2+E3 |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let app = application_with_configuration(
        repository.clone(),
        environments.clone(),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let mut command = creation_command("external-create");
    command.intent.control.runtime_placement =
        awaken_session_contract::SessionRuntimePlacement::Worker;

    let created = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.create_session(command),
    )
    .await
    .expect("W1/E1 create does not invent a physical-ack barrier")
    .expect("W1/E1 durable create succeeds");
    assert_eq!(
        created.execution,
        awaken_session_contract::SessionExecutionState::Preparing,
        "W1/E1+E3"
    );
    assert_eq!(
        environments
            .dispatched
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        ["external-create".to_string()],
        "W1/E2"
    );
    assert!(
        repository.pending_lifecycle().await.unwrap().is_empty(),
        "W1/E3"
    );
}

#[tokio::test]
async fn creation_persists_only_a_complete_frozen_root_before_later_cas_failure() {
    /* Cause/effect graph: C1 all creation inputs compile; C2 the first root
     * insert commits; C3 the later activation CAS is unavailable. Effects: E1
     * create reports retryable failure; E2 durable truth is already Frozen with
     * its Resource/MCP state; E3 no Preparing authoring intent can be stranded.
     * Constraint: physical realization and Work projection remain later,
     * replayable effects.
     *
     * | Rule | Compile | Insert | Later CAS | Result |
     * |---|---|---|---|---|
     * | A1 | pass | pass | fail | E1 + E2 + E3 |
     */
    let durable: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let faulting = Arc::new(FaultingSessionRepository::new(durable.clone()));
    faulting.fail_once("activate");
    let app = application_with_configuration(
        faulting,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let mut command = creation_command("atomic-create");
    command.intent.control.runtime_placement =
        awaken_session_contract::SessionRuntimePlacement::Worker;

    assert!(app.create_session(command).await.is_err(), "A1/E1");
    let committed = durable.get("atomic-create").await.expect("A1/E2");
    assert!(committed.frozen_baseline().is_some(), "A1/E2+E3");
    assert!(committed.resources.pending.is_some(), "A1/E2");
}

#[tokio::test]
async fn session_application_admits_new_and_existing_protocol_threads() {
    // Cause/effect graph: C1 durable thread absent/present; C2 workspace matches
    // the durable owner; C3 Agent is available. Effects: E1 absent creates one
    // frozen Session; E2 present reuses and realizes that same aggregate; E3 an
    // owner mismatch fails before Runtime execution. Decision table: A1 absent+
    // C3 => E1; A2 present+C2 => E2; A3 present+!C2 => E3. This is the concrete
    // application gate shared by AI SDK, AG-UI, and A2A.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let app = application(repository.clone(), Arc::new(AdmissionEnvironment));

    app.admit_run_session("workspace", "thread", "assistant")
        .await
        .expect("A1");
    let created = repository.get("thread").await.expect("A1/E1");
    assert!(created.frozen_baseline().is_some(), "A1/E1");
    assert_eq!(created.agent_id(), Some("assistant"), "A1/E1");

    app.admit_run_session("workspace", "thread", "ignored")
        .await
        .expect("A2/E2");
    let error = app
        .admit_run_session("other-workspace", "thread", "ignored")
        .await
        .expect_err("A3/E3");
    assert_eq!(
        error.kind,
        awaken_session_contract::RunErrorKind::BadRequest,
        "A3/E3"
    );
}

/// Profiled-Session FMECA and cause/effect graph. Failure modes are FM1 a
/// requested model bypasses the published Agent, FM2 an unavailable Agent is
/// admitted. Causes: C1 profile exists, C2 Agent available, C3 requested model
/// absent/equal, C4 requested model differs, C5 complete local inputs are
/// supplied up front, C6 Agent and Session MCP candidates are distinct or
/// overlap by name, and C7 equal-origin names conflict. Effects: E1 freeze the
/// published execution identity and local inputs, E2 reject without a row, E3
/// retain Agent-only and Session-only MCP while Session overrides Agent by
/// logical name. Graph: C1&&C2&&C3&&C5&&C6 -> E1+E3; C4||!C2||C7 -> E2.
///
/// | Rule | Profile | Available | Requested model | Product MCP | Effect |
/// |---|---|---|---|---|---|
/// | P1 | yes | yes | absent/equal | distinct + Agent overlap | E1 + E3 |
/// | P2 | yes | yes | different | any | E2 |
/// | P3 | yes | no | any | any | E2 |
/// | P4 | yes | yes | absent/equal | duplicate Session name | E2 |
#[tokio::test]
async fn profiled_session_creation_enforces_publication_and_upfront_inputs() {
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("profiled Session repository"),
    );
    let mut available = application(repository.clone(), Arc::new(AdmissionEnvironment));
    available.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));
    let command = |session_id: &str, model: Option<&str>| CreateProfiledSessionCommand {
        owner_scope: "workspace".into(),
        session_id: session_id.into(),
        agent_id: "profiled".into(),
        source_revision: None,
        environment_id: None,
        model: model.map(str::to_owned),
        mounts: vec![serde_json::json!({"mount_id": "workspace"})],
        env: vec![serde_json::json!({"name": "PROJECT"})],
        prompts: vec!["project context".into()],
        mcp_candidates: vec![
            McpAttachmentCandidate {
                name: "session-only".into(),
                target: McpAttachmentCandidateTarget::HttpUrl(
                    "https://session-only.example.test/mcp".into(),
                ),
                prompts_as_skills: false,
                published_credential: None,
                origin: awaken_session_contract::McpAttachmentOrigin::Session,
            },
            McpAttachmentCandidate {
                name: "shared".into(),
                target: McpAttachmentCandidateTarget::HttpUrl(
                    "https://session-shared.example.test/mcp".into(),
                ),
                prompts_as_skills: false,
                published_credential: None,
                origin: awaken_session_contract::McpAttachmentOrigin::Session,
            },
        ],
        network_restriction: Some(awaken_session_contract::SessionNetworkPolicy::None),
        title: None,
        metadata: Default::default(),
        tools: None,
    };

    let mut explicit_environment = command("profiled-realized", None);
    explicit_environment.environment_id = Some("project-environment".into());
    let realized = available
        .create_profiled_session(explicit_environment)
        .await
        .expect("P1");
    assert_eq!(realized.model(), Some("published-model"), "P1/E1");
    assert_eq!(
        realized.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "P1/E1"
    );
    let baseline = realized.frozen_baseline().expect("P1 frozen baseline");
    assert_eq!(baseline.mounts.len(), 1, "P1/E1");
    assert_eq!(baseline.env.len(), 1, "P1/E1");
    assert_eq!(baseline.prompts, ["project context"], "P1/E1");
    assert_eq!(
        baseline.environment.environment_id, "project-environment",
        "P1/E1"
    );

    let mut substituted_environment = command("profiled-environment-substitution", None);
    substituted_environment.environment_id = Some("substitute-me".into());
    assert!(
        available
            .create_profiled_session(substituted_environment)
            .await
            .is_err(),
        "an Environment resolver cannot substitute another identity"
    );
    assert!(
        matches!(
            repository.get("profiled-environment-substitution").await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "Environment substitution fails before Session persistence"
    );
    assert_eq!(
        baseline.environment.network,
        awaken_session_contract::SessionNetworkPolicy::None,
        "P1/E1"
    );
    let mcp = realized
        .mcp
        .attachments
        .iter()
        .map(|attachment| {
            (
                attachment.name.as_str(),
                attachment.target.http_url().expect("HTTP MCP target"),
                attachment.origin,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        mcp,
        [
            (
                "agent-only",
                "https://agent-only.example.test/mcp",
                awaken_session_contract::McpAttachmentOrigin::Agent,
            ),
            (
                "session-only",
                "https://session-only.example.test/mcp",
                awaken_session_contract::McpAttachmentOrigin::Session,
            ),
            (
                "shared",
                "https://session-shared.example.test/mcp",
                awaken_session_contract::McpAttachmentOrigin::Session,
            ),
        ],
        "P1/E3"
    );

    let mut historical_command = command("profiled-historical", None);
    historical_command.source_revision = Some(7);
    let historical = available
        .create_profiled_session(historical_command)
        .await
        .expect("exact historical publication");
    assert_eq!(historical.model(), Some("historical-model"));
    assert_eq!(
        historical
            .frozen_baseline()
            .expect("historical baseline")
            .agent_revision,
        Some(7),
        "the requested revision, not current or zero, is frozen"
    );
    // Exact-publication projection decision table:
    // | baseline revision | catalog snapshot | placement | effect |
    // | exact             | exact            | any       | project snapshot |
    // | exact             | missing          | Worker    | fail before effect |
    // The first assertion proves the sole profile source supplies the complete
    // immutable publication; P2 below covers the missing-snapshot Worker rule.
    let historical_projection = available
        .frozen_session_projection("workspace".into(), &historical, true)
        .await
        .expect("exact historical projection");
    assert_eq!(
        historical_projection
            .agent_publication
            .as_ref()
            .map(|snapshot| snapshot.fingerprint.0.as_str()),
        Some("profiled-revision-7")
    );

    let mut missing_command = command("profiled-missing", None);
    missing_command.source_revision = Some(8);
    assert!(
        available
            .create_profiled_session(missing_command)
            .await
            .is_err(),
        "an unproven exact publication fails closed"
    );

    let mut substituting = application(repository.clone(), Arc::new(AdmissionEnvironment));
    substituting.set_config_source(Arc::new(SubstitutingProfileSource));
    let mut substituted_command = command("profiled-substituted-revision", None);
    substituted_command.source_revision = Some(7);
    assert!(
        substituting
            .create_profiled_session(substituted_command)
            .await
            .is_err(),
        "a profile source cannot substitute another revision"
    );
    assert!(
        matches!(
            repository.get("profiled-substituted-revision").await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "revision substitution must fail before Session persistence"
    );

    let mut worker = application_with_configuration(
        repository.clone(),
        Arc::new(AdmissionEnvironment),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    worker.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));
    let mut profile_without_snapshot = command("profiled-worker-missing-snapshot", None);
    profile_without_snapshot.source_revision = Some(9);
    assert!(
        worker
            .create_profiled_session(profile_without_snapshot)
            .await
            .is_err(),
        "P2: a Worker effect cannot start without the complete immutable publication"
    );

    // Override-projection cause/effect table:
    // | Rule | Agent snapshot | Override publication | Effect |
    // | O1 | exact revision | complete | replace the whole candidate roster and identity |
    // | O2 | exact revision | none/equal | preserve the exact Agent snapshot (above) |
    // O1 also proves the original Agent route cannot leak into the derived
    // executable snapshot and all three fingerprint fields move together.
    let primary = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
        awaken_runtime_contract::resolved::ModelBinding::new(
            "override-account",
            "override-upstream",
            "acp:claude",
        ),
    );
    let fallback = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
        awaken_runtime_contract::resolved::ModelBinding::new(
            "override-fallback",
            "override-upstream",
            "genai",
        ),
    );
    let mut override_application = application(repository.clone(), Arc::new(AdmissionEnvironment));
    override_application.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));
    override_application.set_model_publication_resolver(Arc::new(FixedModelPublication {
        publication: awaken_session_contract::SessionModelPublication {
            primary: primary.clone(),
            candidates: vec![fallback.clone()],
        },
    }));
    let mut override_command = command("profiled-override", Some("override-public-id"));
    override_command.source_revision = Some(7);
    let overridden = override_application
        .create_profiled_session(override_command)
        .await
        .expect("O1 admitted override");
    let override_projection = override_application
        .frozen_session_projection("workspace".into(), &overridden, true)
        .await
        .expect("O1 frozen projection");
    let snapshot = override_projection
        .agent_publication
        .expect("O1 executable Agent snapshot");
    assert_eq!(snapshot.resolved_spec.model_binding, primary, "O1");
    assert_eq!(snapshot.resolved_spec.model_candidates, [fallback], "O1");
    assert_eq!(
        snapshot.fingerprint, snapshot.resolved_spec.catalog_fingerprint,
        "O1"
    );
    assert_eq!(
        snapshot.fingerprint.0, snapshot.metadata.fingerprint.0,
        "O1"
    );
    assert_ne!(snapshot.fingerprint.0, "profiled-revision-7", "O1");
    assert!(
        snapshot
            .resolved_spec
            .execution_candidates(None)
            .into_iter()
            .all(|candidate| candidate
                .binding
                .provider_identity_ref
                .starts_with("override-")),
        "O1 original Agent route must be absent"
    );

    let mismatched = available
        .create_profiled_session(command("profiled-mismatch", Some("unpublished-model")))
        .await;
    assert!(mismatched.is_err(), "P2/E2");
    assert!(
        matches!(
            repository.get("profiled-mismatch").await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "P2/E2"
    );

    let mut unavailable = application(repository.clone(), Arc::new(AdmissionEnvironment));
    unavailable.set_config_source(Arc::new(ProfiledAgent { unavailable: true }));
    assert!(
        unavailable
            .create_profiled_session(command("profiled-unavailable", None))
            .await
            .is_err(),
        "P3/E2"
    );
    assert!(
        matches!(
            repository.get("profiled-unavailable").await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "P3/E2"
    );

    let mut conflicting = command("profiled-mcp-conflict", None);
    conflicting.mcp_candidates.push(McpAttachmentCandidate {
        name: "session-only".into(),
        target: McpAttachmentCandidateTarget::HttpUrl(
            "https://duplicate-session.example.test/mcp".into(),
        ),
        prompts_as_skills: false,
        published_credential: None,
        origin: awaken_session_contract::McpAttachmentOrigin::Session,
    });
    assert!(
        available
            .create_profiled_session(conflicting)
            .await
            .is_err(),
        "P4/E2"
    );
    assert!(
        matches!(
            repository.get("profiled-mcp-conflict").await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "P4/E2"
    );
}
