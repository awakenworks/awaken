use super::*;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;

fn coordinated_session(id: &str) -> PersistedSession {
    let mut session = persisted(id, false, "idle");
    let environment = session
        .frozen_baseline()
        .expect("fixture baseline")
        .environment
        .clone();
    session.baseline = awaken_session_contract::SessionBaselineState::Frozen(
        awaken_session_contract::SessionBaseline::compile(
            awaken_session_contract::SessionBaselineInputs {
                environment,
                runtime_placement: SessionRuntimePlacement::Local,
                mcp_authoring: Default::default(),
                agent_id: "coord-root".into(),
                agent_revision: Some(1),
                model_override: None,
                model: "coord-model".into(),
                runtime: None,
                delegate_ids: vec!["coord-child".into()],
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
            },
        ),
    );
    session
}

fn session_with_unsettled_event(id: &str) -> PersistedSession {
    let mut session = persisted(id, false, "idle");
    session.event_batches.push(
        awaken_session_contract::SessionEventBatch::compile(
            id,
            format!("batch:{id}"),
            vec![awaken_session_contract::SessionEventInput::UserMessage {
                content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "accepted before terminal",
                )],
            }],
        )
        .expect("valid retained Event batch"),
    );
    session
}

async fn anchor_unsettled_event(
    app: &SessionApplication,
    repository: &dyn ManagedSessionRepository,
    session_id: &str,
) {
    let mut session = repository.get(session_id).await.expect("retained Session");
    let operation_id = session.event_batches[0].events[0]
        .event
        .operation_id()
        .to_string();
    assert!(
        session.event_batches[0]
            .mark_processed(
                &operation_id,
                awaken_session_contract::SessionEventProjectionAnchor {
                    source_commit_cursor: 17,
                },
            )
            .expect("anchor exact accepted effect")
    );
    app.commit_session_snapshot(
        "workspace",
        session,
        &format!("test-anchor-event:{session_id}"),
        Vec::new(),
    )
    .await
    .expect("commit effect anchor");
}

fn coordination_spawn_command(
    session_id: &str,
) -> awaken_session_contract::SessionAgentMessageCommand {
    awaken_session_contract::SessionAgentMessageCommand {
        session_id: session_id.into(),
        source_thread_id: ThreadId(session_id.into()),
        source_run_id: RunId(format!("{session_id}-parent-run")),
        source_call_id: "send-call".into(),
        operation_id: "send-operation".into(),
        target: awaken_session_contract::SessionAgentTarget::Spawn {
            agent_id: "coord-child".into(),
        },
        message: "investigate".into(),
    }
}

fn configured_admission_application(
    repo: Arc<dyn ManagedSessionRepository>,
    runtime: Arc<RecordingAgentAdmissionRuntime>,
) -> SessionApplication {
    let mut app = application_with_runtime(
        runtime,
        repo,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    app.set_config_source(Arc::new(CoordinatedAgentSource));
    app
}

fn managed_repository_id(session_id: &str) -> String {
    format!("managed:{session_id}:repository:source")
}

fn repository_input(name: &str, remote_url: &str) -> SessionRepositoryResourceInput {
    SessionRepositoryResourceInput {
        id: managed_repository_id("repo-replay"),
        workspace_id: "workspace".into(),
        name: name.into(),
        description: "Session source".into(),
        remote_url: remote_url.into(),
        credential_material: None,
        credential: None,
        mount_path: "/workspace/source".into(),
        initial_branch: Some("main".into()),
        initial_commit: None,
    }
}

fn token_repository_input(session_id: &str, name: &str) -> SessionRepositoryResourceInput {
    let mut input = repository_input(name, "https://github.com/awaken/example.git");
    input.id = managed_repository_id(session_id);
    input.credential_material = Some(CredentialMaterialInput::structured(
        "github",
        awaken_credential_contract::http_basic_material(
            awaken_agent_contract::RedactedString::new("x-access-token"),
            awaken_agent_contract::RedactedString::new("x"),
        ),
    ));
    input
}

fn register_replayed_token_repository(
    registry: &dyn awaken_resource_contract::ResourceRegistry,
    input: &SessionRepositoryResourceInput,
) {
    let owner = SessionRepositoryOwner::from_repository_id(&input.id)
        .expect("test Repository has a canonical Session owner");
    let repository_id = awaken_resource_contract::RepositoryId::from(input.id.clone());
    let definition = awaken_resource_contract::RepositoryDefinition {
        id: repository_id.clone(),
        workspace_id: input.workspace_id.clone(),
        name: input.name.clone(),
        description: input.description.clone(),
        metadata: owner.marker(),
        state: awaken_resource_contract::ResourceState::Suspended,
        current_config_version: awaken_resource_contract::ConfigVersion::INITIAL,
        timestamps: Default::default(),
    };
    let initial_config = awaken_resource_contract::RepositoryConfigVersion {
        repository_id,
        version: awaken_resource_contract::ConfigVersion::INITIAL,
        remote_url: input.remote_url.clone(),
        credential_binding: Some(format!("{}:credential", input.id)),
        initial_branch: input.initial_branch.clone(),
        initial_commit: input.initial_commit.clone(),
        clone_policy: Default::default(),
    };
    awaken_resource_contract::RepositoryAggregate::register(
        definition.clone(),
        initial_config.clone(),
    )
    .expect("valid replay fixture");
    registry
        .register_repository(awaken_resource_contract::RegisterRepository {
            definition,
            initial_config,
        })
        .expect("register replay fixture");
}

fn repository_retirement_resources(
    session_id: &str,
    credential: Option<awaken_credential_contract::CredentialRef>,
) -> awaken_session_contract::ResolvedSessionResources {
    let repository_id = managed_repository_id(session_id);
    let remote_url = "https://github.com/awaken/example.git";
    let credential_binding = credential
        .as_ref()
        .map(|_| format!("{repository_id}:credential"));
    let credential = credential.map(|credential| {
        let holder = awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Worker,
            "spiffe://awaken.test/worker",
        );
        Box::new(awaken_session_contract::ResolvedRepositoryCredential {
            access: awaken_credential_contract::CredentialAccess::new(
                credential,
                awaken_credential_contract::CredentialMaterialSource::ControlPlaneReference,
                awaken_session_contract::repository_transport_credential_usage(),
                awaken_credential_contract::CredentialExecutionPolicy::exact(
                    holder.clone(),
                    awaken_credential_contract::ModelExposurePolicy::Forbidden,
                ),
            )
            .with_target(
                awaken_session_contract::repository_transport_credential_target(remote_url)
                    .expect("Repository target"),
            ),
            selected_plaintext_holder: holder,
        })
    });
    awaken_session_contract::ResolvedSessionResources::try_new(
        vec![awaken_session_contract::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::from(format!(
                "repository-{session_id}"
            )),
            source: awaken_session_contract::ResolvedInputSource::Repository {
                repository_id: repository_id.clone().into(),
                config: awaken_resource_contract::RepositoryConfigVersion {
                    repository_id: repository_id.into(),
                    version: awaken_resource_contract::ConfigVersion::INITIAL,
                    remote_url: remote_url.into(),
                    credential_binding,
                    initial_branch: Some("main".into()),
                    initial_commit: None,
                    clone_policy: Default::default(),
                },
                credential,
            },
            mount_path: "/workspace/source".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }],
        Vec::new(),
    )
    .expect("valid Repository retirement fixture")
}

fn session_with_repository_retirement(
    session_id: &str,
    self_hosted: bool,
    resources: awaken_session_contract::ResolvedSessionResources,
) -> PersistedSession {
    let mut session = persisted(session_id, self_hosted, "idle");
    session.resources = awaken_session_contract::SessionResourceState::from_active(resources);
    session
        .resources
        .prepare(
            session_id,
            awaken_session_contract::ResolvedSessionResources::default(),
        )
        .expect("prepare omission");
    session.resources.commit().expect("commit omission");
    session
}

fn register_retirement_repository(
    registry: &dyn awaken_resource_contract::ResourceRegistry,
    session_id: &str,
    marked: bool,
    credential_bound: bool,
) {
    let repository_id = managed_repository_id(session_id);
    let definition = awaken_resource_contract::RepositoryDefinition {
        id: repository_id.clone().into(),
        workspace_id: "workspace".into(),
        name: "Retirement fixture".into(),
        description: "Retirement fixture".into(),
        metadata: if marked {
            SessionRepositoryOwner::managed(session_id).marker()
        } else {
            Default::default()
        },
        state: awaken_resource_contract::ResourceState::Suspended,
        current_config_version: awaken_resource_contract::ConfigVersion::INITIAL,
        timestamps: Default::default(),
    };
    registry
        .register_repository(awaken_resource_contract::RegisterRepository {
            definition,
            initial_config: awaken_resource_contract::RepositoryConfigVersion {
                repository_id: repository_id.clone().into(),
                version: awaken_resource_contract::ConfigVersion::INITIAL,
                remote_url: "https://github.com/awaken/example.git".into(),
                credential_binding: credential_bound.then(|| format!("{repository_id}:credential")),
                initial_branch: Some("main".into()),
                initial_commit: None,
                clone_policy: Default::default(),
            },
        })
        .expect("register retirement fixture");
    registry
        .change_repository_state(awaken_resource_contract::ChangeRepositoryState {
            workspace_id: "workspace".into(),
            id: repository_id.into(),
            state: awaken_resource_contract::ResourceState::Active,
        })
        .expect("activate retirement fixture");
}

/// Fault-only decorator; the composed Resource Registry remains the sole state
/// and validation authority used by every test rule below.
struct FaultInjectingResourceRegistry {
    inner: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    registration_calls: AtomicUsize,
    activation_calls: AtomicUsize,
    registration_failures: AtomicUsize,
    activation_failures: AtomicUsize,
}

impl FaultInjectingResourceRegistry {
    fn new(inner: Arc<dyn awaken_resource_contract::ResourceRegistry>) -> Self {
        Self {
            inner,
            registration_calls: AtomicUsize::new(0),
            activation_calls: AtomicUsize::new(0),
            registration_failures: AtomicUsize::new(0),
            activation_failures: AtomicUsize::new(0),
        }
    }

    fn fail_next_registration(&self) {
        self.registration_failures.store(1, Ordering::SeqCst);
    }

    fn fail_next_activation(&self) {
        self.activation_failures.store(1, Ordering::SeqCst);
    }

    fn consume_failure(counter: &AtomicUsize) -> bool {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }
}

impl awaken_resource_contract::ResourceInventory for FaultInjectingResourceRegistry {
    fn find_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<
        Option<awaken_resource_contract::MemoryStoreDefinition>,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.find_memory_store(workspace_id, id)
    }

    fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<
        Vec<awaken_resource_contract::MemoryStoreDefinition>,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.list_memory_stores(workspace_id)
    }

    fn find_memory_store_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: awaken_resource_contract::ConfigVersion,
    ) -> Result<
        Option<awaken_resource_contract::MemoryStoreConfigVersion>,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner
            .find_memory_store_config(workspace_id, id, version)
    }

    fn find_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<
        Option<awaken_resource_contract::RepositoryDefinition>,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.find_repository(workspace_id, id)
    }

    fn find_repository_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: awaken_resource_contract::ConfigVersion,
    ) -> Result<
        Option<awaken_resource_contract::RepositoryConfigVersion>,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.find_repository_config(workspace_id, id, version)
    }
}

impl awaken_resource_contract::ResourceAdministration for FaultInjectingResourceRegistry {
    fn register_memory_store(
        &self,
        command: awaken_resource_contract::RegisterMemoryStore,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner.register_memory_store(command)
    }

    fn update_memory_store_profile(
        &self,
        command: awaken_resource_contract::UpdateMemoryStoreProfile,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner.update_memory_store_profile(command)
    }

    fn publish_memory_store_config(
        &self,
        command: awaken_resource_contract::PublishMemoryStoreConfig,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner.publish_memory_store_config(command)
    }

    fn change_memory_store_state(
        &self,
        command: awaken_resource_contract::ChangeMemoryStoreState,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner.change_memory_store_state(command)
    }

    fn register_repository(
        &self,
        command: awaken_resource_contract::RegisterRepository,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.registration_calls.fetch_add(1, Ordering::SeqCst);
        if Self::consume_failure(&self.registration_failures) {
            return Err(
                awaken_resource_contract::ResourceRegistryError::Unavailable(
                    "injected registration failure".into(),
                ),
            );
        }
        self.inner.register_repository(command)
    }

    fn publish_repository_config(
        &self,
        command: awaken_resource_contract::PublishRepositoryConfig,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner.publish_repository_config(command)
    }

    fn change_repository_state(
        &self,
        command: awaken_resource_contract::ChangeRepositoryState,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        if command.state == awaken_resource_contract::ResourceState::Active {
            self.activation_calls.fetch_add(1, Ordering::SeqCst);
            if Self::consume_failure(&self.activation_failures) {
                return Err(
                    awaken_resource_contract::ResourceRegistryError::Unavailable(
                        "injected activation failure".into(),
                    ),
                );
            }
        }
        self.inner.change_repository_state(command)
    }
}

impl awaken_resource_contract::ExecutionResourceResolver for FaultInjectingResourceRegistry {
    fn resolve_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<
        awaken_resource_contract::MemoryStoreConfigVersion,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.resolve_memory_store(workspace_id, id)
    }

    fn resolve_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<
        awaken_resource_contract::RepositoryConfigVersion,
        awaken_resource_contract::ResourceRegistryError,
    > {
        self.inner.resolve_repository(workspace_id, id)
    }
}

impl awaken_resource_contract::LiveResourceBindingVerifier for FaultInjectingResourceRegistry {
    fn verify_memory_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner.verify_memory_binding(workspace_id, id, version)
    }

    fn verify_repository_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        self.inner
            .verify_repository_binding(workspace_id, id, version)
    }
}

/// Fault-only ingress port. Its set exists only to project Applied/Replayed and
/// retirement observations for the cause/effect rules; production Vault state
/// and replay remain owned by the composed credential implementation.
#[derive(Default)]
struct FaultInjectingCredentialMaterialIngress {
    calls: AtomicUsize,
    failures: AtomicUsize,
    retirement_failures: AtomicUsize,
    entered_sources: Mutex<BTreeSet<String>>,
    retirements: Mutex<Vec<awaken_credential_contract::CredentialRef>>,
}

impl FaultInjectingCredentialMaterialIngress {
    fn fail_next(&self) {
        self.failures.store(1, Ordering::SeqCst);
    }

    fn fail_next_retirement(&self) {
        self.retirement_failures.store(1, Ordering::SeqCst);
    }

    fn seed_replayed_source(&self, repository_id: &str) {
        self.entered_sources
            .lock()
            .unwrap()
            .insert(format!("{repository_id}:credential"));
    }

    fn retirements(&self) -> Vec<awaken_credential_contract::CredentialRef> {
        self.retirements.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl CredentialMaterialIngress for FaultInjectingCredentialMaterialIngress {
    async fn enter_material(
        &self,
        command: CredentialMaterialIngressCommand,
    ) -> Result<CredentialMaterialIngressReceipt, String> {
        let source_id = command.source_id;
        self.calls.fetch_add(1, Ordering::SeqCst);
        if FaultInjectingResourceRegistry::consume_failure(&self.failures) {
            Err("injected credential ingress failure".into())
        } else {
            let provenance = if self
                .entered_sources
                .lock()
                .unwrap()
                .insert(source_id.0.clone())
            {
                SessionParticipantProvenance::Applied
            } else {
                SessionParticipantProvenance::Replayed
            };
            Ok(CredentialMaterialIngressReceipt {
                credential: awaken_credential_contract::CredentialRef {
                    id: source_id.0,
                    revision: 1,
                },
                provenance,
            })
        }
    }

    async fn rotate_material(
        &self,
        _command: CredentialMaterialRotationCommand,
    ) -> Result<u64, String> {
        Err("rotation is outside this registration test".into())
    }

    async fn retire_material(
        &self,
        command: CredentialMaterialRetirementCommand,
    ) -> Result<(), String> {
        if FaultInjectingResourceRegistry::consume_failure(&self.retirement_failures) {
            return Err("injected credential retirement failure".into());
        }
        self.retirements.lock().unwrap().push(command.credential);
        Ok(())
    }
}

fn repository_saga_application(
    registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    ingress: Arc<dyn CredentialMaterialIngress>,
) -> SessionApplication {
    let sessions: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Session repository"),
    );
    let mut app = application(sessions, Arc::new(RecordingEnvironmentSource::default()));
    app.set_resource_registry(registry);
    app.set_credential_material_ingress(ingress);
    app
}

fn repository_retirement_application(
    sessions: Arc<dyn ManagedSessionRepository>,
    registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    ingress: Arc<dyn CredentialMaterialIngress>,
) -> SessionApplication {
    let mut app = application(sessions, Arc::new(RecordingEnvironmentSource::default()));
    app.set_resource_registry(registry);
    app.set_credential_material_ingress(ingress);
    app
}

#[tokio::test]
async fn repository_configuration_replays_only_the_exact_registry_aggregate() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Registration crash/retry cause-effect graph: C1 the Repository is absent
    // or already registered; C2 the stored Workspace/id definition exactly
    // matches the intended definition; C3 stored INITIAL config exactly matches
    // the intended config. Effects: E1 register/return the Repository id; E2
    // accept an exact post-registration crash replay without another authority;
    // E3 reject definition or config drift and retain the original aggregate.
    // Decision rules: R1 absent=>E1; R2 present+C2+C3=>E2; R3
    // present+!C2+C3=>E3; R4 present+C2+!C3=>E3. Registry inventory is the one
    // replay receipt; the Session application creates no side store or cache.
    let resources = awaken_resource_persistence::ephemeral().expect("Resource authorities");
    let registry = resources.authorities().resource_registry();
    let repository_id = managed_repository_id("repo-replay");
    let sessions: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Session repository"),
    );

    let mut original = application(
        sessions.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    original.set_resource_registry(registry.clone());
    let registered = original
        .configure_session_repository(repository_input(
            "Repository",
            "https://example.test/source.git",
        ))
        .await
        .expect("R1 registers");
    assert_eq!(
        registered.repository_id,
        awaken_resource_contract::RepositoryId::from(repository_id.clone()),
        "R1/E1"
    );
    assert_eq!(
        registered.registry_provenance,
        SessionParticipantProvenance::Applied,
        "R1/E1 current command owns the Registry participant"
    );
    drop(original);

    let mut restarted = application(sessions, Arc::new(RecordingEnvironmentSource::default()));
    restarted.set_resource_registry(registry.clone());
    let replayed = restarted
        .configure_session_repository(repository_input(
            "Repository",
            "https://example.test/source.git",
        ))
        .await
        .expect("R2 exact replay");
    assert_eq!(
        replayed.repository_id,
        awaken_resource_contract::RepositoryId::from(repository_id.clone()),
        "R2/E2"
    );
    assert_eq!(
        replayed.registry_provenance,
        SessionParticipantProvenance::Replayed,
        "R2/E2 prior durable truth owns the Registry participant"
    );

    let definition_mismatch = restarted
        .configure_session_repository(repository_input(
            "Different Repository",
            "https://example.test/source.git",
        ))
        .await
        .expect_err("R3 definition mismatch");
    assert_eq!(
        definition_mismatch.kind,
        awaken_session_contract::RunErrorKind::BadRequest,
        "R3/E3"
    );
    let config_mismatch = restarted
        .configure_session_repository(repository_input(
            "Repository",
            "https://example.test/other.git",
        ))
        .await
        .expect_err("R4 config mismatch");
    assert_eq!(
        config_mismatch.kind,
        awaken_session_contract::RunErrorKind::BadRequest,
        "R4/E3"
    );
    assert_eq!(
        registry
            .find_repository("workspace", &repository_id)
            .expect("inventory")
            .expect("registered definition")
            .name,
        "Repository",
        "R3-R4 preserve original Registry truth"
    );
    assert_eq!(
        registry
            .find_repository_config(
                "workspace",
                &repository_id,
                awaken_resource_contract::ConfigVersion::INITIAL,
            )
            .expect("inventory")
            .expect("registered config")
            .remote_url,
        "https://example.test/source.git",
        "R3-R4 preserve original Registry truth"
    );
}

