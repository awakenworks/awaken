use super::*;

#[derive(Default)]
struct RecordingCreateRuntime {
    baseline_installs: AtomicUsize,
    preparations: AtomicUsize,
}

#[async_trait::async_trait]
impl SessionRuntime for RecordingCreateRuntime {
    fn validate_session_sandbox_layout(
        &self,
        _thread: &str,
        _layout: &awaken_session_contract::SessionSandboxLayout,
    ) -> Result<(), RunError> {
        // Test-runtime decision rule: C1 creation tests intentionally model an
        // available provider; C2 layouts may contain Repository bindings. E1 an
        // explicit capable fake admits both, while the production trait default
        // remains fail-closed for C2 when no layout authority is installed.
        Ok(())
    }

    async fn install_session_projection(
        &self,
        _thread: &str,
        _projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        self.baseline_installs.fetch_add(1, Ordering::SeqCst);
        if matches!(
            mode,
            awaken_session_contract::SessionProjectionInstallMode::Dispatch
                | awaken_session_contract::SessionProjectionInstallMode::Realization {
                    prepare_session: true,
                    ..
                }
        ) {
            self.preparations.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        unreachable!("create replay test never runs")
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        unreachable!("create replay test never resumes")
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        unreachable!("create replay test never custom-resumes")
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeDrive, RunError> {
        unreachable!("create replay test never defines an outcome")
    }

    fn model(&self) -> String {
        "create-replay-model".into()
    }
}

fn profiled_mount(id: &str) -> awaken_provisioning_contract::MountRequirement {
    awaken_provisioning_contract::MountRequirement {
        mount_id: id.into(),
        source: awaken_provisioning_contract::MountSource::Inline {
            contents: id.into(),
        },
        mount_path: format!("/workspace/{id}"),
        access: awaken_provisioning_contract::MountAccess::ReadOnly,
        lifetime: awaken_provisioning_contract::MountLifetime::PerRun,
        required: true,
    }
}

fn profiled_env(name: &str) -> awaken_provisioning_contract::EnvVar {
    awaken_provisioning_contract::EnvVar {
        name: name.into(),
        value: awaken_provisioning_contract::EnvValue::Inline { value: "1".into() },
        visibility: awaken_provisioning_contract::EnvVisibility::Process,
    }
}

struct AdmissionEnvironment;

struct ProfiledAgent {
    unavailable: bool,
}

struct ResourceProfiledAgent;

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

struct RecordingModelPublication {
    publication: awaken_session_contract::SessionModelPublication,
    requests: Arc<std::sync::Mutex<Vec<(String, String)>>>,
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

#[async_trait::async_trait]
impl awaken_session_contract::SessionModelPublicationResolver for RecordingModelPublication {
    async fn resolve_session_model(
        &self,
        workspace_id: &str,
        model_reference: &str,
    ) -> Result<
        awaken_session_contract::SessionModelPublication,
        awaken_session_contract::SessionModelResolutionError,
    > {
        self.requests
            .lock()
            .expect("model requests")
            .push((workspace_id.to_string(), model_reference.to_string()));
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
                    .model(awaken_runtime_contract::resolved::ModelBinding::new(
                        "test", "model", "native",
                    ))
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

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for ResourceProfiledAgent {
    fn session_profile_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        let mut profile =
            awaken_executable_agent_contract::ExecutableAgentProfileSource::session_profile_in(
                &ProfiledAgent { unavailable: false },
                workspace_id,
                agent_id,
            )?;
        profile.resources = vec![awaken_resource_contract::InputBinding {
            binding_id: awaken_resource_contract::BindingId::from("agent-resource"),
            target: awaken_resource_contract::InputResourceId::File(
                awaken_resource_contract::FileId::from("agent-file"),
            ),
            mount_path: "/workspace/agent-input".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }];
        Some(profile)
    }
}

fn direct_file_attachment(
    binding_id: &str,
    file_id: &str,
    mount_path: &str,
) -> awaken_session_contract::SessionInputAttachment {
    awaken_session_contract::SessionInputAttachment {
        binding: awaken_resource_contract::InputBinding {
            binding_id: awaken_resource_contract::BindingId::from(binding_id),
            target: awaken_resource_contract::InputResourceId::File(
                awaken_resource_contract::FileId::from(file_id),
            ),
            mount_path: mount_path.into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        },
        replaces: None,
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
                mutation_policy: awaken_session_contract::SessionMutationPolicy::Managed,
                environment,
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                agent_id: "agent".into(),
                agent_revision: None,
                model_override: None,
                system_prompt: awaken_session_contract::SessionSystemPromptSelection::Inherit,
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
        repository_configurations: Vec::new(),
        idempotency: None,
        initial_events: None,
    }
}

#[tokio::test]
async fn worker_checkpoint_retention_is_rejected_before_durable_creation() {
    // Creation-topology decision table: C1 frozen placement is Local/Worker;
    // C2 retention is Resident/CheckpointAndRelease. Only Worker+checkpoint
    // requires a remote continuation transport that does not exist. Effect E1
    // rejects before root insert or Runtime projection; the other three rules
    // retain their existing create paths. Constraint K1: the registered-Worker
    // authorize/persist channel is closed over Create/Adopt/Rebuild/Resource
    // reservation, while claimed resume ends at realization projection/MCP and
    // rejects continuation phases; neither is a checkpoint/restore transport.
    //
    // | Rule | placement | retention | Effect |
    // | T1 | Worker | CheckpointAndRelease | E1 reject, zero root/effect |
    // | T2 | Worker | Resident | existing external creation |
    // | T3 | Local | CheckpointAndRelease | existing local continuation |
    // | T4 | Local | Resident | existing local creation |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Session repository"),
    );
    let runtime = Arc::new(RecordingCreateRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let mut command = creation_command("worker-checkpoint-retention");
    command.intent.control.runtime_placement =
        awaken_session_contract::SessionRuntimePlacement::Worker;
    command.intent.control.environment.idle_retention =
        super::continuation::checkpoint_release_retention();

    let error = app
        .create_session(command)
        .await
        .expect_err("T1/E1 unsupported external continuation must fail before insert");
    let SessionCreationError::Rejected(error) = error else {
        panic!("T1/E1 expected deterministic creation rejection")
    };
    assert_eq!(
        error.kind,
        awaken_session_contract::RunErrorKind::BadRequest,
        "T1/E1",
    );
    assert!(matches!(
        repository.get("worker-checkpoint-retention").await,
        Err(awaken_session_contract::SessionRepositoryError::NotFound)
    ));
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 0, "T1/E1");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 0, "T1/E1");
}

fn owned_repository_input(session_id: &str) -> SessionRepositoryResourceInput {
    SessionRepositoryResourceInput {
        id: format!("managed:{session_id}:repository:0"),
        workspace_id: "workspace".into(),
        name: "Session Repository".into(),
        description: "creation compensation fixture".into(),
        remote_url: "https://github.com/awaken/compensation.git".into(),
        credential_material: None,
        credential: None,
        mount_path: "/workspace/repository".into(),
        initial_branch: Some("main".into()),
        initial_commit: None,
    }
}

#[tokio::test]
async fn root_rejection_compensates_only_applied_repository_participants() {
    // Cause/effect graph: C1 Repository participant is Applied/Replayed; C2 a
    // foreign durable root occupies the Session id without referencing that
    // Repository; C3 exact create replay proves idempotency mismatch. Effects:
    // E1 the caller still receives the first root Conflict; E2 Applied is
    // retired; E3 Replayed remains Active because an earlier command owns it.
    //
    // | Rule | Participant | Root receipt | Root reference | Result | Repository |
    // | C1 | Applied | different | absent | Conflict | Deleted |
    // | C2 | Replayed | different | absent | Conflict | Active |
    for (rule, replayed, expected_state) in [
        (
            "C1",
            false,
            awaken_resource_contract::ResourceState::Deleted,
        ),
        ("C2", true, awaken_resource_contract::ResourceState::Active),
    ] {
        let session_id = format!("creation-participant-{rule}");
        let repository = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("participant repository"),
        );
        let resources = awaken_resource_persistence::ephemeral().expect("Resource authorities");
        let catalog = resources.authorities().resource_registry();
        let mut application = application_with_runtime(
            Arc::new(RecordingCreateRuntime::default()),
            repository.clone(),
            Arc::new(AdmissionEnvironment),
        );
        application.set_resource_registry(catalog.clone());

        let first = application
            .configure_session_repository(owned_repository_input(&session_id))
            .await
            .expect("first participant");
        let configured = if replayed {
            application
                .configure_session_repository(owned_repository_input(&session_id))
                .await
                .expect("exact participant replay")
        } else {
            first
        };
        assert_eq!(
            configured.registry_provenance,
            if replayed {
                SessionParticipantProvenance::Replayed
            } else {
                SessionParticipantProvenance::Applied
            },
            "{rule} precondition"
        );
        create(repository.as_ref(), persisted(&session_id, false, "idle")).await;

        let mut command = creation_command(&session_id);
        command.repository_configurations = vec![configured.clone()];
        assert!(
            matches!(
                application.create_session(command).await,
                Err(SessionCreationError::Conflict)
            ),
            "{rule}/E1"
        );
        assert_eq!(
            catalog
                .find_repository("workspace", configured.repository_id.as_str())
                .unwrap()
                .unwrap()
                .state,
            expected_state,
            "{rule}/E2-E3"
        );
    }
}

#[tokio::test]
async fn model_override_decision_reuses_only_an_equal_id_and_resolves_every_mismatch() {
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let mut app = application(repository, Arc::new(RecordingEnvironmentSource::default()));
    let publication = awaken_session_contract::SessionModelPublication {
        primary: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            awaken_runtime_contract::resolved::ModelBinding::new(
                "override-account",
                "override-upstream",
                "genai",
            ),
        ),
        candidates: Vec::new(),
    };
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    app.set_model_publication_resolver(Arc::new(RecordingModelPublication {
        publication: publication.clone(),
        requests: requests.clone(),
    }));

