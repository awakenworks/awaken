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

fn repository_input(name: &str, remote_url: &str) -> SessionRepositoryResourceInput {
    SessionRepositoryResourceInput {
        id: "repo-replay".into(),
        workspace_id: "workspace".into(),
        name: name.into(),
        description: "Session source".into(),
        remote_url: remote_url.into(),
        authorization_token: None,
        credential: None,
        mount_path: "/workspace/source".into(),
        initial_branch: Some("main".into()),
        initial_commit: None,
    }
}

fn token_repository_input(id: &str, name: &str) -> SessionRepositoryResourceInput {
    let mut input = repository_input(name, "https://github.com/awaken/example.git");
    input.id = id.into();
    input.authorization_token = Some(awaken_agent_contract::RedactedString::new("x"));
    input
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

/// Fault-only ingress port; it owns no credential state or replay ledger.
#[derive(Default)]
struct FaultInjectingRepositoryCredentialIngress {
    calls: AtomicUsize,
    failures: AtomicUsize,
}

impl FaultInjectingRepositoryCredentialIngress {
    fn fail_next(&self) {
        self.failures.store(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl RepositoryCredentialIngress for FaultInjectingRepositoryCredentialIngress {
    async fn enter_repository_token(
        &self,
        source_id: awaken_credential_contract::CredentialSourceId,
        _workspace_id: &str,
        _target: awaken_credential_contract::CredentialTarget,
        _token: awaken_agent_contract::RedactedString,
    ) -> Result<awaken_credential_contract::CredentialSourceId, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if FaultInjectingResourceRegistry::consume_failure(&self.failures) {
            Err("injected credential ingress failure".into())
        } else {
            Ok(source_id)
        }
    }

    async fn rotate_repository_token(
        &self,
        _source_id: &awaken_credential_contract::CredentialSourceId,
        _expected_revision: u64,
        _workspace_id: &str,
        _target: awaken_credential_contract::CredentialTarget,
        _token: awaken_agent_contract::RedactedString,
    ) -> Result<u64, String> {
        Err("rotation is outside this registration test".into())
    }
}

fn repository_saga_application(
    registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    ingress: Arc<dyn RepositoryCredentialIngress>,
) -> SessionApplication {
    let sessions: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Session repository"),
    );
    let mut app = application(sessions, Arc::new(RecordingEnvironmentSource::default()));
    app.set_resource_registry(registry);
    app.set_repository_credential_ingress(ingress);
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
    let sessions: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Session repository"),
    );

    let mut original = application(
        sessions.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    original.set_resource_registry(registry.clone());
    assert_eq!(
        original
            .configure_session_repository(repository_input(
                "Repository",
                "https://example.test/source.git",
            ))
            .await
            .expect("R1 registers"),
        awaken_resource_contract::RepositoryId::from("repo-replay"),
        "R1/E1"
    );
    drop(original);

    let mut restarted = application(sessions, Arc::new(RecordingEnvironmentSource::default()));
    restarted.set_resource_registry(registry.clone());
    assert_eq!(
        restarted
            .configure_session_repository(repository_input(
                "Repository",
                "https://example.test/source.git",
            ))
            .await
            .expect("R2 exact replay"),
        awaken_resource_contract::RepositoryId::from("repo-replay"),
        "R2/E2"
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
            .find_repository("workspace", "repo-replay")
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
                "repo-replay",
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
    // Cause/effect graph: C1 token and credential ref are mutually exclusive;
    // C2 the Repository aggregate validates; C3 Suspended registration commits;
    // C4 an existing aggregate is an exact Suspended/Active replay; C5 Vault
    // ingress seals the deterministic binding; C6 Registry activation commits.
    // Effects: E1 pre-admission failure writes neither participant; E2 register
    // failure never reaches Vault; E3 ingress/activation failure retains a
    // non-executable Suspended aggregate; E4 exact retry resumes the same saga
    // and reaches Active; E5 conflicting replay never reaches Vault.
    //
    // | Rule | C1 | C2 | C3/C4 | C5 | C6 | Effect |
    // |---|---|---|---|---|---|---|
    // | S1 dual token+ref | F | - | - | - | - | E1 |
    // | S2 invalid config | T | F | - | - | - | E1 |
    // | S3 register failure | T | T | F | - | - | E2 |
    // | S4 ingress failure | T | T | T | F | - | E3 |
    // | S5 exact retry | T | T | T | T | T | E4 |
    // | S6 conflicting replay | T | T | F | - | - | E5 |
    // | S7 activation failure | T | T | T | T | F | E3 |
    // | S8 activation retry | T | T | T | T | T | E4 |
    let resources = awaken_resource_persistence::ephemeral().expect("Resource authorities");
    let inner = resources.authorities().resource_registry();
    let registry = Arc::new(FaultInjectingResourceRegistry::new(inner.clone()));
    let ingress = Arc::new(FaultInjectingRepositoryCredentialIngress::default());
    let app = repository_saga_application(registry.clone(), ingress.clone());

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
            .find_repository("workspace", "repo-dual")
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
            .find_repository("workspace", "repo-invalid")
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
            .find_repository("workspace", "repo-register")
            .unwrap()
            .is_none(),
        "S3 register failure does not persist"
    );

    ingress.fail_next();
    assert!(
        app.configure_session_repository(token_repository_input("repo-ingress", "Ingress"))
            .await
            .is_err(),
        "S4/E3 ingress failure"
    );
    let suspended = inner
        .find_repository("workspace", "repo-ingress")
        .unwrap()
        .expect("S4 Suspended receipt");
    assert_eq!(
        suspended.state,
        awaken_resource_contract::ResourceState::Suspended,
        "S4/E3"
    );
    assert!(matches!(
        inner.resolve_repository("workspace", "repo-ingress"),
        Err(awaken_resource_contract::ResourceRegistryError::NotActive {
            state: awaken_resource_contract::ResourceState::Suspended,
            ..
        })
    ));
    assert_eq!(
        inner
            .find_repository_config(
                "workspace",
                "repo-ingress",
                awaken_resource_contract::ConfigVersion::INITIAL,
            )
            .unwrap()
            .expect("S4 config")
            .credential_binding
            .as_deref(),
        Some("repo-ingress:credential"),
        "S4 deterministic saga binding"
    );

    assert_eq!(
        app.configure_session_repository(token_repository_input("repo-ingress", "Ingress"))
            .await
            .expect("S5/E4 exact retry"),
        awaken_resource_contract::RepositoryId::from("repo-ingress")
    );
    assert_eq!(ingress.calls.load(Ordering::SeqCst), 2, "S4-S5");
    assert_eq!(
        inner
            .find_repository("workspace", "repo-ingress")
            .unwrap()
            .expect("S5 Active receipt")
            .state,
        awaken_resource_contract::ResourceState::Active,
        "S5/E4"
    );

    let ingress_before_conflict = ingress.calls.load(Ordering::SeqCst);
    assert!(
        app.configure_session_repository(token_repository_input(
            "repo-ingress",
            "Conflicting Ingress",
        ))
        .await
        .is_err(),
        "S6/E5 conflicting replay"
    );
    assert_eq!(
        ingress.calls.load(Ordering::SeqCst),
        ingress_before_conflict,
        "S6/E5 zero Vault ingress"
    );

    let ingress_before_activation = ingress.calls.load(Ordering::SeqCst);
    let activations_before = registry.activation_calls.load(Ordering::SeqCst);
    registry.fail_next_activation();
    assert!(
        app.configure_session_repository(token_repository_input("repo-activate", "Activate"))
            .await
            .is_err(),
        "S7/E3 activation failure"
    );
    assert_eq!(
        ingress.calls.load(Ordering::SeqCst),
        ingress_before_activation + 1,
        "S7 ingress precedes activation"
    );
    assert_eq!(
        inner
            .find_repository("workspace", "repo-activate")
            .unwrap()
            .expect("S7 Suspended receipt")
            .state,
        awaken_resource_contract::ResourceState::Suspended,
        "S7/E3"
    );
    assert!(matches!(
        inner.resolve_repository("workspace", "repo-activate"),
        Err(awaken_resource_contract::ResourceRegistryError::NotActive { .. })
    ));

    app.configure_session_repository(token_repository_input("repo-activate", "Activate"))
        .await
        .expect("S8/E4 activation retry");
    assert_eq!(
        ingress.calls.load(Ordering::SeqCst),
        ingress_before_activation + 2,
        "S8 exact ingress replay"
    );
    assert_eq!(
        registry.activation_calls.load(Ordering::SeqCst),
        activations_before + 2,
        "S7-S8 one failed and one successful activation"
    );
    assert_eq!(
        inner
            .find_repository("workspace", "repo-activate")
            .unwrap()
            .expect("S8 Active receipt")
            .state,
        awaken_resource_contract::ResourceState::Active,
        "S8/E4"
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
/// exactly replay; C3 an existing key carries another hash; C4 another key
/// targets an existing identity. Effects are E1 one insert/revision advance,
/// E2 replay of the same revision, E3 idempotency mismatch, and E4 identity
/// conflict. The application is the only repository-result classifier.
///
/// | Rule | Identity | Key/hash | Payload | Effect |
/// |---|---|---|---|---|
/// | C1 | unused | new | original | E1 |
/// | C2 | existing | exact | exact | E2 |
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
    assert_eq!(
        inserted.revision,
        awaken_session_contract::SessionRevision(1),
        "C1"
    );
    let replayed = app
        .create_session_root("workspace", original.clone(), record.clone(), Vec::new())
        .await
        .expect("C2");
    assert_eq!(replayed.revision, inserted.revision, "C2");

    let mut changed = original.clone();
    changed.title = Some("changed".into());
    assert_eq!(
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
        Err(SessionMutationError::IdempotencyMismatch),
        "C3"
    );
    assert_eq!(
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
        Err(SessionMutationError::Conflict),
        "C4"
    );
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
    // send_to_agent result committed, so the rebuildable relationship query is
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

/// Message-execution FMECA and cause/effect graph. Failure modes are FM1 a
/// successful Runtime step leaves the Session running, FM2 a Runtime error
/// skips settlement, FM3 admission failure invokes Runtime anyway. Causes: C1
/// Session is idle, C2 Runtime succeeds, C3 Runtime fails after admission, C4
/// Session is terminal before admission. Effects: E1 one epoch and idle
/// settlement with a step, E2 one epoch and idle settlement with the original
/// error, E3 no Runtime effect and terminal truth unchanged. Cause graph:
/// C1&&C2 -> E1; C1&&C3 -> E2; C4 -> E3.
///
/// | Rule | Idle | Runtime | Terminal | Effect |
/// |---|---|---|---|---|
/// | M1 | yes | success | no | E1 |
/// | M2 | yes | error | no | E2 |
/// | M3 | no | not called | yes | E3 |
#[tokio::test]
async fn session_message_execution_always_settles_its_activity() {
    let success_repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("M1 repository"),
    );
    create(
        success_repo.as_ref(),
        persisted("message-success", false, "idle"),
    )
    .await;
    let success = application_with_runtime(
        Arc::new(SuccessfulRuntime),
        success_repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    )
    .run_session_message(
        "agent",
        "message-success",
        vec![awaken_agent_contract::agent::content::ContentBlock::text(
            "go",
        )],
        None,
        Arc::new(DiscardProgress),
    )
    .await
    .expect("M1 successful message");
    assert_eq!(success.session.execution, SessionExecutionState::Idle, "M1");
    assert_eq!(success.session.activity_epoch, 1, "M1");

    let failure_repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("M2 repository"),
    );
    create(
        failure_repo.as_ref(),
        persisted("message-failure", false, "idle"),
    )
    .await;
    let failure = application(
        failure_repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    )
    .run_session_message(
        "agent",
        "message-failure",
        vec![awaken_agent_contract::agent::content::ContentBlock::text(
            "go",
        )],
        None,
        Arc::new(DiscardProgress),
    )
    .await;
    assert!(failure.is_err(), "M2");
    let settled = failure_repo.get("message-failure").await.expect("M2 state");
    assert_eq!(settled.execution, SessionExecutionState::Idle, "M2");
    assert_eq!(settled.activity_epoch, 1, "M2");

    let mut terminal = persisted("message-terminal", false, "idle");
    terminal.execution = SessionExecutionState::Terminated;
    create(failure_repo.as_ref(), terminal).await;
    let terminal_before = failure_repo
        .get("message-terminal")
        .await
        .expect("M3 initial state");
    let rejected = application(
        failure_repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    )
    .run_session_message(
        "agent",
        "message-terminal",
        Vec::new(),
        None,
        Arc::new(DiscardProgress),
    )
    .await;
    assert!(rejected.is_err(), "M3");
    assert_eq!(
        failure_repo
            .get("message-terminal")
            .await
            .expect("M3 state"),
        terminal_before,
        "M3"
    );
}

#[tokio::test]
async fn committed_message_boundary_failures_preserve_the_open_interval_for_exact_retry() {
    // Cause/effect graph: C1 the Runtime Step has one exact terminal Run id;
    // C2 cumulative usage is available/unavailable; C3 the matching lifecycle
    // boundary is visible/missing. Effects: E1 a C2 failure returns unavailable
    // before observing or settling; E2 C2-ok+C3-missing does the same; E3 after
    // the missing owner fact appears, the same epoch closes exactly once with
    // one observation and one historical interval. Constraint: neither failure
    // may manufacture a partial close or a second usage/event authority.
    //
    // | Rule | Usage | Boundary | Effect |
    // |---|---|---|---|
    // | F1 | unavailable | visible | E1 |
    // | F2 | available | missing | E2 |
    // | F3 | available | visible on retry | E3 |
    for (rule, session_id, usage_unavailable, publish_boundary) in [
        ("F1", "message-usage-unavailable", true, true),
        ("F2", "message-boundary-unavailable", false, false),
    ] {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("boundary failure repository"),
        );
        create(repo.as_ref(), persisted(session_id, false, "idle")).await;
        let runtime = Arc::new(ScriptedMessageBoundaryRuntime::new(
            usage_unavailable,
            publish_boundary,
        ));
        let app = application_with_runtime(
            runtime.clone(),
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );

        let failed = app
            .run_session_message(
                "agent",
                session_id,
                vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "go",
                )],
                None,
                Arc::new(DiscardProgress),
            )
            .await;
        assert!(failed.is_err(), "{rule} must remain retryable");
        let open = repo.get(session_id).await.expect("open Session");
        assert_eq!(open.execution, SessionExecutionState::Running, "{rule}");
        assert_eq!(open.active_activity_epochs, BTreeSet::from([1]), "{rule}");
        assert!(open.running_interval.is_some(), "{rule}");
        assert!(open.closed_runtime_intervals.is_empty(), "{rule}");

        runtime.set_usage_unavailable(false);
        runtime.commit_root_boundary(session_id);
        let run_id = RunId(format!("{session_id}-run"));
        let state = RunState::Ended(EndCause::NaturalEnd);
        let observation = app
            .runtime_interval_observation(
                session_id,
                1,
                &ThreadId(session_id.into()),
                &run_id,
                &state,
                None,
            )
            .await
            .expect("F3 lifecycle read")
            .expect("F3 exact boundary");
        let usage = app.session_usage(session_id).await.expect("F3 usage read");
        app.reconcile_managed_budget_usage(session_id, usage)
            .await
            .expect("F3 usage reconciliation");
        app.settle_activity_observed(session_id, 1, Some(observation))
            .await
            .expect("F3 exact retry settlement");
        let closed = repo.get(session_id).await.expect("closed Session");
        assert_eq!(closed.execution, SessionExecutionState::Idle, "F3/E3");
        assert!(closed.running_interval.is_none(), "F3/E3");
        assert_eq!(closed.closed_runtime_intervals.len(), 1, "F3/E3");
        assert_eq!(
            closed.closed_runtime_intervals[0].observations.len(),
            1,
            "F3/E3"
        );
    }
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
        mcp_candidates: None,
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
        mcp_candidates: None,
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
        mcp_candidates: None,
        idempotency_key: Some(key.into()),
        request_fingerprint: awaken_session_contract::stable_fingerprint(&(budget, key)),
        if_match: None,
    };

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

    let concurrent = command(Some(2), "concurrent-exact");
    let (left, right) = tokio::join!(
        app.update_session("resume-budget-concurrent", concurrent.clone()),
        app.update_session("resume-budget-concurrent", concurrent),
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

    runtime.set_budget_resume_dispositions([SessionBudgetResumeDisposition::Stale]);
    app.update_session("resume-budget-stale", command(Some(2), "stale-raise"))
        .await
        .expect("R4 stale resume is a successful update");
    let stale = repo.get("resume-budget-stale").await.unwrap();
    assert_eq!(stale.execution, SessionExecutionState::Idle, "R4/E4");
    assert!(stale.active_activity_epochs.is_empty(), "R4/E4");
    assert!(stale.running_interval.is_none(), "R4/E4");
}

/// Cause/effect graph: C1 the Runtime presents the exact durable realization
/// lease; C2 the binding is new or an idempotent replay; C3 the same epoch
/// has been renewed monotonically; C4 a replacement owner/epoch has fenced
/// the Runtime. C1 permits the ordinary root CAS; C3 preserves in-flight
/// work admitted before renewal; C4 rejects a binding change while an equal
/// durable binding remains a side-effect-free recovery replay.
///
/// | Rule | Asserted lease | Binding | Effect |
/// |---|---|---|---|
/// | B1 | exact | new | persist once |
/// | B2 | exact | equal | idempotent success |
/// | B3 | shorter same-epoch assertion under live renewal | new/equal | authorized |
/// | B4 | stale owner/epoch | equal | idempotent success, no mutation |
/// | B5 | stale owner/epoch | different | fenced, no mutation |
/// | B6 | aggregate/assertion both absent | new/equal | legacy CAS path |
/// | B7 | current lease expired | equal | idempotent success, no mutation |
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

    sink.persist(receipt("binding-fence", "sandbox-a", Some(&current)))
        .await
        .expect("B1");
    sink.persist(receipt("binding-fence", "sandbox-a", Some(&current)))
        .await
        .expect("B2");
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