#[tokio::test]
async fn token_repository_registration_is_one_recoverable_registry_vault_saga() {
    // Cause/effect graph: C1 token/ref exclusivity and C2 the pure Repository
    // aggregate validation pass/fail before effects; C3 Registry registration
    // fails, Applies, or Replays; C4 Vault ingress fails, Applies, or Replays;
    // C5 activation succeeds/fails. Effects: E1 C1/C2 failure writes neither
    // participant; E2 C3 failure never reaches Vault; E3 a pre-root failure
    // retires only a Registry participant Applied by this command; E4 it retires
    // only a Vault participant Applied by this command; E5 every Replayed
    // participant remains byte-for-byte durable; E6 success reaches Active.
    // Constraint: compensation preserves the first error and provenance is
    // participant-local; a failed command never infers replayed truth is orphaned.
    //
    // | Rule | C1/C2 | Registry C3 | Vault C4 | Activate C5 | Effect |
    // |---|---|---|---|---|---|
    // | S1/S2 validation | fail | - | - | - | E1 |
    // | S3 registration | pass | fail | - | - | E2 |
    // | S4 ingress | pass | Applied | fail | - | E3 |
    // | S5 ingress | pass | Replayed | fail | - | E5 |
    // | S6 conflict | pass | conflicting replay | - | - | E5 |
    // | S7 activation | pass | Applied | Applied | fail | E3 + E4 |
    // | S8 activation | pass | Applied | Replayed | fail | E3 + E5 |
    // | S9 activation | pass | Replayed | Applied | fail | E5 + E4 |
    // | S10 activation | pass | Replayed | Replayed | fail | E5 |
    // | S11/S12 success/replay | pass | Applied/Replayed | Applied/Replayed | pass/already Active | E6 + E5 |
    let resources = awaken_resource_persistence::ephemeral().expect("Resource authorities");
    let inner = resources.authorities().resource_registry();
    let registry = Arc::new(FaultInjectingResourceRegistry::new(inner.clone()));
    let ingress = Arc::new(FaultInjectingCredentialMaterialIngress::default());
    let app = repository_saga_application(registry.clone(), ingress.clone());
    let repository_state = |session_id: &str| {
        let repository_id = managed_repository_id(session_id);
        inner
            .find_repository("workspace", &repository_id)
            .expect("Registry inventory")
            .expect("Repository definition")
            .state
    };
    let credential_ref = |session_id: &str| awaken_credential_contract::CredentialRef {
        id: format!("{}:credential", managed_repository_id(session_id)),
        revision: 1,
    };

    let mut dual = token_repository_input("repo-dual", "Dual");
    dual.credential = Some(awaken_credential_contract::CredentialRef {
        id: "credential-existing".into(),
        revision: 1,
    });
    assert!(
        app.configure_session_repository(dual).await.is_err(),
        "S1/E1 dual credential admission"
    );
    assert_eq!(registry.registration_calls.load(Ordering::SeqCst), 0, "S1");
    assert_eq!(ingress.calls.load(Ordering::SeqCst), 0, "S1");
    assert!(
        inner
            .find_repository("workspace", &managed_repository_id("repo-dual"))
            .unwrap()
            .is_none(),
        "S1/E1 zero Registry write"
    );

    let mut invalid = token_repository_input("repo-invalid", "Invalid");
    invalid.initial_commit = Some("0123456789abcdef".into());
    assert!(
        app.configure_session_repository(invalid).await.is_err(),
        "S2/E1 aggregate validation"
    );
    assert_eq!(registry.registration_calls.load(Ordering::SeqCst), 0, "S2");
    assert_eq!(ingress.calls.load(Ordering::SeqCst), 0, "S2");
    assert!(
        inner
            .find_repository("workspace", &managed_repository_id("repo-invalid"))
            .unwrap()
            .is_none(),
        "S2/E1 zero Registry write"
    );

    registry.fail_next_registration();
    assert!(
        app.configure_session_repository(token_repository_input("repo-register", "Register"))
            .await
            .is_err(),
        "S3/E2 register failure"
    );
    assert_eq!(registry.registration_calls.load(Ordering::SeqCst), 1, "S3");
    assert_eq!(ingress.calls.load(Ordering::SeqCst), 0, "S3/E2");
    assert!(
        inner
            .find_repository("workspace", &managed_repository_id("repo-register"))
            .unwrap()
            .is_none(),
        "S3 register failure does not persist"
    );

    ingress.fail_next();
    let retirements_before = ingress.retirements().len();
    assert!(
        app.configure_session_repository(token_repository_input(
            "repo-ingress-applied",
            "Ingress Applied",
        ))
        .await
        .is_err(),
        "S4/E3 ingress failure"
    );
    assert_eq!(
        repository_state("repo-ingress-applied"),
        awaken_resource_contract::ResourceState::Deleted,
        "S4/E3 only this command's Applied Registry participant retires"
    );
    assert!(matches!(
        inner.resolve_repository("workspace", &managed_repository_id("repo-ingress-applied")),
        Err(awaken_resource_contract::ResourceRegistryError::NotActive {
            state: awaken_resource_contract::ResourceState::Deleted,
            ..
        })
    ));
    assert_eq!(
        ingress.retirements().len(),
        retirements_before,
        "S4 no Vault participant existed to retire"
    );
    assert_eq!(
        inner
            .find_repository_config(
                "workspace",
                &managed_repository_id("repo-ingress-applied"),
                awaken_resource_contract::ConfigVersion::INITIAL,
            )
            .unwrap()
            .expect("S4 config")
            .credential_binding
            .as_deref(),
        Some(
            format!(
                "{}:credential",
                managed_repository_id("repo-ingress-applied")
            )
            .as_str()
        ),
        "S4 deterministic saga binding"
    );

    let replayed_ingress = token_repository_input("repo-ingress-replayed", "Ingress Replayed");
    register_replayed_token_repository(inner.as_ref(), &replayed_ingress);
    ingress.fail_next();
    let retirements_before = ingress.retirements().len();
    app.configure_session_repository(replayed_ingress)
        .await
        .expect_err("S5 injected Vault failure");
    assert_eq!(
        repository_state("repo-ingress-replayed"),
        awaken_resource_contract::ResourceState::Suspended,
        "S5/E5 replayed Registry truth is not retired"
    );
    assert_eq!(
        ingress.retirements().len(),
        retirements_before,
        "S5 no Vault participant existed to retire"
    );

    let ingress_before_conflict = ingress.calls.load(Ordering::SeqCst);
    let retirements_before = ingress.retirements().len();
    assert!(
        app.configure_session_repository(token_repository_input(
            "repo-ingress-replayed",
            "Conflicting Ingress",
        ))
        .await
        .is_err(),
        "S6 conflicting replay"
    );
    assert_eq!(
        ingress.calls.load(Ordering::SeqCst),
        ingress_before_conflict,
        "S6/E5 conflict never reaches Vault"
    );
    assert_eq!(
        repository_state("repo-ingress-replayed"),
        awaken_resource_contract::ResourceState::Suspended,
        "S6/E5 conflicting replay preserves prior Registry truth"
    );
    assert_eq!(
        ingress.retirements().len(),
        retirements_before,
        "S6/E5 no participant is retired"
    );

    let retirements_before = ingress.retirements().len();
    registry.fail_next_activation();
    app.configure_session_repository(token_repository_input(
        "repo-activate-aa",
        "Activate Applied Applied",
    ))
    .await
    .expect_err("S7 injected activation failure");
    assert_eq!(
        repository_state("repo-activate-aa"),
        awaken_resource_contract::ResourceState::Deleted,
        "S7/E3 Applied Registry participant retires"
    );
    assert_eq!(
        &ingress.retirements()[retirements_before..],
        &[credential_ref("repo-activate-aa")],
        "S7/E4 Applied Vault participant retires"
    );

    let applied_registry_replayed_vault =
        token_repository_input("repo-activate-ar", "Activate Applied Replayed");
    ingress.seed_replayed_source(&applied_registry_replayed_vault.id);
    let retirements_before = ingress.retirements().len();
    registry.fail_next_activation();
    app.configure_session_repository(applied_registry_replayed_vault)
        .await
        .expect_err("S8 injected activation failure");
    assert_eq!(
        repository_state("repo-activate-ar"),
        awaken_resource_contract::ResourceState::Deleted,
        "S8/E3 Applied Registry participant retires"
    );
    assert_eq!(
        ingress.retirements().len(),
        retirements_before,
        "S8/E5 Replayed Vault participant is not retired"
    );

    let replayed_registry_applied_vault =
        token_repository_input("repo-activate-ra", "Activate Replayed Applied");
    register_replayed_token_repository(inner.as_ref(), &replayed_registry_applied_vault);
    let retirements_before = ingress.retirements().len();
    registry.fail_next_activation();
    app.configure_session_repository(replayed_registry_applied_vault)
        .await
        .expect_err("S9 injected activation failure");
    assert_eq!(
        repository_state("repo-activate-ra"),
        awaken_resource_contract::ResourceState::Suspended,
        "S9/E5 Replayed Registry participant is not retired"
    );
    assert_eq!(
        &ingress.retirements()[retirements_before..],
        &[credential_ref("repo-activate-ra")],
        "S9/E4 Applied Vault participant retires"
    );

    let replayed_registry_replayed_vault =
        token_repository_input("repo-activate-rr", "Activate Replayed Replayed");
    register_replayed_token_repository(inner.as_ref(), &replayed_registry_replayed_vault);
    ingress.seed_replayed_source(&replayed_registry_replayed_vault.id);
    let retirements_before = ingress.retirements().len();
    registry.fail_next_activation();
    app.configure_session_repository(replayed_registry_replayed_vault)
        .await
        .expect_err("S10 injected activation failure");
    assert_eq!(
        repository_state("repo-activate-rr"),
        awaken_resource_contract::ResourceState::Suspended,
        "S10/E5 Replayed Registry participant is not retired"
    );
    assert_eq!(
        ingress.retirements().len(),
        retirements_before,
        "S10/E5 Replayed Vault participant is not retired"
    );

    let succeeded = app
        .configure_session_repository(token_repository_input(
            "repo-activate-success",
            "Activate Success",
        ))
        .await
        .expect("S11/E6 fresh success");
    assert_eq!(
        succeeded.registry_provenance,
        SessionParticipantProvenance::Applied,
        "S11 Registry provenance"
    );
    assert_eq!(
        succeeded
            .credential
            .as_ref()
            .expect("S11 credential")
            .provenance,
        SessionParticipantProvenance::Applied,
        "S11 Vault provenance"
    );
    assert_eq!(
        repository_state("repo-activate-success"),
        awaken_resource_contract::ResourceState::Active,
        "S11/E6"
    );

    let activations_before = registry.activation_calls.load(Ordering::SeqCst);
    let retirements_before = ingress.retirements().len();
    let replayed = app
        .configure_session_repository(token_repository_input(
            "repo-activate-success",
            "Activate Success",
        ))
        .await
        .expect("S12/E6 exact replay");
    assert_eq!(
        replayed.registry_provenance,
        SessionParticipantProvenance::Replayed,
        "S12 Registry provenance"
    );
    assert_eq!(
        replayed
            .credential
            .as_ref()
            .expect("S12 credential")
            .provenance,
        SessionParticipantProvenance::Replayed,
        "S12 Vault provenance"
    );
    assert_eq!(
        registry.activation_calls.load(Ordering::SeqCst),
        activations_before,
        "S12 already-Active replay performs no activation"
    );
    assert_eq!(
        ingress.retirements().len(),
        retirements_before,
        "S12/E5 exact replay retires neither participant"
    );
}

#[tokio::test]
async fn repository_retirement_requires_durable_owner_before_any_vault_effect() {
    // Retirement-authority cause/effect graph: C1 the canonical Session
    // namespace has a matching durable owner marker, a mismatching/absent marker,
    // or no surviving definition; C2 the exact removed input carries an inline
    // credential pin; C3 the Session is Worker-placed and otherwise Active.
    // Effects: E1 only a matching marker authorizes Vault/Registry retirement;
    // E2 markerless/shared truth is a successful no-op; E3 a missing definition
    // completes an idempotent replay without inferring credential authority; E4
    // every completed rule clears the durable intent under the Session root CAS.
    //
    // | Rule | Definition | Marker | Inline pin | Worker | Effect |
    // |---|---|---|---|---|---|
    // | A1 | present | mismatch | forged | yes | E2 + E4, no Vault effect |
    // | A2 | absent | n/a | forged | yes | E3 + E4, no Vault effect |
    let resources = awaken_resource_persistence::ephemeral().expect("Resource authorities");
    let registry = resources.authorities().resource_registry();
    let ingress = Arc::new(FaultInjectingCredentialMaterialIngress::default());
    let sessions: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Session repository"),
    );

    let markerless_id = "retirement-markerless";
    let markerless_credential = awaken_credential_contract::CredentialRef {
        id: format!("{}:credential", managed_repository_id(markerless_id)),
        revision: 7,
    };
    register_retirement_repository(registry.as_ref(), markerless_id, false, true);
    create(
        sessions.as_ref(),
        session_with_repository_retirement(
            markerless_id,
            true,
            repository_retirement_resources(markerless_id, Some(markerless_credential)),
        ),
    )
    .await;

    let absent_id = "retirement-already-purged";
    let absent_credential = awaken_credential_contract::CredentialRef {
        id: format!("{}:credential", managed_repository_id(absent_id)),
        revision: 11,
    };
    create(
        sessions.as_ref(),
        session_with_repository_retirement(
            absent_id,
            true,
            repository_retirement_resources(absent_id, Some(absent_credential)),
        ),
    )
    .await;

    let app =
        repository_retirement_application(sessions.clone(), registry.clone(), ingress.clone());
    let report = app.reconcile_resource_activations().await;
    assert!(report.failures.is_empty(), "A1-A2/E4: {report:?}");
    assert!(
        sessions
            .get(markerless_id)
            .await
            .unwrap()
            .resources
            .repository_retirements()
            .is_empty(),
        "A1/E4"
    );
    assert!(
        sessions
            .get(absent_id)
            .await
            .unwrap()
            .resources
            .repository_retirements()
            .is_empty(),
        "A2/E4"
    );
    assert!(ingress.retirements().is_empty(), "A1-A2/E2-E3");
    assert_eq!(
        registry
            .find_repository("workspace", &managed_repository_id(markerless_id))
            .unwrap()
            .unwrap()
            .state,
        awaken_resource_contract::ResourceState::Active,
        "A1/E2"
    );
}

#[tokio::test]
async fn repository_retirement_restart_retry_fences_same_identity_until_cleanup_cas() {
    // Cleanup/reintroduction cause/effect graph: C1 an exact owned Repository
    // retirement is durable; C2 exact credential retirement fails once; C3 a
    // same-id manifest command races while the intent remains; C4 the process
    // and SQLite connection restart; C5 the retry succeeds. Effects: E1 failure
    // leaves both Registry participant and root intent durable; E2 C3 conflicts
    // before creating pending truth; E3 reopen discovers and retries the intent;
    // E4 credential is retired before Repository and the queue clears only after
    // both effects; E5 same-id admission succeeds only after that clear CAS.
    //
    // | Rule | Intent | Credential | Same-id command | Restart | Effect |
    // |---|---|---|---|---|---|
    // | R1 | pending | fails | no | no | E1 |
    // | R2 | pending | n/a | yes | no | E2 |
    // | R3 | pending | succeeds | no | yes | E3 + E4 |
    // | R4 | cleared | complete | yes | yes | E5 |
    let dir = tempfile::tempdir().expect("temporary Session repository");
    let path = dir.path().join("repository-retirement.db");
    let path = path.to_string_lossy().to_string();
    let resources_authority =
        awaken_resource_persistence::ephemeral().expect("Resource authorities");
    let registry = resources_authority.authorities().resource_registry();
    let ingress = Arc::new(FaultInjectingCredentialMaterialIngress::default());
    let session_id = "retirement-restart";
    let credential = awaken_credential_contract::CredentialRef {
        id: format!("{}:credential", managed_repository_id(session_id)),
        revision: 13,
    };
    let manifest = repository_retirement_resources(session_id, Some(credential.clone()));
    register_retirement_repository(registry.as_ref(), session_id, true, true);

    {
        let sessions: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path)
                .expect("open Session authority"),
        );
        create(
            sessions.as_ref(),
            session_with_repository_retirement(session_id, true, manifest.clone()),
        )
        .await;
        let app =
            repository_retirement_application(sessions.clone(), registry.clone(), ingress.clone());

        let before_barrier = sessions.get(session_id).await.unwrap();
        assert!(
            matches!(
                app.replace_session_resource_manifest(
                    session_id,
                    ReplaceSessionResourceManifest {
                        request_fingerprint: awaken_session_contract::stable_fingerprint(&manifest),
                        resources: manifest.clone(),
                        expected_resource_revision: None,
                        idempotency_key: None,
                    },
                )
                .await,
                Err(SessionResourceManifestError::Conflict)
            ),
            "R2/E2"
        );
        assert_eq!(
            sessions.get(session_id).await.unwrap(),
            before_barrier,
            "R2/E2 no root commit"
        );

        ingress.fail_next_retirement();
        let first = app.reconcile_resource_activations().await;
        assert_eq!(first.failures.len(), 1, "R1/E1: {first:?}");
        assert!(
            !sessions
                .get(session_id)
                .await
                .unwrap()
                .resources
                .repository_retirements()
                .is_empty(),
            "R1/E1 durable intent"
        );
        assert_eq!(
            registry
                .find_repository("workspace", &managed_repository_id(session_id))
                .unwrap()
                .unwrap()
                .state,
            awaken_resource_contract::ResourceState::Active,
            "R1/E1 credential-first ordering"
        );
    }

    let sessions: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(&path)
            .expect("reopen Session authority"),
    );
    let app =
        repository_retirement_application(sessions.clone(), registry.clone(), ingress.clone());
    let restarted = app.reconcile_resource_activations().await;
    assert!(restarted.failures.is_empty(), "R3/E3-E4: {restarted:?}");
    assert!(
        sessions
            .get(session_id)
            .await
            .unwrap()
            .resources
            .repository_retirements()
            .is_empty(),
        "R3/E4 clear CAS"
    );
    assert_eq!(
        ingress.retirements(),
        vec![credential],
        "R3/E4 exact Vault pin"
    );
    assert_eq!(
        registry
            .find_repository("workspace", &managed_repository_id(session_id))
            .unwrap()
            .unwrap()
            .state,
        awaken_resource_contract::ResourceState::Deleted,
        "R3/E4 Repository follows Vault"
    );

    let admitted = app
        .replace_session_resource_manifest(
            session_id,
            ReplaceSessionResourceManifest {
                request_fingerprint: awaken_session_contract::stable_fingerprint(&manifest),
                resources: manifest.clone(),
                expected_resource_revision: None,
                idempotency_key: None,
            },
        )
        .await
        .expect("R4/E5 admission after cleanup CAS");
    assert_eq!(
        admitted.session.resources.pending.as_ref(),
        Some(&manifest),
        "R4/E5"
    );
}

#[tokio::test]
async fn root_mutation_cause_effect_decision_table() {
    // Cause-effect graph: C1 expected revision is current; C2 idempotency key
    // and payload hash replay exactly; C3 expected revision is stale; C4 an
    // existing key is reused with another hash. Effects: E1 apply once and
    // advance revision; E2 replay current truth without another advance; E3
    // conflict; E4 idempotency mismatch. No interface adapter owns a second
    // CAS or retry algorithm.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // |---|---|---|---|---|---|
    // | M1 | yes | no | no | no | E1 applied |
    // | M2 | no | yes | no | no | E2 replayed |
    // | M3 | no | no | yes | no | E3 conflict |
    // | M4 | no | no | no | yes | E4 mismatch |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("mutation", false, "idle")).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let original = repo.get("mutation").await.expect("fixture");
    let mut candidate = original.clone();
    candidate.title = Some("applied".into());
    let payload = awaken_session_contract::SessionMutationPayload::Replace(candidate.clone());
    let record = awaken_session_contract::IdempotencyRecord {
        key: "mutation:one".into(),
        payload_hash: payload.stable_hash(),
    };

    let (applied, changed) = app
        .commit_session_snapshot_with_record(
            "workspace",
            candidate.clone(),
            record.clone(),
            Vec::new(),
        )
        .await
        .expect("M1");
    assert!(changed, "M1");
    assert!(applied.revision > original.revision, "M1");

    let (replayed, changed) = app
        .commit_session_snapshot_with_record(
            "workspace",
            candidate.clone(),
            record.clone(),
            Vec::new(),
        )
        .await
        .expect("M2");
    assert!(!changed, "M2");
    assert_eq!(replayed.revision, applied.revision, "M2");

    let stale = app
        .commit_session_snapshot("workspace", candidate.clone(), "stale", Vec::new())
        .await;
    assert_eq!(stale, Err(SessionMutationError::Conflict), "M3");

    let mismatch = app
        .commit_session_snapshot_with_record(
            "workspace",
            candidate,
            awaken_session_contract::IdempotencyRecord {
                key: record.key,
                payload_hash: "another-payload".into(),
            },
            Vec::new(),
        )
        .await;
    assert_eq!(
        mismatch,
        Err(SessionMutationError::IdempotencyMismatch),
        "M4"
    );
}

/// Session-root insertion graph. C1 identity is unused; C2 key/hash/payload
/// exactly replay even when a fresh lowering differs; C3 an existing key carries another hash; C4 another key
/// targets an existing identity. Effects are E1 one insert/revision advance,
/// E2 replay of the current durable aggregate, E3 idempotency mismatch, and E4 identity
/// conflict. The application is the only repository-result classifier.
///
/// | Rule | Identity | Key/hash | Payload | Effect |
/// |---|---|---|---|---|
/// | C1 | unused | new | original | E1 |
/// | C2 | existing | exact | different lowering | E2 |
/// | C3 | existing | same key/different hash | changed | E3 |
/// | C4 | existing | another key | any | E4 |
#[tokio::test]
async fn create_session_root_classifies_insert_replay_and_conflicts() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let original = persisted("create-root", false, "preparing");
    let payload = awaken_session_contract::SessionMutationPayload::Replace(original.clone());
    let record = awaken_session_contract::IdempotencyRecord {
        key: "create-root:one".into(),
        payload_hash: payload.stable_hash(),
    };

    let inserted = app
        .create_session_root("workspace", original.clone(), record.clone(), Vec::new())
        .await
        .expect("C1");
    let awaken_session_contract::SessionCreateResult::Applied(inserted) = inserted else {
        panic!("C1 must apply")
    };
    assert_eq!(
        inserted.revision,
        awaken_session_contract::SessionRevision(1),
        "C1"
    );
    let mut durable = inserted.clone();
    durable.title = Some("durable-after-create".into());
    let durable = app
        .commit_session_snapshot("workspace", durable, "test-create-replay", Vec::new())
        .await
        .expect("prepare C2 durable aggregate");
    let mut lowered_again = original.clone();
    lowered_again.title = Some("new-lowering-must-not-win".into());
    let replayed = app
        .create_session_root("workspace", lowered_again, record.clone(), Vec::new())
        .await
        .expect("C2");
    let awaken_session_contract::SessionCreateResult::Replayed(replayed) = replayed else {
        panic!("C2 must replay")
    };
    assert_eq!(replayed, durable, "C2");

    let mut changed = original.clone();
    changed.title = Some("changed".into());
    assert!(
        matches!(
            app.create_session_root(
                "workspace",
                changed,
                awaken_session_contract::IdempotencyRecord {
                    key: record.key,
                    payload_hash: "changed-hash".into(),
                },
                Vec::new(),
            )
            .await,
            Err(crate::SessionCreationError::IdempotencyMismatch)
        ),
        "C3"
    );
    assert!(
        matches!(
            app.create_session_root(
                "workspace",
                original,
                awaken_session_contract::IdempotencyRecord {
                    key: "create-root:another".into(),
                    payload_hash: "another-hash".into(),
                },
                Vec::new(),
            )
            .await,
            Err(crate::SessionCreationError::Conflict)
        ),
        "C4"
    );
}

/// Profiled Repository-release cause/effect graph. C1 asserted owner scope is
/// exact or foreign; C2 binding_id resolves to exactly one active writable
/// Repository or is unknown; C3 the archive root CAS is fresh or an exact
/// response-loss replay; C4 the expectation is exact or conflicts with the
/// frozen intent; C5 the Runtime has a child; C6 a Completed publication outcome
/// is bound to this Session or foreign; C7 publication is accepted, permanently
/// rejected, or unavailable. Effects: E1 foreign/unknown selectors mutate
/// nothing; E2 one atomic archive fence owns the intent; E3 local effects order
/// child -> publication receipt CAS -> root finalizer; E4 exact replay returns
/// the same durable receipt without another effect; E5 a different expectation
/// conflicts without rewriting the receipt; E6 a foreign Completed outcome
/// fails before any Runtime effect or root mutation; E7 a permanent rejection is
/// durable before ordinary cleanup; E8 its exact replay has no second effect; E9
/// an unavailable dependency retains the command without root cleanup; E10
/// retry publishes and completes ordinary cleanup.
///
/// | Rule | scope | binding | replay | expectation | Effect |
/// |---|---|---|---|---|---|
/// | R1 | foreign | exact | no | exact | E1 |
/// | R2 | exact | unknown | no | exact | E1 |
/// | R3 | exact | exact | no | exact | E2 + E3 |
/// | R4 | exact | exact | yes | exact | E4 |
/// | R5 | exact | exact | yes | different | E5 |
/// | R5-foreign | exact | exact | foreign Completed | exact | E6 |
/// | R6 | exact | exact | no | stale prior | E7 |
/// | R7 | exact | exact | rejected replay | stale prior | E8 |
/// | R8 | exact | exact | no | unavailable | E9 |
/// | R9 | exact | exact | retry | exact | E10 |
#[tokio::test]
async fn profiled_archive_publishes_one_selected_repository_before_root_cleanup() {
    // Constraint: the Session root and its SessionCleanupOperation are the only
    // intent/receipt authority; selector resolution is repeated from each CAS
    // winner and the archived replay uses that same frozen intent.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("profiled Repository publication repository"),
    );
    let mut session = persisted("profiled-publication", false, "idle");
    session.resources = awaken_session_contract::SessionResourceState::from_active(
        repository_resources("source", "repo-1"),
    );
    create(repo.as_ref(), session).await;
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    *runtime.delegated_snapshot.lock().unwrap() = awaken_session_contract::DelegatedRunSnapshot {
        delegated_runs: Vec::new(),
        coordinated_thread_ids: vec![awaken_agent_contract::agent::thread::Id(
            "profiled-publication-child".into(),
        )],
        watermark: 5,
        runtime_commit_cursor: 8,
    };
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );
    let fact = awaken_session_contract::ManagedLifecycleFact {
        id: "profiled-publication:terminated".into(),
        object_id: "profiled-publication".into(),
        workspace_id: Some("workspace".into()),
        event_type: "session.terminated".into(),
        timestamp: 1,
        runtime_interval: None,
    };
    let command = SessionArchiveWithRepositoryPublicationCommand {
        owner_scope: "workspace".into(),
        session_id: "profiled-publication".into(),
        archived_at: "2026-08-28T00:00:00Z".into(),
        lifecycle_fact: fact,
        repository: SessionRepositoryPublicationSelector {
            binding_id: awaken_resource_contract::BindingId::from("source"),
            expectation: awaken_provisioning_contract::RepositoryPublicationExpectation {
                branch: "awf/issue-coding".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                expected_prior_commit: None,
            },
        },
    };

    let mut foreign_scope = command.clone();
    foreign_scope.owner_scope = "another-workspace".into();
    assert!(
        matches!(
            application
                .archive_with_repository_publication(foreign_scope)
                .await,
            Err(SessionArchiveWithRepositoryPublicationError::NotFound)
        ),
        "R1/E1"
    );
    let mut unknown_binding = command.clone();
    unknown_binding.repository.binding_id = awaken_resource_contract::BindingId::from("unknown");
    assert!(
        matches!(
            application
                .archive_with_repository_publication(unknown_binding)
                .await,
            Err(SessionArchiveWithRepositoryPublicationError::Rejected(_))
        ),
        "R2/E1"
    );
    assert_eq!(
        repo.get("profiled-publication").await.unwrap().execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "R1-R2/E1"
    );

    let first = application
        .archive_with_repository_publication(command.clone())
        .await
        .expect("R3 exact release");
    assert!(first.mutation.transitioned, "R3/E2");
    assert_eq!(
        *runtime.terminal_effect_order.lock().unwrap(),
        vec![
            "cleanup:profiled-publication-child".to_string(),
            "publication".to_string(),
            "cleanup:profiled-publication".to_string(),
        ],
        "R3/E3"
    );
    let replay = application
        .archive_with_repository_publication(command.clone())
        .await
        .expect("R4 response-loss replay");
    assert!(!replay.mutation.transitioned, "R4/E4");
    assert_eq!(
        replay.publication_receipt, first.publication_receipt,
        "R4/E4"
    );
    assert_eq!(
        runtime.terminal_effect_order.lock().unwrap().len(),
        3,
        "R4/E4 no duplicate effect"
    );

    let mut conflicting = command.clone();
    conflicting.repository.expectation.commit = "fedcba9876543210fedcba9876543210fedcba98".into();
    assert!(
        matches!(
            application
                .archive_with_repository_publication(conflicting)
                .await,
            Err(SessionArchiveWithRepositoryPublicationError::Conflict)
        ),
        "R5/E5"
    );
    assert_eq!(
        repo.get("profiled-publication")
            .await
            .unwrap()
            .terminal_cleanup
            .repository_publication_receipt("profiled-publication")
            .unwrap(),
        Some(&first.publication_receipt),
        "R5/E5"
    );

    // A custom repository adapter cannot use a self-consistent receipt from a
    // foreign Session to trigger the Completed fast path. The aggregate-bound
    // verifier runs before any Runtime effect or root mutation.
    let mut foreign_completed = repo.get("profiled-publication").await.unwrap();
    foreign_completed.session_id = "profiled-foreign-outcome".into();
    let faulting_repo = Arc::new(FaultingSessionRepository::new(repo.clone()));
    faulting_repo.return_get_once(foreign_completed);
    let guarded = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        faulting_repo,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );
    let effects_before_foreign = runtime.terminal_effect_order.lock().unwrap().len();
    assert!(
        matches!(
            guarded
                .release_terminal_resources("workspace", "profiled-foreign-outcome")
                .await,
            Err(SessionPreparationError::Rejected(_))
        ),
        "R5 foreign Completed outcome fails closed"
    );
    assert_eq!(
        runtime.terminal_effect_order.lock().unwrap().len(),
        effects_before_foreign,
        "R5 foreign Completed outcome cannot suppress into cleanup/tombstone"
    );

    // Permanent publication rejection is durable before ordinary root cleanup;
    // exact profiled replay returns the same rejection without a second Git call.
    let mut rejected_session = persisted("profiled-rejected", false, "idle");
    rejected_session.resources = awaken_session_contract::SessionResourceState::from_active(
        repository_resources("source", "repo-1"),
    );
    create(repo.as_ref(), rejected_session).await;
    runtime.reject_publication.store(true, Ordering::SeqCst);
    let mut rejected = command.clone();
    rejected.session_id = "profiled-rejected".into();
    rejected.lifecycle_fact.id = "profiled-rejected:terminated".into();
    rejected.lifecycle_fact.object_id = "profiled-rejected".into();
    rejected.repository.expectation.expected_prior_commit =
        Some("1111111111111111111111111111111111111111".into());
    let before_rejection = runtime.terminal_effect_order.lock().unwrap().len();
    let first_rejection = application
        .archive_with_repository_publication(rejected.clone())
        .await;
    assert!(
        matches!(
            first_rejection,
            Err(SessionArchiveWithRepositoryPublicationError::Rejected(ref error))
                if error.code == "repository_publication_rejected"
        ),
        "R6 durable permanent rejection"
    );
    let rejected_durable = repo.get("profiled-rejected").await.unwrap();
    assert!(
        rejected_durable.terminal_cleanup.is_completed(),
        "R6 root cleanup"
    );
    assert!(
        rejected_durable
            .terminal_cleanup
            .repository_publication_rejection("profiled-rejected")
            .unwrap()
            .is_some(),
        "R6 rejection durable before completion"
    );
    let after_rejection = runtime.terminal_effect_order.lock().unwrap().len();
    let replay_rejection = application
        .archive_with_repository_publication(rejected)
        .await;
    assert!(
        matches!(
            replay_rejection,
            Err(SessionArchiveWithRepositoryPublicationError::Rejected(ref error))
                if error.code == "repository_publication_rejected"
        ),
        "R7 exact rejection replay"
    );
    assert_eq!(
        runtime.terminal_effect_order.lock().unwrap().len(),
        after_rejection,
        "R7 no second publication or cleanup effect"
    );
    assert!(after_rejection > before_rejection, "R6 effects executed");

    // Dependency loss records no outcome and therefore cannot expose ordinary
    // root cleanup. The same frozen command is retried and then completes.
    runtime.reject_publication.store(false, Ordering::SeqCst);
    runtime.fail_publication_once.store(true, Ordering::SeqCst);
    let mut unavailable_session = persisted("profiled-unavailable", false, "idle");
    unavailable_session.resources = awaken_session_contract::SessionResourceState::from_active(
        repository_resources("source", "repo-1"),
    );
    create(repo.as_ref(), unavailable_session).await;
    let mut unavailable = command;
    unavailable.session_id = "profiled-unavailable".into();
    unavailable.lifecycle_fact.id = "profiled-unavailable:terminated".into();
    unavailable.lifecycle_fact.object_id = "profiled-unavailable".into();
    assert!(
        matches!(
            application
                .archive_with_repository_publication(unavailable.clone())
                .await,
            Err(SessionArchiveWithRepositoryPublicationError::Pending(_))
        ),
        "R8 unavailable remains pending"
    );
    let pending = repo.get("profiled-unavailable").await.unwrap();
    assert!(
        !pending.terminal_cleanup.is_completed(),
        "R8 no root cleanup"
    );
    assert!(
        pending
            .terminal_cleanup
            .publication_command("profiled-unavailable")
            .unwrap()
            .is_some(),
        "R8 exact command retained"
    );
    application
        .archive_with_repository_publication(unavailable)
        .await
        .expect("R9 retry publishes and completes ordinary cleanup");
}