    let reused = app
        .resolve_session_model_override(
            "workspace",
            "published-model",
            "published-model",
            Default::default(),
        )
        .await
        .expect("equal model id reuses the publication");
    assert!(reused.publication.is_none());
    assert!(requests.lock().expect("model requests").is_empty());

    let resolved = app
        .resolve_session_model_override(
            "workspace",
            "requested-model",
            "published-model",
            Default::default(),
        )
        .await
        .expect("mismatched model id resolves a complete publication");
    assert_eq!(resolved.publication.as_deref(), Some(&publication));
    assert_eq!(
        requests.lock().expect("model requests").as_slice(),
        [("workspace".to_string(), "requested-model".to_string())]
    );

    let duplicate = publication.primary.clone();
    app.set_model_publication_resolver(Arc::new(FixedModelPublication {
        publication: awaken_session_contract::SessionModelPublication {
            primary: duplicate.clone(),
            candidates: vec![duplicate],
        },
    }));
    let error = app
        .resolve_session_model_override(
            "workspace",
            "malformed-model",
            "published-model",
            Default::default(),
        )
        .await
        .expect_err("malformed resolver output must fail before persistence");
    assert_eq!(
        error.kind,
        awaken_session_contract::RunErrorKind::BadRequest
    );
    assert!(error.message.contains("duplicate model candidate"));
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
async fn create_response_contains_a_fully_durable_initial_batch() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 create carries a nonempty, fully compiled batch;
    // C2 root insertion succeeds; C3 no reconciliation step has run yet.
    // Effects: E1 the original durable row contains every Event in order with
    // stable ids and cursor zero; E2 the same row owns the batch activity epoch;
    // E3 create returns Running; E4 no follow-up authoring write is required.
    //
    // | Rule | Batch | Insert | Reconciled | Effect |
    // | I1 | nonempty | succeeds | no | E1+E2+E3+E4 |
    // | I2 | exact replay | existing equal root | no | same durable payload |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("test repository"),
    );
    let app = application(
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let mut command = creation_command("initial-root");
    command.initial_events = Some(
        awaken_session_contract::SessionInitialEventPlan::compile(
            "initial-root",
            "initial:initial-root",
            vec![awaken_session_contract::SessionEventInput::UserMessage {
                content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "hello",
                )],
            }],
        )
        .expect("I1 valid plan"),
    );

    let created = app.create_session(command).await.expect("I1 create");
    assert_eq!(
        created.execution,
        awaken_session_contract::SessionExecutionState::Running,
        "I1/E3"
    );
    let durable = repository.get("initial-root").await.expect("I1/E1");
    assert_eq!(durable, created, "I1/E1+E4 response follows durable root");
    let batch = durable.event_batches.first().expect("I1/E1 batch");
    assert!(
        batch.events.iter().all(|entry| !entry.processed),
        "I1/C3/E1"
    );
    assert_eq!(batch.events.len(), 1, "I1/E1");
    assert!(
        durable
            .active_activity_epochs
            .contains(&batch.wake_activity_epoch.expect("I1 create wake")),
        "I1/E2"
    );
    let awaken_session_contract::SessionEventCommand::UserMessage {
        operation_id,
        run_id,
        ..
    } = &batch.events[0].event
    else {
        panic!("I1/E1 User Event")
    };
    assert_eq!(
        run_id,
        &awaken_session_contract::session_event_user_run_id("initial-root", operation_id),
        "I1/E1 stable Run id"
    );
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
async fn accepted_create_is_recovered_from_the_same_preparing_root() {
    // Cause/effect graph: C1 completion policy awaits realization/accepts the
    // durable root; C2 physical realization is not started/then reconciled; C3
    // the process survives/restarts. Effects: E1 accept returns Preparing before
    // any Runtime effect; E2 the repository contains the complete same root; E3
    // the canonical realization reconciler advances it to Idle, reasserting the
    // one complete projection at the Stage and terminal directives while
    // preparing physical state exactly once; E4 no Job row or alternate state
    // machine participates. Rules: Q1 accept+not-started=>E1+E2+E4; Q2 accepted
    // root+reconcile=>E3+E4.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("accepted create repository"),
    );
    let runtime = Arc::new(RecordingCreateRuntime::default());
    let application = SessionApplication::new(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let accepted = application
        .accept_session(creation_command("accepted-create"))
        .await
        .expect("Q1/E1");
    assert_eq!(
        accepted.execution,
        awaken_session_contract::SessionExecutionState::Preparing,
        "Q1/E1"
    );
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 0, "Q1/E1");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 0, "Q1/E1");
    assert_eq!(
        repository.get("accepted-create").await.unwrap(),
        accepted,
        "Q1/E2"
    );

    let recovery = application.reconcile_session_realizations().await;
    assert!(
        recovery.failures.is_empty(),
        "Q2/E3: {:?}",
        recovery.failures
    );
    let ready = repository.get("accepted-create").await.unwrap();
    assert_eq!(
        ready.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "Q2/E3"
    );
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 2, "Q2/E3");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "Q2/E3");
}