/// Terminal-transition cause/effect graph. C1 execution is Idle or Running; C2
/// Running belongs to an ordinary root activity or an admitted child activity
/// (including a not-yet-settled requires-action boundary); C3 two public archive
/// commands race; C4 activity admission races public archive at the root CAS; C5
/// the Session is already Archived; C6 the caller is the internal force-terminal
/// owner; C7 delete targets a live, archived, or failed Session. Effects: E1 an
/// Idle public archive commits one fence/fact and cleanup; E2 Running public
/// archive is rejected without changing revision or active epochs; E3 a CAS race
/// commits either Running or Archived, never a mixed state; E4 terminal replay is
/// an exact no-op; E5 forced termination clears activity and uses the same cleanup;
/// E6 delete records hidden terminal truth with recoverable cleanup.
///
/// | Rule | Durable state | Command | Race/replay | Effect |
/// |---|---|---|---|---|
/// | L1 | Idle | public archive | duplicate race | E1 |
/// | L2 | Archived | public archive | replay | E4 |
/// | L3 | Running(root activity) | public archive | none | E2 |
/// | L4 | Running(active child/requires-action) | public archive | none | E2 |
/// | L5 | Idle | public archive vs activity | CAS race | E3 |
/// | L6 | Running | internal force terminate | none | E5 |
/// | L7 | Idle | delete | none | E6 |
/// | L8 | Archived | delete | none | E6 |
/// | L9 | ActivationFailed | delete | none | E6 |
#[tokio::test]
async fn terminal_transition_decision_table_is_durable_and_idempotent() {
    // Constraint/Invariant: the Session repository CAS and terminal lifecycle are
    // the only transition authority; replays cannot create another terminal fact.
    // Decision rule: execute L1-L9 and require every accepted, blocked, raced,
    // forced, and delete effect in the table above.
    let durable_repo: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let repo = Arc::new(FaultingSessionRepository::new(durable_repo));
    create(repo.as_ref(), persisted("archive-race", false, "idle")).await;
    create(repo.as_ref(), persisted("archive-running", false, "idle")).await;
    create(
        repo.as_ref(),
        persisted("archive-child-active", false, "idle"),
    )
    .await;
    create(
        repo.as_ref(),
        persisted("archive-activity-race", false, "idle"),
    )
    .await;
    create(repo.as_ref(), persisted("archive-force", false, "idle")).await;
    for session_id in ["delete-live", "archive-work"] {
        let mut session = persisted(session_id, true, "idle");
        let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut session.baseline
        else {
            unreachable!("fixture baseline")
        };
        // L1/L7 exercise the synchronous local cleanup receipt while retaining
        // a self-hosted Work projection. Remote receipt transport has its own
        // exact-lease decision table in realization tests.
        baseline.runtime_placement = SessionRuntimePlacement::Local;
        create(repo.as_ref(), session).await;
    }
    create(
        repo.as_ref(),
        persisted("delete-failed", false, "activation_failed"),
    )
    .await;
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let app = application(repo.clone(), environments.clone());
    let fact = |id: &str, event_type: &str| awaken_session_contract::ManagedLifecycleFact {
        id: format!("{id}:{event_type}"),
        object_id: id.into(),
        workspace_id: Some("workspace".into()),
        event_type: event_type.into(),
        timestamp: 1,
        runtime_interval: None,
    };
    let app = Arc::new(app);

    let archive_fact = fact("archive-race", "session.terminated");
    let (first, second) = tokio::join!(
        app.terminate_session("archive-race", "2026-08-06T00:00:00Z", archive_fact.clone()),
        app.terminate_session("archive-race", "2026-08-06T00:00:00Z", archive_fact)
    );
    let first = first.expect("L1 first");
    let second = second.expect("L1 second");
    assert_ne!(first.transitioned, second.transitioned, "L1");
    let archived = repo.get("archive-race").await.expect("L1 durable");
    assert_eq!(archived.execution.as_str(), "terminated", "L1");
    let revision = archived.revision;
    let replay = app
        .terminate_session(
            "archive-race",
            "another-timestamp",
            fact("archive-race", "session.terminated"),
        )
        .await
        .expect("L2");
    assert!(!replay.transitioned, "L2");
    assert_eq!(replay.session.revision, revision, "L2");

    let root_running = app
        .begin_activity("archive-running")
        .await
        .expect("L3 open root activity");
    let root_rejected = app
        .terminate_session(
            "archive-running",
            "2026-08-06T00:00:00Z",
            fact("archive-running", "session.terminated"),
        )
        .await;
    assert!(
        matches!(
            root_rejected,
            Err(SessionPreparationError::Rejected(ref error))
                if error.kind == awaken_session_contract::RunErrorKind::BadRequest
                    && error.message == "only an idle Session may be archived"
        ),
        "L3/E2: {root_rejected:?}"
    );
    assert_eq!(
        repo.get("archive-running").await.expect("L3 durable"),
        root_running,
        "L3/E2 rejection preserves the exact activity receipt"
    );

    // The Session root intentionally stores no child registry. This exact
    // operation-scoped activity epoch is the authoritative aggregate evidence
    // that an admitted child/requires-action boundary is still active.
    let (child_running, child_epoch) = app
        .begin_activity_for_operation("archive-child-active", "child-requires-action")
        .await
        .expect("L4 open child activity");
    let child_rejected = app
        .terminate_session(
            "archive-child-active",
            "2026-08-06T00:00:00Z",
            fact("archive-child-active", "session.terminated"),
        )
        .await;
    assert!(
        matches!(
            child_rejected,
            Err(SessionPreparationError::Rejected(ref error))
                if error.kind == awaken_session_contract::RunErrorKind::BadRequest
        ),
        "L4/E2: {child_rejected:?}"
    );
    let child_after = repo.get("archive-child-active").await.expect("L4 durable");
    assert_eq!(child_after, child_running, "L4/E2");
    assert!(
        child_after.active_activity_epochs.contains(&child_epoch),
        "L4/E2 active child completion owner remains durable"
    );

    repo.commit_running_activity_then_conflict_once("archive");
    let race_archive = app
        .terminate_session(
            "archive-activity-race",
            "2026-08-06T00:00:00Z",
            fact("archive-activity-race", "session.terminated"),
        )
        .await;
    assert!(
        matches!(
            race_archive,
            Err(SessionPreparationError::Rejected(ref error))
                if error.kind == awaken_session_contract::RunErrorKind::BadRequest
        ),
        "L5/E3: {race_archive:?}"
    );
    let race_running = repo
        .get("archive-activity-race")
        .await
        .expect("L5 Running CAS winner");
    assert_eq!(race_running.execution.as_str(), "running", "L5/E3");
    assert_eq!(race_running.active_activity_epochs.len(), 1, "L5/E3");
    assert!(
        !race_running.needs_resource_reconciliation(),
        "L5/E3 archive retry must not apply terminal cleanup to the Running winner"
    );

    let forced_running = app
        .begin_activity("archive-force")
        .await
        .expect("L6 open internal activity");
    assert!(forced_running.has_active_activities(), "L6 precondition");
    let forced = app
        .force_terminate_session(
            "archive-force",
            "2026-08-06T00:00:00Z",
            fact("archive-force", "session.terminated"),
        )
        .await
        .expect("L6 force terminal");
    assert!(forced.transitioned, "L6/E5");
    assert!(forced.session.is_terminal(), "L6/E5");
    assert!(forced.session.active_activity_epochs.is_empty(), "L6/E5");

    let deleted = app
        .delete_session(SessionDeleteCommand::new("delete-live"))
        .await
        .expect("L7");
    let transition = deleted.clone();
    app.release_terminal_resources(&transition.owner_scope, "delete-live")
        .await
        .expect("L7 reconciliation");
    assert!(transition.transitioned, "L7");
    assert_eq!(transition.session.execution.as_str(), "terminated", "L7");
    assert!(transition.session.is_hidden(), "L7");
    assert!(transition.session.needs_resource_reconciliation(), "L7");
    let tombstone = repo.get("delete-live").await.expect_err("L7 tombstone");
    assert!(matches!(
        tombstone,
        awaken_session_contract::SessionRepositoryError::NotFound
    ));
    assert!(
        environments
            .retired
            .lock()
            .unwrap()
            .iter()
            .any(|session_id| session_id == "delete-live"),
        "L7 terminal truth retires its one Work projection"
    );
    app.terminate_session(
        "archive-work",
        "2026-08-06T00:00:00Z",
        fact("archive-work", "session.terminated"),
    )
    .await
    .expect("L1 archive cleanup");
    app.terminate_session(
        "archive-work",
        "2026-08-06T00:00:00Z",
        fact("archive-work", "session.terminated"),
    )
    .await
    .expect("L2 archive replay");
    assert_eq!(
        environments
            .retired
            .lock()
            .unwrap()
            .iter()
            .filter(|session_id| session_id.as_str() == "archive-work")
            .count(),
        1,
        "L1/L2 completed archive cleanup does not retire Work twice"
    );
    let archived_delete = app
        .commit_delete_intent(SessionDeleteCommand::new("archive-race"))
        .await
        .expect("L8 archived Session is deletable");
    assert!(archived_delete.transitioned, "L8");
    assert!(archived_delete.session.is_hidden(), "L8");
    let failed_delete = app
        .commit_delete_intent(SessionDeleteCommand::new("delete-failed"))
        .await
        .expect("L9 failed Session is deletable");
    assert!(failed_delete.transitioned, "L9");
    assert_eq!(
        failed_delete.session.execution,
        SessionExecutionState::ActivationFailed,
        "L9 execution failure remains audit truth"
    );
}

#[tokio::test]
async fn terminal_edges_wait_for_every_accepted_event_effect_anchor() {
    // Cause/effect graph: C1 terminal edge is public archive, internal force,
    // or Delete; C2 an accepted Event is queued or its external effect exists
    // while the root processed+anchor CAS is still pending; C3 that exact CAS
    // later commits. E1 every edge returns retryable unavailable and preserves
    // the nonterminal aggregate; E2 the existing supervisor remains the owner;
    // E3 retry after C3 commits exactly one terminal transition.
    //
    // | Rule | edge | incomplete root entry | anchor committed | Effect |
    // | B1 | archive | yes | no | E1+E2 |
    // | B2 | force | yes | no | E1+E2 |
    // | B3 | delete | yes | no | E1+E2 |
    // | B4 | each | no | yes | E3 |
    //
    // `processed=false` intentionally covers both a merely queued command and
    // crash-after-effect/before-root-CAS: the effect owner may make replay a
    // no-op, but terminal admission cannot infer that fact or drop provenance.
    let repository: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Session repository"),
    );
    for session_id in [
        "batch-before-archive",
        "batch-before-force",
        "batch-before-delete",
    ] {
        create(
            repository.as_ref(),
            session_with_unsettled_event(session_id),
        )
        .await;
    }
    let app = application(
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let fact = |session_id: &str| awaken_session_contract::ManagedLifecycleFact {
        id: format!("{session_id}:terminated"),
        object_id: session_id.into(),
        workspace_id: Some("workspace".into()),
        event_type: "session.status_terminated".into(),
        timestamp: 1,
        runtime_interval: None,
    };

    let archive = app
        .terminate_session(
            "batch-before-archive",
            "2026-08-24T00:00:00Z",
            fact("batch-before-archive"),
        )
        .await;
    let force = app
        .force_terminate_session(
            "batch-before-force",
            "2026-08-24T00:00:00Z",
            fact("batch-before-force"),
        )
        .await;
    let delete = app
        .commit_delete_intent(SessionDeleteCommand::new("batch-before-delete"))
        .await;
    for (rule, result) in [("B1", archive), ("B2", force), ("B3", delete)] {
        assert!(
            matches!(
                result,
                Err(SessionPreparationError::Rejected(ref error))
                    if error.kind == awaken_session_contract::RunErrorKind::Unavailable
                        && error.code == "session_event_batch_pending"
            ),
            "{rule}: {result:?}"
        );
    }
    for session_id in [
        "batch-before-archive",
        "batch-before-force",
        "batch-before-delete",
    ] {
        let durable = repository.get(session_id).await.expect("B1-B3 durable");
        assert!(!durable.is_terminal(), "B1-B3 preserve {session_id}");
        assert!(durable.needs_event_reconciliation(), "B1-B3 retain owner");
        anchor_unsettled_event(&app, repository.as_ref(), session_id).await;
    }

    assert!(
        app.terminate_session(
            "batch-before-archive",
            "2026-08-24T00:00:00Z",
            fact("batch-before-archive"),
        )
        .await
        .expect("B4 archive retry")
        .transitioned
    );
    assert!(
        app.force_terminate_session(
            "batch-before-force",
            "2026-08-24T00:00:00Z",
            fact("batch-before-force"),
        )
        .await
        .expect("B4 force retry")
        .transitioned
    );
    assert!(
        app.commit_delete_intent(SessionDeleteCommand::new("batch-before-delete"))
            .await
            .expect("B4 delete retry")
            .transitioned
    );
}

#[tokio::test]
async fn delete_fact_is_committed_once_at_the_fence_and_not_reemitted_by_tombstone() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("delete-fact-once", false, "idle")).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let transition = app
        .commit_delete_intent(SessionDeleteCommand::new("delete-fact-once"))
        .await
        .expect("Delete fence");
    let facts = repo.pending_lifecycle().await.expect("Delete outbox");
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].id, "session:delete-fact-once:deleted");
    repo.complete_lifecycle(&facts[0].id)
        .await
        .expect("consumer acknowledged Delete fact");

    app.release_terminal_resources(&transition.owner_scope, "delete-fact-once")
        .await
        .expect("verified cleanup and tombstone");
    assert!(repo.pending_lifecycle().await.unwrap().is_empty());
    assert!(matches!(
        repo.get("delete-fact-once").await,
        Err(awaken_session_contract::SessionRepositoryError::NotFound)
    ));
}

#[tokio::test]
async fn session_work_authority_classifies_scope_before_queue_access() {
    // Cause/effect graph: C1 no Session root; C2 a Cloud Session; C3 a
    // self-hosted nonterminal Session whose stopped Work can be revived by a
    // claimed successor; C4 terminal self-hosted Session; C5 renewal without a
    // Run claim. Effects: E1/C1 and
    // E1/C2 are outside the Work ownership boundary; E2/C3 reuses the canonical
    // wake path then leases once; E3/C4 and E3/C5 stay unowned. FMECA: without E2, a
    // predecessor retire racing a successor wake loses the approved input;
    // applying E2 to C4 resurrects terminal effects; applying it to C5 revives
    // settled Work with no Run left to release it and blocks the Environment.
    //
    // | Rule | Session | Environment | Queue result | Effect |
    // |---|---|---|---|---|
    // | W1 | absent | n/a | not called | NotRequired |
    // | W2 | present | Cloud | not called | NotRequired |
    // | W3 | present | self-hosted/nonterminal | stopped | wake + Leased |
    // | W4 | present | self-hosted/terminal | stopped | Unowned; no wake |
    // | W5 | present | self-hosted/nonterminal renewal | stopped | Unowned; no wake |
    use awaken_session_contract::work_queue::{
        SessionWorkAcquisition, SessionWorkLeaseAuthority, SessionWorkOwnership,
    };

    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("cloud-work", false, "idle")).await;
    create(repo.as_ref(), persisted("self-work", true, "idle")).await;
    create(repo.as_ref(), persisted("renewal-work", true, "idle")).await;
    create(
        repo.as_ref(),
        persisted("terminal-work", true, "terminated"),
    )
    .await;
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let app = application(repo, environments.clone());

    assert_eq!(
        app.acquire_session_work(
            "missing",
            "owner",
            1,
            SessionWorkAcquisition::RealizationRenewal,
        )
        .await
        .expect("W1"),
        SessionWorkOwnership::NotRequired,
        "W1"
    );
    assert_eq!(
        app.acquire_session_work(
            "cloud-work",
            "owner",
            1,
            SessionWorkAcquisition::RealizationRenewal,
        )
        .await
        .expect("W2"),
        SessionWorkOwnership::NotRequired,
        "W2"
    );
    let SessionWorkOwnership::Leased(lease) = app
        .acquire_session_work("self-work", "owner", 1, SessionWorkAcquisition::ClaimedRun)
        .await
        .expect("W3")
    else {
        panic!("W3 must revive and lease the stopped Session Work");
    };
    assert_eq!(lease.owner, "owner", "W3");
    assert!(
        environments.awakened.lock().unwrap().contains("self-work"),
        "W3"
    );
    assert_eq!(
        app.acquire_session_work(
            "terminal-work",
            "owner",
            1,
            SessionWorkAcquisition::ClaimedRun,
        )
        .await
        .expect("W4"),
        SessionWorkOwnership::Unowned,
        "W4"
    );
    assert!(
        !environments
            .awakened
            .lock()
            .unwrap()
            .contains("terminal-work"),
        "W4"
    );
    assert_eq!(
        app.acquire_session_work(
            "renewal-work",
            "renewal-owner",
            2,
            SessionWorkAcquisition::RealizationRenewal,
        )
        .await
        .expect("W5"),
        SessionWorkOwnership::Unowned,
        "W5"
    );
    assert!(
        !environments
            .awakened
            .lock()
            .unwrap()
            .contains("renewal-work"),
        "W5"
    );
}

/// Activity-fence FMECA cause/effect graph. Causes: C1 the Session exists; C2 it
/// is ready (idle/running or Worker-owned preparing); C3 the epoch can advance;
/// C4 settlement names an admitted active epoch; C5 other active epochs remain;
/// C6 completion is duplicate/unknown; C7 a terminal transition races. Effects: E1 every
/// admission advances the monotonic environment fence and joins one interval;
/// E2 settlement removes exactly its epoch regardless of completion order; E3
/// only the last active settlement commits Idle and the one interval fact; E4
/// duplicate/stale/terminal completions are no-ops; E5 invalid admissions do not
/// mutate truth; E6 terminal intent clears all active epochs and closes the same
/// interval; E7 a Worker-owned initial activity stays Preparing until realization.
///
/// | Rule | Active before | Completion/admission | Terminal | Effect |
/// |---|---|---|---|---|
/// | A1 | none | two admissions | no | E1, two distinct active epochs |
/// | A2 | oldest+newest | oldest completes first | no | E2, newest remains Running |
/// | A3 | newest | newest completes last | no | E3, Idle + one fact |
/// | A4 | oldest+newest | newest completes first | no | E2, oldest remains Running |
/// | A5 | oldest | oldest completes last | no | E3, Idle + one fact |
/// | A6 | any | duplicate/unknown completion | no | E4, exact no-op |
/// | A7 | any | completion/admission | terminal | E4/E5/E6 |
/// | A8 | none | admission | exhausted/missing/not-ready | E5 |
/// | A9 | none | Worker admission | preparing | E1/E7 |
#[tokio::test]
async fn activity_fence_decision_table_preserves_monotonic_and_terminal_truth() {
    // Constraint/Invariant: one monotonic root activity epoch fences all
    // admission/completion and terminal truth dominates every replay. Decision rule:
    // execute A1-A9 to cover completion order, duplicates, terminal state,
    // policy rejection, and Worker preparation.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repo.as_ref(),
        persisted("activity-oldest-first", false, "idle"),
    )
    .await;
    create(
        repo.as_ref(),
        persisted("activity-newest-first", false, "idle"),
    )
    .await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let (first, second) = tokio::join!(
        app.begin_activity("activity-oldest-first"),
        app.begin_activity("activity-oldest-first")
    );
    let first = first.expect("A1 first admission");
    let second = second.expect("A1 concurrent admission");
    let mut epochs = [first.activity_epoch, second.activity_epoch];
    epochs.sort_unstable();
    assert_eq!(epochs, [1, 2], "A1");
    let active = repo
        .get("activity-oldest-first")
        .await
        .expect("A1 durable Session");
    assert_eq!(active.execution.as_str(), "running", "A1");
    assert_eq!(
        active.active_activity_epochs,
        std::collections::BTreeSet::from(epochs),
        "A1"
    );
    let first_interval = active
        .running_interval
        .clone()
        .expect("A1 one durable interval");
    assert_eq!(first_interval.activity_epoch, epochs[0], "A1 joins overlap");

    let oldest_settled = app
        .settle_activity("activity-oldest-first", epochs[0])
        .await
        .expect("A2 oldest completion");
    assert_eq!(oldest_settled.execution.as_str(), "running", "A2");
    assert_eq!(oldest_settled.activity_epoch, epochs[1], "A2");
    assert_eq!(
        oldest_settled.active_activity_epochs,
        std::collections::BTreeSet::from([epochs[1]]),
        "A2"
    );
    assert_eq!(
        oldest_settled.running_interval,
        Some(first_interval.clone()),
        "A2"
    );

    let idle = app
        .settle_activity("activity-oldest-first", epochs[1])
        .await
        .expect("A3 newest completes last");
    assert_eq!(idle.execution.as_str(), "idle", "A3");
    assert!(idle.active_activity_epochs.is_empty(), "A3");
    assert!(idle.running_interval.is_none(), "A3");
    let pending = repo.pending_lifecycle().await.expect("A3 outbox");
    let closed = pending
        .iter()
        .find(|fact| fact.object_id == "activity-oldest-first")
        .expect("A3 one interval fact")
        .runtime_interval
        .as_ref()
        .expect("A3 typed interval");
    assert_eq!(closed.interval_id, first_interval.interval_id, "A3");
    assert!(closed.ended_at_unix_ms >= closed.started_at_unix_ms, "A3");

    let (first, second) = tokio::join!(
        app.begin_activity("activity-newest-first"),
        app.begin_activity("activity-newest-first")
    );
    let mut reverse_epochs = [
        first.expect("A4 first admission").activity_epoch,
        second.expect("A4 second admission").activity_epoch,
    ];
    reverse_epochs.sort_unstable();
    let newest_settled = app
        .settle_activity("activity-newest-first", reverse_epochs[1])
        .await
        .expect("A4 newest completion");
    assert_eq!(
        newest_settled.execution,
        SessionExecutionState::Running,
        "A4"
    );
    assert_eq!(
        newest_settled.active_activity_epochs,
        std::collections::BTreeSet::from([reverse_epochs[0]]),
        "A4"
    );
    let duplicate = app
        .settle_activity("activity-newest-first", reverse_epochs[1])
        .await
        .expect("A6 duplicate completion");
    assert_eq!(duplicate, newest_settled, "A6 duplicate is an exact no-op");
    let unknown = app
        .settle_activity("activity-newest-first", u64::MAX)
        .await
        .expect("A6 unknown completion");
    assert_eq!(unknown, newest_settled, "A6 unknown is an exact no-op");
    let reverse_idle = app
        .settle_activity("activity-newest-first", reverse_epochs[0])
        .await
        .expect("A5 oldest completes last");
    assert_eq!(reverse_idle.execution, SessionExecutionState::Idle, "A5");
    assert!(reverse_idle.active_activity_epochs.is_empty(), "A5");
    assert!(reverse_idle.running_interval.is_none(), "A5");
    let pending = repo.pending_lifecycle().await.expect("A5 outbox");
    assert_eq!(
        pending
            .iter()
            .filter(
                |fact| fact.object_id == "activity-newest-first" && fact.runtime_interval.is_some()
            )
            .count(),
        1,
        "A5 exactly one interval fact"
    );

    let running = app
        .begin_activity("activity-oldest-first")
        .await
        .expect("A7 activity before terminal transition");
    app.force_terminate_session(
        "activity-oldest-first",
        "2026-08-11T00:00:00Z",
        awaken_session_contract::ManagedLifecycleFact {
            id: "activity-terminal".into(),
            object_id: "activity-oldest-first".into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.status_terminated".into(),
            timestamp: 1,
            runtime_interval: None,
        },
    )
    .await
    .expect("A7 terminal transition");
    let terminated = repo
        .get("activity-oldest-first")
        .await
        .expect("A7 durable terminal cleanup");
    assert!(terminated.active_activity_epochs.is_empty(), "A7/E6");
    assert!(terminated.running_interval.is_none(), "A7/E6");
    let fenced = app
        .settle_activity("activity-oldest-first", running.activity_epoch)
        .await
        .expect("A7 terminal settlement is idempotent");
    assert_eq!(fenced, terminated, "A7");
    assert_eq!(
        app.begin_activity("activity-oldest-first").await,
        Err(SessionActivityError::Terminal),
        "A7"
    );

    let mut exhausted = persisted("activity-exhausted", false, "idle");
    exhausted.activity_epoch = u64::MAX;
    create(repo.as_ref(), exhausted).await;
    let exhausted_before = repo
        .get("activity-exhausted")
        .await
        .expect("A8 durable Session before admission");
    assert_eq!(
        app.begin_activity("activity-exhausted").await,
        Err(SessionActivityError::EpochExhausted),
        "A8"
    );
    assert_eq!(
        repo.get("activity-exhausted")
            .await
            .expect("A8 durable Session"),
        exhausted_before,
        "A8"
    );
    assert_eq!(
        app.begin_activity("missing").await,
        Err(SessionActivityError::NotFound),
        "A8"
    );

    create(
        repo.as_ref(),
        persisted("activity-preparing", false, "preparing"),
    )
    .await;
    assert_eq!(
        app.begin_activity("activity-preparing").await,
        Err(SessionActivityError::NotReady),
        "A8/E5"
    );
    let still_preparing = repo.get("activity-preparing").await.expect("A8 durable");
    assert_eq!(still_preparing.activity_epoch, 0, "A8/E5");
    assert_eq!(still_preparing.execution.as_str(), "preparing", "A8/E5");

    let mut worker_preparing = persisted("activity-worker-preparing", false, "preparing");
    let awaken_session_contract::SessionBaselineState::Frozen(baseline) =
        &mut worker_preparing.baseline
    else {
        unreachable!("fixture is frozen")
    };
    baseline.runtime_placement = SessionRuntimePlacement::Worker;
    create(repo.as_ref(), worker_preparing).await;
    let admitted = app
        .begin_activity("activity-worker-preparing")
        .await
        .expect("A9 Worker claim must be triggered by the driving event");
    assert_eq!(admitted.activity_epoch, 1, "A9/E1");
    assert_eq!(
        admitted.active_activity_epochs,
        std::collections::BTreeSet::from([1]),
        "A9/E7"
    );
    assert_eq!(
        admitted.execution,
        SessionExecutionState::Preparing,
        "A9/E7"
    );

    let mut failed = still_preparing;
    failed.execution = SessionExecutionState::ActivationFailed;
    let failed = app
        .commit_session_snapshot(
            "workspace",
            failed,
            "activity-test-realization-failed",
            Vec::new(),
        )
        .await
        .expect("A7 terminal realization");
    assert_eq!(
        app.settle_activity("activity-preparing", 0)
            .await
            .expect("A7 stale settlement"),
        failed,
        "A7/E4"
    );
}

#[tokio::test]
async fn recovered_scalar_activity_is_settled_or_superseded_without_parallel_truth() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Recovery cause/effect graph: C1 a Running aggregate has a monotonic scalar
    // epoch and an explicitly empty active set; C2 its current epoch settles;
    // C3 a successor is admitted first; C4 the predecessor/successor completion
    // arrives. Effects: E1 C2 treats the scalar as one recoverable activity and
    // closes Idle; E2 C3 installs only the successor in the active set; E3 the
    // predecessor completion is fenced; E4 the successor closes Idle. Rules:
    // A10=C1+C2=>E1; A11=C1+C3+C4(old)=>E2+E3; A12=C1+C3+C4(new)=>E4.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let mut recovered = persisted("activity-recovered-settle", false, "running");
    recovered.activity_epoch = 7;
    assert!(recovered.begin_runtime_interval(1), "A10 fixture");
    assert!(recovered.active_activity_epochs.is_empty(), "A10/C1");
    create(repo.as_ref(), recovered).await;
    let idle = app
        .settle_activity("activity-recovered-settle", 7)
        .await
        .expect("A10 settle");
    assert_eq!(idle.execution, SessionExecutionState::Idle, "A10/E1");
    assert!(idle.running_interval.is_none(), "A10/E1");

    let mut recovered = persisted("activity-recovered-successor", false, "running");
    recovered.activity_epoch = 9;
    assert!(recovered.begin_runtime_interval(1), "A11 fixture");
    create(repo.as_ref(), recovered).await;
    let successor = app
        .begin_activity("activity-recovered-successor")
        .await
        .expect("A11 successor");
    assert_eq!(successor.activity_epoch, 10, "A11/E2");
    assert_eq!(
        successor.active_activity_epochs,
        std::collections::BTreeSet::from([10]),
        "A11/E2"
    );
    assert_eq!(
        app.settle_activity("activity-recovered-successor", 9)
            .await
            .expect("A11 predecessor fenced"),
        successor,
        "A11/E3"
    );
    let idle = app
        .settle_activity("activity-recovered-successor", 10)
        .await
        .expect("A12 successor completion");
    assert_eq!(idle.execution, SessionExecutionState::Idle, "A12/E4");
    assert!(idle.running_interval.is_none(), "A12/E4");
}

#[tokio::test]
async fn operation_activity_replay_uses_the_root_receipt_as_its_only_epoch_authority() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 operation receipt absent/present; C2 its epoch is
    // active/already settled; C3 concurrent exact retry; C4 distinct operation.
    // Effects: E1 first admission uses its target root revision as epoch; E2 an
    // exact retry returns that receipt revision without another mutation; E3 a
    // retry after settlement never reopens activity; E4 a distinct operation
    // gets a later monotonic epoch. The repository receipt is the only
    // operation→epoch mapping—there is no companion map to reconcile.
    //
    // | Rule | Receipt | Activity | Request | Effect |
    // | O1 | absent | idle | first op-a | E1 |
    // | O2 | races absent/present | active | concurrent op-a | E2 |
    // | O3 | present | settled | retry op-a | E3 |
    // | O4 | absent | idle | op-b | E4 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repo.as_ref(),
        persisted("activity-operation", false, "idle"),
    )
    .await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let (left, right) = tokio::join!(
        app.begin_activity_for_operation("activity-operation", "op-a"),
        app.begin_activity_for_operation("activity-operation", "op-a")
    );
    let (left_session, left_epoch) = left.expect("O1/O2 left");
    let (right_session, right_epoch) = right.expect("O1/O2 right");
    assert_eq!(left_epoch, right_epoch, "O2/E2 exact concurrent replay");
    let active = repo.get("activity-operation").await.expect("O1 state");
    assert_eq!(
        active.active_activity_epochs,
        std::collections::BTreeSet::from([left_epoch]),
        "O1/O2 one durable activity"
    );
    assert!(
        left_session.revision.0 == left_epoch || right_session.revision.0 == right_epoch,
        "O1/E1 first committed target revision is the epoch"
    );

    let settled = app
        .settle_activity("activity-operation", left_epoch)
        .await
        .expect("O3 settle");
    let settled_revision = settled.revision;
    let (replayed, replayed_epoch) = app
        .begin_activity_for_operation("activity-operation", "op-a")
        .await
        .expect("O3 exact replay");
    assert_eq!(replayed_epoch, left_epoch, "O3/E3");
    assert_eq!(replayed.revision, settled_revision, "O3/E3 no mutation");
    assert!(replayed.active_activity_epochs.is_empty(), "O3/E3");

    let (next, next_epoch) = app
        .begin_activity_for_operation("activity-operation", "op-b")
        .await
        .expect("O4 distinct operation");
    assert!(next_epoch > left_epoch, "O4/E4");
    assert_eq!(next.revision.0, next_epoch, "O4/E4");
}

#[tokio::test]
async fn coordinated_agent_admission_preserves_activity_across_ambiguous_delivery() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 Runtime admission is accepted, definitively rejected,
    // ambiguously unavailable, or returns a mismatched receipt; C2 an ambiguous
    // operation is retried with the same identity; C3 its committed child boundary
    // later arrives; C4 a terminal child atomically admits its deterministic
    // primary report continuation; C5 that report Run later reaches its committed
    // boundary. Effects: E1 accepted work retains one active epoch; E2 only a
    // BadRequest settles immediately; E3 unavailable/internal outcomes retain the
    // epoch as durable recovery evidence; E4 exact retry reuses the same child Run
    // and epoch; E5 Awaiting settles that one epoch exactly; E6 C4 transfers the
    // same epoch without an intermediate Idle; E7 C5 settles it exactly once;
    // E8 admission reserves 24 ordinary-child slots because the public 25-Thread
    // limit includes the primary Thread (Advisor consultations are exempt).
    //
    // | Rule | Runtime result | Exact retry | Boundary | Effect |
    // |---|---|---|---|---|
    // | S1 | accepted | no | pending | E1+E8 |
    // | S2 | BadRequest | no | none | E2 |
    // | S3 | Unavailable | accepted | pending | E3+E4 |
    // | S4 | mismatched receipt | no | unknown | E3 |
    // | S5 | S3 accepted retry | yes | Awaiting | E5 |
    // | S6 | accepted | no | child Ended | E6 |
    // | S7 | report accepted | no | primary Ended | E7 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Agent admission repository"),
    );
    for id in [
        "send-accepted",
        "send-rejected",
        "send-ambiguous",
        "send-mismatch",
    ] {
        create(repo.as_ref(), coordinated_session(id)).await;
    }

    let accepted_runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
        AgentAdmissionOutcome::Accepted,
    ]));
    let accepted = configured_admission_application(repo.clone(), accepted_runtime.clone());
    awaken_session_contract::SessionAgentCoordination::send_session_agent_message(
        &accepted,
        coordination_spawn_command("send-accepted"),
    )
    .await
    .expect("S1 accepted");
    let accepted_epoch = accepted_runtime.admissions.lock().unwrap()[0].session_activity_epoch;
    assert_eq!(
        accepted_runtime.admissions.lock().unwrap()[0].max_unarchived_threads,
        24,
        "S1/E8 primary plus ordinary children cannot exceed 25 Threads"
    );
    assert_eq!(
        repo.get("send-accepted")
            .await
            .expect("S1 state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([accepted_epoch]),
        "S1/E1"
    );

    let rejected_runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
        AgentAdmissionOutcome::BadRequest,
    ]));
    let rejected = configured_admission_application(repo.clone(), rejected_runtime);
    let rejected_error =
        awaken_session_contract::SessionAgentCoordination::send_session_agent_message(
            &rejected,
            coordination_spawn_command("send-rejected"),
        )
        .await
        .expect_err("S2 definitive rejection");
    assert_eq!(
        rejected_error.kind,
        awaken_session_contract::RunErrorKind::BadRequest
    );
    assert!(
        repo.get("send-rejected")
            .await
            .expect("S2 state")
            .active_activity_epochs
            .is_empty(),
        "S2/E2"
    );

    let ambiguous_runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
        AgentAdmissionOutcome::Unavailable,
        AgentAdmissionOutcome::Accepted,
    ]));
    let ambiguous = configured_admission_application(repo.clone(), ambiguous_runtime.clone());
    awaken_session_contract::SessionAgentCoordination::send_session_agent_message(
        &ambiguous,
        coordination_spawn_command("send-ambiguous"),
    )
    .await
    .expect_err("S3 ambiguous response");
    let ambiguous_epoch = ambiguous_runtime.admissions.lock().unwrap()[0].session_activity_epoch;
    assert_eq!(
        repo.get("send-ambiguous")
            .await
            .expect("S3 state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([ambiguous_epoch]),
        "S3/E3"
    );
    awaken_session_contract::SessionAgentCoordination::send_session_agent_message(
        &ambiguous,
        coordination_spawn_command("send-ambiguous"),
    )
    .await
    .expect("S3/S4 exact retry accepted");
    let (child, run) = {
        let admissions = ambiguous_runtime.admissions.lock().unwrap();
        assert_eq!(admissions.len(), 2, "S3/S4 two delivery attempts");
        assert_eq!(admissions[0].thread_id, admissions[1].thread_id, "S4/E4");
        assert_eq!(admissions[0].run_id, admissions[1].run_id, "S4/E4");
        assert_eq!(
            admissions[0].session_activity_epoch, admissions[1].session_activity_epoch,
            "S4/E4"
        );
        (
            admissions[0].thread_id.clone(),
            admissions[0].run_id.clone(),
        )
    };

    let mismatch_runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
        AgentAdmissionOutcome::MismatchedReceipt,
    ]));
    let mismatch = configured_admission_application(repo.clone(), mismatch_runtime.clone());
    let mismatch_error =
        awaken_session_contract::SessionAgentCoordination::send_session_agent_message(
            &mismatch,
            coordination_spawn_command("send-mismatch"),
        )
        .await
        .expect_err("S4 mismatched receipt");
    assert_eq!(
        mismatch_error.kind,
        awaken_session_contract::RunErrorKind::Internal
    );
    let mismatch_epoch = mismatch_runtime.admissions.lock().unwrap()[0].session_activity_epoch;
    assert_eq!(
        repo.get("send-mismatch")
            .await
            .expect("S4 state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([mismatch_epoch]),
        "S4/E3"
    );

    ambiguous_runtime.commit_boundary("send-ambiguous", &child, &run, RunState::Awaiting, "");
    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &ambiguous,
        awaken_session_contract::SessionAgentBoundaryCommand {
            session_id: "send-ambiguous".into(),
            source_thread_id: child,
            source_run_id: run,
            source_agent_id: "coord-child".into(),
            session_activity_epoch: ambiguous_epoch,
            cancellation_requested: false,
        },
    )
    .await
    .expect("S5 committed Awaiting boundary");
    assert!(
        repo.get("send-ambiguous")
            .await
            .expect("S5 state")
            .active_activity_epochs
            .is_empty(),
        "S5/E5"
    );

    let accepted_admission = accepted_runtime.admissions.lock().unwrap()[0].clone();
    accepted_runtime.commit_boundary(
        "send-accepted",
        &accepted_admission.thread_id,
        &accepted_admission.run_id,
        RunState::Ended(EndCause::NaturalEnd),
        "research complete",
    );
    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &accepted,
        awaken_session_contract::SessionAgentBoundaryCommand {
            session_id: "send-accepted".into(),
            source_thread_id: accepted_admission.thread_id,
            source_run_id: accepted_admission.run_id,
            source_agent_id: "coord-child".into(),
            session_activity_epoch: accepted_epoch,
            cancellation_requested: false,
        },
    )
    .await
    .expect("S6 terminal child hands off to the report continuation");
    {
        let continuations = accepted_runtime.continuations.lock().unwrap();
        assert_eq!(continuations.len(), 1, "S6/E6 one report continuation");
        assert_eq!(
            continuations[0].session_activity_epoch, accepted_epoch,
            "S6/E6 transfers the exact child epoch"
        );
        assert!(
            continuations[0]
                .message
                .text_content()
                .contains("research complete"),
            "S6/E6 report is derived from the committed child snapshot"
        );
    }
    let handed_off = repo.get("send-accepted").await.expect("S6 state");
    assert_eq!(
        handed_off.active_activity_epochs,
        std::collections::BTreeSet::from([accepted_epoch]),
        "S6/E6 no intermediate aggregate Idle"
    );
    assert_eq!(
        handed_off.execution,
        SessionExecutionState::Running,
        "S6/E6"
    );

    accepted_runtime.commit_boundary(
        "send-accepted",
        &ThreadId("send-accepted".into()),
        &RunId("coord-report-run".into()),
        RunState::Ended(EndCause::NaturalEnd),
        "coordinator acknowledged report",
    );
    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &accepted,
        awaken_session_contract::SessionAgentBoundaryCommand {
            session_id: "send-accepted".into(),
            source_thread_id: ThreadId("send-accepted".into()),
            source_run_id: RunId("coord-report-run".into()),
            source_agent_id: "coord-root".into(),
            session_activity_epoch: accepted_epoch,
            cancellation_requested: false,
        },
    )
    .await
    .expect("S7 primary report boundary settles the transferred epoch");
    let completed = repo.get("send-accepted").await.expect("S7 state");
    assert!(completed.active_activity_epochs.is_empty(), "S7/E7");
    assert_eq!(completed.execution, SessionExecutionState::Idle, "S7/E7");
    assert_eq!(
        accepted_runtime.continuations.lock().unwrap().len(),
        1,
        "S7/E7 primary boundary must not recurse into another report"
    );
}

#[tokio::test]
async fn recovered_child_settlement_uses_the_claimed_dispatch_agent_without_a_parent_link() {
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a child has a committed Completed Run and non-empty
    // report; C2 the crash-frozen claimed dispatch identifies an Agent that is
    // in the Session's exact roster, unknown, the Advisor sentinel, or blank;
    // C3 the process stopped after child enqueue but before the parent
    // send_message result committed, so the rebuildable relationship query is
    // empty. Effects: E1 C1+C2(roster)+C3 admits one deterministic primary
    // report and retains the exact activity until that Run settles; E2 every
    // non-roster identity fails closed, admits no report, and retains its
    // activity for exact settlement retry. Constraint: the claimed dispatch
    // snapshot is execution identity; this test creates no link fallback or
    // second child registry.
    //
    // | Rule | Parent link | Dispatch Agent | Frozen roster | Effect |
    // |---|---|---|---|---|
    // | R1 | absent | coord-child | member | E1 |
    // | R2 | absent | foreign-agent | absent | E2 |
    // | R3 | absent | __awaken_advisor__ | absent | E2 |
    // | R4 | absent | blank/legacy | absent | E2 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("child settlement recovery repository"),
    );
    let cases = [
        ("R1", "coord-child", true),
        ("R2", "foreign-agent", false),
        ("R3", "__awaken_advisor__", false),
        ("R4", "", false),
    ];
    for (rule, _, _) in cases {
        create(
            repo.as_ref(),
            coordinated_session(&format!("settlement-{rule}")),
        )
        .await;
    }
    let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
    let app = configured_admission_application(repo.clone(), runtime.clone());

    for (rule, source_agent_id, accepted) in cases {
        let session_id = format!("settlement-{rule}");
        let child = ThreadId(format!("settlement-child-{rule}"));
        let run = RunId(format!("settlement-run-{rule}"));
        let (_, epoch) = app
            .begin_activity_for_operation(&session_id, &format!("settlement-operation-{rule}"))
            .await
            .expect("activity precondition");
        runtime.commit_boundary(
            &session_id,
            &child,
            &run,
            RunState::Ended(EndCause::NaturalEnd),
            format!("recovered report {rule}"),
        );
        assert!(
            awaken_session_contract::SessionRuntime::coordinated_threads(
                runtime.as_ref(),
                &session_id,
            )
            .await
            .expect("relationship projection")
            .is_empty(),
            "{rule}/C3"
        );

        let result =
            awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
                &app,
                awaken_session_contract::SessionAgentBoundaryCommand {
                    session_id: session_id.clone(),
                    source_thread_id: child,
                    source_run_id: run,
                    source_agent_id: source_agent_id.into(),
                    session_activity_epoch: epoch,
                    cancellation_requested: false,
                },
            )
            .await;
        let session = repo.get(&session_id).await.expect("settled Session");
        if accepted {
            result.expect("R1/E1 frozen dispatch Agent is admitted");
            assert_eq!(runtime.continuations.lock().unwrap().len(), 1, "R1/E1");
            assert!(session.active_activity_epochs.contains(&epoch), "R1/E1");
        } else {
            let error = result.expect_err("R2-R4/E2 unknown identity fails closed");
            assert_eq!(
                error.kind,
                awaken_session_contract::RunErrorKind::BadRequest,
                "{rule}/E2"
            );
            assert_eq!(runtime.continuations.lock().unwrap().len(), 1, "{rule}/E2");
            assert!(session.active_activity_epochs.contains(&epoch), "{rule}/E2");
        }
    }
}

#[tokio::test]
async fn coordinated_follow_up_admission_uses_existing_thread_authorities() {
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use awaken_agent_contract::agent::run::Failure;

    // Cause/effect graph: C1 an ordinary coordinated Thread exists; C2 its
    // latest committed Run Completed, Cancelled, or Failed; C3 a follow-up is
    // submitted after that durable boundary; C4 the target relationship is an
    // ordinary Agent, Advisor, or absent; C5 the ordinary Thread disposition is
    // Active/Archived. Effects: E1 Completed/Cancelled ordinary Agents admit a
    // fresh Run on the same Thread; E2 Failed, Advisor, unknown, and Archived
    // targets reject before opening a Session activity or calling Runtime
    // admission. Constraints: RunState and ThreadDisposition remain the only
    // failure/archive authorities, and the committed relationship projection is
    // the only target-kind authority; the application stores no parallel flag.
    //
    // | Rule | Latest lifecycle | Follow-up | Effects |
    // |---|---|---|---|
    // | A1 | Completed | submitted | E1 accepted |
    // | A2 | Cancelled | submitted | E1 accepted |
    // | A3 | Failed(Error) | submitted | E2 BadRequest/no admission |
    // | A4 | any | unknown relationship | E2 BadRequest/no admission |
    // | A5 | any | Advisor relationship | E2 BadRequest/no admission |
    // | A6 | any | Archived ordinary Thread | E2 BadRequest/no admission |
    for (rule, state, accepted) in [
        ("A1", RunState::Ended(EndCause::NaturalEnd), true),
        ("A2", RunState::Ended(EndCause::Cancelled), true),
        (
            "A3",
            RunState::Ended(EndCause::Error(Failure::Inference {
                code: "unauthorized".into(),
                message: "terminal child failure".into(),
            })),
            false,
        ),
    ] {
        let session_id = format!("follow-up-{rule}");
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("follow-up repository"),
        );
        create(repo.as_ref(), coordinated_session(&session_id)).await;
        let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
            AgentAdmissionOutcome::Accepted,
            AgentAdmissionOutcome::Accepted,
        ]));
        let app = configured_admission_application(repo.clone(), runtime.clone());
        let receipt =
            awaken_session_contract::SessionAgentCoordination::send_session_agent_message(
                &app,
                coordination_spawn_command(&session_id),
            )
            .await
            .expect("spawn precondition");
        let first = runtime.admissions.lock().unwrap()[0].clone();
        runtime.commit_boundary(
            &session_id,
            &receipt.thread_id,
            &first.run_id,
            state,
            "terminal transcript",
        );
        let active_before = repo
            .get(&session_id)
            .await
            .expect("activity before follow-up")
            .active_activity_epochs;
        let follow_up = awaken_session_contract::SessionAgentMessageCommand {
            session_id: session_id.clone(),
            source_thread_id: ThreadId(session_id.clone()),
            source_run_id: RunId(format!("{session_id}-parent-follow-up")),
            source_call_id: format!("{rule}-follow-up-call"),
            operation_id: format!("{rule}-follow-up-operation"),
            target: awaken_session_contract::SessionAgentTarget::ExistingThread {
                thread_id: receipt.thread_id.clone(),
            },
            message: "continue".into(),
        };
        let result = awaken_session_contract::SessionAgentCoordination::send_session_agent_message(
            &app, follow_up,
        )
        .await;
        if accepted {
            let accepted_receipt = result.expect(rule);
            assert_eq!(accepted_receipt.thread_id, receipt.thread_id, "{rule}/E1");
            let admissions = runtime.admissions.lock().unwrap();
            assert_eq!(admissions.len(), 2, "{rule}/E1");
            assert_eq!(
                admissions[1].intent,
                awaken_session_contract::CoordinatedRunIntent::FollowUp,
                "{rule}/E1"
            );
        } else {
            let error = result.expect_err("A3 Failed must reject");
            assert_eq!(
                error.kind,
                awaken_session_contract::RunErrorKind::BadRequest,
                "A3/E2"
            );
            assert_eq!(runtime.admissions.lock().unwrap().len(), 1, "A3/E2");
            assert_eq!(
                repo.get(&session_id)
                    .await
                    .expect("A3 activity after rejection")
                    .active_activity_epochs,
                active_before,
                "A3/E2 admission rejection opens no activity"
            );
        }
    }

    for rule in ["A4", "A5", "A6"] {
        let session_id = format!("follow-up-{rule}");
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("follow-up target repository"),
        );
        create(repo.as_ref(), coordinated_session(&session_id)).await;
        let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
        let thread_id = ThreadId(format!("{session_id}-target"));
        match rule {
            "A4" => {}
            "A5" => runtime.commit_link(awaken_session_contract::CoordinatedThreadLink {
                session_id: session_id.clone(),
                thread_id: thread_id.clone(),
                target: awaken_session_contract::CoordinatedThreadTarget::Advisor {
                    model: "advisor-model".into(),
                },
                created_by_operation_id: "advisor-operation".into(),
                latest_run_id: Some(RunId("advisor-run".into())),
            }),
            "A6" => {
                runtime.commit_link(awaken_session_contract::CoordinatedThreadLink {
                    session_id: session_id.clone(),
                    thread_id: thread_id.clone(),
                    target: awaken_session_contract::CoordinatedThreadTarget::Agent {
                        agent_id: "coord-child".into(),
                    },
                    created_by_operation_id: "archived-agent-operation".into(),
                    latest_run_id: Some(RunId("archived-agent-run".into())),
                });
                runtime.archive_thread(&session_id, &thread_id);
            }
            _ => unreachable!("closed decision table"),
        }
        let app = configured_admission_application(repo.clone(), runtime.clone());
        let active_before = repo
            .get(&session_id)
            .await
            .expect("target state before rejection")
            .active_activity_epochs;
        let error = awaken_session_contract::SessionAgentCoordination::send_session_agent_message(
            &app,
            awaken_session_contract::SessionAgentMessageCommand {
                session_id: session_id.clone(),
                source_thread_id: ThreadId(session_id.clone()),
                source_run_id: RunId(format!("{session_id}-parent-run")),
                source_call_id: format!("{rule}-call"),
                operation_id: format!("{rule}-operation"),
                target: awaken_session_contract::SessionAgentTarget::ExistingThread { thread_id },
                message: "must reject before activity".into(),
            },
        )
        .await
        .expect_err("non-ordinary/closed target must reject");
        assert_eq!(
            error.kind,
            awaken_session_contract::RunErrorKind::BadRequest,
            "{rule}/E2"
        );
        assert!(
            runtime.admissions.lock().unwrap().is_empty(),
            "{rule}/E2 no Runtime admission"
        );
        assert_eq!(
            repo.get(&session_id)
                .await
                .expect("target state after rejection")
                .active_activity_epochs,
            active_before,
            "{rule}/E2 no leaked Session activity"
        );
    }
}