#[tokio::test]
async fn exact_create_replay_returns_durable_root_without_repeating_external_effects() {
    // Application create-replay cause/effect table. C1 receipt absent creates a
    // Worker-owned Session; C2 the same receipt is retried after a different
    // lowering; C3 the durable root already advanced through activation. Effects:
    // E1 first create installs/prepares once, performs one activation CAS, and
    // dispatches one Work item; E2 replay returns the exact current durable root;
    // E3 Runtime, activation revision, WorkQueue, and outbox counts do not change;
    // C4 a concurrent winner has durably entered ActivationFailed before the
    // replay result is returned; E4 the loser receives typed terminal conflict
    // and still performs no external effect.
    //
    // | Rule | Receipt | Lowering | Durable root | Effect |
    // |---|---|---|---|---|
    // | R1 | absent | first | absent | E1 |
    // | R2 | exact | changed | activated | E2+E3 |
    // | R3 | exact | same | ActivationFailed | E3+E4 |
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("create replay repository"),
    );
    let runtime = Arc::new(RecordingCreateRuntime::default());
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repository.clone(),
        environments.clone(),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let idempotency = awaken_session_contract::IdempotencyRecord {
        key: "create-replay-effects".into(),
        payload_hash: "stable-product-request".into(),
    };
    let mut first = creation_command("create-replay-effects");
    first.intent.control.runtime_placement =
        awaken_session_contract::SessionRuntimePlacement::Worker;
    first.idempotency = Some(idempotency.clone());
    let created = application.create_session(first).await.expect("R1/E1");
    let durable_before = repository
        .get(&created.session_id)
        .await
        .expect("R1 durable");
    let revision_before = durable_before.revision;
    let outbox_before = repository.pending_lifecycle().await.expect("R1 outbox");
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 1, "R1/E1");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "R1/E1");
    assert_eq!(
        environments.dispatch_calls.load(Ordering::SeqCst),
        1,
        "R1/E1"
    );

    let mut replay = creation_command("create-replay-effects");
    replay.intent.control.runtime_placement =
        awaken_session_contract::SessionRuntimePlacement::Worker;
    replay.title = Some("changed lowering must not replace durable truth".into());
    replay.idempotency = Some(idempotency);
    let replayed = application.create_session(replay).await.expect("R2/E2");
    assert_eq!(replayed, durable_before, "R2/E2");
    assert_eq!(
        repository.get(&replayed.session_id).await.unwrap().revision,
        revision_before,
        "R2/E3 no second activation CAS"
    );
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 1, "R2/E3");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "R2/E3");
    assert_eq!(
        environments.dispatch_calls.load(Ordering::SeqCst),
        1,
        "R2/E3"
    );
    assert_eq!(
        repository.pending_lifecycle().await.unwrap(),
        outbox_before,
        "R2/E3"
    );

    let mut failed = repository.get(&replayed.session_id).await.unwrap();
    let expected_revision = failed.revision;
    failed.execution = awaken_session_contract::SessionExecutionState::ActivationFailed;
    let payload = awaken_session_contract::SessionMutationPayload::Replace(failed);
    let payload_hash = payload.stable_hash();
    assert!(matches!(
        repository
            .commit_mutation(
                "workspace",
                awaken_session_contract::SessionMutation {
                    expected_revision,
                    idempotency: awaken_session_contract::IdempotencyRecord {
                        key: "test:create-replay-activation-failed".into(),
                        payload_hash,
                    },
                    payload,
                    lifecycle_facts: Vec::new(),
                },
            )
            .await
            .unwrap(),
        awaken_session_contract::SessionMutationResult::Applied { .. }
    ));
    let mut terminal_replay = creation_command("create-replay-effects");
    terminal_replay.intent.control.runtime_placement =
        awaken_session_contract::SessionRuntimePlacement::Worker;
    terminal_replay.idempotency = Some(awaken_session_contract::IdempotencyRecord {
        key: "create-replay-effects".into(),
        payload_hash: "stable-product-request".into(),
    });
    assert!(
        matches!(
            application.create_session(terminal_replay).await,
            Err(SessionCreationError::Tombstoned)
        ),
        "R3/E4"
    );
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 1, "R3/E3");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "R3/E3");
    assert_eq!(
        environments.dispatch_calls.load(Ordering::SeqCst),
        1,
        "R3/E3"
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

fn profiled_session_command(session_id: &str, model: Option<&str>) -> CreateProfiledSessionCommand {
    CreateProfiledSessionCommand {
        owner_scope: "workspace".into(),
        session_id: session_id.into(),
        mutation_policy: awaken_session_contract::SessionMutationPolicy::Managed,
        agent_id: "profiled".into(),
        source_revision: None,
        environment_id: None,
        model: model.map(str::to_owned),
        mounts: vec![profiled_mount("workspace")],
        env: vec![profiled_env("PROJECT")],
        prompts: vec!["project context".into()],
        resource_inputs: Vec::new(),
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
        repositories: Vec::new(),
        network_restriction: Some(awaken_session_contract::SessionNetworkPolicy::None),
        title: None,
        metadata: Default::default(),
        tools: None,
        idempotency: None,
    }
}

async fn assert_profiled_realization_and_environment_rules(
    repository: Arc<dyn ManagedSessionRepository>,
) {
    let mut available = application(repository.clone(), Arc::new(AdmissionEnvironment));
    available.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));

    let mut explicit_environment = profiled_session_command("profiled-realized", None);
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

    let mut substituted_environment =
        profiled_session_command("profiled-environment-substitution", None);
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
}

async fn assert_profiled_publication_revision_rules(repository: Arc<dyn ManagedSessionRepository>) {
    let mut available = application(repository.clone(), Arc::new(AdmissionEnvironment));
    available.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));

    let mut historical_command = profiled_session_command("profiled-historical", None);
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

    let mut missing_command = profiled_session_command("profiled-missing", None);
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
    let mut substituted_command = profiled_session_command("profiled-substituted-revision", None);
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
    let mut profile_without_snapshot =
        profiled_session_command("profiled-worker-missing-snapshot", None);
    profile_without_snapshot.source_revision = Some(9);
    assert!(
        worker
            .create_profiled_session(profile_without_snapshot)
            .await
            .is_err(),
        "P2: a Worker effect cannot start without the complete immutable publication"
    );
}

async fn assert_profiled_override_projection(repository: Arc<dyn ManagedSessionRepository>) {
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
    let mut override_command =
        profiled_session_command("profiled-override", Some("override-public-id"));
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
                .binding()
                .provider_identity_ref
                .starts_with("override-")),
        "O1 original Agent route must be absent"
    );
}

async fn assert_profiled_rejection_rules(repository: Arc<dyn ManagedSessionRepository>) {
    let mut available = application(repository.clone(), Arc::new(AdmissionEnvironment));
    available.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));

    let mismatched = available
        .create_profiled_session(profiled_session_command(
            "profiled-mismatch",
            Some("unpublished-model"),
        ))
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
            .create_profiled_session(profiled_session_command("profiled-unavailable", None))
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

    let mut conflicting = profiled_session_command("profiled-mcp-conflict", None);
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