#[tokio::test]
async fn foreground_resume_reuses_exact_ticket_coordination_path() {
    // Cause/effect graph: C1 asserted owner matches/does not match the durable
    // owner; C2 operation identity is empty/stable; C3 asserted tool id
    // matches/does not match the committed ResumeTicket; C4 reply kind matches
    // the ticket; C5 durable delivery accepts/definitively rejects. Effects: E1
    // unauthorized or invalid requests create neither delivery nor Session
    // activity; E2 an accepted request publishes one Primary delivery containing
    // the exact Run/correlation/Thread version and one receipt-backed activity
    // epoch; E3 the result is observed from committed truth; E4 definitive
    // rejection settles the transferred epoch. The fake's legacy inline
    // `resume` methods always fail, so E2+E3 also prove this API has no second
    // execution path.
    //
    // | Rule | identity | ticket/tool/kind | delivery | Effect |
    // |---|---|---|---|---|
    // | F0 | wrong | stable | exact | n/a | E1 |
    // | F1 | exact | empty | exact | n/a | E1 |
    // | F2 | exact | stable | wrong tool or kind | n/a | E1 |
    // | F3 | exact | stable | exact | accepted | E2+E3 |
    // | F4 | exact | stable | exact | rejected | E4 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("foreground resume repository"),
    );
    for session_id in ["foreground-resume", "foreground-resume-rejected"] {
        create(repo.as_ref(), persisted(session_id, false, "idle")).await;
    }
    let accepted_runtime = Arc::new(RecordingReplyRuntime::new(ReplyRuntimeOutcome::Accepted));
    let accepted = application_with_runtime(
        accepted_runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let allow = || {
        awaken_session_contract::RunResume::Permission(
            awaken_agent_contract::agent::awaiting::PermissionDecision::Allow { note: None },
        )
    };

    assert!(
        accepted
            .resume_session_run_for_owner(
                "foreign-workspace",
                "foreground-request-foreign",
                "foreground-resume",
                "reply-tool",
                allow(),
            )
            .await
            .is_err(),
        "F0 foreign owner fails closed"
    );

    for (operation_id, tool_use_id, resume) in [
        ("", "reply-tool", allow()),
        ("foreground-request-wrong-tool", "other-tool", allow()),
        (
            "foreground-request-wrong-kind",
            "reply-tool",
            awaken_session_contract::RunResume::ClientResult {
                content: Vec::new(),
                is_error: false,
            },
        ),
    ] {
        assert!(
            accepted
                .resume_session_run(operation_id, "foreground-resume", tool_use_id, resume,)
                .await
                .is_err(),
            "F1/F2 invalid request fails closed"
        );
    }
    assert!(
        accepted_runtime.deliveries.lock().unwrap().is_empty(),
        "F0/F1/F2/E1"
    );
    assert!(
        repo.get("foreground-resume")
            .await
            .expect("F0/F1/F2 state")
            .active_activity_epochs
            .is_empty(),
        "F0/F1/F2/E1"
    );

    let outcome = accepted
        .resume_session_run(
            "foreground-request-accepted",
            "foreground-resume",
            "reply-tool",
            allow(),
        )
        .await
        .expect("F3 accepted foreground resume");
    assert!(
        matches!(
            outcome.state(),
            awaken_agent_contract::agent::run::RunState::Ended(_)
        ),
        "F3/E3 committed outcome observation"
    );
    let delivery = accepted_runtime.deliveries.lock().unwrap()[0].clone();
    assert_eq!(
        delivery.command.target,
        awaken_session_contract::SessionThreadTarget::Primary,
        "F3/E2"
    );
    assert_eq!(delivery.command.expected_run_id.0, "reply-run", "F3/E2");
    assert_eq!(
        delivery.command.expected_correlation_id, "reply-correlation",
        "F3/E2"
    );
    assert_eq!(delivery.command.expected_thread_version, Some(1), "F3/E2");
    assert_eq!(
        repo.get("foreground-resume")
            .await
            .expect("F3 state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([delivery.session_activity_epoch]),
        "F3/E2"
    );

    let rejected = application_with_runtime(
        Arc::new(RecordingReplyRuntime::new(ReplyRuntimeOutcome::BadRequest)),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(
        rejected
            .resume_session_run(
                "foreground-request-rejected",
                "foreground-resume-rejected",
                "reply-tool",
                allow(),
            )
            .await
            .is_err(),
        "F4 rejection surfaces"
    );
    let rejected_state = repo
        .get("foreground-resume-rejected")
        .await
        .expect("F4 state");
    assert!(rejected_state.active_activity_epochs.is_empty(), "F4/E4");
    assert_eq!(
        rejected_state.execution,
        SessionExecutionState::Idle,
        "F4/E4"
    );
}

#[tokio::test]
async fn coordinated_reply_activity_follows_acceptance_and_ambiguity_decision_table() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the Runtime accepts/rejects/ambiguously fails reply
    // delivery; C2 the same reply operation is first/replayed; C3 its committed
    // child boundary is pending/arrives. Effects: E1 acceptance leaves exactly
    // the attached epoch active; E2 exact replay reuses that epoch without a
    // second active entry; E3 an explicit caller rejection settles it; E4 a
    // dependency failure retains it as retry evidence; E5 the later committed
    // Awaiting boundary settles the accepted epoch and returns Session to Idle;
    // E6 Primary skips child topology validation but uses the same fence,
    // transfer, and delivery path.
    //
    // | Rule | Runtime | Operation | Boundary | Effect |
    // |---|---|---|---|---|
    // | P1 | accepted | first | pending | E1 |
    // | P2 | accepted | exact replay | pending | E2 |
    // | P3 | bad request | first | none | E3 |
    // | P4 | unavailable | first | unknown | E4 |
    // | P4R | unavailable | exact retry after CAS | unknown | E2+E4 |
    // | P5 | accepted | first/replay | Awaiting | E5 |
    // | P6 | accepted Primary | first | pending | E6 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("reply activity repository"),
    );
    for session_id in [
        "reply-accepted",
        "reply-rejected",
        "reply-ambiguous",
        "reply-primary",
    ] {
        create(repo.as_ref(), persisted(session_id, false, "idle")).await;
    }
    let command = |session_id: &str| awaken_session_contract::SessionThreadToolReplyCommand {
        session_id: session_id.to_string(),
        tool_request_event_id: None,
        expected_thread_version: None,
        target: awaken_session_contract::SessionThreadTarget::Child(ThreadId("reply-child".into())),
        expected_run_id: RunId("reply-run".into()),
        expected_correlation_id: "reply-correlation".into(),
        tool_use_id: "reply-tool".into(),
        reply: awaken_session_contract::SessionThreadToolReply::Confirm(
            awaken_agent_contract::agent::awaiting::PermissionDecision::Allow { note: None },
        ),
        accompanying_system: None,
    };

    let accepted_runtime = Arc::new(RecordingReplyRuntime::new(ReplyRuntimeOutcome::Accepted));
    let accepted = application_with_runtime(
        accepted_runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    awaken_session_contract::SessionAgentCoordination::reply_session_thread_tool(
        &accepted,
        command("reply-accepted"),
    )
    .await
    .expect("P1 accepted reply");
    let first_epoch = accepted_runtime.deliveries.lock().unwrap()[0].session_activity_epoch;
    let active = repo.get("reply-accepted").await.expect("P1 state");
    assert_eq!(
        active.active_activity_epochs,
        std::collections::BTreeSet::from([first_epoch]),
        "P1/E1"
    );

    awaken_session_contract::SessionAgentCoordination::reply_session_thread_tool(
        &accepted,
        command("reply-accepted"),
    )
    .await
    .expect("P2 exact accepted replay");
    {
        let deliveries = accepted_runtime.deliveries.lock().unwrap();
        assert_eq!(
            deliveries.len(),
            2,
            "P2 Runtime receives an idempotent retry"
        );
        assert_eq!(
            deliveries[1].session_activity_epoch, first_epoch,
            "P2/E2 same receipt-backed epoch"
        );
    }
    assert_eq!(
        repo.get("reply-accepted")
            .await
            .expect("P2 state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([first_epoch]),
        "P2/E2 one active membership"
    );

    let mut primary_command = command("reply-primary");
    primary_command.target = awaken_session_contract::SessionThreadTarget::Primary;
    awaken_session_contract::SessionAgentCoordination::reply_session_thread_tool(
        &accepted,
        primary_command,
    )
    .await
    .expect("P6 accepted Primary reply");
    let primary_delivery = accepted_runtime
        .deliveries
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("P6 delivery");
    assert_eq!(
        primary_delivery.command.target,
        awaken_session_contract::SessionThreadTarget::Primary,
        "P6/E6"
    );
    assert_eq!(
        repo.get("reply-primary")
            .await
            .expect("P6 state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([primary_delivery.session_activity_epoch]),
        "P6/E6"
    );

    let rejected = application_with_runtime(
        Arc::new(RecordingReplyRuntime::new(ReplyRuntimeOutcome::BadRequest)),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(
        awaken_session_contract::SessionAgentCoordination::reply_session_thread_tool(
            &rejected,
            command("reply-rejected"),
        )
        .await
        .is_err(),
        "P3 rejection surfaces"
    );
    let rejected_state = repo.get("reply-rejected").await.expect("P3 state");
    assert!(rejected_state.active_activity_epochs.is_empty(), "P3/E3");
    assert_eq!(
        rejected_state.execution,
        SessionExecutionState::Idle,
        "P3/E3"
    );

    let ambiguous_runtime = Arc::new(RecordingReplyRuntime::new(ReplyRuntimeOutcome::Unavailable));
    let ambiguous = application_with_runtime(
        ambiguous_runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(
        awaken_session_contract::SessionAgentCoordination::reply_session_thread_tool(
            &ambiguous,
            command("reply-ambiguous"),
        )
        .await
        .is_err(),
        "P4 ambiguity surfaces for retry"
    );
    let ambiguous_epoch = ambiguous_runtime.deliveries.lock().unwrap()[0].session_activity_epoch;
    let ambiguous_state = repo.get("reply-ambiguous").await.expect("P4 state");
    assert_eq!(
        ambiguous_state.active_activity_epochs,
        std::collections::BTreeSet::from([ambiguous_epoch]),
        "P4/E4"
    );
    assert_eq!(
        ambiguous_state.execution,
        SessionExecutionState::Running,
        "P4/E4"
    );
    assert!(
        awaken_session_contract::SessionAgentCoordination::reply_session_thread_tool(
            &ambiguous,
            command("reply-ambiguous"),
        )
        .await
        .is_err(),
        "P4R ambiguity remains retryable"
    );
    {
        let ambiguous_deliveries = ambiguous_runtime.deliveries.lock().unwrap();
        assert_eq!(ambiguous_deliveries.len(), 2, "P4R/E2");
        assert_eq!(
            ambiguous_deliveries[1].session_activity_epoch, ambiguous_epoch,
            "P4R/E2 exact crash retry reuses the receipt-backed epoch"
        );
    }
    assert_eq!(
        repo.get("reply-ambiguous")
            .await
            .expect("P4R state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([ambiguous_epoch]),
        "P4R/E4 one retained activity"
    );

    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &accepted,
        awaken_session_contract::SessionAgentBoundaryCommand {
            session_id: "reply-accepted".into(),
            source_thread_id: ThreadId("reply-child".into()),
            source_run_id: RunId("reply-run".into()),
            source_agent_id: "reply-agent".into(),
            session_activity_epoch: first_epoch,
            cancellation_requested: false,
        },
    )
    .await
    .expect("P5 committed Awaiting boundary");
    let settled = repo.get("reply-accepted").await.expect("P5 state");
    assert!(settled.active_activity_epochs.is_empty(), "P5/E5");
    assert_eq!(settled.execution, SessionExecutionState::Idle, "P5/E5");
}

#[tokio::test]
async fn reached_budget_allows_only_the_exact_required_action_continuation() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the one Session budget is reached; C2 a command is
    // ordinary new coordination or a child tool reply; C3 the reply does/does
    // not match Runtime's exact committed Awaiting ticket. Effects: E1 ordinary
    // activity admission remains budget-blocked; E2 the exact reply opens one
    // receipt-backed activity to finish the already-started Run; E3 a forged
    // reply is rejected before any activity mutation. The exception is therefore
    // continuation of one unfinished Run, not a second budget admission path.
    //
    // | Rule | Budget | Command | Exact ticket | Effect |
    // |---|---|---|---|---|
    // | A1 | reached | new operation | n/a | E1 |
    // | A2 | reached | tool reply | yes | E2 |
    // | A3 | reached | tool reply | no | E3 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("required-action budget repository"),
    );
    for session_id in ["reached-valid-reply", "reached-invalid-reply"] {
        let mut session = persisted(session_id, false, "idle");
        session.budget = awaken_session_contract::SessionBudgetState::Active {
            max_list_cost_minor: 1,
            consumed_numerator: awaken_session_contract::SessionBudgetState::MICROS_PER_MINOR_USD
                * awaken_session_contract::SessionBudgetState::COST_DENOMINATOR,
            usage_cursor: Default::default(),
            snapshot: awaken_session_contract::ManagedListPriceSnapshot {
                snapshot_id: "required-action-price-v1".into(),
                version: 1,
                effective_at_unix_ms: 1,
                arithmetic_version: 1,
                model_rates: Default::default(),
                runtime_rates: Default::default(),
                fingerprint: "required-action-price-v1-fingerprint".into(),
            },
            reach_transitions: Vec::new(),
        };
        create(repo.as_ref(), session).await;
    }
    let runtime = Arc::new(RecordingReplyRuntime::new(ReplyRuntimeOutcome::Accepted));
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    assert!(
        matches!(
            app.begin_activity_for_operation("reached-valid-reply", "new-run")
                .await,
            Err(SessionActivityError::BudgetReached)
        ),
        "A1/E1"
    );
    awaken_session_contract::SessionAgentCoordination::reply_session_thread_tool(
        &app,
        awaken_session_contract::SessionThreadToolReplyCommand {
            session_id: "reached-valid-reply".into(),
            tool_request_event_id: None,
            expected_thread_version: None,
            target: awaken_session_contract::SessionThreadTarget::Child(ThreadId(
                "reply-child".into(),
            )),
            expected_run_id: RunId("reply-run".into()),
            expected_correlation_id: "reply-correlation".into(),
            tool_use_id: "reply-tool".into(),
            reply: awaken_session_contract::SessionThreadToolReply::Confirm(
                awaken_agent_contract::agent::awaiting::PermissionDecision::Allow { note: None },
            ),
            accompanying_system: None,
        },
    )
    .await
    .expect("A2 exact Awaiting continuation");
    let admitted = repo.get("reached-valid-reply").await.expect("A2 state");
    assert_eq!(admitted.active_activity_epochs.len(), 1, "A2/E2");
    assert_eq!(runtime.deliveries.lock().unwrap().len(), 1, "A2/E2");

    let before = repo
        .get("reached-invalid-reply")
        .await
        .expect("A3 state before");
    let error = awaken_session_contract::SessionAgentCoordination::reply_session_thread_tool(
        &app,
        awaken_session_contract::SessionThreadToolReplyCommand {
            session_id: "reached-invalid-reply".into(),
            tool_request_event_id: None,
            expected_thread_version: None,
            target: awaken_session_contract::SessionThreadTarget::Child(ThreadId(
                "reply-child".into(),
            )),
            expected_run_id: RunId("reply-run".into()),
            expected_correlation_id: "reply-correlation".into(),
            tool_use_id: "forged-tool".into(),
            reply: awaken_session_contract::SessionThreadToolReply::Confirm(
                awaken_agent_contract::agent::awaiting::PermissionDecision::Allow { note: None },
            ),
            accompanying_system: None,
        },
    )
    .await
    .expect_err("A3 forged reply");
    assert_eq!(
        error.kind,
        awaken_session_contract::RunErrorKind::BadRequest,
        "A3/E3"
    );
    assert_eq!(
        repo.get("reached-invalid-reply")
            .await
            .expect("A3 state after"),
        before,
        "A3/E3"
    );
}

#[tokio::test]
async fn root_activity_cas_is_the_only_budget_admission_choke() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the shared budget is reached; C2 the caller only
    // reads the frozen Agent roster or submits a new child Run; C3 the root CAS
    // has/not opened an activity. Effects: E1 read-only roster access remains
    // available; E2 the new Run fails at the authoritative activity CAS; E3 no
    // Runtime admission or activity epoch is produced. Removing the earlier
    // coordination preflight avoids two competing check-then-act gates while
    // retaining one source of truth and one serialized admission decision.
    //
    // | Rule | Budget | Operation | Root CAS | Effects |
    // |---|---|---|---|---|
    // | G1 | reached | list roster | none | E1 |
    // | G2 | reached | spawn child | rejects | E2+E3 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("budget choke repository"),
    );
    let mut session = coordinated_session("budget-admission-choke");
    session.budget = awaken_session_contract::SessionBudgetState::Active {
        max_list_cost_minor: 1,
        consumed_numerator: awaken_session_contract::SessionBudgetState::MICROS_PER_MINOR_USD
            * awaken_session_contract::SessionBudgetState::COST_DENOMINATOR,
        usage_cursor: Default::default(),
        snapshot: awaken_session_contract::ManagedListPriceSnapshot {
            snapshot_id: "budget-choke-price-v1".into(),
            version: 1,
            effective_at_unix_ms: 1,
            arithmetic_version: 1,
            model_rates: Default::default(),
            runtime_rates: Default::default(),
            fingerprint: "budget-choke-price-v1-fingerprint".into(),
        },
        reach_transitions: Vec::new(),
    };
    create(repo.as_ref(), session).await;
    let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
        AgentAdmissionOutcome::Accepted,
    ]));
    let app = configured_admission_application(repo.clone(), runtime.clone());

    let roster = awaken_session_contract::SessionAgentCoordination::list_session_agents(
        &app,
        "budget-admission-choke",
    )
    .await
    .expect("G1 read-only roster remains visible");
    assert_eq!(roster.len(), 1, "G1/E1");

    let error = awaken_session_contract::SessionAgentCoordination::send_session_agent_message(
        &app,
        coordination_spawn_command("budget-admission-choke"),
    )
    .await
    .expect_err("G2 reached budget");
    assert_eq!(error.code, "budget_reached", "G2/E2");
    assert!(runtime.admissions.lock().unwrap().is_empty(), "G2/E3");
    assert!(
        repo.get("budget-admission-choke")
            .await
            .expect("G2 state")
            .active_activity_epochs
            .is_empty(),
        "G2/E3"
    );
}

#[tokio::test]
async fn child_boundary_settlement_separates_activity_from_terminal_continuation() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 boundary is Awaiting/Ended/Running; C2 Session is
    // live/terminal; C3 roster/runtime continuation dependencies are absent;
    // C4 trusted dispatch provenance marks/does not mark cancellation; C5 a
    // child commits Failed while a later continuation may already be queued.
    // Effects: E1 live Awaiting durably settles the exact activity and never
    // calls the primary continuation; E2 terminal Session accepts late exact or
    // unknown completion as a no-op without reopening publication dependencies;
    // E3 Running is rejected before activity mutation; E4 a cancelled terminal
    // child settles directly and emits no primary continuation; E5 Failed first
    // interrupts the Thread's existing Dispatch queue, then settles directly
    // without manufacturing a successful primary report. Normal Completed
    // report admission remains covered by S6 in the admission decision table.
    //
    // | Rule | Boundary | Session | Dependencies | Effect |
    // | B1 | Awaiting | live | absent | E1 |
    // | B2 | Ended | terminal | absent | E2 |
    // | B3 | Running | live | absent | E3 |
    // | B4 | Ended + cancelled | live | absent | E4 |
    // | B5 | Failed | live | interrupt port | E5 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("boundary repository"),
    );
    create(repo.as_ref(), persisted("boundary-awaiting", false, "idle")).await;
    create(
        repo.as_ref(),
        persisted("boundary-terminal", false, "terminated"),
    )
    .await;
    create(repo.as_ref(), persisted("boundary-running", false, "idle")).await;
    create(
        repo.as_ref(),
        persisted("boundary-interrupted", false, "idle"),
    )
    .await;
    create(repo.as_ref(), persisted("boundary-failed", false, "idle")).await;
    let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let (_, awaiting_epoch) = app
        .begin_activity_for_operation("boundary-awaiting", "boundary-op")
        .await
        .expect("B1 activity");
    runtime.commit_boundary(
        "boundary-awaiting",
        &ThreadId("boundary-child".into()),
        &RunId("boundary-child-run".into()),
        RunState::Awaiting,
        "pending",
    );
    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_contract::SessionAgentBoundaryCommand {
            session_id: "boundary-awaiting".into(),
            source_thread_id: ThreadId("boundary-child".into()),
            source_run_id: RunId("boundary-child-run".into()),
            source_agent_id: "unused-awaiting-agent".into(),
            session_activity_epoch: awaiting_epoch,
            cancellation_requested: false,
        },
    )
    .await
    .expect("B1 settles without roster or continuation");
    let awaiting = repo.get("boundary-awaiting").await.expect("B1 state");
    assert!(awaiting.active_activity_epochs.is_empty(), "B1/E1");
    assert_eq!(awaiting.execution, SessionExecutionState::Idle, "B1/E1");

    let terminal_before = repo.get("boundary-terminal").await.expect("B2 before");
    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_contract::SessionAgentBoundaryCommand {
            session_id: "boundary-terminal".into(),
            source_thread_id: ThreadId("boundary-terminal-child".into()),
            source_run_id: RunId("boundary-terminal-run".into()),
            source_agent_id: "unused-terminal-agent".into(),
            session_activity_epoch: 99,
            cancellation_requested: false,
        },
    )
    .await
    .expect("B2 terminal late settlement");
    assert_eq!(
        repo.get("boundary-terminal").await.expect("B2 after"),
        terminal_before,
        "B2/E2"
    );

    let (_, running_epoch) = app
        .begin_activity_for_operation("boundary-running", "boundary-op")
        .await
        .expect("B3 activity");
    runtime.commit_boundary(
        "boundary-running",
        &ThreadId("boundary-running-child".into()),
        &RunId("boundary-running-run".into()),
        RunState::Running,
        "",
    );
    let rejected =
        awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
            &app,
            awaken_session_contract::SessionAgentBoundaryCommand {
                session_id: "boundary-running".into(),
                source_thread_id: ThreadId("boundary-running-child".into()),
                source_run_id: RunId("boundary-running-run".into()),
                source_agent_id: "unused-running-agent".into(),
                session_activity_epoch: running_epoch,
                cancellation_requested: false,
            },
        )
        .await;
    assert!(rejected.is_err(), "B3/E3");
    assert!(
        repo.get("boundary-running")
            .await
            .expect("B3 state")
            .active_activity_epochs
            .contains(&running_epoch),
        "B3/E3"
    );

    let (_, interrupted_epoch) = app
        .begin_activity_for_operation("boundary-interrupted", "boundary-op")
        .await
        .expect("B4 activity");
    runtime.commit_boundary(
        "boundary-interrupted",
        &ThreadId("boundary-interrupted-child".into()),
        &RunId("boundary-interrupted-run".into()),
        RunState::Ended(EndCause::NaturalEnd),
        "must not become a primary report",
    );
    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_contract::SessionAgentBoundaryCommand {
            session_id: "boundary-interrupted".into(),
            source_thread_id: ThreadId("boundary-interrupted-child".into()),
            source_run_id: RunId("boundary-interrupted-run".into()),
            source_agent_id: "unused-cancelled-agent".into(),
            session_activity_epoch: interrupted_epoch,
            cancellation_requested: true,
        },
    )
    .await
    .expect("B4 cancellation settles without roster/report dependencies");
    let interrupted = repo.get("boundary-interrupted").await.expect("B4 state");
    assert!(interrupted.active_activity_epochs.is_empty(), "B4/E4");
    assert_eq!(interrupted.execution, SessionExecutionState::Idle, "B4/E4");
    assert!(runtime.continuations.lock().unwrap().is_empty(), "B4/E4");

    let (_, failed_epoch) = app
        .begin_activity_for_operation("boundary-failed", "boundary-op")
        .await
        .expect("B5 activity");
    let failed_child = ThreadId("boundary-failed-child".into());
    let failed_run = RunId("boundary-failed-run".into());
    runtime.commit_boundary(
        "boundary-failed",
        &failed_child,
        &failed_run,
        RunState::Ended(EndCause::Error(
            awaken_agent_contract::agent::run::Failure::StateConflict,
        )),
        "must not become a primary report",
    );
    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_contract::SessionAgentBoundaryCommand {
            session_id: "boundary-failed".into(),
            source_thread_id: failed_child.clone(),
            source_run_id: failed_run,
            source_agent_id: "unused-failed-agent".into(),
            session_activity_epoch: failed_epoch,
            cancellation_requested: false,
        },
    )
    .await
    .expect("B5 Failed settles after queue interruption");
    let failed = repo.get("boundary-failed").await.expect("B5 state");
    assert!(failed.active_activity_epochs.is_empty(), "B5/E5");
    assert_eq!(failed.execution, SessionExecutionState::Idle, "B5/E5");
    assert_eq!(
        runtime.interruptions.lock().unwrap().as_slice(),
        &[("boundary-failed".into(), failed_child)],
        "B5/E5 one existing Dispatch cancellation path"
    );
    assert!(runtime.continuations.lock().unwrap().is_empty(), "B5/E5");
}