async fn assert_profiled_direct_resource_rules() {
    let durable: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("direct resource repository"),
    );
    let capture = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let runtime = Arc::new(RecordingCreateRuntime::default());
    let mut available = application_with_runtime(
        runtime.clone(),
        capture.clone(),
        Arc::new(AdmissionEnvironment),
    );
    available.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));
    let mut valid = profiled_session_command("profiled-direct-file", None);
    valid.resource_inputs.push(direct_file_attachment(
        "direct-file",
        "file-direct",
        "/workspace/direct-file",
    ));
    available
        .create_profiled_session(valid)
        .await
        .expect("D2 valid direct File");
    let roots = capture.applied_create_roots();
    assert_eq!(roots.len(), 1, "D2 one original root");
    assert_eq!(
        roots[0].revision,
        awaken_session_contract::SessionRevision(1),
        "D2 original root revision"
    );
    assert!(
        roots[0].resources.desired().inputs().iter().any(|input| {
            input.binding_id.as_str() == "direct-file"
                && matches!(
                    &input.source,
                    awaken_session_contract::ResolvedInputSource::File { file_id }
                        if file_id.as_str() == "file-direct"
                )
        }),
        "D2 direct File desired intent freezes in original root before realization"
    );

    let agent_repo: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Agent collision repository"),
    );
    let agent_runtime = Arc::new(RecordingCreateRuntime::default());
    let mut agent_collision = application_with_runtime(
        agent_runtime.clone(),
        agent_repo.clone(),
        Arc::new(AdmissionEnvironment),
    );
    agent_collision.set_config_source(Arc::new(ResourceProfiledAgent));
    let mut collides_agent = profiled_session_command("profiled-direct-agent-collision", None);
    collides_agent.resource_inputs.push(direct_file_attachment(
        "direct-collides-agent",
        "file-collides-agent",
        "/workspace/agent-input",
    ));
    assert!(
        agent_collision
            .create_profiled_session(collides_agent)
            .await
            .is_err(),
        "D3 Agent collision"
    );
    assert_eq!(
        agent_repo.get("profiled-direct-agent-collision").await,
        Err(awaken_session_contract::SessionRepositoryError::NotFound),
        "D3 no root"
    );
    assert_eq!(
        agent_runtime.baseline_installs.load(Ordering::SeqCst),
        0,
        "D3"
    );
    assert_eq!(agent_runtime.preparations.load(Ordering::SeqCst), 0, "D3");

    // Final-path cause/effect rules: D4 gives a File and Repository the same
    // authored spelling, but the Runtime projects the File below
    // `/mnt/session/uploads`, so creation succeeds and the canonical
    // Stage-to-Publish sequence installs two complete phase projections while
    // preparing physical state once. D5 gives two Repository trees a
    // parent/child overlap; because both paths are already final, admission
    // rejects the whole set before Registry or Runtime effects.
    let repository_repo: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Repository path Session store"),
    );
    let repository_runtime = Arc::new(RecordingCreateRuntime::default());
    let resources = awaken_resource_persistence::ephemeral().expect("Resource authorities");
    let registry = resources.authorities().resource_registry();
    let mut repository_application = application_with_runtime(
        repository_runtime.clone(),
        repository_repo.clone(),
        Arc::new(AdmissionEnvironment),
    );
    repository_application.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));
    repository_application.set_resource_registry(registry.clone());
    let disjoint_session = "profiled-direct-projected-disjoint";
    let disjoint_repository_id = format!("profiled:{disjoint_session}:repository:0");
    let mut projected_disjoint = profiled_session_command(disjoint_session, None);
    projected_disjoint
        .resource_inputs
        .push(direct_file_attachment(
            "direct-file-shared-spelling",
            "file-shared-spelling",
            "/workspace/repository",
        ));
    projected_disjoint.repositories = vec![crate::ProfiledSessionRepositoryInput {
        binding_id: awaken_resource_contract::BindingId::new(
            "profiled-disjoint-repository-binding",
        ),
        repository: SessionRepositoryResourceInput {
            id: disjoint_repository_id.clone(),
            workspace_id: "workspace".into(),
            name: "Projected-disjoint Repository".into(),
            description: "File spelling is projected to another final tree".into(),
            remote_url: "https://example.test/disjoint.git".into(),
            credential_material: None,
            credential: None,
            mount_path: "/workspace/repository".into(),
            initial_branch: Some("main".into()),
            initial_commit: None,
        },
    }];
    repository_application
        .create_profiled_session(projected_disjoint)
        .await
        .expect("D4 projected paths are disjoint");
    assert!(
        registry
            .find_repository("workspace", &disjoint_repository_id)
            .expect("D4 inventory")
            .is_some(),
        "D4 Repository configuration is admitted"
    );
    assert_eq!(
        repository_runtime.baseline_installs.load(Ordering::SeqCst),
        2,
        "D4 Stage and Publish install two complete phase projections"
    );
    assert_eq!(
        repository_runtime.preparations.load(Ordering::SeqCst),
        1,
        "D4 one physical preparation"
    );

    let overlapping_session = "profiled-direct-repository-overlap";
    let repository_input =
        |index: usize, binding: &str, mount_path: &str| crate::ProfiledSessionRepositoryInput {
            binding_id: awaken_resource_contract::BindingId::new(binding),
            repository: SessionRepositoryResourceInput {
                id: format!("profiled:{overlapping_session}:repository:{index}"),
                workspace_id: "workspace".into(),
                name: format!("Overlapping Repository {index}"),
                description: "must fail before registration".into(),
                remote_url: format!("https://example.test/overlap-{index}.git"),
                credential_material: None,
                credential: None,
                mount_path: mount_path.into(),
                initial_branch: Some("main".into()),
                initial_commit: None,
            },
        };
    let mut overlaps = profiled_session_command(overlapping_session, None);
    overlaps.repositories = vec![
        repository_input(0, "overlap-parent", "/workspace/parent"),
        repository_input(1, "overlap-child", "/workspace/parent/child"),
    ];
    assert!(
        repository_application
            .create_profiled_session(overlaps)
            .await
            .is_err(),
        "D5 Repository trees overlap"
    );
    for index in 0..2 {
        assert_eq!(
            registry
                .find_repository(
                    "workspace",
                    &format!("profiled:{overlapping_session}:repository:{index}"),
                )
                .expect("D5 inventory"),
            None,
            "D5 no Registry effect"
        );
    }
    assert_eq!(
        repository_repo.get(overlapping_session).await,
        Err(awaken_session_contract::SessionRepositoryError::NotFound),
        "D5 no root"
    );
    assert_eq!(
        repository_runtime.baseline_installs.load(Ordering::SeqCst),
        2,
        "D5 no additional phase projection install"
    );
    assert_eq!(
        repository_runtime.preparations.load(Ordering::SeqCst),
        1,
        "D5 no additional physical preparation"
    );
}