#[tokio::test]
async fn boundary_redelivery_accepts_an_older_exact_run_without_settling_its_successor() {
    // Crash-window cause/effect graph: C0 the receiving replica's generic
    // latest-Run projection is warm/cold while durable exact-Run recovery is
    // available; C1 the frozen dispatch identifies a
    // source Run that is present/absent from the recovered Thread; C2 that Run
    // is latest/older than a committed successor; C3 its activity epoch is
    // active/already settled while the successor epoch is active; C4 the source
    // is a committed boundary/Running. Effects: E1 an exact committed source
    // is accepted idempotently; E2 an already-settled source cannot remove or
    // idle the successor; E3 an absent source or non-boundary is rejected before
    // Session mutation. Constraint: the current dispatch claim is the sole
    // source identity authority; recovery only proves that exact Run committed.
    //
    // | Rule | Generic projection | Source | Position | Epochs | State | Effect |
    // | R1 | warm | present | latest | source active | boundary | E1 (B1/B4) |
    // | R2 | cold | present | older | source settled + successor active | boundary | E1+E2 |
    // | R3 | cold | absent | successor latest | successor active | n/a | E3 |
    // | R4 | warm | present | latest | source active | Running | E3 (B3) |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("redelivery repository"),
    );
    let session_id = "boundary-redelivery";
    create(repo.as_ref(), persisted(session_id, false, "idle")).await;
    let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let (_, source_epoch) = app
        .begin_activity_for_operation(session_id, "source-operation")
        .await
        .expect("R2 source activity");
    app.settle_activity(session_id, source_epoch)
        .await
        .expect("R2 observer effect committed before queue settlement");
    let (_, successor_epoch) = app
        .begin_activity_for_operation(session_id, "successor-operation")
        .await
        .expect("R2 successor activity");

    let thread_id = ThreadId("boundary-redelivery-child".into());
    let source_run_id = RunId("boundary-redelivery-source".into());
    let successor_run_id = RunId("boundary-redelivery-successor".into());
    runtime.set_recovery_snapshot(
        session_id,
        &thread_id,
        awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
            thread_id: thread_id.clone(),
            claimed_run_id: successor_run_id.clone(),
            runs: vec![
                awaken_agent_contract::agent::run::Record {
                    id: source_run_id.clone(),
                    thread_id: thread_id.clone(),
                    state: RunState::Ended(EndCause::NaturalEnd),
                },
                awaken_agent_contract::agent::run::Record {
                    id: successor_run_id.clone(),
                    thread_id: thread_id.clone(),
                    state: RunState::Running,
                },
            ],
            latest_run_id: Some(successor_run_id),
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 2,
            store_cursor: 2,
            next_commit_ordinal: 1,
        },
    );
    runtime.set_generic_recovery_available(false);
    assert!(
        awaken_session_contract::SessionRuntime::session_thread_recovery_snapshot(
            runtime.as_ref(),
            session_id,
            &thread_id.0,
        )
        .await
        .expect("R2 cold generic recovery")
        .is_none(),
        "R2 process-local latest projection is intentionally cold"
    );
    let before_replay = repo
        .get(session_id)
        .await
        .expect("R2 Session before replay");
    assert_eq!(
        before_replay.active_activity_epochs,
        [successor_epoch].into_iter().collect(),
        "R2 precondition"
    );
    assert_eq!(
        before_replay.execution,
        SessionExecutionState::Running,
        "R2 precondition"
    );

    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_contract::SessionAgentBoundaryCommand {
            session_id: session_id.into(),
            source_thread_id: thread_id.clone(),
            source_run_id,
            source_agent_id: "unused-redelivery-agent".into(),
            session_activity_epoch: source_epoch,
            cancellation_requested: false,
        },
    )
    .await
    .expect("R2 older exact source redelivery");
    let after_replay = repo.get(session_id).await.expect("R2 Session");
    assert_eq!(after_replay, before_replay, "R2/E1+E2");

    let missing = awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_contract::SessionAgentBoundaryCommand {
            session_id: session_id.into(),
            source_thread_id: thread_id,
            source_run_id: RunId("boundary-redelivery-missing".into()),
            source_agent_id: "unused-redelivery-agent".into(),
            session_activity_epoch: source_epoch,
            cancellation_requested: false,
        },
    )
    .await;
    assert!(
        matches!(
            missing,
            Err(RunError {
                kind: awaken_session_contract::RunErrorKind::BadRequest,
                ..
            })
        ),
        "R3/E3"
    );
    assert_eq!(
        repo.get(session_id).await.expect("R3 Session"),
        after_replay,
        "R3/E3"
    );
}

#[tokio::test]
async fn completed_child_without_a_committed_reply_settles_without_a_report_run() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the trusted child boundary is Completed; C2 its
    // authoritative recovery snapshot has/has-not a non-empty reply selected by
    // the shared report classifier. Effects: E1 C1+C2 transfers the activity to
    // exactly one root report Run (covered by admission rule S6); E2 C1+!C2
    // settles the exact activity directly and admits zero root report Runs.
    //
    // | Rule | Completed | committed selected reply | Effect |
    // | M1   | yes       | yes                      | E1 (S6) |
    // | M2   | yes       | no                       | E2      |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("missing-report repository"),
    );
    create(
        repo.as_ref(),
        persisted("completed-without-report", false, "idle"),
    )
    .await;
    let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let (_, activity_epoch) = app
        .begin_activity_for_operation("completed-without-report", "missing-report-operation")
        .await
        .expect("M2 activity");
    let child = ThreadId("completed-without-report-child".into());
    let run = RunId("completed-without-report-run".into());
    runtime.commit_boundary(
        "completed-without-report",
        &child,
        &run,
        RunState::Ended(EndCause::NaturalEnd),
        "",
    );

    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_contract::SessionAgentBoundaryCommand {
            session_id: "completed-without-report".into(),
            source_thread_id: child,
            source_run_id: run,
            source_agent_id: "unused-empty-report-agent".into(),
            session_activity_epoch: activity_epoch,
            cancellation_requested: false,
        },
    )
    .await
    .expect("M2 direct settlement");

    assert!(runtime.continuations.lock().unwrap().is_empty(), "M2/E2");
    let settled = repo
        .get("completed-without-report")
        .await
        .expect("M2 settled Session");
    assert!(settled.active_activity_epochs.is_empty(), "M2/E2");
    assert_eq!(settled.execution, SessionExecutionState::Idle, "M2/E2");
}

#[tokio::test]
async fn advisor_thread_usage_is_folded_once_into_session_usage() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the primary and one first-class Advisor Thread own
    // distinct committed usage; C2 the Advisor appears once in the Runtime's
    // coordinated links; C3 the model-request gate may also supply that same
    // Thread while its link races projection. Effects: E1 each Thread keeps its
    // own usage attribution; E2 Session usage includes the Advisor exactly once;
    // E3 neither target-kind handling nor the racing coordinate creates a second
    // accounting rule beside the coordinated-Thread fold.
    //
    // | Rule | Child target | Link count | Additional id | Effect |
    // |---|---|---:|---|---|
    // | A1 | Advisor | 1 | none | E1+E2+E3 |
    // | A2 | Advisor | 1 | same child | E1+E2+E3 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Advisor usage repository"),
    );
    create(repo.as_ref(), persisted("advisor-usage", false, "idle")).await;
    let runtime = Arc::new(
        RecordingBoundaryBudgetRuntime::new(awaken_session_contract::SessionUsage {
            output_tokens: 7,
            ..Default::default()
        })
        .with_child_target(awaken_session_contract::CoordinatedThreadTarget::Advisor {
            model: "advisor-model".into(),
        }),
    );
    runtime.set_root_usage(awaken_session_contract::SessionUsage {
        output_tokens: 14,
        ..Default::default()
    });
    runtime.commit_boundary(
        "advisor-usage",
        &ThreadId("advisor-thread".into()),
        &RunId("advisor-run".into()),
        RunState::Ended(EndCause::NaturalEnd),
        "advice",
    );
    let app = application_with_runtime(
        runtime,
        repo,
        Arc::new(RecordingEnvironmentSource::default()),
    );

    assert_eq!(
        app.session_thread_usage("advisor-usage", "advisor-usage")
            .await
            .expect("A1 primary usage")
            .output_tokens,
        14,
        "A1/E1 primary attribution"
    );
    assert_eq!(
        app.session_thread_usage("advisor-usage", "advisor-thread")
            .await
            .expect("A1 Advisor usage")
            .output_tokens,
        7,
        "A1/E1 Advisor attribution"
    );
    let usage = app
        .session_usage("advisor-usage")
        .await
        .expect("A1 Session usage");
    assert_eq!(usage.output_tokens, 21, "A1/E2");
    let gated_usage = app
        .session_usage_for_model_request("advisor-usage", "advisor-thread")
        .await
        .expect("A2 gated Session usage");
    assert_eq!(gated_usage.output_tokens, 21, "A2/E2+E3");
}

#[tokio::test]
async fn model_request_admission_reconciles_root_and_child_usage_at_one_cas() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the latest committed root or child Run is live; C2
    // cumulative Session usage is below or at the active cap; C3 the same gate
    // check is replayed. Effects: E1 C2-below admits without provenance; E2
    // C2-at-cap reconciles the root cursor, appends one reach generation, and
    // denies the next Provider request; E3 C3 remains denied without a second
    // transition. Runtime supplies only exact Run/Thread coordinates and never
    // owns a budget copy.
    //
    // | Rule | Thread | Usage | Replay | Effect |
    // |---|---|---|---|---|
    // | G1 | root | below | no | E1 |
    // | G2 | root | reaches | no/yes | E2+E3 |
    // | G3 | child | below | no | E1 |
    // | G4 | child | reaches | no/yes | E2+E3 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("request gate repository"),
    );
    let snapshot = awaken_session_contract::ManagedListPriceSnapshot {
        snapshot_id: "request-gate-prices-v1".into(),
        version: 1,
        effective_at_unix_ms: 1,
        arithmetic_version: 1,
        model_rates: std::collections::BTreeMap::from([(
            "model".into(),
            awaken_session_contract::ManagedTokenListRates {
                input_micros_per_million: 1_000_000,
                ..Default::default()
            },
        )]),
        runtime_rates: Default::default(),
        fingerprint: "request-gate-prices-v1-fingerprint".into(),
    };
    for id in ["request-gate-root", "request-gate-child"] {
        let mut session = persisted(id, false, "idle");
        session.budget = awaken_session_contract::SessionBudgetState::active(1, snapshot.clone());
        create(repo.as_ref(), session).await;
    }
    let runtime = Arc::new(RecordingBoundaryBudgetRuntime::new(Default::default()));
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let reached_usage = awaken_session_contract::SessionUsage {
        input_tokens: 10_000,
        by_model: std::collections::BTreeMap::from([(
            "model".into(),
            awaken_session_contract::SessionModelUsage {
                input_tokens: 10_000,
                ..Default::default()
            },
        )]),
        ..Default::default()
    };

    let root_thread = ThreadId("request-gate-root".into());
    let root_run = RunId("request-gate-root-run".into());
    runtime.commit_boundary(
        "request-gate-root",
        &root_thread,
        &root_run,
        RunState::Running,
        "",
    );
    assert!(
        awaken_session_contract::SessionAgentCoordination::admit_session_model_request(
            &app,
            "request-gate-root",
            &root_thread,
            &root_run,
        )
        .await
        .expect("G1 root admission"),
        "G1/E1"
    );
    runtime.set_root_usage(reached_usage.clone());
    for replay in [false, true] {
        assert!(
            !awaken_session_contract::SessionAgentCoordination::admit_session_model_request(
                &app,
                "request-gate-root",
                &root_thread,
                &root_run,
            )
            .await
            .expect("G2 root denial"),
            "G2/E2 replay={replay}"
        );
    }
    assert_eq!(
        repo.get("request-gate-root")
            .await
            .unwrap()
            .budget
            .reach_transitions()
            .len(),
        1,
        "G2/E2+E3"
    );

    runtime.set_root_usage(Default::default());
    let child_thread = ThreadId("request-gate-child-thread".into());
    let child_run = RunId("request-gate-child-run".into());
    runtime.commit_boundary(
        "request-gate-child",
        &child_thread,
        &child_run,
        RunState::Running,
        "",
    );
    assert!(
        awaken_session_contract::SessionAgentCoordination::admit_session_model_request(
            &app,
            "request-gate-child",
            &child_thread,
            &child_run,
        )
        .await
        .expect("G3 child admission"),
        "G3/E1"
    );
    runtime.set_child_usage(reached_usage);
    for replay in [false, true] {
        assert!(
            !awaken_session_contract::SessionAgentCoordination::admit_session_model_request(
                &app,
                "request-gate-child",
                &child_thread,
                &child_run,
            )
            .await
            .expect("G4 child denial"),
            "G4/E2 replay={replay}"
        );
    }
    assert_eq!(
        repo.get("request-gate-child")
            .await
            .unwrap()
            .budget
            .reach_transitions()
            .len(),
        1,
        "G4/E2+E3"
    );
}

#[tokio::test]
async fn child_boundary_reconciles_budget_before_the_report_request_gate() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the child boundary is Awaiting or terminal; C2 an
    // active budget is below or reaches its exact cap; C3 the same committed
    // boundary is redelivered after a crash. Effects: E1 aggregate child usage
    // advances the one Session budget cursor and appends one generic cap
    // transition with its exact usage/price coordinate; E2 Awaiting settles its
    // activity without starting a primary continuation; E3 a terminal child
    // hands its exact activity to the deterministic durable report Run, whose
    // next logical model request is paused by the Runtime gate rather than being
    // discarded at this boundary; E4 redelivery neither charges, duplicates
    // provenance, nor emits budget_reached twice. The Host atomic report test
    // proves exact delivery retries still own one Dispatch/input, and the Runtime
    // request-gate table proves a denied request makes zero Provider calls.
    //
    // | Rule | Boundary | Cost vs cap | Delivery | Effect |
    // |---|---|---|---|---|
    // | B1 | Awaiting | reaches | first | E1 + E2 |
    // | B2 | Ended | reaches | first | E1 + E3 |
    // | B3 | Ended | already reached | replay | E4 + same E3 delivery |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("boundary budget repository"),
    );
    let price_snapshot = awaken_session_contract::ManagedListPriceSnapshot {
        snapshot_id: "boundary-price-v1".into(),
        version: 1,
        effective_at_unix_ms: 1,
        arithmetic_version: 1,
        model_rates: std::collections::BTreeMap::from([(
            "model".into(),
            awaken_session_contract::ManagedTokenListRates {
                input_micros_per_million: 1_000_000,
                ..Default::default()
            },
        )]),
        runtime_rates: Default::default(),
        fingerprint: "boundary-price-v1-fingerprint".into(),
    };
    for id in ["boundary-budget-awaiting", "boundary-budget-ended"] {
        let mut session = coordinated_session(id);
        session.budget =
            awaken_session_contract::SessionBudgetState::active(1, price_snapshot.clone());
        create(repo.as_ref(), session).await;
    }
    let runtime = Arc::new(RecordingBoundaryBudgetRuntime::new(
        awaken_session_contract::SessionUsage {
            input_tokens: 10_000,
            by_model: std::collections::BTreeMap::from([(
                "model".into(),
                awaken_session_contract::SessionModelUsage {
                    input_tokens: 10_000,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        },
    ));
    let mut app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    app.set_config_source(Arc::new(CoordinatedAgentSource));

    let settle =
        |session_id: &str, epoch: u64| -> awaken_session_contract::SessionAgentBoundaryCommand {
            awaken_session_contract::SessionAgentBoundaryCommand {
                session_id: session_id.into(),
                source_thread_id: ThreadId("budget-child".into()),
                source_run_id: RunId("budget-child-run".into()),
                source_agent_id: "coord-child".into(),
                session_activity_epoch: epoch,
                cancellation_requested: false,
            }
        };

    let (_, awaiting_epoch) = app
        .begin_activity_for_operation("boundary-budget-awaiting", "awaiting-op")
        .await
        .expect("B1 activity");
    runtime.commit_boundary(
        "boundary-budget-awaiting",
        &ThreadId("budget-child".into()),
        &RunId("budget-child-run".into()),
        RunState::Awaiting,
        "budget report",
    );
    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        settle("boundary-budget-awaiting", awaiting_epoch),
    )
    .await
    .expect("B1 boundary");
    let awaiting = repo
        .get("boundary-budget-awaiting")
        .await
        .expect("B1 state");
    assert!(!awaiting.budget.can_admit_model_request(), "B1/E1");
    assert_eq!(
        awaiting.budget.reach_transitions().len(),
        1,
        "B1/E1 one aggregate transition"
    );
    let awaiting_transition = &awaiting.budget.reach_transitions()[0];
    assert_eq!(awaiting_transition.generation, 1, "B1/E1 generation");
    assert_eq!(
        awaiting_transition.price_snapshot_id, "boundary-price-v1",
        "B1/E1 frozen price coordinate"
    );
    assert_eq!(
        awaiting_transition.usage_cursor.by_model["model"].input_tokens, 10_000,
        "B1/E1 exact cumulative usage"
    );
    assert_eq!(runtime.continuations.load(Ordering::SeqCst), 0, "B1/E2");

    let (_, ended_epoch) = app
        .begin_activity_for_operation("boundary-budget-ended", "ended-op")
        .await
        .expect("B2 activity");
    runtime.commit_boundary(
        "boundary-budget-ended",
        &ThreadId("budget-child".into()),
        &RunId("budget-child-run".into()),
        RunState::Ended(EndCause::NaturalEnd),
        "budget report",
    );
    let ended_command = settle("boundary-budget-ended", ended_epoch);
    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        ended_command.clone(),
    )
    .await
    .expect("B2 boundary");
    let ended = repo.get("boundary-budget-ended").await.expect("B2 state");
    assert!(!ended.budget.can_admit_model_request(), "B2/E1");
    assert_eq!(
        ended.budget.reach_transitions().len(),
        1,
        "B2/E1 terminal keeps the same aggregate provenance shape"
    );
    assert_eq!(runtime.continuations.load(Ordering::SeqCst), 1, "B2/E3");
    assert_eq!(
        ended.active_activity_epochs,
        std::collections::BTreeSet::from([ended_epoch]),
        "B2/E3 the durable report Run inherits the child activity"
    );
    assert_eq!(ended.execution, SessionExecutionState::Running, "B2/E3");
    let revision = ended.revision;

    awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        ended_command,
    )
    .await
    .expect("B3 replay");
    let replayed = repo.get("boundary-budget-ended").await.expect("B3 state");
    assert_eq!(replayed.revision, revision, "B3/E4");
    assert_eq!(replayed.budget.reach_transitions().len(), 1, "B3/E4");
    assert_eq!(
        runtime.continuations.load(Ordering::SeqCst),
        2,
        "B3/E4 the application retries delivery; the Host owner deduplicates its Dispatch"
    );
    let reached = repo
        .pending_lifecycle()
        .await
        .expect("B3 lifecycle")
        .into_iter()
        .filter(|fact| {
            fact.object_id == "boundary-budget-ended" && fact.event_type == "session.budget_reached"
        })
        .count();
    assert_eq!(reached, 1, "B3/E4");
}

#[tokio::test]
async fn concurrent_child_cap_crossing_has_one_root_cas_winner_and_no_second_ledger() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 two ordinary children are simultaneously in flight;
    // C2 both terminal commits are already part of the cumulative usage which
    // exactly reaches the shared cap; C3 their settlement observers race. Effects:
    // E1 root usage is charged once; E2 the transition winner appends one generic
    // aggregate transition while both Thread terminals remain in lifecycle; E3
    // only the actual cap transition emits budget_reached; E4 both activity
    // epochs transfer to their deterministic durable report Runs, whose logical
    // model requests remain fenced by the one Runtime gate.
    //
    // | Rule | In-flight | Aggregate cost | Settlement | Effects |
    // |---|---:|---|---|---|
    // | C1 | 2 | reaches cap | concurrent | E1+E2+E3+E4 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("concurrent budget repository"),
    );
    let mut session = coordinated_session("concurrent-child-budget");
    session.budget = awaken_session_contract::SessionBudgetState::active(
        1,
        awaken_session_contract::ManagedListPriceSnapshot {
            snapshot_id: "concurrent-price-v1".into(),
            version: 1,
            effective_at_unix_ms: 1,
            arithmetic_version: 1,
            model_rates: std::collections::BTreeMap::from([(
                "model".into(),
                awaken_session_contract::ManagedTokenListRates {
                    input_micros_per_million: 1_000_000,
                    ..Default::default()
                },
            )]),
            runtime_rates: Default::default(),
            fingerprint: "concurrent-price-v1-fingerprint".into(),
        },
    );
    create(repo.as_ref(), session).await;
    let runtime = Arc::new(RecordingBoundaryBudgetRuntime::new(
        awaken_session_contract::SessionUsage {
            input_tokens: 5_000,
            by_model: std::collections::BTreeMap::from([(
                "model".into(),
                awaken_session_contract::SessionModelUsage {
                    input_tokens: 5_000,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        },
    ));
    let mut app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    app.set_config_source(Arc::new(CoordinatedAgentSource));
    let child_a = ThreadId("budget-child-a".into());
    let child_b = ThreadId("budget-child-b".into());
    let run_a = RunId("budget-run-a".into());
    let run_b = RunId("budget-run-b".into());
    runtime.commit_boundary(
        "concurrent-child-budget",
        &child_a,
        &run_a,
        RunState::Ended(EndCause::NaturalEnd),
        "a",
    );
    runtime.commit_boundary(
        "concurrent-child-budget",
        &child_b,
        &run_b,
        RunState::Ended(EndCause::NaturalEnd),
        "b",
    );
    let (_, epoch_a) = app
        .begin_activity_for_operation("concurrent-child-budget", "child-a-op")
        .await
        .expect("C1 child A activity");
    let (_, epoch_b) = app
        .begin_activity_for_operation("concurrent-child-budget", "child-b-op")
        .await
        .expect("C1 child B activity");
    let command = |thread_id, run_id, session_activity_epoch| {
        awaken_session_contract::SessionAgentBoundaryCommand {
            session_id: "concurrent-child-budget".into(),
            source_thread_id: thread_id,
            source_run_id: run_id,
            source_agent_id: "coord-child".into(),
            session_activity_epoch,
            cancellation_requested: false,
        }
    };
    let (settled_a, settled_b) = tokio::join!(
        awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
            &app,
            command(child_a.clone(), run_a.clone(), epoch_a),
        ),
        awaken_session_contract::SessionAgentCoordination::settle_session_agent_boundary(
            &app,
            command(child_b.clone(), run_b.clone(), epoch_b),
        )
    );
    settled_a.expect("C1 child A settlement");
    settled_b.expect("C1 child B settlement");

    let settled = repo
        .get("concurrent-child-budget")
        .await
        .expect("C1 settled Session");
    assert!(!settled.budget.can_admit_model_request(), "C1/E1");
    assert_eq!(settled.budget.reach_transitions().len(), 1, "C1/E2");
    assert_eq!(
        settled.budget.reach_transitions()[0].generation,
        1,
        "C1/E2 serialized root CAS has one transition winner"
    );
    assert_eq!(
        settled.active_activity_epochs,
        std::collections::BTreeSet::from([epoch_a, epoch_b]),
        "C1/E4 both durable report Runs inherit their exact child activity"
    );
    assert_eq!(runtime.continuations.load(Ordering::SeqCst), 2, "C1/E4");
    let reached = repo
        .pending_lifecycle()
        .await
        .expect("C1 lifecycle")
        .into_iter()
        .filter(|fact| fact.event_type == "session.budget_reached")
        .count();
    assert_eq!(reached, 1, "C1/E3");
}

#[tokio::test]
async fn transferred_activity_fences_a_late_distinct_boundary_before_conflict_validation() {
    // Cause/effect graph: C1 epoch A closes with observation X; C2 a reply
    // opens successor epoch B; C3 A's still-leased Worker reports later
    // observation Y. Effects: E1 C3 is a stale no-op, not epoch-reuse
    // corruption; E2 B remains the sole active epoch and its interval stays
    // open. Decision table: A1=C1=>closed(X); A2=C1+C2=>active(B);
    // A3=C1+C2+C3=>E1+E2. The active-epoch set is the fencing authority.
    let session_id = "stale-transferred-boundary";
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("activity fence repository"),
    );
    create(repo.as_ref(), persisted(session_id, false, "idle")).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let first = app.begin_activity(session_id).await.expect("A1 activity");
    let first_epoch = first.activity_epoch;
    let observation =
        |source_commit_cursor| awaken_session_contract::SessionRuntimeIntervalObservation {
            activity_epoch: first_epoch,
            thread_id: ThreadId(session_id.into()),
            run_id: RunId("shared-run".into()),
            lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor(source_commit_cursor),
            source_commit_cursor,
        };
    app.settle_activity_observed(session_id, first_epoch, Some(observation(10)))
        .await
        .expect("A1 closes with X");
    let successor = app.begin_activity(session_id).await.expect("A2 successor");
    let successor_epoch = successor.activity_epoch;
    assert_ne!(successor_epoch, first_epoch, "A2 owns a new fence");

    let after_stale = app
        .settle_activity_observed(session_id, first_epoch, Some(observation(20)))
        .await
        .expect("A3/E1 stale distinct boundary is fenced");
    assert_eq!(
        after_stale.active_activity_epochs,
        BTreeSet::from([successor_epoch]),
        "A3/E2"
    );
    assert_eq!(
        after_stale.execution,
        SessionExecutionState::Running,
        "A3/E2"
    );
    assert!(after_stale.running_interval.is_some(), "A3/E2");
    assert_eq!(after_stale.closed_runtime_intervals.len(), 1, "A3/E1");
}

#[tokio::test]
async fn cancellation_before_external_realization_settles_without_a_runtime_interval() {
    // Cause/effect graph: C1 an externally realized Session is Preparing; C2 a
    // Session Run activity is admitted during that stronger phase; C3 the Run
    // commits a cancellation boundary before realization opens Running. Effects:
    // E1 settlement removes the exact activity epoch; E2 Preparing remains
    // authoritative; E3 no synthetic open/closed Runtime interval or lifecycle
    // fact is created. Constraint: committed Run truth remains auditable in the
    // Runtime feed; this reducer owns only the customer-visible Session interval.
    // Decision rule P1=C1+C2+C3=>E1+E2+E3. Running-without-interval remains an
    // invalid historical boundary and is covered by the aggregate reducer tests.
    let session_id = "cancelled-before-external-realization";
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("pre-realization cancellation repository"),
    );
    let mut session = persisted(session_id, false, "preparing");
    let epoch = SessionApplication::open_activity_on_snapshot(&mut session, None, true, false)
        .expect("P1/C2 activity admitted during external realization");
    assert!(
        session.running_interval.is_none(),
        "P1/C1 no Running interval"
    );
    create(repo.as_ref(), session).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let observation = awaken_session_contract::SessionRuntimeIntervalObservation {
        activity_epoch: epoch,
        thread_id: ThreadId(session_id.into()),
        run_id: RunId("cancelled-before-realization-run".into()),
        lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor(7),
        source_commit_cursor: 7,
    };

    let settled = app
        .settle_activity_observed(session_id, epoch, Some(observation))
        .await
        .expect("P1 cancellation boundary settles the admitted activity");
    assert!(settled.active_activity_epochs.is_empty(), "P1/E1");
    assert_eq!(settled.execution, SessionExecutionState::Preparing, "P1/E2");
    assert!(settled.running_interval.is_none(), "P1/E3");
    assert!(settled.closed_runtime_intervals.is_empty(), "P1/E3");
    assert!(
        repo.pending_lifecycle()
            .await
            .expect("P1 lifecycle outbox")
            .into_iter()
            .all(|fact| fact.event_type != "session.runtime_interval_closed"),
        "P1/E3"
    );
}

#[derive(Default)]
struct FailOnceMcpRealizer {
    stage_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl McpAttachmentRealizer for FailOnceMcpRealizer {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        if self.stage_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(RunError::unavailable("injected MCP stage outage"));
        }
        McpAttachmentRealizer::stage_mcp_attachment(&NoopMcpRealizer, request).await
    }

    async fn publish_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        McpAttachmentRealizer::publish_mcp_generation(&NoopMcpRealizer, generation).await
    }

    async fn drain_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        McpAttachmentRealizer::drain_mcp_generation(&NoopMcpRealizer, generation).await
    }
}

#[tokio::test]
async fn mixed_update_commits_one_receipted_root_before_realization_and_replays_the_effect() {
    // Cause/effect graph: C1 one command mixes title, metadata, budget, and MCP
    // Agent desired state; C2 its root CAS and durable command receipt commit;
    // C3 MCP realization succeeds or fails retryably; C4 the caller replays the
    // exact command after C3 failure. Effects: E1 every desired field and MCP
    // generation share the single next root revision named by the receipt; E2 a
    // C3 failure is ProjectionAfterCommit carrying that complete committed
    // outcome; E3 C4 keeps the original command revision and allocates exactly
    // the aggregate-owned N+1 recovery generation, then realizes durable truth
    // without re-authoring any public field.
    // Constraint: normalization is pure before C2; no title/metadata/budget or
    // MCP intent may commit through an independent command mutation.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // |---|---|---|---|---|---|
    // | M1 | yes | commits | retryable failure | no | E1 + E2 |
    // | M2 | same command | already committed | succeeds | exact | E3 |
    let session_id = "mixed-update-one-root";
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("mixed update repository"),
    );
    let mut session = persisted(session_id, false, "idle");
    session.budget = awaken_session_contract::SessionBudgetState::Active {
        max_list_cost_minor: 10,
        consumed_numerator: 0,
        usage_cursor: Default::default(),
        snapshot: awaken_session_contract::ManagedListPriceSnapshot {
            snapshot_id: "mixed-prices-v1".into(),
            version: 1,
            effective_at_unix_ms: 1,
            arithmetic_version: 1,
            model_rates: Default::default(),
            runtime_rates: Default::default(),
            fingerprint: "mixed-prices-v1-fingerprint".into(),
        },
        reach_transitions: Vec::new(),
    };
    create(repo.as_ref(), session).await;
    let before = repo.get(session_id).await.expect("initial mixed Session");
    let command_revision = awaken_session_contract::SessionRevision(
        before
            .revision
            .0
            .checked_add(1)
            .expect("test command revision"),
    );
    let realizer = Arc::new(FailOnceMcpRealizer::default());
    let app = SessionApplication::new(
        Arc::new(NoopRuntime),
        realizer.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let idempotency_key = "mixed-update-key";
    let command = SessionUpdateCommand {
        title: Some(SessionFieldUpdate::Replace("one root".into())),
        metadata: Some(SessionMetadataUpdate::Patch(
            std::collections::BTreeMap::from([("owner".into(), Some("session".into()))]),
        )),
        budget: Some(SessionFieldUpdate::Replace(20)),
        tools: None,
        mcp_update: Some(SessionMcpUpdate::PublicReplacement(vec![
            McpAttachmentCandidate {
                name: "docs".into(),
                target: McpAttachmentCandidateTarget::HttpUrl(
                    "https://docs.example.test/mcp".into(),
                ),
                prompts_as_skills: false,
                published_credential: None,
                origin: awaken_session_contract::McpAttachmentOrigin::Session,
            },
        ])),
        idempotency_key: Some(idempotency_key.into()),
        request_fingerprint: awaken_session_contract::stable_fingerprint(&(
            "mixed-update-one-root-v1",
            session_id,
        )),
        if_match: None,
    };

    let (committed, failure) = match app.update_session(session_id, command.clone()).await {
        Err(SessionUpdateError::ProjectionAfterCommit { outcome, source }) => (*outcome, source),
        other => panic!("M1/E2 expected committed realization failure, got {other:?}"),
    };
    assert_eq!(
        failure.kind,
        awaken_session_contract::RunErrorKind::Unavailable,
        "M1/E2"
    );
    assert_eq!(committed.command_revision, command_revision, "M1/E1");
    assert!(committed.command_applied, "M1/E1");
    assert_eq!(
        committed.changes,
        SessionUpdateChanges {
            title: true,
            metadata: true,
            tools: false,
            mcp: true,
            budget: true,
        },
        "M1/E1 complete mixed command"
    );
    assert_eq!(
        committed.session.revision, command_revision,
        "M1/E1 one command CAS"
    );
    assert_eq!(
        committed.session.title.as_deref(),
        Some("one root"),
        "M1/E1"
    );
    assert_eq!(
        committed.session.metadata.get("owner").map(String::as_str),
        Some("session"),
        "M1/E1"
    );
    assert_eq!(
        committed.session.budget.max_list_cost_minor(),
        Some(20),
        "M1/E1"
    );
    assert_eq!(
        committed.session.mcp.desired_attachments().len(),
        1,
        "M1/E1"
    );

    let receipt_key = format!(
        "managed:update-command:{session_id}:{}",
        SessionApplication::update_operation_id(session_id, idempotency_key)
    );
    let receipt = repo
        .idempotency_receipt(session_id, &receipt_key)
        .await
        .expect("M1 receipt read")
        .expect("M1 durable command receipt");
    assert_eq!(receipt.committed_revision, command_revision, "M1/E1");
    let durable_after_failure = repo.get(session_id).await.expect("M1 durable mixed truth");
    assert_eq!(
        durable_after_failure.title.as_deref(),
        Some("one root"),
        "M1/E2"
    );
    assert_eq!(
        durable_after_failure.budget.max_list_cost_minor(),
        Some(20),
        "M1/E2"
    );
    assert_eq!(realizer.stage_calls.load(Ordering::SeqCst), 1, "M1/E2");

    let replayed = app
        .update_session(session_id, command)
        .await
        .expect("M2 exact replay repairs realization");
    assert!(!replayed.command_applied, "M2/E3");
    assert_eq!(replayed.command_revision, command_revision, "M2/E3");
    assert_eq!(replayed.changes, SessionUpdateChanges::default(), "M2/E3");
    assert_eq!(replayed.session.title.as_deref(), Some("one root"), "M2/E3");
    assert_eq!(
        replayed.session.budget.max_list_cost_minor(),
        Some(20),
        "M2/E3"
    );
    let desired = replayed.session.mcp.desired_attachments();
    assert_eq!(desired.len(), 1, "M2/E3");
    assert_eq!(
        replayed.session.mcp.attachments.len(),
        2,
        "M2/E3 one failed generation plus one exact recovery generation"
    );
    assert_eq!(
        desired[0].generation.0, 2,
        "M2/E3 Failed -> N+1 uses the canonical MCP retry rule"
    );
    assert_eq!(
        desired[0].state,
        awaken_session_contract::McpAttachmentState::Active,
        "M2/E3"
    );
    assert!(desired[0].publication_acknowledged, "M2/E3");
    assert_eq!(realizer.stage_calls.load(Ordering::SeqCst), 2, "M2/E3");
    assert_eq!(
        repo.idempotency_receipt(session_id, &receipt_key)
            .await
            .expect("M2 receipt read")
            .expect("M2 durable command receipt")
            .committed_revision,
        command_revision,
        "M2/E3 receipt remains the original command CAS"
    );
}

/// Update-admission authority graph. C1 durable status is idle; C2 durable
/// status is running/terminal; C3 an interface cache is absent or stale.
/// Only C1 permits mutation (E1); C2 always rejects without a root revision
/// change (E2), independently of C3. This prevents another process's wire
/// projection from becoming a parallel lifecycle authority.
///
/// | Rule | Durable status | Wire cache | Effect |
/// |---|---|---|---|
/// | U1 | idle | any/absent | apply through root CAS |
/// | U2 | running | any/absent | reject, no mutation |
/// | U3 | terminal | any/absent | reject, no mutation |
#[tokio::test]
async fn update_admission_uses_only_durable_session_status() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("update-idle", false, "idle")).await;
    create(repo.as_ref(), persisted("update-running", false, "running")).await;
    create(
        repo.as_ref(),
        persisted("update-terminal", false, "terminated"),
    )
    .await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let command = |title: &str| SessionUpdateCommand {
        title: Some(SessionFieldUpdate::Replace(title.into())),
        metadata: None,
        budget: None,
        tools: None,
        mcp_update: None,
        idempotency_key: None,
        request_fingerprint: awaken_session_contract::stable_fingerprint(&title),
        if_match: None,
    };

    let updated = app
        .update_session("update-idle", command("accepted"))
        .await
        .expect("U1");
    assert_eq!(updated.session.title.as_deref(), Some("accepted"), "U1");

    for (rule, id) in [("U2", "update-running"), ("U3", "update-terminal")] {
        let before = repo.get(id).await.expect(rule);
        assert!(
            matches!(
                app.update_session(id, command("rejected")).await,
                Err(SessionUpdateError::NotIdle)
            ),
            "{rule}"
        );
        assert_eq!(repo.get(id).await.expect(rule), before, "{rule}");
    }
}

#[tokio::test]
async fn profiled_update_authority_separates_public_agent_and_credential_lifecycle_commands() {
    // Cause/effect graph: C1 baseline policy Managed/Frozen/FileResources; C2
    // public update changes tools or replaces MCP desired topology; C3 the
    // repository-owned credential lifecycle replays an otherwise empty exact
    // topology; C4 that lifecycle command also carries a public authoring field;
    // C5 title-only public mutation. Effects: E1 Managed+C2 commits; E2 profiled
    // C2 rejects before MCP refresh/root mutation; E3 profiled C3 is admitted to
    // the canonical MCP state machine; E4 C4 always rejects; E5 C5 remains wire
    // compatible and does not become an agent hidden-axis mutation.
    // Rules U1=Managed+C2=>E1, U2/U3=profiled+C2=>E2,
    // U4=profiled+C3=>E3, U5=C4=>E4, U6=profiled+C5=>E5.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("profiled update repository"),
    );
    for (id, policy) in [
        (
            "update-managed-policy",
            awaken_session_contract::SessionMutationPolicy::Managed,
        ),
        (
            "update-frozen-policy",
            awaken_session_contract::SessionMutationPolicy::Frozen,
        ),
        (
            "update-files-policy",
            awaken_session_contract::SessionMutationPolicy::FileResources,
        ),
    ] {
        let mut session = persisted(id, false, "idle");
        let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut session.baseline
        else {
            unreachable!("fixture baseline is frozen")
        };
        baseline.mutation_policy = policy;
        create(repo.as_ref(), session).await;
    }
    let runtime = Arc::new(super::realization::RecordingResourceRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let tools = awaken_session_contract::SessionToolConfiguration {
        toolsets: Vec::new(),
        client_tools: vec![awaken_agent_contract::ClientToolDescriptor {
            name: "client-tool".into(),
            description: "client owned".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
    };
    let command = |tools, mcp_update, title, fingerprint: &str| SessionUpdateCommand {
        title,
        metadata: None,
        budget: None,
        tools,
        mcp_update,
        idempotency_key: None,
        request_fingerprint: fingerprint.into(),
        if_match: None,
    };

    app.update_session(
        "update-managed-policy",
        command(Some(tools.clone()), None, None, "U1"),
    )
    .await
    .expect("U1/E1");
    assert_eq!(
        repo.get("update-managed-policy").await.unwrap().tools,
        tools,
        "U1/E1"
    );
    assert_eq!(
        runtime.replaced_tools.lock().unwrap().as_slice(),
        [("update-managed-policy".into(), tools.clone())].as_slice(),
        "U1/E1 post-commit projection"
    );

    let frozen_before = repo.get("update-frozen-policy").await.unwrap();
    assert!(
        matches!(
            app.update_session(
                "update-frozen-policy",
                command(Some(tools.clone()), None, None, "U2")
            )
            .await,
            Err(SessionUpdateError::Rejected(_))
        ),
        "U2/E2"
    );
    assert_eq!(
        repo.get("update-frozen-policy").await.unwrap(),
        frozen_before,
        "U2/E2"
    );

    let files_before = repo.get("update-files-policy").await.unwrap();
    assert!(
        matches!(
            app.update_session(
                "update-files-policy",
                command(
                    None,
                    Some(SessionMcpUpdate::PublicReplacement(Vec::new())),
                    None,
                    "U3"
                )
            )
            .await,
            Err(SessionUpdateError::Rejected(_))
        ),
        "U3/E2"
    );
    assert_eq!(
        repo.get("update-files-policy").await.unwrap(),
        files_before,
        "U3/E2"
    );

    app.update_session(
        "update-files-policy",
        command(
            None,
            Some(SessionMcpUpdate::CredentialLifecycle {
                source_id: "vault-source".into(),
                revoked: false,
                candidates: Vec::new(),
            }),
            None,
            "U4",
        ),
    )
    .await
    .expect("U4/E3");
    assert!(
        matches!(
            app.update_session(
                "update-files-policy",
                command(
                    None,
                    Some(SessionMcpUpdate::CredentialLifecycle {
                        source_id: "vault-source".into(),
                        revoked: false,
                        candidates: Vec::new(),
                    }),
                    Some(SessionFieldUpdate::Replace("forbidden".into())),
                    "U5"
                )
            )
            .await,
            Err(SessionUpdateError::Rejected(_))
        ),
        "U5/E4"
    );
    let titled = app
        .update_session(
            "update-frozen-policy",
            command(
                None,
                None,
                Some(SessionFieldUpdate::Replace("visible title".into())),
                "U6",
            ),
        )
        .await
        .expect("U6/E5");
    assert_eq!(
        titled.session.title.as_deref(),
        Some("visible title"),
        "U6/E5"
    );
}

#[tokio::test]
async fn budget_update_lifecycle_follows_the_one_way_decision_table() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 budget absent/active/removed; C2 requested limit
    // absent/present; C3 a present limit is greater than exact consumed cost.
    // Effects: E1 absent cannot acquire a budget; E2 active+C3 changes the cap
    // without erasing prior cap-transition history; E3 active+!C3 rejects; E4
    // active+removal preserves snapshot/cursor/transitions and disables admission
    // enforcement; E5 removed cannot become active again.
    // Decision table: B1 absent+present=>E1; B2 active+present+C3=>E2; B3
    // active+present+!C3=>E3; B4 active+absent=>E4; B5 removed+present=>E5.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let mut active = persisted("budget-active", false, "idle");
    let snapshot = awaken_session_contract::ManagedListPriceSnapshot {
        snapshot_id: "prices-v1".into(),
        version: 1,
        effective_at_unix_ms: 1,
        arithmetic_version: 1,
        model_rates: std::collections::BTreeMap::new(),
        runtime_rates: Default::default(),
        fingerprint: "prices-v1-fingerprint".into(),
    };
    active.budget = awaken_session_contract::SessionBudgetState::Active {
        max_list_cost_minor: 10,
        consumed_numerator: 2
            * awaken_session_contract::SessionBudgetState::MICROS_PER_MINOR_USD
            * awaken_session_contract::SessionBudgetState::COST_DENOMINATOR,
        usage_cursor: Default::default(),
        snapshot,
        reach_transitions: vec![awaken_session_contract::BudgetReachTransition {
            generation: 1,
            max_list_cost_minor: 1,
            consumed_numerator: awaken_session_contract::SessionBudgetState::MICROS_PER_MINOR_USD
                * awaken_session_contract::SessionBudgetState::COST_DENOMINATOR,
            usage_cursor: Default::default(),
            price_snapshot_id: "prices-v1".into(),
        }],
    };
    create(repo.as_ref(), active).await;
    create(repo.as_ref(), persisted("budget-absent", false, "idle")).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let command = |budget: Option<u64>| SessionUpdateCommand {
        title: None,
        metadata: None,
        budget: Some(match budget {
            Some(value) => SessionFieldUpdate::Replace(value),
            None => SessionFieldUpdate::Clear,
        }),
        tools: None,
        mcp_update: None,
        idempotency_key: None,
        request_fingerprint: awaken_session_contract::stable_fingerprint(&budget),
        if_match: None,
    };

    assert!(
        matches!(
            app.update_session("budget-absent", command(Some(3))).await,
            Err(SessionUpdateError::Rejected(_))
        ),
        "B1/E1"
    );
    let updated = app
        .update_session("budget-active", command(Some(3)))
        .await
        .expect("B2");
    assert_eq!(
        updated.session.budget.max_list_cost_minor(),
        Some(3),
        "B2/E2"
    );
    assert_eq!(
        updated.session.budget.reach_transitions().len(),
        1,
        "B2/E2 append-only history"
    );
    assert!(
        matches!(
            app.update_session("budget-active", command(Some(2))).await,
            Err(SessionUpdateError::Rejected(_))
        ),
        "B3/E3"
    );
    let removed = app
        .update_session("budget-active", command(None))
        .await
        .expect("B4");
    assert!(
        matches!(
            removed.session.budget,
            awaken_session_contract::SessionBudgetState::Removed { .. }
        ),
        "B4/E4"
    );
    assert!(removed.session.budget.can_admit_model_request(), "B4/E4");
    assert_eq!(
        removed.session.budget.reach_transitions().len(),
        1,
        "B4/E4 append-only history"
    );
    assert!(
        matches!(
            app.update_session("budget-active", command(Some(4))).await,
            Err(SessionUpdateError::Rejected(_))
        ),
        "B5/E5"
    );
}

#[tokio::test]
async fn budget_update_resumes_exact_committed_pauses_without_activity_leaks() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a reached budget is raised or removed; C2 the
    // committed BudgetReached ticket belongs to the root or a child Thread; C3
    // an exact idempotent update races its replay; C4 Runtime reports the ticket
    // stale before dispatch. Effects: E1 the existing Run/correlation/generation
    // are delivered unchanged; E2 root CAS opens one deterministic continuation
    // activity and transfers the child's prior epoch without a second ledger;
    // E3 concurrent exact updates share that epoch; E4 stale delivery closes its
    // newly opened epoch and restores Idle. The Host test owns exactly-once
    // Dispatch staging; this test owns only Session root mutation and fencing.
    //
    // | Rule | Budget update | Ticket owner | Runtime result | Effect |
    // |---|---|---|---|---|
    // | R1 | raise | root | dispatched | E1+E2 |
    // | R2 | remove | child | dispatched | E1+E2 |
    // | R3 | exact concurrent raise | root | dispatched/replay | E1+E3 |
    // | R4 | raise | root | stale | E4 |
    use awaken_agent_contract::agent::awaiting::{AwaitTarget, PauseReason, ResumeTicket};
    use awaken_session_contract::{
        SessionBudgetResumeDisposition, SessionBudgetResumeTicket, SessionBudgetState,
    };

    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("budget resume repository"),
    );
    let snapshot = awaken_session_contract::ManagedListPriceSnapshot {
        snapshot_id: "resume-prices-v1".into(),
        version: 1,
        effective_at_unix_ms: 1,
        arithmetic_version: 1,
        model_rates: Default::default(),
        runtime_rates: Default::default(),
        fingerprint: "resume-prices-v1-fingerprint".into(),
    };
    let reached = |id: &str| {
        let mut session = persisted(id, false, "idle");
        session.budget = SessionBudgetState::Active {
            max_list_cost_minor: 1,
            consumed_numerator: SessionBudgetState::MICROS_PER_MINOR_USD
                * SessionBudgetState::COST_DENOMINATOR,
            usage_cursor: Default::default(),
            snapshot: snapshot.clone(),
            reach_transitions: Vec::new(),
        };
        session
    };
    for id in [
        "resume-budget-root",
        "resume-budget-child",
        "resume-budget-concurrent",
        "resume-budget-stale",
    ] {
        create(repo.as_ref(), reached(id)).await;
    }
    let pause = |thread: &str, run: &str, generation: u64, prior_epoch: Option<u64>| {
        SessionBudgetResumeTicket {
            ticket: ResumeTicket::new(
                run,
                RunId(run.into()),
                ThreadId(thread.into()),
                format!("{run}-snapshot"),
                format!("{run}-catalog"),
                AwaitTarget::Pause(PauseReason::BudgetReached),
            ),
            pause_generation: generation,
            prior_session_activity_epoch: prior_epoch,
        }
    };
    let runtime = Arc::new(RecordingBoundaryBudgetRuntime::new(Default::default()));
    runtime.set_budget_resume_tickets(
        "resume-budget-root",
        vec![pause("resume-budget-root", "root-run", 7, None)],
    );
    runtime.set_budget_resume_tickets(
        "resume-budget-child",
        vec![pause("child-thread", "child-run", 9, Some(41))],
    );
    runtime.set_budget_resume_tickets(
        "resume-budget-concurrent",
        vec![pause(
            "resume-budget-concurrent",
            "concurrent-run",
            11,
            None,
        )],
    );
    runtime.set_budget_resume_tickets(
        "resume-budget-stale",
        vec![pause("resume-budget-stale", "stale-run", 13, None)],
    );
    let app = Arc::new(application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let command = |budget: Option<u64>, key: &str| SessionUpdateCommand {
        title: None,
        metadata: None,
        budget: Some(match budget {
            Some(value) => SessionFieldUpdate::Replace(value),
            None => SessionFieldUpdate::Clear,
        }),
        tools: None,
        mcp_update: None,
        idempotency_key: Some(key.into()),
        request_fingerprint: awaken_session_contract::stable_fingerprint(&(budget, key)),
        if_match: None,
    };

    Box::pin(async {
        let raised = app
            .update_session("resume-budget-root", command(Some(2), "raise-root"))
            .await
            .expect("R1 raise resumes root");
        assert_eq!(raised.session.budget.max_list_cost_minor(), Some(2), "R1");
        let removed = app
            .update_session("resume-budget-child", command(None, "remove-child"))
            .await
            .expect("R2 removal resumes child");
        assert!(
            matches!(removed.session.budget, SessionBudgetState::Removed { .. }),
            "R2"
        );
        let initial_deliveries = runtime.budget_resume_deliveries.lock().unwrap().clone();
        assert_eq!(initial_deliveries.len(), 2, "R1/R2 one delivery each");
        assert_eq!(
            initial_deliveries[0].run_id,
            RunId("root-run".into()),
            "R1/E1"
        );
        assert_eq!(initial_deliveries[0].pause_generation, 7, "R1/E1");
        assert_eq!(
            initial_deliveries[0].prior_session_activity_epoch, None,
            "R1/E2"
        );
        assert_eq!(
            initial_deliveries[1].run_id,
            RunId("child-run".into()),
            "R2/E1"
        );
        assert_eq!(
            initial_deliveries[1].thread_id,
            ThreadId("child-thread".into()),
            "R2/E1"
        );
        assert_eq!(initial_deliveries[1].pause_generation, 9, "R2/E1");
        assert_eq!(
            initial_deliveries[1].prior_session_activity_epoch,
            Some(41),
            "R2/E2"
        );
    })
    .await;

    Box::pin(async {
        let concurrent = command(Some(2), "concurrent-exact");
        let (left, right) = tokio::join!(
            Box::pin(app.update_session("resume-budget-concurrent", concurrent.clone())),
            Box::pin(app.update_session("resume-budget-concurrent", concurrent)),
        );
        left.expect("R3 left exact update");
        right.expect("R3 right exact update");
        let concurrent_deliveries = runtime
            .budget_resume_deliveries
            .lock()
            .unwrap()
            .iter()
            .filter(|delivery| delivery.session_id == "resume-budget-concurrent")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            concurrent_deliveries.len(),
            2,
            "R3 recovery reaches Runtime twice"
        );
        assert_eq!(
            concurrent_deliveries[0], concurrent_deliveries[1],
            "R3/E1+E3"
        );
        let concurrent_session = repo.get("resume-budget-concurrent").await.unwrap();
        assert_eq!(concurrent_session.active_activity_epochs.len(), 1, "R3/E3");
        assert!(
            concurrent_session
                .active_activity_epochs
                .contains(&concurrent_deliveries[0].session_activity_epoch),
            "R3/E3"
        );
    })
    .await;

    Box::pin(async {
        runtime.set_budget_resume_dispositions([SessionBudgetResumeDisposition::Stale]);
        app.update_session("resume-budget-stale", command(Some(2), "stale-raise"))
            .await
            .expect("R4 stale resume is a successful update");
        let stale = repo.get("resume-budget-stale").await.unwrap();
        assert_eq!(stale.execution, SessionExecutionState::Idle, "R4/E4");
        assert!(stale.active_activity_epochs.is_empty(), "R4/E4");
        assert!(stale.running_interval.is_none(), "R4/E4");
    })
    .await;
}