#[tokio::test]
async fn profiled_session_creation_enforces_publication_and_upfront_inputs() {
    // Profiled-Session FMECA and cause/effect graph. Failure modes are FM1 a
    // requested model bypasses the published Agent, FM2 an unavailable Agent is
    // admitted. Causes: C1 profile exists, C2 Agent available, C3 requested model
    // absent/equal, C4 requested model differs, C5 complete local inputs are
    // supplied up front, C6 Agent and Session MCP candidates are distinct or
    // overlap by name, C7 equal-origin names conflict, C8 direct resources are
    // none/valid/collide with an Agent final path/share authored spelling with
    // a Repository but project disjointly, and C9 final Repository trees
    // overlap. Effects: E1 freeze the published execution identity and local
    // inputs, E2 reject without a row, E3 retain Agent-only and Session-only MCP
    // while Session overrides Agent by logical name, E4 admit the final-path
    // disjoint pair through two phase projection installs and one physical
    // preparation, and E5 reject final Repository overlap before effects.
    // Graph: C1&&C2&&C3&&C5&&C6 -> E1+E3; C4||!C2||C7 -> E2;
    // C8(projected disjoint) -> E4; C9 -> E5.
    //
    // | Rule | Profile | Available | Requested model | Product MCP | Effect |
    // |---|---|---|---|---|---|
    // | P1 | yes | yes | absent/equal | distinct + Agent overlap | E1 + E3 |
    // | P2 | yes | yes | different | any | E2 |
    // | P3 | yes | no | any | any | E2 |
    // | P4 | yes | yes | absent/equal | duplicate Session name | E2 |
    // | D1 | yes | yes | absent/equal | no direct resource | existing P1-P4 semantics |
    // | D2 | yes | yes | absent/equal | valid direct File | original rev1 root contains File |
    // | D3 | yes | yes | absent/equal | direct collides Agent | E2 before root/effect |
    // | D4 | yes | yes | absent/equal | File/Repository spelling projects disjointly | E4: admit; 2 installs/1 prepare |
    // | D5 | yes | yes | absent/equal | Repository final trees overlap | E5 before registry/root/effect |
    //
    // Each cause/effect partition owns a separate boxed future. This keeps the
    // test's large, deeply composed Session values out of one aggregate async
    // frame without changing production stack limits or the decision oracles.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("profiled Session repository"),
    );
    Box::pin(assert_profiled_realization_and_environment_rules(
        repository.clone(),
    ))
    .await;
    Box::pin(assert_profiled_publication_revision_rules(
        repository.clone(),
    ))
    .await;
    Box::pin(assert_profiled_override_projection(repository.clone())).await;
    Box::pin(assert_profiled_rejection_rules(repository)).await;
    Box::pin(assert_profiled_direct_resource_rules()).await;
}