/// Cause/effect graph: C1 the Runtime presents the exact durable realization
/// lease; C2 the binding is new or an idempotent replay; C3 the same epoch
/// has been renewed monotonically; C4 a replacement owner/epoch has fenced
/// the Runtime. C1 permits the ordinary root CAS; C3 preserves in-flight
/// work admitted before renewal; C4 rejects every receipt, including an equal
/// binding replay, before any aggregate mutation.
///
/// | Rule | Asserted lease | Binding | Effect |
/// |---|---|---|---|
/// | B1 | exact | new | persist once |
/// | B2 | exact | equal | idempotent success |
/// | B3 | shorter same-epoch assertion under live renewal | new/equal | authorized |
/// | B4 | stale owner/epoch | equal | fenced, no mutation |
/// | B5 | stale owner/epoch | different | fenced, no mutation |
/// | B6 | aggregate/assertion both absent | new/equal | legacy CAS path |
/// | B7 | current lease expired | equal | fenced, no mutation |
/// | B8 | current lease expired | different | fenced, no mutation |
#[tokio::test]
async fn environment_binding_persistence_is_fenced_by_exact_realization() {
    fn receipt(
        session_id: &str,
        binding: &str,
        realization: Option<&awaken_session_contract::SessionRealizationLease>,
    ) -> awaken_session_contract::SessionEnvironmentReceipt {
        awaken_session_contract::SessionEnvironmentReceipt::new(
            session_id,
            awaken_session_contract::SessionEnvironmentEffectKind::Create,
            binding,
            realization.cloned(),
        )
    }
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let mut session = persisted("binding-fence", false, "idle");
    let current = awaken_session_contract::SessionRealizationLease {
        owner: "runtime-a".into(),
        runtime_incarnation: "runtime-a/boot-1".into(),
        epoch: 3,
        expires_at_unix_ms: u64::MAX,
    };
    session.realization = Some(current.clone());
    create(repo.as_ref(), session).await;
    let sink = RepositoryEnvironmentBindingSink::new(repo.clone());

    let committed = sink
        .persist(receipt("binding-fence", "sandbox-a", Some(&current)))
        .await
        .expect("B1");
    let revision_after_bind = repo.get("binding-fence").await.expect("B1").revision;
    let replayed = sink
        .persist(receipt("binding-fence", "sandbox-a", Some(&current)))
        .await
        .expect("B2");
    assert_eq!(replayed, committed, "B2 returns exact Store-read authority");
    assert_eq!(
        repo.get("binding-fence").await.expect("B2").revision,
        revision_after_bind,
        "B2 exact replay performs no root write"
    );
    assert!(
        matches!(
            committed,
            awaken_session_contract::SessionEnvironmentState::Resident {
                effect_id: Some(_),
                generation: Some(_),
                ..
            }
        ),
        "B1 returns generated durable authority"
    );
    let mut renewed_session = persisted("binding-renewed", false, "idle");
    renewed_session.realization = Some(awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: u64::MAX,
        ..current.clone()
    });
    create(repo.as_ref(), renewed_session).await;
    let admitted_before_renewal = awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: u64::MAX - 1,
        ..current.clone()
    };
    sink.persist(receipt(
        "binding-renewed",
        "sandbox-renewed",
        Some(&admitted_before_renewal),
    ))
    .await
    .expect("B3 monotonic renewal authorizes admitted work");
    create(
        repo.as_ref(),
        persisted("binding-unassigned", false, "idle"),
    )
    .await;
    sink.persist(receipt("binding-unassigned", "sandbox-legacy", None))
        .await
        .expect("B6");
    let stale = awaken_session_contract::SessionRealizationLease {
        owner: "runtime-b".into(),
        runtime_incarnation: "runtime-b/boot-1".into(),
        epoch: 4,
        expires_at_unix_ms: u64::MAX,
    };
    let revision_before_stale_replay = repo.get("binding-fence").await.expect("B4").revision;
    assert_eq!(
        sink.persist(receipt("binding-fence", "sandbox-a", Some(&stale)))
            .await
            .expect_err("B4 stale equal-binding replay is fenced")
            .code,
        "session_realization_stale",
        "B4"
    );
    assert_eq!(
        repo.get("binding-fence").await.expect("B4").revision,
        revision_before_stale_replay,
        "B4 must not write"
    );
    let error = sink
        .persist(receipt("binding-fence", "sandbox-b", Some(&stale)))
        .await
        .expect_err("B5 stale binding replacement is fenced");
    assert_eq!(error.code, "session_realization_stale", "B5");
    let expired = awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: 0,
        ..current.clone()
    };
    let mut expired_session = repo.get("binding-fence").await.expect("B6 fixture");
    expired_session.realization = Some(expired.clone());
    let expected_revision = expired_session.revision;
    let payload = awaken_session_contract::SessionMutationPayload::Replace(expired_session);
    let payload_hash = payload.stable_hash();
    assert!(matches!(
        repo.commit_mutation(
            "workspace",
            awaken_session_contract::SessionMutation {
                expected_revision,
                idempotency: awaken_session_contract::IdempotencyRecord {
                    key: "binding-fence:expire".into(),
                    payload_hash,
                },
                payload,
                lifecycle_facts: Vec::new(),
            },
        )
        .await
        .expect("B7 fixture mutation"),
        awaken_session_contract::SessionMutationResult::Applied { .. }
    ));
    assert_eq!(
        sink.persist(receipt("binding-fence", "sandbox-a", Some(&expired)))
            .await
            .expect_err("B7 expired equal-binding replay is fenced")
            .code,
        "session_realization_stale",
        "B7"
    );
    assert_eq!(
        sink.persist(receipt("binding-fence", "sandbox-b", Some(&expired)))
            .await
            .expect_err("B8")
            .code,
        "session_realization_stale",
        "B8"
    );
    assert_eq!(
        repo.get("binding-fence")
            .await
            .expect("binding-fence Session")
            .environment
            .binding()
            .map(str::to_owned)
            .as_deref(),
        Some("sandbox-a"),
        "B4/B5/B7/B8"
    );
}

/// Binding transition cause/effect graph: C1 aggregate is Unmaterialized;
/// C2 it is exact generated Resident; C3 it is terminal; C4 it is in a
/// continuation phase; C5 receipt is Create; C6 receipt is Adopt with the
/// same/different source binding; C7 receipt source/fingerprint is invalid.
/// Effects: E1 one typed root mutation; E2 exact no-write replay; E3 fail
/// closed with byte-identical Store state. The sink
/// must also return only a nonterminal, exact binding/effect/generated Store
/// read after CAS.
///
/// | Rule | Aggregate | Receipt | Effect |
/// |---|---|---|---|
/// | T1 | Unmaterialized | Create/Adopt | E1 generated Resident |
/// | T2 | exact generated Resident | exact | E2 no write |
/// | T2A | Resident | Adopt same binding | E1 update effect, preserve generation |
/// | T3 | Resident | second Create | E3 denied |
/// | T4 | Resident | Adopt different binding | E3 denied |
/// | T5 | terminal | any | E3 denied |
/// | T6 | Suspending/Hibernated/Restoring | any | E3 denied |
/// | T7 | any | invalid receipt source/fingerprint | E3 denied |
#[tokio::test]
async fn environment_binding_sink_uses_only_the_aggregate_typed_transition() {
    fn receipt(
        session_id: &str,
        kind: awaken_session_contract::SessionEnvironmentEffectKind,
        binding: &str,
    ) -> awaken_session_contract::SessionEnvironmentReceipt {
        awaken_session_contract::SessionEnvironmentReceipt::new(session_id, kind, binding, None)
    }

    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repo.as_ref(),
        persisted("binding-transition", false, "idle"),
    )
    .await;
    let sink = RepositoryEnvironmentBindingSink::new(repo.clone());
    let create_receipt = receipt(
        "binding-transition",
        awaken_session_contract::SessionEnvironmentEffectKind::Create,
        "sandbox-a",
    );
    let resident = sink
        .persist(create_receipt.clone())
        .await
        .expect("T1 Create");
    assert!(matches!(
        resident,
        awaken_session_contract::SessionEnvironmentState::Resident {
            generation: Some(_),
            ..
        }
    ));
    create(
        repo.as_ref(),
        persisted("binding-adopt-unmaterialized", false, "idle"),
    )
    .await;
    assert!(matches!(
        sink.persist(receipt(
            "binding-adopt-unmaterialized",
            awaken_session_contract::SessionEnvironmentEffectKind::Adopt,
            "sandbox-adopted",
        ))
        .await
        .expect("T1 Adopt"),
        awaken_session_contract::SessionEnvironmentState::Resident {
            generation: Some(_),
            ..
        }
    ));
    create(
        repo.as_ref(),
        persisted("binding-adopt-existing", false, "idle"),
    )
    .await;
    let prior = sink
        .persist(receipt(
            "binding-adopt-existing",
            awaken_session_contract::SessionEnvironmentEffectKind::Create,
            "sandbox-existing",
        ))
        .await
        .expect("T2A source");
    let adopt_existing = receipt(
        "binding-adopt-existing",
        awaken_session_contract::SessionEnvironmentEffectKind::Adopt,
        "sandbox-existing",
    );
    let adopted = sink.persist(adopt_existing.clone()).await.expect("T2A");
    assert_eq!(adopted.binding(), Some("sandbox-existing"), "T2A/E1");
    assert_eq!(
        adopted.effect_id(),
        Some(adopt_existing.effect_id.as_str()),
        "T2A/E1"
    );
    assert_eq!(adopted.generation(), prior.generation(), "T2A/E1");
    let exact_revision = repo.get("binding-transition").await.expect("T2").revision;
    assert_eq!(
        sink.persist(create_receipt).await.expect("T2"),
        resident,
        "T2 exact Store read"
    );
    assert_eq!(
        repo.get("binding-transition").await.expect("T2").revision,
        exact_revision,
        "T2 no root write"
    );
    for (rule, denied) in [
        (
            "T3",
            receipt(
                "binding-transition",
                awaken_session_contract::SessionEnvironmentEffectKind::Create,
                "sandbox-b",
            ),
        ),
        (
            "T4",
            receipt(
                "binding-transition",
                awaken_session_contract::SessionEnvironmentEffectKind::Adopt,
                "sandbox-b",
            ),
        ),
    ] {
        assert_eq!(
            sink.persist(denied).await.expect_err(rule).code,
            "session_environment_binding_denied",
            "{rule}"
        );
        let after = repo.get("binding-transition").await.expect(rule);
        assert_eq!(after.revision, exact_revision, "{rule}/E3");
        assert_eq!(after.environment, resident, "{rule}/E3");
    }
    let mut invalid_source = receipt(
        "binding-transition",
        awaken_session_contract::SessionEnvironmentEffectKind::Adopt,
        "sandbox-a",
    );
    invalid_source.binding = "tampered-after-signing".into();
    assert_eq!(
        sink.persist(invalid_source).await.expect_err("T7").code,
        "session_environment_binding_denied",
        "T7"
    );
    let after = repo.get("binding-transition").await.expect("T7");
    assert_eq!(after.revision, exact_revision, "T7/E3");
    assert_eq!(after.environment, resident, "T7/E3");

    let terminal = persisted("binding-terminal", false, "terminated");
    create(repo.as_ref(), terminal).await;
    let terminal_before = repo
        .get("binding-terminal")
        .await
        .expect("T5 fixture Store read");
    assert_eq!(
        sink.persist(receipt(
            "binding-terminal",
            awaken_session_contract::SessionEnvironmentEffectKind::Create,
            "sandbox-terminal",
        ))
        .await
        .expect_err("T5")
        .code,
        "session_environment_binding_denied"
    );
    assert_eq!(
        repo.get("binding-terminal").await.expect("T5"),
        terminal_before,
        "T5/E3"
    );

    let mut continuation = persisted("binding-continuation", false, "idle");
    let source_receipt = receipt(
        "binding-continuation",
        awaken_session_contract::SessionEnvironmentEffectKind::Create,
        "sandbox-source",
    );
    continuation.environment = awaken_session_contract::SessionEnvironmentState::Resident {
        binding: source_receipt.binding.clone(),
        effect_id: Some(source_receipt.effect_id),
        generation: Some(awaken_session_contract::SandboxGeneration::new(
            "binding-continuation",
            1,
            u64::MAX,
            "environment",
            "image",
        )),
        idle_since_unix_ms: None,
    };
    continuation
        .environment
        .begin_suspend("workspace", "binding-continuation", 0, None)
        .expect("T6 fixture");
    create(repo.as_ref(), continuation).await;
    let continuation_before = repo
        .get("binding-continuation")
        .await
        .expect("T6 fixture Store read");
    assert_eq!(
        sink.persist(receipt(
            "binding-continuation",
            awaken_session_contract::SessionEnvironmentEffectKind::Adopt,
            "sandbox-source",
        ))
        .await
        .expect_err("T6")
        .code,
        "session_environment_binding_denied"
    );
    assert_eq!(
        repo.get("binding-continuation").await.expect("T6"),
        continuation_before,
        "T6/E3"
    );
}

#[test]
fn environment_binding_readback_rejects_terminal_or_ungenerated_authority() {
    // Cause/effect table: C1 Store read is exact binding+effect; C2 it is
    // terminal; C3 generation is absent. E1 only C1+!C2+!C3 may publish;
    // C2 or C3 returns typed denial. This owns the post-CAS/readback race fence,
    // while the async table above owns mutation/no-mutation effects.
    let receipt = awaken_session_contract::SessionEnvironmentReceipt::new(
        "binding-readback",
        awaken_session_contract::SessionEnvironmentEffectKind::Create,
        "sandbox-readback",
        None,
    );
    let mut session = persisted("binding-readback", false, "idle");
    session.environment = awaken_session_contract::SessionEnvironmentState::Resident {
        binding: receipt.binding.clone(),
        effect_id: Some(receipt.effect_id.clone()),
        generation: None,
        idle_since_unix_ms: None,
    };
    assert_eq!(
        validate_environment_binding_authority(&session, &receipt)
            .expect_err("C3")
            .code,
        "session_environment_binding_denied"
    );
    session
        .environment
        .assign_generation(awaken_session_contract::SandboxGeneration::new(
            "binding-readback",
            1,
            u64::MAX,
            "environment",
            "image",
        ));
    session.execution = awaken_session_contract::SessionExecutionState::Terminated;
    assert_eq!(
        validate_environment_binding_authority(&session, &receipt)
            .expect_err("C2")
            .code,
        "session_environment_binding_denied"
    );
}

/// In-flight renewal cause/effect graph: C1 one MCP generation is Realizing
/// under an admitted lease; C2 the same owner/incarnation/epoch is durably
/// extended before its Stage receipt returns. E1 accepts the old exact
/// receipt under the monotonic lease fence; E2 activates the durable
/// generation but returns one renewal Stage; E3 the renewal receipt leads
/// to Publish with the extended generation. Different owner/epoch is A2 in
/// the contract table; conflicting receipt bindings remain rejected by the
/// canonical receipt verification gate.
///
/// | Rule | C1 | C2 | First effect | Next effect |
/// |---|---|---|---|---|
/// | F1 | yes | yes | E1 + E2, no publish | E3 |
#[tokio::test]
async fn activation_restages_a_generation_renewed_while_its_stage_was_in_flight() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let admitted_expiry = now + 60_000;
    let renewed_expiry = now + 120_000;
    let asserted_lease = awaken_session_contract::SessionRealizationLease {
        owner: "runtime-a".into(),
        runtime_incarnation: "runtime-a/boot-1".into(),
        epoch: 7,
        expires_at_unix_ms: admitted_expiry,
    };
    let current_lease = awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: renewed_expiry,
        ..asserted_lease.clone()
    };
    let mut session = persisted("in-flight-renewal", false, "activating");
    session.realization = Some(current_lease.clone());
    session.mcp = awaken_session_contract::SessionMcpAttachmentSet::from_initial(
        vec![awaken_session_contract::McpAttachmentDraft {
            name: "browser".into(),
            target: McpTarget::parse_http("https://browser.example.test/mcp").unwrap(),
            prompts_as_skills: false,
            credential: None,
            origin: awaken_session_contract::McpAttachmentOrigin::Session,
        }],
        None,
    )
    .unwrap();
    let attachment_id = session.mcp.attachments[0].attachment_id.clone();
    session
        .mcp
        .claim_realization(
            &attachment_id,
            awaken_session_contract::McpGeneration(1),
            awaken_session_contract::McpRealizationClaim {
                realization_id: "realization-1".into(),
                runtime_incarnation: asserted_lease.runtime_incarnation.clone(),
                lease_epoch: asserted_lease.epoch,
                lease_expires_at_unix_ms: admitted_expiry,
                stage_idempotency_key: "stage-1".into(),
            },
        )
        .unwrap();
    let admitted_request = projection::stage_mcp_request(
        "workspace",
        &session.session_id,
        &session.mcp.attachments[0],
    )
    .unwrap();
    create(repo.as_ref(), session).await;
    let app = application(repo, Arc::new(RecordingEnvironmentSource::default()));
    let admitted_receipt = awaken_session_contract::McpRealizationReceipt {
        receipt_fingerprint: admitted_request.fingerprint(),
        generation: admitted_request.generation.clone(),
        realization_id: admitted_request.realization_id.clone(),
        selected_plaintext_holder: admitted_request.selected_plaintext_holder.clone(),
        actual_realization_kind: None,
    };

    let directive =
        awaken_session_contract::SessionRealizationControl::activate_session_realization(
            &app,
            awaken_session_contract::ActivateSessionRealization {
                session_id: "in-flight-renewal".into(),
                lease: asserted_lease,
                prepared_resource_revision: None,
                mcp_receipts: vec![admitted_receipt],
            },
        )
        .await
        .expect("F1/E1");
    let awaken_session_contract::SessionRealizationAction::Stage {
        prepare_session,
        mut mcp_stages,
    } = directive.action
    else {
        panic!("F1/E2 must restage the extended exact fence before publish")
    };
    assert!(!prepare_session, "F1/E2 does not recreate the Environment");
    let renewed_request = mcp_stages.remove(0);
    assert_eq!(
        renewed_request.renewal_binding_fingerprint(),
        admitted_request.renewal_binding_fingerprint(),
        "F1/E2"
    );
    assert_eq!(
        renewed_request.generation.lease_expires_at_unix_ms, renewed_expiry,
        "F1/E2"
    );
    let renewed_receipt = awaken_session_contract::McpRealizationReceipt {
        receipt_fingerprint: renewed_request.fingerprint(),
        generation: renewed_request.generation.clone(),
        realization_id: renewed_request.realization_id,
        selected_plaintext_holder: renewed_request.selected_plaintext_holder,
        actual_realization_kind: None,
    };
    let directive =
        awaken_session_contract::SessionRealizationControl::activate_session_realization(
            &app,
            awaken_session_contract::ActivateSessionRealization {
                session_id: "in-flight-renewal".into(),
                lease: current_lease,
                prepared_resource_revision: None,
                mcp_receipts: vec![renewed_receipt],
            },
        )
        .await
        .expect("F1/E3");
    let awaken_session_contract::SessionRealizationAction::Publish { publish, .. } =
        directive.action
    else {
        panic!("F1/E3 must publish after exact renewal restage")
    };
    assert_eq!(publish, vec![renewed_request.generation], "F1/E3");
}

#[test]
fn lifecycle_supervisor_claim_is_one_shot() {
    // Cause/effect decision table: C1=unclaimed fence, C2=already claimed.
    // R1 C1 -> E1 first caller becomes owner; R2 C2 -> E2 every later caller
    // is rejected. This proves moving the fence out of the protocol adapter
    // cannot start parallel Session lifecycle supervisors.
    let fence = AtomicBool::new(false);
    assert!(claim_once(&fence), "R1");
    assert!(!claim_once(&fence), "R2");
}

#[test]
fn native_only_lazy_provisioning_decision_table() {
    // Causes: C1 eager policy; C2 lazy policy; C3 implicit/native backend;
    // C4 ACP/A2A backend. Effects: E1 accept; E2 reject before Session
    // realization. Environment policy tests own disabled/exact-version rules.
    //
    // | Rule | policy | runtime | effect |
    // | R1 | eager | any | accept |
    // | R2 | lazy | implicit/native | accept |
    // | R3 | lazy | ACP/A2A | reject |
    use SandboxProvisioning::{Eager, OnToolUse};
    for (case, provisioning, runtime, accepted) in [
        ("R1 eager native", Eager, None, true),
        ("R1 eager ACP", Eager, Some("acp:claude"), true),
        ("R2 lazy implicit native", OnToolUse, None, true),
        ("R2 lazy explicit native", OnToolUse, Some("awaken"), true),
        ("R2 lazy genai native", OnToolUse, Some("genai"), true),
        (
            "R2 lazy custom native",
            OnToolUse,
            Some("provider-native"),
            true,
        ),
        ("R3 lazy ACP", OnToolUse, Some("acp:claude"), false),
        (
            "R3 lazy A2A",
            OnToolUse,
            Some("a2a:https://agent.example"),
            false,
        ),
    ] {
        assert_eq!(
            validate_sandbox_provisioning_runtime(provisioning, runtime).is_ok(),
            accepted,
            "{case}"
        );
    }
}

#[tokio::test]
async fn durable_session_truth_owns_one_work_projection_path() {
    // Cause/effect graph: C1 frozen Environment is self-hosted; C2 Session is
    // nonterminal; C3 the projection command is replayed. Effects: E1 only
    // C1+C2 dispatches;
    // E2 replay uses the same idempotent port and creates no second identity.
    //
    // | Rule | self-hosted | terminal | replay | effect |
    // | R1 | yes | no | no | project one |
    // | R2 | yes | no | yes | retain one |
    // | R3 | no | no | any | skip |
    // | R4 | yes | yes | any | skip |
    let repo = awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
        .expect("session repository");
    create(&repo, persisted("external", true, "idle")).await;
    create(&repo, persisted("local", false, "idle")).await;
    create(&repo, persisted("terminal", true, "terminated")).await;
    let environments = RecordingEnvironmentSource::default();

    let first = reconcile_work_dispatches(&repo, &environments).await;
    assert_eq!(first.settled, 1, "R1/R3/R4");
    assert!(first.failures.is_empty());
    assert_eq!(environments.dispatched.lock().unwrap().len(), 1, "R1");

    let replay = reconcile_work_dispatches(&repo, &environments).await;
    assert_eq!(replay.settled, 1, "R2");
    assert!(replay.failures.is_empty());
    assert_eq!(environments.dispatched.lock().unwrap().len(), 1, "R2");
}

#[tokio::test]
async fn work_dispatch_reconciliation_isolates_each_session_failure() {
    // Work-projection FMECA decision table. Causes: C1 a durable Session needs
    // external work; C2 its queue write succeeds; C3 a sibling queue write
    // fails; C4 an unrelated Session needs no dispatch. Effects: E1 every
    // eligible Session is attempted; E2 successes settle independently; E3
    // failures retain exact Session/Environment diagnostics for later retry;
    // E4 ineligible Sessions cause no side effect. Rules: W1 C1+C2=>E1+E2;
    // W2 C1+C3=>E1+E3 without aborting W1; W3 C4=>E4.
    let repo = awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
        .expect("session repository");
    create(&repo, persisted("external-failed", true, "idle")).await;
    create(&repo, persisted("external-settled", true, "idle")).await;
    create(&repo, persisted("local-skip", false, "idle")).await;
    let environments = RecordingEnvironmentSource::default();
    environments.fail_for("external-failed");

    let report = reconcile_work_dispatches(&repo, &environments).await;
    assert_eq!(report.settled, 1, "W1/W2");
    assert_eq!(
        environments
            .dispatched
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        ["external-settled".to_string()],
        "W1/W3"
    );
    assert_eq!(report.failures.len(), 1, "W2");
    assert_eq!(report.failures[0].session_id, "external-failed", "W2");
    assert_eq!(report.failures[0].environment_id, "env-worker", "W2");
    assert!(
        report.failures[0].message.contains("injected failure"),
        "W2 preserves the retry diagnostic"
    );
}
