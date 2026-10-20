// @formal evidence session.agent_message_completed_target_allows_late_source_receipt_without_reexecution
// @formal evidence session.agent_message_exact_replay_reuses_one_dispatch_and_payload_conflict_stutters
// @formal evidence session.agent_message_missing_lifecycle_projection_relinquishes_without_settlement
// @formal evidence session.agent_message_target_boundary_not_source_receipt_settles_activity
// @formal evidence session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
// @formal evidence session_execution.activity_receipts_are_unique_durable_and_exactly_settled
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal evidence session_run.execution_requires_dispatch_work_and_realization_owner
// @formal evidence session_time.root_observations_are_revision_bound_and_replay_stable
// @formal evidence session_work.only_a_claimed_nonterminal_run_may_revive_stopped_work
// awaken-allow(file-limit-ceiling): 7165 (legacy oversized test file: split, never grow)
use super::*;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_credential_contract::CredentialMaterialInput;
use awaken_session_repository_contract::LifecycleOutbox as _;
use awaken_session_state_contract::ManagedLifecycleFact;
use awaken_session_types::SessionId;
mod coordinated_resources;
mod creation_inputs;
mod credential_ingress;
use credential_ingress::FaultInjectingCredentialMaterialIngress;
mod activity_settlement_owner;
mod first_materialization;
mod input_mounts;
mod interval_admission;
mod resource_registry;
use resource_registry::FaultInjectingResourceRegistry;
mod lost_root_cas_replay;
mod session_updates;
mod unattempted_archive_witness;
pub(super) mod unattempted_profiled_release;
mod work_projection;

async fn authority_repository(
    context: &str,
) -> Arc<awaken_session_store::SqliteManagedSessionRepository> {
    Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect(context),
    )
}

fn coordinated_session(id: &str) -> PersistedSession {
    let mut session = persisted(id, false, "idle");
    let environment = session
        .frozen_baseline()
        .expect("fixture baseline")
        .environment
        .clone();
    session.baseline = awaken_session_configuration_contract::SessionBaselineState::Frozen(
        awaken_session_configuration_contract::SessionBaseline::compile(
            awaken_session_configuration_contract::SessionBaselineInputs {
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
    // Coordinated admission cases create a new Session, unlike persisted(),
    // which deliberately represents a legacy missing activity history. Use the
    // original constructor to retain complete pre-effect membership evidence.
    let mut fresh = PersistedSession::frozen_with_budget(
        id,
        session.frozen_baseline().unwrap().clone(),
        session.resources,
        session.mcp,
        session.title,
        session.metadata,
        session.tools,
        session.budget,
    );
    fresh.execution = session.execution;
    fresh
}

fn session_with_unsettled_event(id: &str) -> PersistedSession {
    let mut session = persisted(id, false, "idle");
    let mut batch = awaken_session_event_contract::SessionEventBatch::compile(
        &awaken_session_types::SessionId::from(id),
        format!("batch:{id}"),
        vec![
            awaken_session_event_contract::SessionEventInput::UserMessage {
                content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "accepted before terminal",
                )],
            },
        ],
    )
    .expect("valid retained Event batch");
    batch.admitted_revision = awaken_session_types::SessionRevision(1);
    batch
        .anchor_admission_projection(
            awaken_session_event_contract::SessionEventProjectionAnchor {
                source_commit_cursor: 0,
            },
        )
        .unwrap();
    session.event_batches.push(batch);
    session
}

async fn anchor_unsettled_event(
    app: &SessionApplication,
    repository: &dyn awaken_session_repository_contract::SessionStore,
    session_id: &str,
) {
    let mut session = repository
        .get(&SessionId::from(session_id))
        .await
        .expect("retained Session");
    let operation_id = session.event_batches[0].events[0]
        .event
        .operation_id()
        .to_string();
    assert!(
        session.event_batches[0]
            .mark_processed(
                &operation_id,
                awaken_session_event_contract::SessionEventProjectionAnchor {
                    source_commit_cursor: 17,
                },
                1_700_000_000_000
            )
            .expect("anchor exact accepted effect")
    );
    let _fixture = app
        .mutations
        .commit_session_snapshot_with_outcome(
            "workspace",
            awaken_session_state_contract::SessionAggregate::try_from(session)
                .expect("valid test snapshot"),
            &format!("test-anchor-event:{session_id}"),
            Vec::new(),
        )
        .await
        .expect("commit effect anchor");
}

fn coordination_spawn_command(
    session_id: &str,
) -> awaken_session_execution_contract::SessionAgentMessageCommand {
    awaken_session_execution_contract::SessionAgentMessageCommand {
        session_id: session_id.into(),
        source_thread_id: ThreadId(session_id.into()),
        source_run_id: RunId(format!("{session_id}-parent-run")),
        source_call_id: "send-call".into(),
        operation_id: "send-operation".into(),
        target: awaken_session_execution_contract::SessionAgentTarget::Spawn {
            agent_id: "coord-child".into(),
        },
        message: "investigate".into(),
    }
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

async fn register_replayed_token_repository(
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
        .await
        .expect("register replay fixture");
}

fn repository_retirement_resources(
    session_id: &str,
    credential: Option<awaken_credential_contract::CredentialRef>,
) -> crate::session_resource::ResolvedSessionResources {
    let repository_id = managed_repository_id(session_id);
    let remote_url = "https://github.com/awaken/example.git";
    let credential_binding = credential
        .as_ref()
        .map(|_| format!("{repository_id}:credential"));
    let credential = credential.map(|credential| {
        let holder = awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Workload,
            "spiffe://awaken.test/sandbox",
        );
        Box::new(crate::session_resource::ResolvedRepositoryCredential {
            access: awaken_credential_contract::CredentialAccess::new(
                credential,
                awaken_credential_contract::CredentialMaterialSource::ControlPlaneReference,
                crate::session_resource::repository_transport_credential_usage(),
                awaken_credential_contract::CredentialExecutionPolicy::exact(
                    holder.clone(),
                    awaken_credential_contract::ModelExposurePolicy::Forbidden,
                ),
            )
            .with_target(
                crate::session_resource::repository_transport_credential_target(remote_url)
                    .expect("Repository target"),
            ),
            selected_plaintext_holder: holder,
        })
    });
    crate::session_resource::ResolvedSessionResources::try_new(
        vec![crate::session_resource::ResolvedInput {
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            binding_id: awaken_resource_contract::BindingId::from(format!(
                "repository-{session_id}"
            )),
            source: crate::session_resource::ResolvedInputSource::Repository {
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
    resources: crate::session_resource::ResolvedSessionResources,
) -> PersistedSession {
    let mut session = persisted(session_id, self_hosted, "idle");
    session.resources = awaken_session_state_contract::SessionResourceState::from_active(resources);
    session
        .resources
        .prepare(
            &awaken_session_types::SessionId::from(session_id),
            crate::session_resource::ResolvedSessionResources::default(),
        )
        .expect("prepare omission");
    session.resources.commit().expect("commit omission");
    session
}

async fn register_retirement_repository(
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
        .await
        .expect("register retirement fixture");
    registry
        .change_repository_state(awaken_resource_contract::ChangeRepositoryState {
            workspace_id: "workspace".into(),
            id: repository_id.into(),
            state: awaken_resource_contract::ResourceState::Active,
        })
        .await
        .expect("activate retirement fixture");
}

async fn repository_saga_application(
    registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    ingress: Arc<dyn awaken_credential_contract::CredentialMaterialIngress>,
) -> SessionApplication {
    let sessions: Arc<dyn awaken_session_repository_contract::SessionStore> =
        authority_repository("Session repository").await;
    repository_retirement_application(sessions, registry, ingress)
}

fn repository_retirement_application(
    sessions: Arc<dyn awaken_session_repository_contract::SessionStore>,
    registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    ingress: Arc<dyn awaken_credential_contract::CredentialMaterialIngress>,
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
    let registry = super::creation::ephemeral_resource_registry().await;
    let repository_id = managed_repository_id("repo-replay");
    let sessions: Arc<dyn awaken_session_repository_contract::SessionStore> =
        authority_repository("Session repository").await;

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
        awaken_session_types::RunErrorKind::BadRequest,
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
        awaken_session_types::RunErrorKind::BadRequest,
        "R4/E3"
    );
    assert_eq!(
        registry
            .find_repository("workspace", &repository_id)
            .await
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
            .await
            .expect("inventory")
            .expect("registered config")
            .remote_url,
        "https://example.test/source.git",
        "R3-R4 preserve original Registry truth"
    );
}

#[tokio::test]
async fn token_repository_registration_is_one_recoverable_registry_vault_saga() {
    // O8 cause/effect: C1 token/ref exclusivity and C2 the pure Repository
    // aggregate validation pass/fail before effects; C3 Registry registration
    // fails, Applies, or Replays; C4 Vault ingress fails, Applies, or Replays;
    // C5 activation succeeds/fails. Effects: E1 C1/C2 failure writes neither
    // participant; E2 C3 failure never reaches Vault; E3 a pre-root failure
    // retires only a Registry participant Applied by this command; E4 it retires
    // only a Vault participant Applied by this command; E5 every Replayed
    // participant remains byte-for-byte durable; E6 success reaches Active.
    // Constraint: compensation preserves the first error and provenance is
    // participant-local; a failed command never infers replayed truth is orphaned.
    // MaterialEntry.created is the original insertion fact: Applied=true, Replayed=false.
    // S7-S10 keep Registry-independent compensation; S11/S12 keep success/replay ownership.
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
    let inner = super::creation::ephemeral_resource_registry().await;
    let registry = Arc::new(FaultInjectingResourceRegistry::new(inner.clone()));
    let ingress = Arc::new(FaultInjectingCredentialMaterialIngress::default());
    let app = repository_saga_application(registry.clone(), ingress.clone()).await;
    let repository_state = |session_id: &str| {
        let repository_id = managed_repository_id(session_id);
        let inner = inner.clone();
        async move {
            inner
                .find_repository("workspace", &repository_id)
                .await
                .expect("Registry inventory")
                .expect("Repository definition")
                .state
        }
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
            .await
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
            .await
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
            .await
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
        repository_state("repo-ingress-applied").await,
        awaken_resource_contract::ResourceState::Deleted,
        "S4/E3 only this command's Applied Registry participant retires"
    );
    assert!(matches!(
        inner
            .resolve_repository("workspace", &managed_repository_id("repo-ingress-applied"))
            .await,
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
            .await
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
    register_replayed_token_repository(inner.as_ref(), &replayed_ingress).await;
    ingress.fail_next();
    let retirements_before = ingress.retirements().len();
    app.configure_session_repository(replayed_ingress)
        .await
        .expect_err("S5 injected Vault failure");
    assert_eq!(
        repository_state("repo-ingress-replayed").await,
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
        repository_state("repo-ingress-replayed").await,
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
        repository_state("repo-activate-aa").await,
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
        repository_state("repo-activate-ar").await,
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
    register_replayed_token_repository(inner.as_ref(), &replayed_registry_applied_vault).await;
    let retirements_before = ingress.retirements().len();
    registry.fail_next_activation();
    app.configure_session_repository(replayed_registry_applied_vault)
        .await
        .expect_err("S9 injected activation failure");
    assert_eq!(
        repository_state("repo-activate-ra").await,
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
    register_replayed_token_repository(inner.as_ref(), &replayed_registry_replayed_vault).await;
    ingress.seed_replayed_source(&replayed_registry_replayed_vault.id);
    let retirements_before = ingress.retirements().len();
    registry.fail_next_activation();
    app.configure_session_repository(replayed_registry_replayed_vault)
        .await
        .expect_err("S10 injected activation failure");
    assert_eq!(
        repository_state("repo-activate-rr").await,
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
    assert!(
        succeeded
            .credential
            .as_ref()
            .expect("S11 credential")
            .created,
        "S11 Vault provenance"
    );
    assert_eq!(
        repository_state("repo-activate-success").await,
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
    assert!(
        !replayed
            .credential
            .as_ref()
            .expect("S12 credential")
            .created,
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
    // O9 retirement-authority cause/effect: C1 the canonical Session
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
    let registry = super::creation::ephemeral_resource_registry().await;
    let ingress = Arc::new(FaultInjectingCredentialMaterialIngress::default());
    let sessions: Arc<dyn awaken_session_repository_contract::SessionStore> =
        authority_repository("Session repository").await;

    let markerless_id = "retirement-markerless";
    let markerless_credential = awaken_credential_contract::CredentialRef {
        id: format!("{}:credential", managed_repository_id(markerless_id)),
        revision: 7,
    };
    register_retirement_repository(registry.as_ref(), markerless_id, false, true).await;
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
            .get(&SessionId::from(markerless_id))
            .await
            .unwrap()
            .resources
            .repository_retirements()
            .is_empty(),
        "A1/E4"
    );
    assert!(
        sessions
            .get(&SessionId::from(absent_id))
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
            .await
            .unwrap()
            .unwrap()
            .state,
        awaken_resource_contract::ResourceState::Active,
        "A1/E2"
    );
}

#[tokio::test]
async fn repository_retirement_restart_retry_fences_same_identity_until_cleanup_cas() {
    // O9 cleanup/reintroduction cause/effect: C1 an exact owned Repository
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
    let registry = super::creation::ephemeral_resource_registry().await;
    let ingress = Arc::new(FaultInjectingCredentialMaterialIngress::default());
    let session_id = "retirement-restart";
    let credential = awaken_credential_contract::CredentialRef {
        id: format!("{}:credential", managed_repository_id(session_id)),
        revision: 13,
    };
    let manifest = repository_retirement_resources(session_id, Some(credential.clone()));
    register_retirement_repository(registry.as_ref(), session_id, true, true).await;

    {
        let sessions: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path)
                .await
                .expect("open Session authority"),
        );
        create(
            sessions.as_ref(),
            session_with_repository_retirement(session_id, true, manifest.clone()),
        )
        .await;
        let app =
            repository_retirement_application(sessions.clone(), registry.clone(), ingress.clone());

        let before_barrier = sessions.get(&SessionId::from(session_id)).await.unwrap();
        assert!(
            matches!(
                app.replace_session_resource_manifest(
                    &SessionId::from(session_id),
                    ReplaceSessionResourceManifest {
                        request_fingerprint: awaken_agent_contract::stable_fingerprint(&manifest),
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
            sessions.get(&SessionId::from(session_id)).await.unwrap(),
            before_barrier,
            "R2/E2 no root commit"
        );

        ingress.fail_next_retirement();
        let first = app.reconcile_resource_activations().await;
        assert_eq!(first.failures.len(), 1, "R1/E1: {first:?}");
        assert!(
            !sessions
                .get(&SessionId::from(session_id))
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
                .await
                .unwrap()
                .unwrap()
                .state,
            awaken_resource_contract::ResourceState::Active,
            "R1/E1 credential-first ordering"
        );
    }

    let sessions: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(&path)
            .await
            .expect("reopen Session authority"),
    );
    let app =
        repository_retirement_application(sessions.clone(), registry.clone(), ingress.clone());
    let restarted = app.reconcile_resource_activations().await;
    assert!(restarted.failures.is_empty(), "R3/E3-E4: {restarted:?}");
    assert!(
        sessions
            .get(&SessionId::from(session_id))
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
            .await
            .unwrap()
            .unwrap()
            .state,
        awaken_resource_contract::ResourceState::Deleted,
        "R3/E4 Repository follows Vault"
    );

    let admitted = app
        .replace_session_resource_manifest(
            &SessionId::from(session_id),
            ReplaceSessionResourceManifest {
                request_fingerprint: awaken_agent_contract::stable_fingerprint(&manifest),
                resources: manifest.clone(),
                expected_resource_revision: None,
                idempotency_key: None,
            },
        )
        .await
        .expect("R4/E5 admission after cleanup CAS");
    // R4 a re-added binding is a new observation after the retired identity
    // was released; compare the same manifest including that durable metadata.
    let at = admitted
        .session
        .resources
        .pending
        .as_ref()
        .unwrap()
        .inputs()[0]
        .created_at_unix_ms
        .expect("R4 new binding time");
    let mut expected = manifest;
    expected.observe_input_times(&Default::default(), at);
    assert_eq!(
        admitted.session.resources.pending.as_ref(),
        Some(&expected),
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
    // CAS or retry algorithm. Additional causes: C5 durable root advanced after
    // the original receipt; C6 replay reload is NotFound. E5 returns the whole
    // current root (including its later time/payload), never the candidate or
    // historical receipt revision; E6 propagates absence without fallback.
    // C2 takes precedence over stale revision/time; neither C5 nor C6 permits
    // another durable mutation or lifecycle outbox append.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // |---|---|---|---|---|---|
    // | M1 | yes | no | no | no | E1 applied |
    // | M2 | no | yes | no | no | E2 replayed |
    // | M3 | no | no | yes | no | E3 conflict |
    // | M4 | no | no | no | yes | E4 mismatch |
    // M5: C2+C3+C5+!C6 => E5; M6: C2+C3+C5+C6 => E6.
    // A lower-time candidate and a nonempty replay-only fact make a
    // candidate fallback, repeated write, or outbox append observable.
    let repo = authority_repository("session repository").await;
    create(repo.as_ref(), persisted("mutation", false, "idle")).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let original = repo
        .get(&SessionId::from("mutation"))
        .await
        .expect("fixture");
    let mut candidate = original.clone();
    candidate.title = Some("applied".into());
    let payload = awaken_session_state_contract::SessionMutationPayload::Replace(candidate.clone());
    let record = awaken_session_state_contract::IdempotencyRecord {
        key: "mutation:one".into(),
        payload_hash: payload.stable_hash(),
    };

    let commit = app
        .mutations
        .commit_session_snapshot_with_record(
            "workspace",
            // M1 is a genuine authoring CAS; prepare its one State fact.
            // M2-M6 below retain direct replay/conflict candidates unchanged.
            awaken_store_conformance::session_store::record_update(&original, candidate.clone(), 0),
            record.clone(),
            Vec::new(),
        )
        .await
        .expect("M1");
    assert!(commit.transitioned(), "M1");
    let applied = commit.into_session();
    assert!(applied.revision > original.revision, "M1");

    let commit = app
        .mutations
        .commit_session_snapshot_with_record(
            "workspace",
            awaken_session_state_contract::SessionAggregate::try_from(candidate.clone())
                .expect("valid test snapshot"),
            record.clone(),
            Vec::new(),
        )
        .await
        .expect("M2");
    assert!(!commit.transitioned(), "M2");
    assert_eq!(commit.into_session().revision, applied.revision, "M2");

    let mut later = applied.clone();
    later.title = Some("later durable root".into());
    later.metadata.insert("later".into(), "authority".into());
    later.updated_at_unix_ms = Some(u64::MAX);
    let later = app
        .mutations
        .commit_session_snapshot_with_outcome(
            "workspace",
            // M5 advances legal current truth before testing receipt priority.
            awaken_store_conformance::session_store::record_update(&applied, later, 0),
            "later",
            Vec::new(),
        )
        .await
        .expect("M5 advance durable root")
        .into_session();
    assert!(later.revision > applied.revision);
    let facts_before = repo.pending_lifecycle().await.expect("M5 outbox before");
    let replay_fact = awaken_session_state_contract::ManagedLifecycleFact {
        id: "must-not-append-on-replay".into(),
        object_id: "mutation".into(),
        workspace_id: Some("workspace".into()),
        event_type: "session.updated".into(),
        timestamp: 1,
        runtime_interval: None,
    };
    let commit = app
        .mutations
        .commit_session_snapshot_with_record(
            "workspace",
            awaken_session_state_contract::SessionAggregate::try_from(candidate.clone())
                .expect("valid test snapshot"),
            record.clone(),
            vec![replay_fact.clone()],
        )
        .await
        .expect("M5 old receipt reloads current root");
    assert!(!commit.transitioned(), "M5");
    assert_eq!(
        commit.into_session(),
        later,
        "M5 exact current aggregate, not receipt revision"
    );
    assert_eq!(
        repo.get(&SessionId::from("mutation")).await.unwrap(),
        later,
        "M5 no extra CAS"
    );
    assert_eq!(
        repo.pending_lifecycle().await.unwrap(),
        facts_before,
        "M5 no extra fact"
    );

    let faulting = Arc::new(FaultingSessionRepository::new(repo.clone()));
    let replay_app = application(
        faulting.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    faulting.get_not_found_once.store(true, Ordering::SeqCst);
    assert_eq!(
        replay_app
            .mutations
            .commit_session_snapshot_with_record(
                "workspace",
                awaken_session_state_contract::SessionAggregate::try_from(candidate.clone())
                    .expect("valid test snapshot"),
                record.clone(),
                vec![replay_fact],
            )
            .await,
        Err(SessionMutationError::NotFound),
        "M6 reload failure cannot publish a candidate",
    );
    assert!(
        !faulting.get_not_found_once.load(Ordering::SeqCst),
        "M6 actually reloaded"
    );
    assert_eq!(
        repo.get(&SessionId::from("mutation")).await.unwrap(),
        later,
        "M6 durable root unchanged"
    );
    assert_eq!(
        repo.pending_lifecycle().await.unwrap(),
        facts_before,
        "M6 outbox unchanged"
    );

    let stale = app
        .mutations
        .commit_session_snapshot_with_outcome(
            "workspace",
            awaken_session_state_contract::SessionAggregate::try_from(candidate.clone())
                .expect("valid test snapshot"),
            "stale",
            Vec::new(),
        )
        .await;
    assert_eq!(stale, Err(SessionMutationError::Conflict), "M3");

    let mismatch = app
        .mutations
        .commit_session_snapshot_with_record(
            "workspace",
            awaken_session_state_contract::SessionAggregate::try_from(candidate)
                .expect("valid test snapshot"),
            awaken_session_state_contract::IdempotencyRecord {
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

/// C1 -> E1 one insert/revision; C2 -> E2 durable replay despite different fresh lowering.
/// C3 -> E3 hash mismatch; C4 -> E4 identity conflict. Only App classifies repository results.
///
/// | Rule | Identity | Key/hash | Payload | Effect |
/// |---|---|---|---|---|
/// | C1 | unused | new | original | E1 |
/// | C2 | existing | exact | different lowering | E2 |
/// | C3 | existing | same key/different hash | changed | E3 |
/// | C4 | existing | another key | any | E4 |
#[tokio::test]
async fn create_session_root_classifies_insert_replay_and_conflicts() {
    use crate::SessionCreationError::{Conflict, IdempotencyMismatch};
    use crate::creation::{CompleteSessionCreateResult, CompleteSessionRoot};
    let repo = authority_repository("session repository").await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    // C0 legacy missing history is rejected independently, never backfilled.
    assert!(
        CompleteSessionRoot::from_fresh(persisted("create-root", false, "preparing")).is_err(),
        "C0 legacy Unknown is not a new creation"
    );
    // C1-C4 use the actual fresh constructor's empty fact. The shared fixture
    // has test observations/Idle state; this uninserted candidate is Preparing
    // and unobserved so the application alone stamps its atomic create time.
    let mut original = first_materialization::fresh_subject("create-root");
    original.execution = SessionExecutionState::Preparing;
    original.created_at_unix_ms = None;
    original.updated_at_unix_ms = None;
    let payload = awaken_session_state_contract::SessionMutationPayload::Replace(original.clone());
    let record = awaken_session_state_contract::IdempotencyRecord {
        key: "create-root:one".into(),
        payload_hash: payload.stable_hash(),
    };

    let inserted = app
        .mutations
        .create_session_root(
            "workspace",
            CompleteSessionRoot::from_fresh(original.clone()).unwrap(),
            record.clone(),
            Vec::new(),
        )
        .await
        .expect("C1");
    let CompleteSessionCreateResult::Applied(inserted) = inserted else {
        panic!("C1 must apply")
    };
    let mut durable = inserted.into_session();
    assert_eq!(
        durable.revision,
        awaken_session_types::SessionRevision(1),
        "C1"
    );
    let before_authoring = durable.clone();
    durable.title = Some("durable-after-create".into());
    let durable = app
        .mutations
        .commit_session_snapshot_with_outcome(
            "workspace",
            // C2 requires a legal durable successor, not a new lowering replay.
            awaken_store_conformance::session_store::record_update(&before_authoring, durable, 0),
            "test-create-replay",
            Vec::new(),
        )
        .await
        .expect("prepare C2 durable aggregate")
        .into_session();
    let mut lowered_again = original.clone();
    lowered_again.title = Some("new-lowering-must-not-win".into());
    let replayed = app
        .mutations
        .create_session_root(
            "workspace",
            CompleteSessionRoot::from_fresh(lowered_again).unwrap(),
            record.clone(),
            Vec::new(),
        )
        .await
        .expect("C2");
    let CompleteSessionCreateResult::Replayed(replayed) = replayed else {
        panic!("C2 must replay")
    };
    assert_eq!(replayed.into_session(), durable, "C2");

    let mut changed = original.clone();
    changed.title = Some("changed".into());
    // C3/C4 differ only in candidate/key/hash and the closed repository result;
    // one shared call retains both original rejection rules and key values.
    for (candidate, key, payload_hash, rule) in [
        (changed, record.key, "changed-hash", "C3"),
        (original, "create-root:another".into(), "another-hash", "C4"),
    ] {
        let result = app
            .mutations
            .create_session_root(
                "workspace",
                CompleteSessionRoot::from_fresh(candidate).unwrap(),
                awaken_session_state_contract::IdempotencyRecord {
                    key,
                    payload_hash: payload_hash.into(),
                },
                Vec::new(),
            )
            .await;
        assert!(
            matches!(
                (rule, result),
                ("C3", Err(IdempotencyMismatch)) | ("C4", Err(Conflict))
            ),
            "{rule}"
        );
    }
}

/// Profiled Repository-release cause/effect graph. C1 asserted owner scope is
/// exact or foreign; C2 binding_id resolves to exactly one active writable
/// Repository or is unknown; C3 the archive root CAS is fresh or an exact
/// response-loss replay; C4 the expectation is exact or conflicts with the
/// frozen intent; C5 the Runtime has a child; C6 a Completed publication outcome
/// is bound to this Session or foreign; C7 publication is accepted, permanently
/// rejected, or unavailable. Effects: E1 foreign/unknown selectors mutate
/// nothing; E2 one atomic archive fence owns the intent; E3 local effects order
/// local effects order child preparation -> publication receipt CAS -> root
/// preparation -> one aggregate disposal; E4 exact replay returns the same
/// durable receipt without another effect; E5 a different expectation conflicts
/// without rewriting the receipt; E6 a foreign Completed outcome fails before
/// any Runtime effect or root mutation; E7 a permanent rejection is durable
/// before ordinary root preparation/disposal; E8 its exact replay has no second
/// effect; E9 an unavailable dependency retains the command without root
/// cleanup; E10 retry publishes and completes the two-stage cleanup.
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
/// | R10 | exact | exact closed source | no | exact | negative unknown-remote result -> E7 |
/// | R11 | exact | released source | exact negative replay | exact | E8 |
#[test]
fn profiled_archive_publishes_one_selected_repository_before_root_cleanup() {
    // Coverage rationale: the async case below remains the sole R1-R9 oracle.
    // The shared composed-test executor changes stack placement only and adds
    // no Session authority, Repository publication path, or cleanup outcome.
    run_composed_async_test(
        profiled_archive_publishes_one_selected_repository_before_root_cleanup_case,
    );
}

async fn profiled_archive_publishes_one_selected_repository_before_root_cleanup_case() {
    // Constraint: the Session root and its SessionCleanupOperation are the only
    // intent/receipt authority; selector resolution is repeated from each CAS
    // winner and the archived replay uses that same frozen intent.
    let repo = authority_repository("profiled Repository publication repository").await;
    let mut session = persisted("profiled-publication", false, "idle");
    session.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: "profiled-publication-source".into(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    session.resources = awaken_session_state_contract::SessionResourceState::from_active(
        repository_resources("source", "repo-1"),
    );
    create(repo.as_ref(), session).await;
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    *runtime.delegated_snapshot.lock().unwrap() =
        awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: Vec::new(),
            coordinated_thread_ids: vec![awaken_agent_contract::agent::thread::Id(
                "profiled-publication-child".into(),
            )],
            watermark: 5,
            runtime_commit_cursor: 8,
        };
    let application = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let fact = awaken_session_state_contract::ManagedLifecycleFact {
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
        repo.get(&SessionId::from("profiled-publication"))
            .await
            .unwrap()
            .execution,
        awaken_session_state_contract::SessionExecutionState::Idle,
        "R1-R2/E1"
    );

    let first = application
        .archive_with_repository_publication(command.clone())
        .await
        .expect("R3 exact release");
    assert!(first.mutation.transitioned(), "R3/E2");
    assert_eq!(
        *runtime.terminal_effect_order.lock().unwrap(),
        vec![
            "cleanup:profiled-publication-child".to_string(),
            "publication".to_string(),
            "cleanup:profiled-publication".to_string(),
            "dispose".to_string(),
        ],
        "R3/E3"
    );
    let replay = application
        .archive_with_repository_publication(command.clone())
        .await
        .expect("R4 response-loss replay");
    assert!(!replay.mutation.transitioned(), "R4/E4");
    assert_eq!(
        replay.publication_receipt, first.publication_receipt,
        "R4/E4"
    );
    assert_eq!(
        runtime.terminal_effect_order.lock().unwrap().len(),
        4,
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
        repo.get(&SessionId::from("profiled-publication"))
            .await
            .unwrap()
            .terminal_cleanup
            .repository_publication_receipt(&awaken_session_types::SessionId::from(
                "profiled-publication"
            ))
            .unwrap(),
        Some(&first.publication_receipt),
        "R5/E5"
    );

    // A custom repository adapter cannot use a self-consistent receipt from a
    // foreign Session to trigger the Completed fast path. The aggregate-bound
    // verifier runs before any Runtime effect or root mutation.
    let mut foreign_completed = repo
        .get(&SessionId::from("profiled-publication"))
        .await
        .unwrap();
    foreign_completed.session_id = "profiled-foreign-outcome".into();
    let faulting_repo = Arc::new(FaultingSessionRepository::new(repo.clone()));
    faulting_repo.return_get_once(foreign_completed);
    let guarded = application_with_runtime(
        runtime.clone(),
        faulting_repo,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let effects_before_foreign = runtime.terminal_effect_order.lock().unwrap().len();
    let foreign_outcome = guarded
        .release_terminal_resources("workspace", "profiled-foreign-outcome")
        .await;
    assert!(
        matches!(
            &foreign_outcome,
            Err(SessionPreparationError::Unavailable(message))
                if message == "Session terminal cleanup is inconsistent with its root: Session cleanup operation does not match its Session identity"
        ),
        "R5 foreign Completed outcome fails closed at aggregate import: {foreign_outcome:?}"
    );
    assert_eq!(
        runtime.terminal_effect_order.lock().unwrap().len(),
        effects_before_foreign,
        "R5 foreign Completed outcome cannot suppress into cleanup/tombstone"
    );

    // Permanent publication rejection is durable before ordinary root cleanup;
    // exact profiled replay returns the same rejection without a second Git call.
    let mut rejected_session = persisted("profiled-rejected", false, "idle");
    rejected_session.environment =
        awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: "profiled-rejected-source".into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
    rejected_session.resources = awaken_session_state_contract::SessionResourceState::from_active(
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
    let rejected_durable = repo
        .get(&SessionId::from("profiled-rejected"))
        .await
        .unwrap();
    assert!(
        rejected_durable.terminal_cleanup.is_completed(),
        "R6 root cleanup"
    );
    assert!(
        rejected_durable
            .terminal_cleanup
            .repository_publication_rejection(&awaken_session_types::SessionId::from(
                "profiled-rejected"
            ))
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

    // R10/R11 source closure is not remote-ref conflict or successful delivery.
    // This composed Runtime/SQLite/control fixture proves negative admission,
    // ordered cleanup, reopen/replay and public failure classification; the
    // Runtime Host/provider tests own the actual physical observation.
    runtime.reject_publication.store(false, Ordering::SeqCst);
    runtime
        .close_publication_source
        .store(true, Ordering::SeqCst);
    let mut closed_source = persisted("profiled-source-closed", false, "idle");
    closed_source
        .environment
        .set_resident("profiled-source-closed-source");
    closed_source.resources = awaken_session_state_contract::SessionResourceState::from_active(
        repository_resources("source", "repo-1"),
    );
    create(repo.as_ref(), closed_source).await;
    let mut closed_command = command.clone();
    closed_command.session_id = "profiled-source-closed".into();
    closed_command.lifecycle_fact.id = "profiled-source-closed:terminated".into();
    closed_command.lifecycle_fact.object_id = "profiled-source-closed".into();
    let before_closed = runtime.terminal_effect_order.lock().unwrap().len();
    assert!(
        matches!(
            application.archive_with_repository_publication(closed_command.clone()).await,
            Err(SessionArchiveWithRepositoryPublicationError::Rejected(ref error))
                if error.code == "repository_publication_source_closed" && error.message.contains("unknown")
        ),
        "R10 truthful non-retryable source prerequisite failure"
    );
    let closed = repo
        .get(&SessionId::from("profiled-source-closed"))
        .await
        .unwrap();
    assert!(
        closed.terminal_cleanup.is_completed(),
        "R10 ordinary cleanup"
    );
    assert!(
        closed
            .terminal_cleanup
            .repository_publication_receipt(&closed.session_id)
            .unwrap()
            .is_none(),
        "R10 no success"
    );
    assert!(
        closed
            .terminal_cleanup
            .repository_publication_rejection(&closed.session_id)
            .unwrap()
            .unwrap()
            .effect_rejection
            .is_source_closed(),
        "R10 distinct durable cause"
    );
    let effects = runtime.terminal_effect_order.lock().unwrap().clone();
    let effects = &effects[before_closed..];
    let publication = effects
        .iter()
        .position(|effect| effect == "publication")
        .unwrap();
    let preparation = effects
        .iter()
        .position(|effect| effect == "cleanup:profiled-source-closed")
        .unwrap();
    let disposal = effects
        .iter()
        .position(|effect| effect == "dispose")
        .unwrap();
    assert!(
        publication < preparation && preparation < disposal,
        "R10 source outcome precedes disposal"
    );
    let after_closed = runtime.terminal_effect_order.lock().unwrap().len();
    assert!(
        matches!(
            application.archive_with_repository_publication(closed_command).await,
            Err(SessionArchiveWithRepositoryPublicationError::Rejected(ref error))
                if error.code == "repository_publication_source_closed"
        ),
        "R11 released-source replay"
    );
    assert_eq!(
        runtime.terminal_effect_order.lock().unwrap().len(),
        after_closed,
        "R11 no second effect"
    );
    runtime
        .close_publication_source
        .store(false, Ordering::SeqCst);

    // Dependency loss records no outcome and therefore cannot expose ordinary
    // root cleanup. The same frozen command is retried and then completes.
    runtime.reject_publication.store(false, Ordering::SeqCst);
    runtime.fail_publication_once.store(true, Ordering::SeqCst);
    let mut unavailable_session = persisted("profiled-unavailable", false, "idle");
    unavailable_session.environment =
        awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: "profiled-unavailable-source".into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
    unavailable_session.resources =
        awaken_session_state_contract::SessionResourceState::from_active(repository_resources(
            "source", "repo-1",
        ));
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
    let pending = repo
        .get(&SessionId::from("profiled-unavailable"))
        .await
        .unwrap();
    assert!(
        !pending.terminal_cleanup.is_completed(),
        "R8 no root cleanup"
    );
    assert!(
        pending
            .terminal_cleanup
            .publication_command(&awaken_session_types::SessionId::from(
                "profiled-unavailable"
            ))
            .unwrap()
            .is_some(),
        "R8 exact command retained"
    );
    application
        .archive_with_repository_publication(unavailable)
        .await
        .expect("R9 retry publishes and completes ordinary cleanup");
}

#[test]
fn terminal_only_release_rejects_live_execution_and_preserves_failure_truth() {
    run_composed_async_test(|| async {
        // T1 every nonterminal execution class rejects with zero root writes;
        // T2 public archive still rejects failed execution; T3 the internal
        // terminal-only edge preserves failure while requiring real cleanup;
        // T4 duplicate release and activity admission cannot reopen that root.
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let fact = |id: &str| ManagedLifecycleFact {
            id: format!("{id}:terminated"),
            object_id: id.into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.terminated".into(),
            timestamp: 1,
            runtime_interval: None,
        };
        for state in ["preparing", "activating", "running", "rescheduling", "idle"] {
            let id = format!("terminal-release-{state}");
            create(repo.as_ref(), persisted(&id, false, state)).await;
            let before = repo.get(&SessionId::from(&id)).await.unwrap();
            assert!(
                matches!(
                    app.release_terminal_session(&id, "2026-09-07T00:00:00Z", fact(&id)).await,
                    Err(SessionPreparationError::Rejected(ref error))
                        if error.message == "terminal Session release requires already-terminal execution"
                ),
                "T1 {state}"
            );
            assert_eq!(
                repo.get(&SessionId::from(id)).await.unwrap(),
                before,
                "T1 no write: {state}"
            );
        }
        for state in ["activation_failed", "terminated"] {
            let id = format!("terminal-release-{state}");
            create(repo.as_ref(), persisted(&id, false, state)).await;
            assert!(
                app.begin_activity(&SessionId::from(&id)).await.is_err(),
                "T3 terminal absorption before archive"
            );
            assert!(
                matches!(
                    app.terminate_session(&id, "2026-09-07T00:00:00Z", fact(&id)).await,
                    Err(SessionPreparationError::Rejected(ref error))
                        if error.message == "only an idle Session may be archived"
                ),
                "T2 unchanged public archive: {state}"
            );
            app.release_terminal_session(&id, "2026-09-07T00:00:00Z", fact(&id))
                .await
                .expect("T3 terminal-only release");
            let completed = repo.get(&SessionId::from(&id)).await.unwrap();
            assert_eq!(completed.execution.as_str(), state, "T3 retained failure");
            assert!(
                completed
                    .verified_terminal_cleanup()
                    .unwrap()
                    .is_completed(),
                "T3 receipt"
            );
            assert!(matches!(
                completed.disposition,
                awaken_session_state_contract::SessionDisposition::Archived { .. }
            ));
            assert!(
                app.begin_activity(&SessionId::from(&id)).await.is_err(),
                "T4 no reopening"
            );
            app.release_terminal_session(&id, "another-timestamp", fact(&id))
                .await
                .expect("T4 exact terminal replay");
            assert_eq!(
                repo.get(&SessionId::from(&id)).await.unwrap(),
                completed,
                "T4 unchanged root"
            );
            assert_eq!(
                repo.pending_lifecycle()
                    .await
                    .unwrap()
                    .iter()
                    .filter(|event| event.id == fact(&id).id)
                    .count(),
                1,
                "T4 one fact"
            );
        }
        let id = "terminal-release-event-pending";
        let mut session = session_with_unsettled_event(id);
        session.execution = SessionExecutionState::ActivationFailed;
        create(repo.as_ref(), session).await;
        let before = repo.get(&SessionId::from(id)).await.unwrap();
        assert!(
            matches!(
                app.release_terminal_session(id, "2026-09-07T00:00:00Z", fact(id)).await,
                Err(SessionPreparationError::Rejected(ref error))
                    if error.code == "session_event_batch_pending"
            ),
            "T7 accepted Event effects retain their fence"
        );
        assert_eq!(
            repo.get(&SessionId::from(id)).await.unwrap(),
            before,
            "T7 zero lost Event writes"
        );
    });
}

#[test]
fn terminal_only_release_waits_for_cleanup_and_recovers_committed_failure() {
    run_composed_async_test(|| async {
        // T5 archive commits before an unavailable physical cleanup; the caller
        // must not observe successful release. T6 the same command recovers
        // its retained intent and completes once, without normal activation.
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let id = "terminal-release-pending";
        create(repo.as_ref(), persisted(id, false, "activation_failed")).await;
        let runtime = Arc::new(RecordingCleanupRuntime::default());
        runtime.fail_quiesce_once.store(true, Ordering::SeqCst);
        let app = application_with_runtime(
            runtime.clone(),
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let fact = ManagedLifecycleFact {
            id: format!("{id}:terminated"),
            object_id: id.into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.terminated".into(),
            timestamp: 1,
            runtime_interval: None,
        };
        assert!(
            app.release_terminal_session(id, "2026-09-07T00:00:00Z", fact.clone())
                .await
                .is_err(),
            "T5 no premature release"
        );
        let pending = repo.get(&SessionId::from(id)).await.unwrap();
        assert!(matches!(
            pending.disposition,
            awaken_session_state_contract::SessionDisposition::Archived { .. }
        ));
        assert_eq!(pending.execution, SessionExecutionState::ActivationFailed);
        assert!(!pending.verified_terminal_cleanup().unwrap().is_completed());
        app.release_terminal_session(id, "2026-09-07T00:00:00Z", fact.clone())
            .await
            .expect("T6 retry existing cleanup");
        assert!(
            repo.get(&SessionId::from(id))
                .await
                .unwrap()
                .verified_terminal_cleanup()
                .unwrap()
                .is_completed()
        );
        let effects = runtime.effective_ids.lock().unwrap().len();
        assert!(effects > 0, "T6 actual fixture cleanup effects");
        app.release_terminal_session(id, "2026-09-07T00:00:00Z", fact)
            .await
            .expect("T6 absorbing replay");
        assert_eq!(runtime.effective_ids.lock().unwrap().len(), effects);
        assert_eq!(
            runtime.ordinary_resource_apply_calls.load(Ordering::SeqCst),
            0
        );
    });
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
#[test]
fn terminal_transition_decision_table_is_durable_and_idempotent() {
    // Coverage rationale: the async case below remains the sole L1-L9 oracle.
    // The shared composed-test executor changes stack placement only and adds
    // no Session authority, scenario, or alternate transition path.
    run_composed_async_test(terminal_transition_decision_table_is_durable_and_idempotent_case);
}

async fn terminal_transition_decision_table_is_durable_and_idempotent_case() {
    // Constraint/Invariant: the Session repository CAS and terminal lifecycle are
    // the only transition authority; replays cannot create another terminal fact.
    // Decision rule: execute L1-L9 and require every accepted, blocked, raced,
    // forced, and delete effect in the table above.
    let durable_repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        authority_repository("session repository").await;
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
        let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
            &mut session.baseline
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
    let fact = |id: &str, event_type: &str| awaken_session_state_contract::ManagedLifecycleFact {
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
    assert_ne!(first.transitioned(), second.transitioned(), "L1");
    let archived = repo
        .get(&SessionId::from("archive-race"))
        .await
        .expect("L1 durable");
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
    assert!(!replay.transitioned(), "L2");
    assert_eq!(replay.session.revision, revision, "L2");

    let root_running = app
        .begin_activity(&SessionId::from("archive-running"))
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
                if error.kind == awaken_session_types::RunErrorKind::BadRequest
                    && error.message == "only an idle Session may be archived"
        ),
        "L3/E2: {root_rejected:?}"
    );
    assert_eq!(
        repo.get(&SessionId::from("archive-running"))
            .await
            .expect("L3 durable"),
        root_running,
        "L3/E2 rejection preserves the exact activity receipt"
    );

    // The Session root intentionally stores no child registry. This exact
    // operation-scoped activity epoch is the authoritative aggregate evidence
    // that an admitted child/requires-action boundary is still active.
    let (child_running, child_epoch) = app
        .begin_activity_for_operation(
            &SessionId::from("archive-child-active"),
            "child-requires-action",
        )
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
                if error.kind == awaken_session_types::RunErrorKind::BadRequest
        ),
        "L4/E2: {child_rejected:?}"
    );
    let child_after = repo
        .get(&SessionId::from("archive-child-active"))
        .await
        .expect("L4 durable");
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
                if error.kind == awaken_session_types::RunErrorKind::BadRequest
        ),
        "L5/E3: {race_archive:?}"
    );
    let race_running = repo
        .get(&SessionId::from("archive-activity-race"))
        .await
        .expect("L5 Running CAS winner");
    assert_eq!(race_running.execution.as_str(), "running", "L5/E3");
    assert_eq!(race_running.active_activity_epochs.len(), 1, "L5/E3");
    assert!(
        !race_running.needs_resource_reconciliation(),
        "L5/E3 archive retry must not apply terminal cleanup to the Running winner"
    );

    let forced_running = app
        .begin_activity(&SessionId::from("archive-force"))
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
    assert!(forced.transitioned(), "L6/E5");
    assert!(forced.session.is_terminal(), "L6/E5");
    assert!(forced.session.active_activity_epochs.is_empty(), "L6/E5");

    let deleted =
        SessionApplication::delete_session(&app, SessionDeleteCommand::new("delete-live"))
            .await
            .expect("L7");
    let transition = deleted.clone();
    app.release_terminal_resources(&transition.owner_scope, "delete-live")
        .await
        .expect("L7 reconciliation");
    assert!(transition.transitioned(), "L7");
    assert_eq!(transition.session.execution.as_str(), "terminated", "L7");
    assert!(transition.session.is_hidden(), "L7");
    assert!(transition.session.needs_resource_reconciliation(), "L7");
    let tombstone = repo
        .get(&SessionId::from("delete-live"))
        .await
        .expect_err("L7 tombstone");
    assert!(matches!(
        tombstone,
        awaken_session_state_contract::SessionRepositoryError::NotFound
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
        .lifecycle
        .commit_delete_intent(SessionDeleteCommand::new("archive-race"))
        .await
        .expect("L8 archived Session is deletable");
    assert!(archived_delete.transitioned(), "L8");
    assert!(archived_delete.session.is_hidden(), "L8");
    let failed_delete = app
        .lifecycle
        .commit_delete_intent(SessionDeleteCommand::new("delete-failed"))
        .await
        .expect("L9 failed Session is deletable");
    assert!(failed_delete.transitioned(), "L9");
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
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> =
        authority_repository("Session repository").await;
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
    let fact = |session_id: &str| awaken_session_state_contract::ManagedLifecycleFact {
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
        .lifecycle
        .commit_delete_intent(SessionDeleteCommand::new("batch-before-delete"))
        .await;
    for (rule, result) in [("B1", archive), ("B2", force), ("B3", delete)] {
        assert!(
            matches!(
                result,
                Err(SessionPreparationError::Rejected(ref error))
                    if error.kind == awaken_session_types::RunErrorKind::Unavailable
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
        let durable = repository
            .get(&SessionId::from(session_id))
            .await
            .expect("B1-B3 durable");
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
        .transitioned()
    );
    assert!(
        app.force_terminate_session(
            "batch-before-force",
            "2026-08-24T00:00:00Z",
            fact("batch-before-force"),
        )
        .await
        .expect("B4 force retry")
        .transitioned()
    );
    assert!(
        app.lifecycle
            .commit_delete_intent(SessionDeleteCommand::new("batch-before-delete"))
            .await
            .expect("B4 delete retry")
            .transitioned()
    );
}

async fn deletion_fixture(
    initial: &str,
    session_id: &str,
) -> (
    Arc<dyn awaken_session_repository_contract::SessionStore>,
    Arc<FaultingSessionRepository>,
    Arc<RecordingCleanupRuntime>,
    Arc<SessionApplication>,
) {
    let durable: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let repo = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let app = Arc::new(application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let mut root = persisted(session_id, false, "idle");
    if initial != "active" {
        root.archive("original-archive", 0).unwrap();
    }
    if initial == "archived-requested" {
        root.freeze_terminal_cleanup_targets(Vec::<String>::new(), 17, 19)
            .unwrap();
    }
    create(repo.as_ref(), root).await;
    if initial == "archived-completed" {
        let completed = app
            .release_terminal_resources("workspace", session_id)
            .await
            .unwrap()
            .unwrap();
        assert!(completed.terminal_cleanup.is_completed());
    }
    (durable, repo, runtime, app)
}

#[tokio::test]
async fn delete_fact_is_committed_once_at_the_fence_and_not_reemitted_by_tombstone() {
    // Causes: C1 Active or Archived with Fenced/Requested/Completed cleanup;
    // C2 the first fence returns normally or commit_then_conflict_once injects
    // Conflict after the real SQL commit, forcing authoritative reread (not
    // arbitrary network/storage errors); C3 DELETE repeats through a
    // cold application before/after outbox ACK and cleanup progress; C4 an
    // external preparation fails; C5 completed cleanup loses its tombstone CAS.
    // Effects: D1 exactly one hidden terminal fence/fact; D2 existing archive
    // cleanup is retained in full; D3 all retries preserve the complete durable
    // root/outbox, including revision and cleanup identity; D4 errors retain the
    // same operation for recovery; D5 tombstone/late retry emits no second fact.
    // Decision table: each C1 x C2 runs first fence + cold replay + ACK; every
    // incomplete cleanup takes C4 and Requested replay, then all cases take C5
    // and Completed replay before D5. SQL, application and the canonical driver
    // execute unchanged. The SAME model relations consume these observations;
    // the projection retains full root/cleanup/outbox bytes for replay equality.
    // D1/D3 also bind returned owner and entire receipt root to durable truth;
    // a correct write with an incorrect response is still a failure. D5 reads
    // retained deletion evidence through the existing owner-scoped port and
    // checks its exact revision; absence alone cannot stand in for a tombstone.
    use super::observation_refinement::deletion_observation;
    let mut observations = Vec::new();
    for initial in [
        "active",
        "archived-fenced",
        "archived-requested",
        "archived-completed",
    ] {
        for lost_ack in [false, true] {
            let session_id = format!("delete-{initial}-{lost_ack}");
            let (durable, repo, runtime, app) = deletion_fixture(initial, &session_id).await;
            let before_root = durable.get(&SessionId::from(&session_id)).await.unwrap();
            let before = deletion_observation(durable.as_ref(), &session_id, "workspace").await;
            if lost_ack {
                repo.commit_then_conflict_once("delete-intent");
            }
            let transition = app
                .lifecycle
                .commit_delete_intent(SessionDeleteCommand::new(&session_id))
                .await
                .unwrap();
            assert_eq!(
                transition.transitioned(),
                !lost_ack,
                "D1 response loss recovers the winner"
            );
            let after_root = durable.get(&SessionId::from(&session_id)).await.unwrap();
            assert_eq!(
                transition.owner_scope,
                durable.owner(&SessionId::from(&session_id)).await.unwrap(),
                "D1 receipt owner"
            );
            assert_eq!(
                transition.session, after_root,
                "D1 receipt is the complete committed root"
            );
            assert!(after_root.is_hidden() && after_root.execution.is_terminal());
            assert_eq!(
                after_root.revision.0,
                before_root.revision.0 + 1,
                "D1 one fence CAS"
            );
            if initial != "active" {
                assert_eq!(
                    after_root.terminal_cleanup, before_root.terminal_cleanup,
                    "D2 archived cleanup preserved"
                );
            }
            let facts = durable.pending_lifecycle().await.unwrap();
            assert_eq!(facts.len(), 1, "D1 exactly one fact");
            assert_eq!(facts[0].id, format!("session:{session_id}:deleted"));
            assert_eq!(facts[0].workspace_id.as_deref(), Some("workspace"));
            observations.push(serde_json::json!({"rule": format!("{initial}-{lost_ack}-fence"), "kind": "fence", "lostAck": lost_ack, "result": if transition.transitioned() { "applied" } else { "replayed" }, "receiptOwner": transition.owner_scope, "receiptRoot": serde_json::to_string(&transition.session).unwrap(), "before": before, "after": deletion_observation(durable.as_ref(), &session_id, "workspace").await}));
            let original_effect = observations.last().unwrap()["after"]["effect"].clone();
            let cold = application_with_runtime(
                runtime.clone(),
                repo.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            );
            for stage in ["first", "requested", "completed"] {
                if stage == "requested" {
                    durable.complete_lifecycle(&facts[0].id).await.unwrap();
                    if after_root.terminal_cleanup.is_completed() {
                        continue;
                    }
                    runtime
                        .fail_before_effect_once
                        .store(true, Ordering::SeqCst);
                    assert!(
                        cold.release_terminal_resources("workspace", &session_id)
                            .await
                            .is_err(),
                        "D4 pre-effect failure"
                    );
                    assert!(
                        durable
                            .get(&SessionId::from(&session_id))
                            .await
                            .unwrap()
                            .terminal_cleanup
                            .is_requested()
                    );
                }
                if stage == "completed" {
                    // Match only the final tombstone key, not other mutations
                    // that happen to contain this Session's delete-* id.
                    repo.fail_once("session:delete:");
                    assert!(
                        cold.release_terminal_resources("workspace", &session_id)
                            .await
                            .is_err(),
                        "D4 tombstone outage"
                    );
                    assert!(
                        durable
                            .get(&SessionId::from(&session_id))
                            .await
                            .unwrap()
                            .terminal_cleanup
                            .is_completed()
                    );
                }
                let before = deletion_observation(durable.as_ref(), &session_id, "workspace").await;
                assert_eq!(
                    before["effect"], original_effect,
                    "D4 stable cleanup operation"
                );
                let replay = cold
                    .lifecycle
                    .commit_delete_intent(SessionDeleteCommand::new(&session_id))
                    .await
                    .unwrap();
                assert!(!replay.transitioned(), "D3 retry is an absorbing replay");
                assert_eq!(
                    replay.owner_scope,
                    durable.owner(&SessionId::from(&session_id)).await.unwrap(),
                    "D3 receipt owner"
                );
                assert_eq!(
                    replay.session,
                    durable.get(&SessionId::from(&session_id)).await.unwrap(),
                    "D3 receipt is the complete current root"
                );
                let after = deletion_observation(durable.as_ref(), &session_id, "workspace").await;
                assert_eq!(after, before, "D3 full durable replay frame");
                observations.push(serde_json::json!({"rule": format!("{initial}-{lost_ack}-{stage}"), "kind": "retry", "lostAck": lost_ack, "result": if replay.transitioned() { "applied" } else { "replayed" }, "receiptOwner": replay.owner_scope, "receiptRoot": serde_json::to_string(&replay.session).unwrap(), "before": before, "after": after}));
            }
            let completed_revision = durable
                .get(&SessionId::from(&session_id))
                .await
                .unwrap()
                .revision
                .0;
            let effect_calls = runtime.intents.lock().unwrap().len();
            assert!(
                cold.release_terminal_resources("workspace", &session_id)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                durable.pending_lifecycle().await.unwrap().is_empty(),
                "D5 acknowledged fact not re-emitted"
            );
            let before = deletion_observation(durable.as_ref(), &session_id, "workspace").await;
            assert_eq!(
                before["revision"],
                completed_revision + 1,
                "D5 exact durable tombstone revision"
            );
            assert!(
                durable
                    .deleted_event_admissions("foreign", &SessionId::from(&session_id))
                    .await
                    .unwrap()
                    .is_none(),
                "D5 tombstone remains owner scoped"
            );
            let absent = cold
                .lifecycle
                .commit_delete_intent(SessionDeleteCommand::new(&session_id))
                .await;
            assert!(matches!(absent, Err(SessionPreparationError::NotFound)));
            assert!(
                cold.release_terminal_resources("workspace", &session_id)
                    .await
                    .unwrap()
                    .is_none()
            );
            let after = deletion_observation(durable.as_ref(), &session_id, "workspace").await;
            assert_eq!(after, before, "D5 tombstone remains absorbing");
            assert_eq!(
                runtime.intents.lock().unwrap().len(),
                effect_calls,
                "D5 no repeated provider effect"
            );
            observations.push(serde_json::json!({"rule": format!("{initial}-{lost_ack}-tombstone"), "kind": "retry", "lostAck": lost_ack, "result": "not_found", "receiptOwner": "none", "receiptRoot": "null", "before": before, "after": after}));
        }
    }
    // Real CAS table (not the post-commit Conflict injection above): C6 two
    // detached Delete candidates, with distinct deterministic command times;
    // C7 a legitimate intervening root write changes the winner's revision;
    // C8 delivery ACK / completed cleanup and tombstone win before the delayed
    // CAS. Every initial cleanup class crosses both timestamp orderings.
    // R1 !C7,!C8: SQLite Applied then Replayed, only winner transitioned=true.
    // R2 C7: SQLite Conflict, then the application rereads the full winner.
    // R3 ACK: same receipt replay cannot overwrite/re-enqueue the acknowledged
    // fact, even with the delayed command's distinct timestamp.
    // R4 tombstone: receipt replay's authoritative reload returns typed NotFound;
    // it cannot recreate a root, cleanup operation, fact, or provider effect.
    // Observation reads are deliberately made at quiescent cuts: the delayed
    // candidate is paused before the CAS, or both calls have returned. These
    // separate reads are not claimed to be one transactional snapshot.
    let mut race_rules = 0;
    for initial in [
        "active",
        "archived-fenced",
        "archived-requested",
        "archived-completed",
    ] {
        for schedule in [
            "same_snapshot",
            "different_revision",
            "after_ack",
            "after_tombstone",
        ] {
            for winner_time in [111, 222] {
                let loser_time = if winner_time == 111 { 222 } else { 111 };
                let session_id = "delete-cas";
                let (durable, repo, runtime, app) = deletion_fixture(initial, session_id).await;
                let origin = durable.get(&SessionId::from(session_id)).await.unwrap();
                let pause = repo.pause_next_mutation("session:delete-intent:");
                let delayed = app.clone();
                let loser = tokio::spawn(async move {
                    delayed
                        .lifecycle
                        .commit_delete_intent(SessionDeleteCommand::at_for_test(
                            session_id, loser_time,
                        ))
                        .await
                });
                tokio::time::timeout(std::time::Duration::from_secs(5), pause.entered.notified())
                    .await
                    .unwrap();
                let candidate = pause.candidate.lock().unwrap().clone().unwrap();
                assert_eq!(
                    candidate.expected_revision, origin.revision,
                    "R1/R2 both requests start from the actual origin"
                );
                let expected_fact = |timestamp| ManagedLifecycleFact {
                    id: "session:delete-cas:deleted".into(),
                    object_id: session_id.into(),
                    workspace_id: Some("workspace".into()),
                    event_type: "session.deleted".into(),
                    timestamp,
                    runtime_interval: None,
                };
                assert_eq!(
                    candidate.lifecycle_facts,
                    vec![expected_fact(loser_time)],
                    "R1 detached losing fact is real and distinguishable"
                );
                if schedule == "different_revision" {
                    let mut predecessor = origin.clone();
                    predecessor.title = Some("independent committed predecessor".into());
                    let _fixture = app
                        .mutations
                        .commit_session_snapshot_with_outcome(
                            "workspace",
                            // R1/R2 race against a genuinely committed author.
                            awaken_store_conformance::session_store::record_update(
                                &origin,
                                predecessor,
                                0,
                            ),
                            "delete-race-predecessor",
                            Vec::new(),
                        )
                        .await
                        .unwrap();
                }
                let before_root = durable.get(&SessionId::from(session_id)).await.unwrap();
                let before = deletion_observation(durable.as_ref(), session_id, "workspace").await;
                let winner = app
                    .lifecycle
                    .commit_delete_intent(SessionDeleteCommand::at_for_test(
                        session_id,
                        winner_time,
                    ))
                    .await
                    .unwrap();
                assert!(winner.transitioned(), "R1/R2 winner really applied");
                assert_eq!(winner.owner_scope, "workspace");
                assert_eq!(
                    winner.session,
                    durable.get(&SessionId::from(session_id)).await.unwrap(),
                    "R1 complete winner receipt"
                );
                assert_eq!(winner.session.revision.0, before_root.revision.0 + 1);
                if initial != "active" {
                    assert_eq!(
                        winner.session.terminal_cleanup, before_root.terminal_cleanup,
                        "R2 archived cleanup identity and entire progress are preserved"
                    );
                }
                let facts = durable.pending_lifecycle().await.unwrap();
                assert_eq!(
                    facts,
                    vec![expected_fact(winner_time)],
                    "R1 complete winner fact retains its own command time"
                );
                let after = deletion_observation(durable.as_ref(), session_id, "workspace").await;
                let mut fence_observation = serde_json::json!({"rule": format!("race-{initial}-{schedule}-{winner_time}-fence"), "kind": "fence", "lostAck": false, "result": "applied", "receiptOwner": winner.owner_scope, "receiptRoot": serde_json::to_string(&winner.session).unwrap(), "before": before, "after": after});
                if matches!(schedule, "after_ack" | "after_tombstone") {
                    durable.complete_lifecycle(&facts[0].id).await.unwrap();
                }
                if schedule == "after_tombstone" {
                    assert!(
                        app.release_terminal_resources("workspace", session_id)
                            .await
                            .unwrap()
                            .is_none()
                    );
                }
                let before = deletion_observation(durable.as_ref(), session_id, "workspace").await;
                let effect_calls = runtime.intents.lock().unwrap().len();
                pause.release.notify_one();
                let loser = tokio::time::timeout(std::time::Duration::from_secs(5), loser)
                    .await
                    .unwrap()
                    .unwrap();
                let actual_result = pause.result.lock().unwrap().clone().unwrap();
                if schedule == "different_revision" {
                    assert!(
                        matches!(
                            actual_result,
                            awaken_session_state_contract::SessionMutationResult::Conflict { .. }
                        ),
                        "R2 real stale-root CAS conflict"
                    );
                } else {
                    assert!(
                        matches!(
                            actual_result,
                            awaken_session_state_contract::SessionMutationResult::Replayed { .. }
                        ),
                        "R1/R3/R4 actual SQLite receipt replay"
                    );
                }
                let sqlite_result = match actual_result {
                    awaken_session_state_contract::SessionMutationResult::Conflict { .. } => {
                        "conflict"
                    }
                    awaken_session_state_contract::SessionMutationResult::Replayed { .. } => {
                        "replayed"
                    }
                    _ => unreachable!("race must actually reach a losing CAS branch"),
                };
                let race = serde_json::json!({
                    "candidateRevision": candidate.expected_revision.0,
                    "winnerRevision": winner.session.revision.0,
                    "sqliteResult": sqlite_result,
                    "winnerTimestamp": facts[0].timestamp,
                    "delayedTimestamp": candidate.lifecycle_facts[0].timestamp,
                    "acknowledged": before["facts"] == 0,
                    "tombstoned": before["disposition"] == "tombstoned",
                });
                fence_observation["race"] = race.clone();
                observations.push(fence_observation);
                let (result, receipt_owner, receipt_root) = if schedule == "after_tombstone" {
                    assert!(
                        matches!(loser, Err(SessionPreparationError::NotFound)),
                        "R4 typed reload absence"
                    );
                    ("not_found", "none".to_string(), "null".to_string())
                } else {
                    let loser = loser.unwrap();
                    assert!(
                        !loser.transitioned(),
                        "R1 replay cannot claim a second applied Delete"
                    );
                    assert_eq!(loser.owner_scope, "workspace");
                    assert_eq!(
                        loser.session,
                        durable.get(&SessionId::from(session_id)).await.unwrap(),
                        "R2 return the complete current winner after Conflict/replay"
                    );
                    (
                        "replayed",
                        loser.owner_scope,
                        serde_json::to_string(&loser.session).unwrap(),
                    )
                };
                let after = deletion_observation(durable.as_ref(), session_id, "workspace").await;
                assert_eq!(
                    after, before,
                    "R1-R4 complete durable cleanup/root/outbox/tombstone stutter"
                );
                assert_eq!(
                    runtime.intents.lock().unwrap().len(),
                    effect_calls,
                    "R4 no additional physical cleanup"
                );
                observations.push(serde_json::json!({"rule": format!("race-{initial}-{schedule}-{winner_time}-retry"), "race": race, "kind": "retry", "lostAck": false, "result": result, "receiptOwner": receipt_owner, "receiptRoot": receipt_root, "before": before, "after": after}));
                race_rules += 1;
            }
        }
    }
    assert_eq!(
        race_rules, 32,
        "R1-R4 x four initial cleanup frames x both command timestamp orderings"
    );
    awaken_reliability_testkit::write_observation_trace(
        "delete-retry-lifecycle",
        "deletion",
        serde_json::Value::Array(observations),
    );
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
    use awaken_session_application_contract::{SessionWorkAcquisition, SessionWorkOwnership};

    use awaken_session_application_contract::SessionWorkerLeaseAuthority;

    let repo = authority_repository("session repository").await;
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
            &SessionId::from("missing"),
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
            &SessionId::from("cloud-work"),
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
        .acquire_session_work(
            &SessionId::from("self-work"),
            "owner",
            1,
            SessionWorkAcquisition::ClaimedRun,
        )
        .await
        .expect("W3")
    else {
        panic!("W3 must revive and lease the stopped Session Work");
    };
    assert_eq!(lease.owner(), "owner", "W3");
    assert!(
        environments.awakened.lock().unwrap().contains("self-work"),
        "W3"
    );
    assert_eq!(
        app.acquire_session_work(
            &SessionId::from("terminal-work"),
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
            &SessionId::from("renewal-work"),
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
    let repo = authority_repository("session repository").await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let mut recovered = persisted("activity-recovered-settle", false, "running");
    recovered.activity_epoch = 7;
    assert!(recovered.begin_runtime_interval(1, 0), "A10 fixture");
    assert!(recovered.active_activity_epochs.is_empty(), "A10/C1");
    create(repo.as_ref(), recovered).await;
    let idle = app
        .lifecycle()
        .settle_activity_observed(
            &awaken_session_types::SessionId::from("activity-recovered-settle"),
            7,
            None,
        )
        .await
        .expect("A10 settle");
    assert_eq!(idle.execution, SessionExecutionState::Idle, "A10/E1");
    assert!(idle.running_interval.is_none(), "A10/E1");

    let mut recovered = persisted("activity-recovered-successor", false, "running");
    recovered.activity_epoch = 9;
    assert!(recovered.begin_runtime_interval(1, 0), "A11 fixture");
    create(repo.as_ref(), recovered).await;
    let successor = app
        .begin_activity(&SessionId::from("activity-recovered-successor"))
        .await
        .expect("A11 successor");
    assert_eq!(successor.activity_epoch, 10, "A11/E2");
    assert_eq!(
        successor.active_activity_epochs,
        std::collections::BTreeSet::from([10]),
        "A11/E2"
    );
    assert_eq!(
        app.lifecycle()
            .settle_activity_observed(
                &awaken_session_types::SessionId::from("activity-recovered-successor"),
                9,
                None
            )
            .await
            .expect("A11 predecessor fenced"),
        successor,
        "A11/E3"
    );
    let idle = app
        .lifecycle()
        .settle_activity_observed(
            &awaken_session_types::SessionId::from("activity-recovered-successor"),
            10,
            None,
        )
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
    let repo = authority_repository("session repository").await;
    create(
        repo.as_ref(),
        persisted("activity-operation", false, "idle"),
    )
    .await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let joined_session = SessionId::from("activity-operation");
    let (left, right) = tokio::join!(
        app.begin_activity_for_operation(&joined_session, "op-a"),
        app.begin_activity_for_operation(&joined_session, "op-a")
    );
    let (left_session, left_epoch) = left.expect("O1/O2 left");
    let (right_session, right_epoch) = right.expect("O1/O2 right");
    assert_eq!(left_epoch, right_epoch, "O2/E2 exact concurrent replay");
    let active = repo
        .get(&SessionId::from("activity-operation"))
        .await
        .expect("O1 state");
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
        .lifecycle()
        .settle_activity_observed(
            &awaken_session_types::SessionId::from("activity-operation"),
            left_epoch,
            None,
        )
        .await
        .expect("O3 settle");
    let settled_revision = settled.revision;
    let (replayed, replayed_epoch) = app
        .begin_activity_for_operation(&SessionId::from("activity-operation"), "op-a")
        .await
        .expect("O3 exact replay");
    assert_eq!(replayed_epoch, left_epoch, "O3/E3");
    assert_eq!(replayed.revision, settled_revision, "O3/E3 no mutation");
    assert!(replayed.active_activity_epochs.is_empty(), "O3/E3");

    let (next, next_epoch) = app
        .begin_activity_for_operation(&SessionId::from("activity-operation"), "op-b")
        .await
        .expect("O4 distinct operation");
    assert!(next_epoch > left_epoch, "O4/E4");
    assert_eq!(next.revision.0, next_epoch, "O4/E4");
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
    let repo = authority_repository("child settlement recovery repository").await;
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
    runtime.set_admission_observations([Ok(None)]);
    let app = configured_admission_application(repo.clone(), runtime.clone());

    for (rule, source_agent_id, accepted) in cases {
        let session_id = format!("settlement-{rule}");
        let child = ThreadId(format!("settlement-child-{rule}"));
        let run = RunId(format!("settlement-run-{rule}"));
        let (_, epoch) = app
            .begin_activity_for_operation_with_policy(
                &SessionId::from(&session_id),
                &format!("settlement-operation-{rule}"),
                false,
                None,
                None,
                Some((&child, &run)),
            )
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
            awaken_session_execution_contract::SessionCoordinationReads::coordinated_threads(
                runtime.as_ref(),
                &SessionId::from(&session_id),
                None,
            )
            .await
            .expect("relationship projection")
            .is_empty(),
            "{rule}/C3"
        );

        let result =
            awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
                &app,
                awaken_session_execution_contract::SessionAgentBoundaryCommand {
                    session_id: SessionId::from(&session_id),
                    source_thread_id: child,
                    source_run_id: run,
                    source_agent_id: source_agent_id.into(),
                    session_activity_epoch: epoch,
                    cancellation_requested: false,
                },
            )
            .await;
        let session = repo
            .get(&SessionId::from(session_id))
            .await
            .expect("settled Session");
        if accepted {
            result.expect("R1/E1 frozen dispatch Agent is admitted");
            assert_eq!(runtime.continuations.lock().unwrap().len(), 1, "R1/E1");
            assert!(session.active_activity_epochs.contains(&epoch), "R1/E1");
        } else {
            let error = result.expect_err("R2-R4/E2 unknown identity fails closed");
            assert_eq!(
                error.kind,
                awaken_session_types::RunErrorKind::BadRequest,
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
    // latest committed Run Running, Awaiting, Completed, Cancelled, or Failed;
    // C3 a follow-up is
    // submitted after that durable boundary; C4 the target relationship is an
    // ordinary Agent, Advisor, or absent; C5 the ordinary Thread disposition is
    // Active/Archived. Effects: E1 Running/Awaiting/Completed/Cancelled ordinary
    // Agents admit a fresh queued Run on the same Thread; Awaiting is not
    // resumed and its ticket is not consumed. E2 Failed, Advisor, unknown, and Archived
    // targets reject before opening a Session activity or calling Runtime
    // admission. Constraints: RunState and ThreadDisposition remain the only
    // failure/archive authorities, and the committed relationship projection is
    // the only target-kind authority; the application stores no parallel flag.
    //
    // | Rule | Latest lifecycle | Follow-up | Effects |
    // |---|---|---|---|
    // | A1 | Running | submitted | E1 queued fresh Run |
    // | A2 | Awaiting | submitted | E1 queued fresh Run/no resume |
    // | A3 | Completed | submitted | E1 accepted |
    // | A4 | Cancelled | submitted | E1 accepted |
    // | A5 | Failed(Error) | submitted | E2 BadRequest/no admission |
    // | A6 | any | unknown relationship | E2 BadRequest/no admission |
    // | A7 | any | Advisor relationship | E2 BadRequest/no admission |
    // | A8 | any | Archived ordinary Thread | E2 BadRequest/no admission |
    for (rule, state, accepted) in [
        ("A1", RunState::Running, true),
        ("A2", RunState::Awaiting, true),
        ("A3", RunState::Ended(EndCause::NaturalEnd), true),
        ("A4", RunState::Ended(EndCause::Cancelled), true),
        (
            "A5",
            RunState::Ended(EndCause::Error(Failure::Inference {
                code: "unauthorized".into(),
                message: "terminal child failure".into(),
            })),
            false,
        ),
    ] {
        let session_id = format!("follow-up-{rule}");
        let repo = authority_repository("follow-up repository").await;
        create(repo.as_ref(), coordinated_session(&session_id)).await;
        let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
            AgentAdmissionOutcome::Accepted,
            AgentAdmissionOutcome::Accepted,
        ]));
        runtime.set_admission_observations([Ok(None), Ok(None)]);
        let app = configured_admission_application(repo.clone(), runtime.clone());
        let receipt =
            awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
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
            .get(&SessionId::from(&session_id))
            .await
            .expect("activity before follow-up")
            .active_activity_epochs;
        let follow_up = awaken_session_execution_contract::SessionAgentMessageCommand {
            session_id: SessionId::from(&session_id),
            source_thread_id: ThreadId(session_id.clone()),
            source_run_id: RunId(format!("{session_id}-parent-follow-up")),
            source_call_id: format!("{rule}-follow-up-call"),
            operation_id: format!("{rule}-follow-up-operation"),
            target: awaken_session_execution_contract::SessionAgentTarget::ExistingThread {
                thread_id: receipt.thread_id.clone(),
            },
            message: "continue".into(),
        };
        let result =
            awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
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
                awaken_session_execution_contract::CoordinatedRunIntent::FollowUp,
                "{rule}/E1"
            );
        } else {
            let error = result.expect_err("A5 Failed must reject");
            assert_eq!(
                error.kind,
                awaken_session_types::RunErrorKind::BadRequest,
                "A5/E2"
            );
            assert_eq!(runtime.admissions.lock().unwrap().len(), 1, "A5/E2");
            assert_eq!(
                repo.get(&SessionId::from(session_id))
                    .await
                    .expect("A5 activity after rejection")
                    .active_activity_epochs,
                active_before,
                "A5/E2 admission rejection opens no activity"
            );
        }
    }

    for rule in ["A6", "A7", "A8"] {
        let session_id = format!("follow-up-{rule}");
        let repo = authority_repository("follow-up target repository").await;
        create(repo.as_ref(), coordinated_session(&session_id)).await;
        let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
        runtime.set_admission_observations([Ok(None)]);
        let thread_id = ThreadId(format!("{session_id}-target"));
        match rule {
            "A6" => {}
            "A7" => runtime.commit_link(awaken_session_execution_contract::CoordinatedThreadLink {
                session_id: SessionId::from(&session_id),
                thread_id: thread_id.clone(),
                target: awaken_session_execution_contract::CoordinatedThreadTarget::Advisor {
                    model: "advisor-model".into(),
                },
                created_by_operation_id: "advisor-operation".into(),
                latest_run_id: Some(RunId("advisor-run".into())),
            }),
            "A8" => {
                runtime.commit_link(awaken_session_execution_contract::CoordinatedThreadLink {
                    session_id: SessionId::from(&session_id),
                    thread_id: thread_id.clone(),
                    target: awaken_session_execution_contract::CoordinatedThreadTarget::Agent {
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
            .get(&SessionId::from(&session_id))
            .await
            .expect("target state before rejection")
            .active_activity_epochs;
        let error =
            awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
                &app,
                awaken_session_execution_contract::SessionAgentMessageCommand {
                    session_id: SessionId::from(&session_id),
                    source_thread_id: ThreadId(session_id.clone()),
                    source_run_id: RunId(format!("{session_id}-parent-run")),
                    source_call_id: format!("{rule}-call"),
                    operation_id: format!("{rule}-operation"),
                    target: awaken_session_execution_contract::SessionAgentTarget::ExistingThread {
                        thread_id,
                    },
                    message: "must reject before activity".into(),
                },
            )
            .await
            .expect_err("non-ordinary/closed target must reject");
        assert_eq!(
            error.kind,
            awaken_session_types::RunErrorKind::BadRequest,
            "{rule}/E2"
        );
        assert!(
            runtime.admissions.lock().unwrap().is_empty(),
            "{rule}/E2 no Runtime admission"
        );
        assert_eq!(
            repo.get(&SessionId::from(session_id))
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
    let repo = authority_repository("foreground resume repository").await;
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
        awaken_session_application_contract::RunResume::Permission(
            awaken_agent_contract::agent::awaiting::PermissionDecision::Allow { note: None },
        )
    };

    assert!(
        accepted
            .resume_session_run_streaming_for_owner(
                "foreign-workspace",
                "foreground-request-foreign",
                &SessionId::from("foreground-resume"),
                "reply-tool",
                allow(),
                Arc::new(awaken_session_application_contract::EventForwardingSink::new(|_| Ok(()))),
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
            awaken_session_application_contract::RunResume::ClientResult {
                content: Vec::new(),
                is_error: false,
            },
        ),
    ] {
        assert!(
            accepted
                .resume_session_run(
                    operation_id,
                    &SessionId::from("foreground-resume"),
                    tool_use_id,
                    resume,
                )
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
        repo.get(&SessionId::from("foreground-resume"))
            .await
            .expect("F0/F1/F2 state")
            .active_activity_epochs
            .is_empty(),
        "F0/F1/F2/E1"
    );

    let outcome = accepted
        .resume_session_run_streaming_for_owner(
            "workspace",
            "foreground-request-accepted",
            &SessionId::from("foreground-resume"),
            "reply-tool",
            allow(),
            Arc::new(awaken_session_application_contract::EventForwardingSink::new(|_| Ok(()))),
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
    // F5 observation sink passes through the same authorized resume path;
    // F0 rejected owner never reaches the Runtime observer.
    assert_eq!(accepted_runtime.observed_sinks.load(Ordering::SeqCst), 1);
    let delivery = accepted_runtime.deliveries.lock().unwrap()[0].clone();
    assert_eq!(
        delivery.command.target,
        awaken_session_execution_contract::SessionThreadTarget::Primary,
        "F3/E2"
    );
    assert_eq!(delivery.command.expected_run_id.0, "reply-run", "F3/E2");
    assert_eq!(
        delivery.command.expected_correlation_id, "reply-correlation",
        "F3/E2"
    );
    assert_eq!(delivery.command.expected_thread_version, Some(1), "F3/E2");
    assert_eq!(
        repo.get(&SessionId::from("foreground-resume"))
            .await
            .expect("F3 state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([delivery
            .session_activity_epoch
            .expect("Session reply owns an activity")]),
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
                &SessionId::from("foreground-resume-rejected"),
                "reply-tool",
                allow(),
            )
            .await
            .is_err(),
        "F4 rejection surfaces"
    );
    let rejected_state = repo
        .get(&SessionId::from("foreground-resume-rejected"))
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
    let repo = authority_repository("reply activity repository").await;
    for session_id in [
        "reply-accepted",
        "reply-rejected",
        "reply-ambiguous",
        "reply-primary",
    ] {
        create(repo.as_ref(), persisted(session_id, false, "idle")).await;
    }
    let command =
        |session_id: &str| awaken_session_execution_contract::SessionThreadToolReplyCommand {
            session_id: SessionId::from(session_id),
            tool_request_event_id: None,
            expected_thread_version: None,
            target: awaken_session_execution_contract::SessionThreadTarget::Child(ThreadId(
                "reply-child".into(),
            )),
            expected_run_id: RunId("reply-run".into()),
            expected_correlation_id: "reply-correlation".into(),
            tool_use_id: "reply-tool".into(),
            reply: awaken_session_execution_contract::SessionThreadToolReply::Confirm(
                awaken_agent_contract::agent::awaiting::PermissionDecision::Allow { note: None },
            ),
            accompanying_system: None,
        };

    // P7: extension-owned primary reply -> delivered without opening any root activity.
    // Exact replay has the same effect; the Outcome's ordinary Run remains its sole owner.
    create(repo.as_ref(), persisted("reply-extension", false, "idle")).await;
    let mut extension_runtime = RecordingReplyRuntime::new(ReplyRuntimeOutcome::Accepted);
    extension_runtime.activity =
        awaken_session_execution_contract::SessionThreadToolReplyActivity::ThreadExtension;
    let extension_runtime = Arc::new(extension_runtime);
    let extension = application_with_runtime(
        extension_runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let mut extension_command = command("reply-extension");
    extension_command.target = awaken_session_execution_contract::SessionThreadTarget::Primary;
    for _ in 0..2 {
        awaken_session_execution_contract::SessionAgentCoordination::reply_session_thread_tool(
            &extension,
            extension_command.clone(),
        )
        .await
        .expect("extension reply");
        assert!(
            repo.get(&SessionId::from("reply-extension"))
                .await
                .expect("extension state")
                .active_activity_epochs
                .is_empty()
        );
    }
    assert!(
        extension_runtime
            .deliveries
            .lock()
            .unwrap()
            .iter()
            .all(|delivery| delivery.session_activity_epoch.is_none())
    );

    let accepted_runtime = Arc::new(RecordingReplyRuntime::new(ReplyRuntimeOutcome::Accepted));
    let accepted = application_with_runtime(
        accepted_runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    awaken_session_execution_contract::SessionAgentCoordination::reply_session_thread_tool(
        &accepted,
        command("reply-accepted"),
    )
    .await
    .expect("P1 accepted reply");
    let first_epoch = accepted_runtime.deliveries.lock().unwrap()[0].session_activity_epoch;
    let active = repo
        .get(&SessionId::from("reply-accepted"))
        .await
        .expect("P1 state");
    assert_eq!(
        active.active_activity_epochs,
        std::collections::BTreeSet::from([first_epoch.expect("Session reply owns an activity")]),
        "P1/E1"
    );

    awaken_session_execution_contract::SessionAgentCoordination::reply_session_thread_tool(
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
        repo.get(&SessionId::from("reply-accepted"))
            .await
            .expect("P2 state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([first_epoch.expect("Session reply owns an activity")]),
        "P2/E2 one active membership"
    );

    let mut primary_command = command("reply-primary");
    primary_command.target = awaken_session_execution_contract::SessionThreadTarget::Primary;
    awaken_session_execution_contract::SessionAgentCoordination::reply_session_thread_tool(
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
        awaken_session_execution_contract::SessionThreadTarget::Primary,
        "P6/E6"
    );
    assert_eq!(
        repo.get(&SessionId::from("reply-primary"))
            .await
            .expect("P6 state")
            .active_activity_epochs,
        std::collections::BTreeSet::from([primary_delivery
            .session_activity_epoch
            .expect("Session reply owns an activity")]),
        "P6/E6"
    );

    let rejected = application_with_runtime(
        Arc::new(RecordingReplyRuntime::new(ReplyRuntimeOutcome::BadRequest)),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(
        awaken_session_execution_contract::SessionAgentCoordination::reply_session_thread_tool(
            &rejected,
            command("reply-rejected"),
        )
        .await
        .is_err(),
        "P3 rejection surfaces"
    );
    let rejected_state = repo
        .get(&SessionId::from("reply-rejected"))
        .await
        .expect("P3 state");
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
        awaken_session_execution_contract::SessionAgentCoordination::reply_session_thread_tool(
            &ambiguous,
            command("reply-ambiguous"),
        )
        .await
        .is_err(),
        "P4 ambiguity surfaces for retry"
    );
    let ambiguous_epoch = ambiguous_runtime.deliveries.lock().unwrap()[0].session_activity_epoch;
    let ambiguous_state = repo
        .get(&SessionId::from("reply-ambiguous"))
        .await
        .expect("P4 state");
    assert_eq!(
        ambiguous_state.active_activity_epochs,
        std::collections::BTreeSet::from(
            [ambiguous_epoch.expect("Session reply owns an activity")]
        ),
        "P4/E4"
    );
    assert_eq!(
        ambiguous_state.execution,
        SessionExecutionState::Running,
        "P4/E4"
    );
    assert!(
        awaken_session_execution_contract::SessionAgentCoordination::reply_session_thread_tool(
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
        repo.get(&SessionId::from("reply-ambiguous"))
            .await
            .expect("P4R state")
            .active_activity_epochs,
        std::collections::BTreeSet::from(
            [ambiguous_epoch.expect("Session reply owns an activity")]
        ),
        "P4R/E4 one retained activity"
    );

    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &accepted,
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
            session_id: "reply-accepted".into(),
            source_thread_id: ThreadId("reply-child".into()),
            source_run_id: RunId("reply-run".into()),
            source_agent_id: "reply-agent".into(),
            session_activity_epoch: first_epoch.expect("Session reply owns an activity"),
            cancellation_requested: false,
        },
    )
    .await
    .expect("P5 committed Awaiting boundary");
    let settled = repo
        .get(&SessionId::from("reply-accepted"))
        .await
        .expect("P5 state");
    assert!(settled.active_activity_epochs.is_empty(), "P5/E5");
    assert_eq!(settled.execution, SessionExecutionState::Idle, "P5/E5");
}

#[tokio::test]
async fn reached_budget_allows_only_the_exact_required_action_continuation() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // B3 cause/effect graph: C1 the one Session budget is reached; C2 a command is
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
    let repo = authority_repository("required-action budget repository").await;
    for session_id in ["reached-valid-reply", "reached-invalid-reply"] {
        let mut session = persisted(session_id, false, "idle");
        session.budget = awaken_session_budget_contract::SessionBudgetState::Active {
            max_list_cost_minor: 1,
            consumed_numerator:
                awaken_session_budget_contract::SessionBudgetState::MICROS_PER_MINOR_USD
                    * awaken_session_budget_contract::SessionBudgetState::COST_DENOMINATOR,
            usage_cursor: Default::default(),
            snapshot: awaken_session_budget_contract::ManagedListPriceSnapshot {
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
            app.begin_activity_for_operation(&SessionId::from("reached-valid-reply"), "new-run")
                .await,
            Err(SessionActivityError::BudgetReached)
        ),
        "A1/E1"
    );
    awaken_session_execution_contract::SessionAgentCoordination::reply_session_thread_tool(
        &app,
        awaken_session_execution_contract::SessionThreadToolReplyCommand {
            session_id: "reached-valid-reply".into(),
            tool_request_event_id: None,
            expected_thread_version: None,
            target: awaken_session_execution_contract::SessionThreadTarget::Child(ThreadId(
                "reply-child".into(),
            )),
            expected_run_id: RunId("reply-run".into()),
            expected_correlation_id: "reply-correlation".into(),
            tool_use_id: "reply-tool".into(),
            reply: awaken_session_execution_contract::SessionThreadToolReply::Confirm(
                awaken_agent_contract::agent::awaiting::PermissionDecision::Allow { note: None },
            ),
            accompanying_system: None,
        },
    )
    .await
    .expect("A2 exact Awaiting continuation");
    let admitted = repo
        .get(&SessionId::from("reached-valid-reply"))
        .await
        .expect("A2 state");
    assert_eq!(admitted.active_activity_epochs.len(), 1, "A2/E2");
    assert_eq!(runtime.deliveries.lock().unwrap().len(), 1, "A2/E2");

    let before = repo
        .get(&SessionId::from("reached-invalid-reply"))
        .await
        .expect("A3 state before");
    let error =
        awaken_session_execution_contract::SessionAgentCoordination::reply_session_thread_tool(
            &app,
            awaken_session_execution_contract::SessionThreadToolReplyCommand {
                session_id: "reached-invalid-reply".into(),
                tool_request_event_id: None,
                expected_thread_version: None,
                target: awaken_session_execution_contract::SessionThreadTarget::Child(ThreadId(
                    "reply-child".into(),
                )),
                expected_run_id: RunId("reply-run".into()),
                expected_correlation_id: "reply-correlation".into(),
                tool_use_id: "forged-tool".into(),
                reply: awaken_session_execution_contract::SessionThreadToolReply::Confirm(
                    awaken_agent_contract::agent::awaiting::PermissionDecision::Allow {
                        note: None,
                    },
                ),
                accompanying_system: None,
            },
        )
        .await
        .expect_err("A3 forged reply");
    assert_eq!(
        error.kind,
        awaken_session_types::RunErrorKind::BadRequest,
        "A3/E3"
    );
    assert_eq!(
        repo.get(&SessionId::from("reached-invalid-reply"))
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
    // B3 cause/effect graph: C1 the shared budget is reached; C2 the caller only
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
    let repo = authority_repository("budget choke repository").await;
    let mut session = coordinated_session("budget-admission-choke");
    session.budget = awaken_session_budget_contract::SessionBudgetState::Active {
        max_list_cost_minor: 1,
        consumed_numerator: awaken_session_budget_contract::SessionBudgetState::MICROS_PER_MINOR_USD
            * awaken_session_budget_contract::SessionBudgetState::COST_DENOMINATOR,
        usage_cursor: Default::default(),
        snapshot: awaken_session_budget_contract::ManagedListPriceSnapshot {
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
    runtime.set_admission_observations([Ok(None)]);
    let app = configured_admission_application(repo.clone(), runtime.clone());

    let roster = awaken_session_execution_contract::SessionAgentCoordination::list_session_agents(
        &app,
        &SessionId::from("budget-admission-choke"),
    )
    .await
    .expect("G1 read-only roster remains visible");
    assert_eq!(roster.len(), 1, "G1/E1");

    let error =
        awaken_session_execution_contract::SessionAgentCoordination::send_session_agent_message(
            &app,
            coordination_spawn_command("budget-admission-choke"),
        )
        .await
        .expect_err("G2 reached budget");
    assert_eq!(error.code, "budget_reached", "G2/E2");
    assert!(runtime.admissions.lock().unwrap().is_empty(), "G2/E3");
    assert!(
        repo.get(&SessionId::from("budget-admission-choke"))
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
    // child commits Failed while a later continuation may already be queued;
    // C6 the exact Run is durable but its lifecycle projection is not visible.
    // Effects: E1 live Awaiting durably settles the exact activity and never
    // calls the primary continuation; E2 terminal Session accepts late exact or
    // unknown completion as a no-op without reopening publication dependencies;
    // E3 Running is rejected before activity mutation; E4 a cancelled terminal
    // child settles directly and emits no primary continuation; E5 Failed first
    // interrupts the Thread's existing Dispatch queue, then settles directly
    // without manufacturing a successful primary report; E6 missing lifecycle
    // returns retryable failure without settling activity or emitting effects. Normal Completed
    // report admission remains covered by S6 in the admission decision table.
    //
    // | Rule | Boundary | Session | Dependencies | Effect |
    // | B1 | Awaiting | live | absent | E1 |
    // | B2 | Ended | terminal | absent | E2 |
    // | B3 | Running | live | absent | E3 |
    // | B4 | Ended + cancelled | live | absent | E4 |
    // | B5 | Failed | live | interrupt port | E5 |
    // | B6 | Ended | live, lifecycle missing | absent | E6 |
    let repo = authority_repository("boundary repository").await;
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
    create(
        repo.as_ref(),
        persisted("boundary-unprojected", false, "idle"),
    )
    .await;
    let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let (_, awaiting_epoch) = app
        .begin_activity_for_operation(&SessionId::from("boundary-awaiting"), "boundary-op")
        .await
        .expect("B1 activity");
    runtime.commit_boundary(
        "boundary-awaiting",
        &ThreadId("boundary-child".into()),
        &RunId("boundary-child-run".into()),
        RunState::Awaiting,
        "pending",
    );
    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
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
    let awaiting = repo
        .get(&SessionId::from("boundary-awaiting"))
        .await
        .expect("B1 state");
    assert!(awaiting.active_activity_epochs.is_empty(), "B1/E1");
    assert_eq!(awaiting.execution, SessionExecutionState::Idle, "B1/E1");

    let terminal_before = repo
        .get(&SessionId::from("boundary-terminal"))
        .await
        .expect("B2 before");
    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
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
        repo.get(&SessionId::from("boundary-terminal"))
            .await
            .expect("B2 after"),
        terminal_before,
        "B2/E2"
    );

    let (_, running_epoch) = app
        .begin_activity_for_operation(&SessionId::from("boundary-running"), "boundary-op")
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
        awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
            &app,
            awaken_session_execution_contract::SessionAgentBoundaryCommand {
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
        repo.get(&SessionId::from("boundary-running"))
            .await
            .expect("B3 state")
            .active_activity_epochs
            .contains(&running_epoch),
        "B3/E3"
    );

    let (_, interrupted_epoch) = app
        .begin_activity_for_operation(&SessionId::from("boundary-interrupted"), "boundary-op")
        .await
        .expect("B4 activity");
    runtime.commit_boundary(
        "boundary-interrupted",
        &ThreadId("boundary-interrupted-child".into()),
        &RunId("boundary-interrupted-run".into()),
        RunState::Ended(EndCause::NaturalEnd),
        "must not become a primary report",
    );
    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
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
    let interrupted = repo
        .get(&SessionId::from("boundary-interrupted"))
        .await
        .expect("B4 state");
    assert!(interrupted.active_activity_epochs.is_empty(), "B4/E4");
    assert_eq!(interrupted.execution, SessionExecutionState::Idle, "B4/E4");
    assert!(runtime.continuations.lock().unwrap().is_empty(), "B4/E4");

    let (_, failed_epoch) = app
        .begin_activity_for_operation(&SessionId::from("boundary-failed"), "boundary-op")
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
    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
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
    let failed = repo
        .get(&SessionId::from("boundary-failed"))
        .await
        .expect("B5 state");
    assert!(failed.active_activity_epochs.is_empty(), "B5/E5");
    assert_eq!(failed.execution, SessionExecutionState::Idle, "B5/E5");
    assert_eq!(
        runtime.interruptions.lock().unwrap().as_slice(),
        &[("boundary-failed".into(), failed_child)],
        "B5/E5 one existing Dispatch cancellation path"
    );
    assert!(runtime.continuations.lock().unwrap().is_empty(), "B5/E5");

    let (_, unprojected_epoch) = app
        .begin_activity_for_operation(&SessionId::from("boundary-unprojected"), "boundary-op")
        .await
        .expect("B6 activity");
    runtime.commit_boundary(
        "boundary-unprojected",
        &ThreadId("boundary-unprojected-child".into()),
        &RunId("boundary-unprojected-run".into()),
        RunState::Ended(EndCause::NaturalEnd),
        "must remain pending until projected",
    );
    runtime.set_lifecycle_visible(false);
    let unavailable =
        awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
            &app,
            awaken_session_execution_contract::SessionAgentBoundaryCommand {
                session_id: "boundary-unprojected".into(),
                source_thread_id: ThreadId("boundary-unprojected-child".into()),
                source_run_id: RunId("boundary-unprojected-run".into()),
                source_agent_id: "unused-unprojected-agent".into(),
                session_activity_epoch: unprojected_epoch,
                cancellation_requested: false,
            },
        )
        .await
        .expect_err("B6 missing lifecycle must retry");
    assert_eq!(unavailable.code, "unavailable", "B6/E6");
    let unprojected = repo
        .get(&SessionId::from("boundary-unprojected"))
        .await
        .expect("B6 state");
    assert!(
        unprojected
            .active_activity_epochs
            .contains(&unprojected_epoch),
        "B6/E6"
    );
    assert!(runtime.continuations.lock().unwrap().is_empty(), "B6/E6");
    assert_eq!(runtime.interruptions.lock().unwrap().len(), 1, "B6/E6");
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
    let repo = authority_repository("redelivery repository").await;
    let session_id = "boundary-redelivery";
    create(repo.as_ref(), persisted(session_id, false, "idle")).await;
    let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let (_, source_epoch) = app
        .begin_activity_for_operation(&SessionId::from(session_id), "source-operation")
        .await
        .expect("R2 source activity");
    app.lifecycle()
        .settle_activity_observed(
            &awaken_session_types::SessionId::from(session_id),
            source_epoch,
            None,
        )
        .await
        .expect("R2 observer effect committed before queue settlement");
    let (_, successor_epoch) = app
        .begin_activity_for_operation(&SessionId::from(session_id), "successor-operation")
        .await
        .expect("R2 successor activity");

    let thread_id = ThreadId("boundary-redelivery-child".into());
    let source_run_id = RunId("boundary-redelivery-source".into());
    let successor_run_id = RunId("boundary-redelivery-successor".into());
    runtime.set_recovery_snapshot(
        session_id,
        &thread_id,
        awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
            commit_timestamps: Default::default(),
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
            latest_run_id: Some(successor_run_id.clone()),
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            // R2 both immutable boundaries belong to this same atomic prefix;
            // latest Run metadata cannot synthesize the older terminal audit.
            events: [
                (source_run_id.clone(), RunState::Ended(EndCause::NaturalEnd)),
                (successor_run_id, RunState::Running),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (run_id, state))| {
                let draft: awaken_agent_contract::audit::draft::Draft =
                    awaken_agent_contract::audit::run_event::RunEvent::RunStateChanged {
                        state,
                        await_reason: None,
                        await_target: None,
                    }
                    .into();
                awaken_agent_contract::audit::record::Record {
                    sequence: awaken_agent_contract::encode_run_lifecycle_cursor(
                        index as u64 + 1,
                        0,
                    )
                    .unwrap()
                    .0,
                    run_id,
                    kind: draft.kind,
                    payload: draft.payload,
                }
            })
            .collect(),
            resume_tickets: Vec::new(),
            thread_version: 2,
            store_cursor: 2,
            next_commit_ordinal: 1,
        },
    );
    runtime.set_generic_recovery_available(false);
    assert!(
        awaken_session_execution_contract::ThreadReadRuntime::session_thread_recovery_snapshot(
            runtime.as_ref(),
            &SessionId::from(session_id),
            &thread_id.0,
        )
        .await
        .expect("R2 cold generic recovery")
        .is_none(),
        "R2 process-local latest projection is intentionally cold"
    );
    let before_replay = repo
        .get(&SessionId::from(session_id))
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

    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
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
    let after_replay = repo
        .get(&SessionId::from(session_id))
        .await
        .expect("R2 Session");
    assert_eq!(after_replay, before_replay, "R2/E1+E2");

    let missing =
        awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
            &app,
            awaken_session_execution_contract::SessionAgentBoundaryCommand {
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
                kind: awaken_session_types::RunErrorKind::BadRequest,
                ..
            })
        ),
        "R3/E3"
    );
    assert_eq!(
        repo.get(&SessionId::from(session_id))
            .await
            .expect("R3 Session"),
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
    let repo = authority_repository("missing-report repository").await;
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
        .begin_activity_for_operation(
            &SessionId::from("completed-without-report"),
            "missing-report-operation",
        )
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

    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
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
        .get(&SessionId::from("completed-without-report"))
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
    // B4 cause/effect graph: C1 the primary and one first-class Advisor Thread own
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
    let repo = authority_repository("Advisor usage repository").await;
    create(repo.as_ref(), persisted("advisor-usage", false, "idle")).await;
    let runtime = Arc::new(
        RecordingBoundaryBudgetRuntime::new(awaken_session_budget_contract::SessionUsage {
            input_tokens: 3,
            output_tokens: 7,
            cache_read_tokens: 2,
            cache_creation_tokens: 5,
            web_fetch_requests: 4,
            web_search_requests: 2,
            active_seconds: 888,
            by_model: std::collections::BTreeMap::from([(
                "shared-model".into(),
                awaken_session_budget_contract::SessionModelUsage {
                    input_tokens: 3,
                    output_tokens: 7,
                    cache_read_tokens: 2,
                    cache_creation_tokens: 5,
                },
            )]),
        })
        .with_child_target(
            awaken_session_execution_contract::CoordinatedThreadTarget::Advisor {
                model: "advisor-model".into(),
            },
        ),
    );
    runtime.set_root_usage(awaken_session_budget_contract::SessionUsage {
        input_tokens: 10,
        output_tokens: 14,
        cache_read_tokens: 4,
        cache_creation_tokens: 1,
        web_fetch_requests: 9,
        web_search_requests: 6,
        active_seconds: 999,
        by_model: std::collections::BTreeMap::from([(
            "shared-model".into(),
            awaken_session_budget_contract::SessionModelUsage {
                input_tokens: 10,
                output_tokens: 14,
                cache_read_tokens: 4,
                cache_creation_tokens: 1,
            },
        )]),
    });
    runtime.commit_boundary(
        "advisor-usage",
        &ThreadId("advisor-thread".into()),
        &RunId("advisor-run".into()),
        RunState::Ended(EndCause::NaturalEnd),
        "advice",
    );
    let app = application_with_runtime(
        runtime.clone(),
        repo,
        Arc::new(RecordingEnvironmentSource::default()),
    );

    assert_eq!(
        app.session_thread_usage(&SessionId::from("advisor-usage"), "advisor-usage")
            .await
            .expect("A1 primary usage")
            .output_tokens,
        14,
        "A1/E1 primary attribution"
    );
    assert_eq!(
        app.session_thread_usage(&SessionId::from("advisor-usage"), "advisor-thread")
            .await
            .expect("A1 Advisor usage")
            .output_tokens,
        7,
        "A1/E1 Advisor attribution"
    );
    let usage = app
        .session_usage("advisor-usage", None)
        .await
        .expect("A1 Session usage");
    let expected_one = awaken_session_budget_contract::SessionUsage {
        input_tokens: 13,
        output_tokens: 21,
        cache_read_tokens: 6,
        cache_creation_tokens: 6,
        web_fetch_requests: 13,
        web_search_requests: 8,
        active_seconds: 0,
        by_model: std::collections::BTreeMap::from([(
            "shared-model".into(),
            awaken_session_budget_contract::SessionModelUsage {
                input_tokens: 13,
                output_tokens: 21,
                cache_read_tokens: 6,
                cache_creation_tokens: 6,
            },
        )]),
    };
    assert_eq!(
        usage, expected_one,
        "A1 every counter and model attribution; Session owns active time"
    );
    let gated_usage = app
        .session_usage_for_model_request("advisor-usage", "advisor-thread")
        .await
        .expect("A2 gated Session usage");
    assert_eq!(gated_usage.output_tokens, 21, "A2/E2+E3");
    // A3 the same logical child has another committed Run, so this Runtime's
    // relation projection contains that Thread twice. Its cumulative counter
    // is still 7, not two independent per-Run charges. Both ordinary and
    // model-request totals must stay 14+7, reusing the existing identity set.
    let mut snapshot = runtime
        .boundaries
        .snapshot("advisor-usage", "advisor-thread")
        .unwrap();
    let second_run = RunId("advisor-run-2".into());
    snapshot
        .runs
        .push(awaken_agent_contract::agent::run::Record {
            id: second_run.clone(),
            thread_id: ThreadId("advisor-thread".into()),
            state: RunState::Ended(EndCause::NaturalEnd),
        });
    snapshot
        .messages
        .push(awaken_agent_contract::agent::message::Message::text(
            awaken_agent_contract::agent::message::Id::assistant(&second_run, 0),
            awaken_agent_contract::agent::message::Role::Assistant,
            "second advice",
        ));
    snapshot.thread_version += 1;
    snapshot.store_cursor += 1;
    snapshot.message_commit_cursors.push(snapshot.store_cursor);
    snapshot.claimed_run_id = second_run.clone();
    snapshot.latest_run_id = Some(second_run);
    runtime.boundaries.set_recovery_snapshot(
        "advisor-usage",
        &ThreadId("advisor-thread".into()),
        snapshot,
    );
    assert_eq!(
        runtime
            .coordinated_threads(&SessionId::from("advisor-usage"), None)
            .await
            .unwrap()
            .len(),
        2,
        "A3 repeated Thread fixture"
    );
    assert_eq!(
        app.session_usage("advisor-usage", None)
            .await
            .unwrap()
            .output_tokens,
        21,
        "A3 cumulative usage once"
    );
    assert_eq!(
        app.session_usage_for_model_request("advisor-usage", "advisor-thread")
            .await
            .unwrap()
            .output_tokens,
        21,
        "A3 model gate cumulative usage once"
    );
    // A4 observed links (including repeated same-Thread links) omit a later
    // child until the next complete prefix. The counters themselves stay current;
    // no relationship read occurs and every included logical Thread is read once.
    // A5 an observed empty set sums only the primary. A6 ordinary/model reads
    // still discover current links and include an unlinked executing child once.
    // A7 a foreign Session link fails before any child query; the primary may
    // already have been read. A8 a root entry cannot charge the primary twice.
    let observed = runtime
        .coordinated_threads(&SessionId::from("advisor-usage"), None)
        .await
        .unwrap();
    runtime.commit_boundary(
        "advisor-usage",
        &ThreadId("later-child".into()),
        &RunId("later-run".into()),
        RunState::Ended(EndCause::NaturalEnd),
        "later advice",
    );
    let before = runtime.usage_relationship_reads.load(Ordering::SeqCst);
    runtime.usage_thread_reads.lock().unwrap().clear();
    let usage = app
        .session_usage("advisor-usage", Some(&observed))
        .await
        .unwrap();
    assert_eq!(usage, expected_one, "A4 exact observed child set");
    assert_eq!(
        runtime.usage_relationship_reads.load(Ordering::SeqCst),
        before,
        "A4 no repeated relationship query"
    );
    assert_eq!(
        *runtime.usage_thread_reads.lock().unwrap(),
        ["advisor-thread"],
        "A4 one cumulative child read"
    );
    runtime.usage_thread_reads.lock().unwrap().clear();
    assert_eq!(
        app.session_usage("advisor-usage", Some(&[]))
            .await
            .unwrap()
            .output_tokens,
        14,
        "A5 root only"
    );
    assert!(
        runtime.usage_thread_reads.lock().unwrap().is_empty(),
        "A5 no child reads"
    );
    assert_eq!(
        runtime.usage_relationship_reads.load(Ordering::SeqCst),
        before,
        "A5 explicit empty observation is not discovery"
    );
    assert_eq!(
        app.session_usage("advisor-usage", None)
            .await
            .unwrap()
            .output_tokens,
        28,
        "A6 latest child set"
    );
    assert_eq!(
        runtime.usage_relationship_reads.load(Ordering::SeqCst),
        before + 1,
        "A6 discovery retained"
    );
    for (additional, expected) in [
        ("advisor-usage", 28),
        ("advisor-thread", 28),
        ("executing-before-link", 35),
    ] {
        assert_eq!(
            app.session_usage_for_model_request("advisor-usage", additional)
                .await
                .unwrap()
                .output_tokens,
            expected,
            "A6 {additional}"
        );
    }
    // A6b failed fresh discovery propagates; the already observed read does
    // not invoke that dependency. The Managed read-cut matrix separately proves
    // that its later final relationship failure still rejects publication.
    runtime
        .fail_usage_relationship_read
        .store(true, Ordering::SeqCst);
    runtime.usage_thread_reads.lock().unwrap().clear();
    assert_eq!(
        app.session_usage("advisor-usage", None)
            .await
            .unwrap_err()
            .kind,
        awaken_session_types::RunErrorKind::Unavailable,
        "A6b fresh dependency error"
    );
    assert!(runtime.usage_thread_reads.lock().unwrap().is_empty());
    assert_eq!(
        app.session_usage("advisor-usage", Some(&observed))
            .await
            .unwrap(),
        expected_one,
        "A6b no hidden discovery in observed read"
    );
    runtime
        .fail_usage_relationship_read
        .store(false, Ordering::SeqCst);
    let mut foreign = observed[0].clone();
    foreign.session_id = "foreign-session".into();
    for invalid in [vec![foreign.clone()], vec![observed[0].clone(), foreign]] {
        runtime.usage_thread_reads.lock().unwrap().clear();
        let error = app
            .session_usage("advisor-usage", Some(&invalid))
            .await
            .unwrap_err();
        assert_eq!(
            error.kind,
            awaken_session_types::RunErrorKind::Internal,
            "A7"
        );
        assert!(
            runtime.usage_thread_reads.lock().unwrap().is_empty(),
            "A7 prevalidate all source scopes before child queries"
        );
    }
    let mut root_entry = observed[0].clone();
    root_entry.thread_id = ThreadId("advisor-usage".into());
    assert_eq!(
        app.session_usage("advisor-usage", Some(&[root_entry]))
            .await
            .unwrap()
            .output_tokens,
        14,
        "A8 primary once"
    );
}

#[tokio::test]
async fn model_request_admission_reconciles_root_and_child_usage_at_one_cas() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // B1 cause/effect graph: C1 the latest committed root or child Run is live; C2
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
    let repo = authority_repository("request gate repository").await;
    let snapshot = awaken_session_budget_contract::ManagedListPriceSnapshot {
        snapshot_id: "request-gate-prices-v1".into(),
        version: 1,
        effective_at_unix_ms: 1,
        arithmetic_version: 1,
        model_rates: std::collections::BTreeMap::from([(
            "model".into(),
            awaken_session_budget_contract::ManagedTokenListRates {
                input_micros_per_million: 1_000_000,
                ..Default::default()
            },
        )]),
        runtime_rates: Default::default(),
        fingerprint: "request-gate-prices-v1-fingerprint".into(),
    };
    for id in ["request-gate-root", "request-gate-child"] {
        let mut session = persisted(id, false, "idle");
        session.budget =
            awaken_session_budget_contract::SessionBudgetState::active(1, snapshot.clone());
        create(repo.as_ref(), session).await;
    }
    let runtime = Arc::new(RecordingBoundaryBudgetRuntime::new(Default::default()));
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let reached_usage = awaken_session_budget_contract::SessionUsage {
        input_tokens: 10_000,
        by_model: std::collections::BTreeMap::from([(
            "model".into(),
            awaken_session_budget_contract::SessionModelUsage {
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
        awaken_session_execution_contract::SessionAgentCoordination::admit_session_model_request(
            &app,
            &SessionId::from("request-gate-root"),
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
            !awaken_session_execution_contract::SessionAgentCoordination::admit_session_model_request(
                &app,
                &SessionId::from("request-gate-root"),
                &root_thread,
                &root_run,
            )
            .await
            .expect("G2 root denial"),
            "G2/E2 replay={replay}"
        );
    }
    assert_eq!(
        repo.get(&SessionId::from("request-gate-root"))
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
        awaken_session_execution_contract::SessionAgentCoordination::admit_session_model_request(
            &app,
            &SessionId::from("request-gate-child"),
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
            !awaken_session_execution_contract::SessionAgentCoordination::admit_session_model_request(
                &app,
                &SessionId::from("request-gate-child"),
                &child_thread,
                &child_run,
            )
            .await
            .expect("G4 child denial"),
            "G4/E2 replay={replay}"
        );
    }
    assert_eq!(
        repo.get(&SessionId::from("request-gate-child"))
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
    // B2 cause/effect graph: C1 the child boundary is Awaiting or terminal; C2 an
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
    let repo = authority_repository("boundary budget repository").await;
    let price_snapshot = awaken_session_budget_contract::ManagedListPriceSnapshot {
        snapshot_id: "boundary-price-v1".into(),
        version: 1,
        effective_at_unix_ms: 1,
        arithmetic_version: 1,
        model_rates: std::collections::BTreeMap::from([(
            "model".into(),
            awaken_session_budget_contract::ManagedTokenListRates {
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
            awaken_session_budget_contract::SessionBudgetState::active(1, price_snapshot.clone());
        create(repo.as_ref(), session).await;
    }
    let runtime = Arc::new(RecordingBoundaryBudgetRuntime::new(
        awaken_session_budget_contract::SessionUsage {
            input_tokens: 10_000,
            by_model: std::collections::BTreeMap::from([(
                "model".into(),
                awaken_session_budget_contract::SessionModelUsage {
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

    let settle = |session_id: &str,
                  epoch: u64|
     -> awaken_session_execution_contract::SessionAgentBoundaryCommand {
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
            session_id: session_id.into(),
            source_thread_id: ThreadId("budget-child".into()),
            source_run_id: RunId("budget-child-run".into()),
            source_agent_id: "coord-child".into(),
            session_activity_epoch: epoch,
            cancellation_requested: false,
        }
    };

    let (_, awaiting_epoch) = app
        .begin_activity_for_operation(&SessionId::from("boundary-budget-awaiting"), "awaiting-op")
        .await
        .expect("B1 activity");
    runtime.commit_boundary(
        "boundary-budget-awaiting",
        &ThreadId("budget-child".into()),
        &RunId("budget-child-run".into()),
        RunState::Awaiting,
        "budget report",
    );
    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        settle("boundary-budget-awaiting", awaiting_epoch),
    )
    .await
    .expect("B1 boundary");
    let awaiting = repo
        .get(&SessionId::from("boundary-budget-awaiting"))
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

    runtime.set_admission_observations([
        Ok(None),
        Ok(Some(
            awaken_session_execution_contract::SessionAgentMessageReceipt {
                thread_id: ThreadId("boundary-budget-ended".into()),
            },
        )),
    ]);
    let (_, ended_epoch) = app
        .begin_activity_for_operation_with_policy(
            &SessionId::from("boundary-budget-ended"),
            "ended-op",
            false,
            None,
            None,
            Some((
                &ThreadId("budget-child".into()),
                &RunId("budget-child-run".into()),
            )),
        )
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
    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        ended_command.clone(),
    )
    .await
    .expect("B2 boundary");
    let ended = repo
        .get(&SessionId::from("boundary-budget-ended"))
        .await
        .expect("B2 state");
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

    awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
        &app,
        ended_command,
    )
    .await
    .expect("B3 replay");
    let replayed = repo
        .get(&SessionId::from("boundary-budget-ended"))
        .await
        .expect("B3 state");
    assert_eq!(replayed.revision, revision, "B3/E4");
    assert_eq!(replayed.budget.reach_transitions().len(), 1, "B3/E4");
    assert_eq!(
        runtime.continuations.load(Ordering::SeqCst),
        1,
        "B3/E4 original accepted receipt returns without redelivery"
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
    // B2 cause/effect graph: C1 two ordinary children are simultaneously in flight;
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
    let repo = authority_repository("concurrent budget repository").await;
    let mut session = coordinated_session("concurrent-child-budget");
    session.budget = awaken_session_budget_contract::SessionBudgetState::active(
        1,
        awaken_session_budget_contract::ManagedListPriceSnapshot {
            snapshot_id: "concurrent-price-v1".into(),
            version: 1,
            effective_at_unix_ms: 1,
            arithmetic_version: 1,
            model_rates: std::collections::BTreeMap::from([(
                "model".into(),
                awaken_session_budget_contract::ManagedTokenListRates {
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
        awaken_session_budget_contract::SessionUsage {
            input_tokens: 5_000,
            by_model: std::collections::BTreeMap::from([(
                "model".into(),
                awaken_session_budget_contract::SessionModelUsage {
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
        .begin_activity_for_operation_with_policy(
            &SessionId::from("concurrent-child-budget"),
            "child-a-op",
            false,
            None,
            None,
            Some((&child_a, &run_a)),
        )
        .await
        .expect("C1 child A activity");
    let (_, epoch_b) = app
        .begin_activity_for_operation_with_policy(
            &SessionId::from("concurrent-child-budget"),
            "child-b-op",
            false,
            None,
            None,
            Some((&child_b, &run_b)),
        )
        .await
        .expect("C1 child B activity");
    let command = |thread_id, run_id, session_activity_epoch| {
        awaken_session_execution_contract::SessionAgentBoundaryCommand {
            session_id: "concurrent-child-budget".into(),
            source_thread_id: thread_id,
            source_run_id: run_id,
            source_agent_id: "coord-child".into(),
            session_activity_epoch,
            cancellation_requested: false,
        }
    };
    runtime.set_admission_observations([Ok(None), Ok(None)]);
    let (settled_a, settled_b) = tokio::join!(
        awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
            &app,
            command(child_a.clone(), run_a.clone(), epoch_a),
        ),
        awaken_session_execution_contract::SessionAgentCoordination::settle_session_agent_boundary(
            &app,
            command(child_b.clone(), run_b.clone(), epoch_b),
        )
    );
    settled_a.expect("C1 child A settlement");
    settled_b.expect("C1 child B settlement");

    let settled = repo
        .get(&SessionId::from("concurrent-child-budget"))
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
    let repo = authority_repository("session repository").await;
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
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&title),
        if_match: None,
    };

    let updated = app
        .update_session(&SessionId::from("update-idle"), command("accepted"))
        .await
        .expect("U1");
    assert_eq!(updated.session.title.as_deref(), Some("accepted"), "U1");

    for (rule, id) in [("U2", "update-running"), ("U3", "update-terminal")] {
        let before = repo.get(&SessionId::from(id)).await.expect(rule);
        assert!(
            matches!(
                app.update_session(&SessionId::from(id), command("rejected"))
                    .await,
                Err(SessionUpdateError::NotIdle)
            ),
            "{rule}"
        );
        assert_eq!(
            repo.get(&SessionId::from(id)).await.expect(rule),
            before,
            "{rule}"
        );
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
    // U7 the resource recorder has no committed Thread: its existing explicit
    // empty-read role supplies cursor0; production unsupported reads still refuse.
    let repo = authority_repository("profiled update repository").await;
    for (id, policy) in [
        (
            "update-managed-policy",
            crate::session_resource::SessionMutationPolicy::Managed,
        ),
        (
            "update-frozen-policy",
            crate::session_resource::SessionMutationPolicy::Frozen,
        ),
        (
            "update-files-policy",
            crate::session_resource::SessionMutationPolicy::FileResources,
        ),
    ] {
        let mut session = persisted(id, false, "idle");
        let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
            &mut session.baseline
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
    let tools = awaken_session_configuration_contract::SessionToolConfiguration {
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
        &SessionId::from("update-managed-policy"),
        command(Some(tools.clone()), None, None, "U1"),
    )
    .await
    .expect("U1/E1");
    assert_eq!(
        repo.get(&SessionId::from("update-managed-policy"))
            .await
            .unwrap()
            .tools,
        tools,
        "U1/E1"
    );
    assert_eq!(
        runtime.replaced_tools.lock().unwrap().as_slice(),
        [("update-managed-policy".into(), tools.clone())].as_slice(),
        "U1/E1 post-commit projection"
    );

    let frozen_before = repo
        .get(&SessionId::from("update-frozen-policy"))
        .await
        .unwrap();
    assert!(
        matches!(
            app.update_session(
                &SessionId::from("update-frozen-policy"),
                command(Some(tools.clone()), None, None, "U2")
            )
            .await,
            Err(SessionUpdateError::Rejected(_))
        ),
        "U2/E2"
    );
    assert_eq!(
        repo.get(&SessionId::from("update-frozen-policy"))
            .await
            .unwrap(),
        frozen_before,
        "U2/E2"
    );

    let files_before = repo
        .get(&SessionId::from("update-files-policy"))
        .await
        .unwrap();
    assert!(
        matches!(
            app.update_session(
                &SessionId::from("update-files-policy"),
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
        repo.get(&SessionId::from("update-files-policy"))
            .await
            .unwrap(),
        files_before,
        "U3/E2"
    );

    app.update_session(
        &SessionId::from("update-files-policy"),
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
                &SessionId::from("update-files-policy"),
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
            &SessionId::from("update-frozen-policy"),
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
    // B6 cause/effect graph: C1 budget absent/active/removed; C2 requested limit
    // absent/present; C3 a present limit is greater than exact consumed cost.
    // Effects: E1 absent cannot acquire a budget; E2 active+C3 changes the cap
    // without erasing prior cap-transition history; E3 active+!C3 rejects; E4
    // active+removal preserves snapshot/cursor/transitions and disables admission
    // enforcement; E5 removed cannot become active again.
    // Decision table: B1 absent+present=>E1; B2 active+present+C3=>E2; B3
    // active+present+!C3=>E3; B4 active+absent=>E4; B5 removed+present=>E5.
    let repo = authority_repository("session repository").await;
    let mut active = persisted("budget-active", false, "idle");
    let snapshot = awaken_session_budget_contract::ManagedListPriceSnapshot {
        snapshot_id: "prices-v1".into(),
        version: 1,
        effective_at_unix_ms: 1,
        arithmetic_version: 1,
        model_rates: std::collections::BTreeMap::new(),
        runtime_rates: Default::default(),
        fingerprint: "prices-v1-fingerprint".into(),
    };
    active.budget = awaken_session_budget_contract::SessionBudgetState::Active {
        max_list_cost_minor: 10,
        consumed_numerator: 2
            * awaken_session_budget_contract::SessionBudgetState::MICROS_PER_MINOR_USD
            * awaken_session_budget_contract::SessionBudgetState::COST_DENOMINATOR,
        usage_cursor: Default::default(),
        snapshot,
        reach_transitions: vec![awaken_session_budget_contract::BudgetReachTransition {
            generation: 1,
            max_list_cost_minor: 1,
            consumed_numerator:
                awaken_session_budget_contract::SessionBudgetState::MICROS_PER_MINOR_USD
                    * awaken_session_budget_contract::SessionBudgetState::COST_DENOMINATOR,
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
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&budget),
        if_match: None,
    };

    assert!(
        matches!(
            app.update_session(&SessionId::from("budget-absent"), command(Some(3)))
                .await,
            Err(SessionUpdateError::Rejected(_))
        ),
        "B1/E1"
    );
    let updated = app
        .update_session(&SessionId::from("budget-active"), command(Some(3)))
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
            app.update_session(&SessionId::from("budget-active"), command(Some(2)))
                .await,
            Err(SessionUpdateError::Rejected(_))
        ),
        "B3/E3"
    );
    let removed = app
        .update_session(&SessionId::from("budget-active"), command(None))
        .await
        .expect("B4");
    assert!(
        matches!(
            removed.session.budget,
            awaken_session_budget_contract::SessionBudgetState::Removed { .. }
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
            app.update_session(&SessionId::from("budget-active"), command(Some(4)))
                .await,
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
    // B7 cause/effect graph: C1 a reached budget is raised or removed; C2 the
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
    use awaken_session_application_contract::{
        SessionBudgetResumeDisposition, SessionBudgetResumeTicket,
    };
    use awaken_session_budget_contract::SessionBudgetState;

    let repo = authority_repository("budget resume repository").await;
    let snapshot = awaken_session_budget_contract::ManagedListPriceSnapshot {
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
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&(budget, key)),
        if_match: None,
    };

    Box::pin(async {
        let raised = app
            .update_session(
                &SessionId::from("resume-budget-root"),
                command(Some(2), "raise-root"),
            )
            .await
            .expect("R1 raise resumes root");
        assert_eq!(raised.session.budget.max_list_cost_minor(), Some(2), "R1");
        let removed = app
            .update_session(
                &SessionId::from("resume-budget-child"),
                command(None, "remove-child"),
            )
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
        let session_id = SessionId::from("resume-budget-concurrent");
        let (left, right) = tokio::join!(
            Box::pin(app.update_session(&session_id, concurrent.clone())),
            Box::pin(app.update_session(&session_id, concurrent)),
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
        let concurrent_session = repo
            .get(&SessionId::from("resume-budget-concurrent"))
            .await
            .unwrap();
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
        app.update_session(
            &SessionId::from("resume-budget-stale"),
            command(Some(2), "stale-raise"),
        )
        .await
        .expect("R4 stale resume is a successful update");
        let stale = repo
            .get(&SessionId::from("resume-budget-stale"))
            .await
            .unwrap();
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
/// work admitted before renewal; C4 rejects a binding change while an equal
/// durable binding remains a side-effect-free recovery replay.
///
/// | Rule | Asserted lease | Binding | Effect |
/// |---|---|---|---|
/// | B1 | exact | new | persist once |
/// | B2 | exact | equal | idempotent success |
/// | B3 | shorter same-epoch assertion under live renewal | new/equal | authorized |
/// | B4 | stale owner/epoch | equal | fenced before replay, no mutation |
/// | B5 | stale owner/epoch | different | fenced, no mutation |
/// | B6 | aggregate/assertion both absent | new/equal | legacy CAS path |
/// | B7 | current lease expired | equal | fenced before replay, no mutation |
/// | B8 | current lease expired | different | fenced, no mutation |
#[tokio::test]
async fn environment_binding_persistence_is_fenced_by_exact_realization() {
    fn receipt(
        session_id: &str,
        binding: &str,
        realization: Option<&crate::session_resource::SessionRealizationLease>,
    ) -> awaken_session_state_contract::SessionEnvironmentReceipt {
        awaken_session_state_contract::SessionEnvironmentReceipt::new(
            session_id,
            awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
            binding,
            realization.cloned(),
        )
        .for_environment("env-7")
    }
    let repo = authority_repository("session repository").await;
    let current = crate::session_resource::SessionRealizationLease {
        owner: "runtime-a".into(),
        runtime_incarnation: "runtime-a/boot-1".into(),
        epoch: 3,
        expires_at_unix_ms: u64::MAX,
    };
    let id = SessionId::from("binding-first-fresh");
    let session = first_materialization::fresh_subject("binding-first-fresh");
    create(repo.as_ref(), session).await;
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(200));
    // B12 the selected faulting repository and clock prepare one sink before
    // Runtime/App construction. The application retains that same Arc; an
    // unrelated empty repository detects any hidden or stale source selection.
    let original_repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let faulting = Arc::new(FaultingSessionRepository::new(repo.clone()));
    let persistence = crate::SessionPersistence::new(faulting.clone(), clock.clone());
    let sink = persistence.environment_binding_sink();
    let app = SessionApplicationBuilder::fixture(
        Arc::new(RecordingCleanupRuntime::default()),
        Arc::new(NoopMcpRealizer),
        persistence,
        Arc::new(RecordingEnvironmentSource::default()),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .expect("valid fixture Session application");
    assert!(
        Arc::ptr_eq(&sink, &app.environment_binding_sink()),
        "B12 one selected sink"
    );
    first_materialization::claim_subject(&app, repo.as_ref(), "binding-first-fresh", &current)
        .await;
    let first = receipt("binding-first-fresh", "sandbox-a", Some(&current));
    let first_intent = first.effect_intent().expect("B1 intent");
    assert_eq!(
        sink.authorize(&first_intent).await.expect("B1 authorize"),
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized {
            intent: Box::new(first_intent.clone())
        },
        "B1"
    );
    let before_binding = repo.get(&id).await.unwrap();
    // B13 the CAS commits but its acknowledgement is lost. Retry must read back
    // exactly that root: one revision and the original observation survive.
    faulting.commit_then_conflict_once("bind-environment");
    sink.persist(first).await.expect("B1");
    assert!(
        faulting.conflict_operation_once.lock().unwrap().is_none(),
        "B13 fault consumed"
    );
    assert!(
        original_repo.get(&id).await.is_err(),
        "B12 original repository untouched"
    );
    // B9 new binding advances the existing root observation at the same CAS;
    // B10 replay after clock advance changes neither the root nor its time.
    let bound = repo.get(&id).await.unwrap();
    assert_eq!(
        bound.revision.0,
        before_binding.revision.0 + 1,
        "B13 one committed revision"
    );
    assert_eq!(bound.created_at_unix_ms, Some(100), "B9");
    assert_eq!(bound.updated_at_unix_ms, Some(200), "B9");
    super::observation_refinement::root_step("binding-apply", &before_binding, &bound, 200);
    clock.set(300);
    assert_eq!(
        sink.authorize(&first_intent).await.expect("B2 authorize"),
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
            binding: "sandbox-a".into(),
            intent: Box::new(first_intent.clone()),
        },
        "B2"
    );
    sink.persist(receipt("binding-first-fresh", "sandbox-a", Some(&current)))
        .await
        .expect("B2");
    let replayed = repo.get(&id).await.unwrap();
    assert_eq!(replayed, bound, "B10 exact replay");
    super::observation_refinement::root_step("binding-replay", &bound, &replayed, 300);
    let mut renewed_session = persisted("binding-renewed", false, "idle");
    renewed_session.realization = Some(crate::session_resource::SessionRealizationLease {
        expires_at_unix_ms: u64::MAX,
        ..current.clone()
    });
    create(repo.as_ref(), renewed_session).await;
    let admitted_before_renewal = crate::session_resource::SessionRealizationLease {
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
    let stale = crate::session_resource::SessionRealizationLease {
        owner: "runtime-b".into(),
        runtime_incarnation: "runtime-b/boot-1".into(),
        epoch: 4,
        expires_at_unix_ms: u64::MAX,
    };
    let revision_before_stale_replay = repo.get(&id).await.expect("B4").revision;
    let stale_error = sink
        .persist(receipt("binding-first-fresh", "sandbox-a", Some(&stale)))
        .await
        .expect_err("B4 stale equal-binding replay is fenced");
    assert_eq!(
        stale_error.kind,
        awaken_session_types::RunErrorKind::Unavailable,
        "B4"
    );
    assert_eq!(stale_error.code, "session_realization_stale", "B4");
    let rejected = repo.get(&id).await.unwrap();
    assert_eq!(rejected, bound, "B11 stale authority preserves full root");
    super::observation_refinement::root_step("binding-reject", &bound, &rejected, 300);
    assert_eq!(
        repo.get(&id).await.expect("B4").revision,
        revision_before_stale_replay,
        "B4 must not write"
    );
    let error = sink
        .persist(receipt("binding-first-fresh", "sandbox-b", Some(&stale)))
        .await
        .expect_err("B5 stale binding replacement is fenced");
    assert_eq!(
        error.kind,
        awaken_session_types::RunErrorKind::Unavailable,
        "B5"
    );
    assert_eq!(error.code, "session_realization_stale", "B5");
    let expired = crate::session_resource::SessionRealizationLease {
        expires_at_unix_ms: 0,
        ..current.clone()
    };
    let mut expired_session = repo.get(&id).await.expect("B6 fixture");
    expired_session.realization = Some(expired.clone());
    let expected_revision = expired_session.revision;
    let payload = awaken_session_state_contract::SessionMutationPayload::Replace(expired_session);
    let payload_hash = payload.stable_hash();
    assert!(matches!(
        repo.commit(
            "workspace",
            awaken_session_state_contract::SessionMutation {
                expected_revision,
                idempotency: awaken_session_state_contract::IdempotencyRecord {
                    key: "binding-fence:expire".into(),
                    payload_hash,
                },
                payload,
                lifecycle_facts: Vec::new(),
            },
        )
        .await
        .expect("B7 fixture mutation"),
        awaken_session_state_contract::SessionMutationResult::Applied { .. }
    ));
    let expired_error = sink
        .persist(receipt("binding-first-fresh", "sandbox-a", Some(&expired)))
        .await
        .expect_err("B7 expired equal-binding replay is fenced");
    assert_eq!(
        expired_error.kind,
        awaken_session_types::RunErrorKind::Unavailable,
        "B7"
    );
    assert_eq!(expired_error.code, "session_realization_stale", "B7");
    assert_eq!(
        sink.persist(receipt("binding-first-fresh", "sandbox-b", Some(&expired)))
            .await
            .expect_err("B8")
            .code,
        "session_realization_stale",
        "B8"
    );
    assert_eq!(
        repo.get(&id)
            .await
            .expect("binding-fence Session")
            .environment
            .binding()
            .map(str::to_owned)
            .as_deref(),
        Some("sandbox-a"),
        "B4/B5/B7/B8"
    );
    // B14 causes: prior observation absent/equal/ahead of the clock; effect:
    // successful binding stamps max(old, now), preserves legacy creation, and
    // advances one revision. Same authority/binding subsequently replays whole-root.
    let mut clock_cases = Vec::new();
    for (index, previous, now, expected) in [
        (0, None, 50, 50),
        (1, Some(100), 100, 100),
        (2, Some(200), 100, 200),
    ] {
        let id = format!("binding-clock-{index}");
        let mut root = persisted(&id, false, "idle");
        root.created_at_unix_ms = None;
        root.updated_at_unix_ms = previous;
        create(repo.as_ref(), root).await;
        let before = repo.get(&SessionId::from(&id)).await.unwrap();
        clock.set(now);
        sink.persist(receipt(&id, "sandbox-clock", None))
            .await
            .unwrap();
        let after = repo.get(&SessionId::from(&id)).await.unwrap();
        assert_eq!(
            after.created_at_unix_ms, None,
            "B14 legacy creation remains unknown"
        );
        assert_eq!(after.updated_at_unix_ms, Some(expected), "B14");
        assert_eq!(after.revision.0, before.revision.0 + 1, "B14");
        clock_cases.push(super::observation_refinement::root_observation(
            Some(&before),
            Some(&after),
            now,
            ["legacy", "equal", "backward"][index],
        ));
        clock.set(now + 1);
        sink.persist(receipt(&id, "sandbox-clock", None))
            .await
            .unwrap();
        assert_eq!(
            repo.get(&SessionId::from(id)).await.unwrap(),
            after,
            "B14 exact replay"
        );
    }
    awaken_reliability_testkit::write_observation_trace(
        "binding-clock-cases",
        "root",
        serde_json::json!(clock_cases),
    );
    let absent = receipt("binding-missing", "sandbox", Some(&current))
        .effect_intent()
        .expect("missing intent");
    assert_eq!(
        sink.authorize(&absent).await.unwrap_err().code,
        "session_environment_root_missing",
        "B17 known lease cannot downgrade to Unowned"
    );
    let direct = receipt("binding-missing-direct", "sandbox", None)
        .effect_intent()
        .unwrap();
    assert_eq!(
        sink.authorize(&direct).await.unwrap(),
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned {
            intent: Box::new(direct.clone())
        },
        "B18 direct missing root retains only its request value"
    );
}

/// In-flight renewal cause/effect graph: C1 one MCP generation is Realizing
/// under an admitted lease; C2 the same owner/incarnation/epoch is durably
/// extended before its Stage receipt returns. E1 accepts the old exact
/// receipt under the stable realization binding; E2 activates durably;
/// E3 publishes the exact completed receipt under current root authority,
/// without restaging or another root write on replay.
/// Different owner/epoch is A2 in the contract table; conflicting receipt
/// bindings remain rejected by the canonical receipt verification gate.
///
/// | Rule | C1 | C2 | Effect |
/// |---|---|---|---|
/// | F1 | yes | yes | E1 + E2 + E3; direct Publish despite renewal |
/// | F2 | yes | no | E1 + E3; publish directly, no redundant Stage |
/// Resource crossover causes: C3 MCP was already Active/unacknowledged when
/// heartbeat renewed it; C4 Resource 2 is active and 3 is pending; C5 the Stage
/// reports Some(3), None, or a mismatched prepared revision. E4 commits
/// precisely prepared generation 3 instead of rolling Runtime back to 2;
/// E5 an unprepared pending mutation remains pending; E6 reject a mismatched
/// preparation without a root write, even during renewal.
/// | F3 | C2+C3+C4 + Some(3) | E4; no repeated Resource or MCP preparation |
/// | F4 | C2+C3+C4 + None | E5, use active 2; no new Resource authority |
/// | F5 | C4 + mismatched Some | E6, with and without C2/C3 |
/// | F6 | !C3+C4 + Some(3) | commit 3 before Publish; never regress |
/// | F7 | C4 + Some(2) | rebuild retained active 2; leave pending 3 untouched |
/// F5 means neither active nor pending, not merely different from pending.
/// Cross F5/F7 with renewal and MCP state to preserve both independent fences.
#[tokio::test]
async fn activation_preserves_completed_receipts_during_same_generation_renewal() {
    for (renewal, already_active, pending_resources, prepared_resource_revision) in [
        (false, false, false, None),
        (true, false, false, None),
        (true, true, true, Some(3)),
        (false, true, true, Some(3)),
        (true, false, true, Some(3)),
        (false, false, true, Some(3)),
        (true, true, true, None),
        (true, false, true, None),
        (true, true, true, Some(2)),
        (true, true, true, Some(4)),
        (false, true, true, Some(2)),
        (true, false, true, Some(2)),
        (false, false, true, Some(2)),
        (false, true, true, Some(4)),
        (true, false, true, Some(4)),
        (false, false, true, Some(4)),
        (true, true, true, Some(1)),
        (true, false, true, Some(1)),
        (false, true, true, Some(1)),
        (false, false, true, Some(1)),
    ] {
        let repo = authority_repository("session repository").await;
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        let admitted_expiry = now + 60_000;
        let renewed_expiry = if renewal {
            now + 120_000
        } else {
            admitted_expiry
        };
        let asserted_lease = crate::session_resource::SessionRealizationLease {
            owner: "runtime-a".into(),
            runtime_incarnation: "runtime-a/boot-1".into(),
            epoch: 7,
            expires_at_unix_ms: admitted_expiry,
        };
        let current_lease = crate::session_resource::SessionRealizationLease {
            expires_at_unix_ms: renewed_expiry,
            ..asserted_lease.clone()
        };
        let mut session = persisted("in-flight-renewal", false, "activating");
        session.realization = Some(current_lease.clone());
        session.mcp = crate::session_mcp::SessionMcpAttachmentSet::from_initial(
            vec![crate::session_mcp::McpAttachmentDraft {
                name: "browser".into(),
                target: McpTarget::parse_http("https://browser.example.test/mcp").unwrap(),
                prompts_as_skills: false,
                credential: None,
                origin: crate::session_mcp::McpAttachmentOrigin::Session,
            }],
            None,
        )
        .unwrap();
        let generation = session.mcp.attachments[0].generation.clone();
        session
            .mcp
            .claim_realization(
                &generation,
                crate::session_mcp::McpRealizationClaim {
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
        if already_active {
            session.mcp.activate(&generation, "realization-1").unwrap();
            session
                .mcp
                .extend_active_realization_leases(
                    &current_lease.runtime_incarnation,
                    current_lease.epoch,
                    renewed_expiry,
                )
                .unwrap();
        }
        let active_resources = file_resources("resource-2");
        let desired_resources = file_resources("resource-3");
        if pending_resources {
            session.resources = awaken_session_state_contract::SessionResourceState::from_active(
                file_resources("resource-1"),
            );
            session
                .resources
                .prepare(&session.session_id, active_resources.clone())
                .unwrap();
            session.resources.commit().unwrap();
            session
                .resources
                .prepare(&session.session_id, desired_resources.clone())
                .unwrap();
            session.resources.start_attempt().unwrap();
        }
        create(repo.as_ref(), session).await;
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let admitted_receipt = crate::session_mcp::McpRealizationReceipt {
            receipt_fingerprint: admitted_request.fingerprint(),
            generation: admitted_request.generation.clone(),
            realization_id: admitted_request.realization_id.clone(),
            selected_plaintext_holder: admitted_request.selected_plaintext_holder.clone(),
            actual_realization_kind: None,
        };

        let before = repo
            .get(&SessionId::from("in-flight-renewal"))
            .await
            .unwrap();
        let command = awaken_session_realization_contract::ActivateSessionRealization {
            session_id: "in-flight-renewal".into(),
            lease: asserted_lease.clone(),
            prepared_resource_revision,
            mcp_receipts: vec![admitted_receipt],
        };
        let result =
            awaken_session_realization_contract::SessionRealizationControl::activate_session_realization(
                &app,
                command.clone(),
            )
            .await;
        if pending_resources
            && prepared_resource_revision.is_some_and(|revision| revision != 2 && revision != 3)
        {
            assert!(
                matches!(
                    result,
                    Err(awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_))
                ),
                "F5/E6 reject mismatched preparation before catch-up"
            );
            assert_eq!(
                repo.get(&SessionId::from("in-flight-renewal"))
                    .await
                    .unwrap(),
                before,
                "F5/E6 no root mutation"
            );
            continue;
        }
        let directive = result.expect("F1/E1");
        if pending_resources {
            let (expected_revision, expected_resources) = if prepared_resource_revision == Some(3) {
                (3, &desired_resources)
            } else {
                (2, &active_resources)
            };
            assert_eq!(
                directive.projection.resource_revision, expected_revision,
                "F3/F4/F6 preserve the exact prepared or active pair"
            );
            assert_eq!(
                &directive.projection.resources, expected_resources,
                "F3/F4/F6 manifest and revision agree"
            );
        }
        let revision = repo
            .get(&SessionId::from("in-flight-renewal"))
            .await
            .unwrap()
            .revision;
        // F1/F2 replay preserves phase, projection and durable lease, while
        // each response independently samples its remaining authority horizon.
        assert!(
            awaken_session_realization_contract::SessionRealizationControl::activate_session_realization(
                &app, command
            )
            .await
            .unwrap()
            .same_progress_as(&directive),
            "F1/F2 response-loss replay preserves the exact physical receipt"
        );
        assert_eq!(
            repo.get(&SessionId::from("in-flight-renewal"))
                .await
                .unwrap()
                .revision,
            revision,
            "F1/F2 no duplicate activation write"
        );
        let awaken_session_realization_contract::SessionRealizationAction::Publish {
            publish,
            drain,
        } = directive.action
        else {
            panic!("F1/E3 and F2/E3 publish only an exact staged generation")
        };
        assert!(drain.is_empty(), "F1/E2 does not invent an obsolete drain");
        assert_eq!(publish.len(), 1, "F1/E2 publishes the one admitted claim");
        let renewed_generation = &publish[0];
        assert_eq!(
            renewed_generation.attachment_id(),
            admitted_request.generation.attachment_id(),
            "F1/E2 preserves the admitted attachment"
        );
        assert_eq!(
            renewed_generation.tenure(),
            admitted_request.generation.tenure(),
            "F1/E2 preserves the admitted Session, generation, runtime incarnation and lease epoch"
        );
        assert_eq!(
            renewed_generation.lease_expires_at_unix_ms(),
            admitted_expiry,
            "F1/E2 publishes the exact staged receipt, not a fabricated renewed receipt"
        );
        if pending_resources {
            let resources = repo
                .get(&SessionId::from("in-flight-renewal"))
                .await
                .unwrap()
                .resources;
            if prepared_resource_revision == Some(3) {
                assert_eq!(
                    resources.active_generation(),
                    (3, &desired_resources),
                    "F3/F6 exact generation committed"
                );
                assert!(
                    resources.pending.is_none(),
                    "F3/F6 no pending mutation remains"
                );
            } else {
                assert_eq!(
                    resources.active_generation(),
                    (2, &active_resources),
                    "F4 active generation unchanged"
                );
                assert_eq!(
                    resources.pending.as_ref(),
                    Some(&desired_resources),
                    "F4 pending intent is not consumed by renewal"
                );
            }
        }
    }
}

#[tokio::test]
async fn recovery_replacement_partitions_staged_receipts_and_replays_activation() {
    use awaken_session_configuration_contract::mcp::{
        McpAttachmentDraft, McpAttachmentOrigin, McpAttachmentState, McpRealizationClaim,
        McpRealizationReceipt, SessionMcpAttachmentSet,
    };
    use awaken_session_configuration_contract::resource::SessionRealizationLease;
    use awaken_session_realization_contract::{
        AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
        RenewSessionRealization, SessionRealizationAction, SessionRealizationControl,
        SessionRealizationControlFailure, SessionRealizationTarget,
    };
    // Cause/effect graph: C1 hot owner versus restarted owner; C2 renewal
    // between Stage and activation; C3 activation response loss/replay;
    // C4 valid versus corrupted recovered-old receipt. The full replacement
    // changes one name, removes another, and retains a third in the same CAS.
    // E1 every required Stage receipt is checked, even if it will only drain;
    // E2 Publish contains precisely still-Active staged receipts with their
    // original expiry; E3 superseded/removed names drain, never publish;
    // E4 exact replay stutters durably; E5 strict acknowledgement reaches Idle.
    //
    // | Rule | owner | renewal | receipt | response | effects |
    // | R1 | hot | no/yes | exact | delivered/lost | E1-E5; only new name publishes |
    // | R2 | restart | no/yes | exact | delivered/lost | E1-E5; retained name also publishes |
    // | R3 | restart | no/yes | corrupted old | any | reject before root write |
    // | R4 | either | no/yes | foreign/duplicate/bad fingerprint/incarnation/future expiry | replay | reject before root write |
    // C3 follows every R1/R2 activation; a heartbeat before activation checks
    // that neither a current expiry nor a drained generation fabricates Stage.
    for recovery in [false, true] {
        for renewal in [false, true] {
            let repo = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                    .await
                    .unwrap(),
            );
            let app = application(
                repo.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            );
            let expiry = awaken_time::system_clock().now().as_epoch_ms() + 60_000;
            let lease = SessionRealizationLease {
                owner: "owner".into(),
                runtime_incarnation: "old-boot".into(),
                epoch: 1,
                expires_at_unix_ms: expiry,
            };
            let draft = |name: &str, version: &str| McpAttachmentDraft {
                name: name.into(),
                target: McpTarget::parse_http(format!("https://{name}.example.test/{version}"))
                    .unwrap(),
                prompts_as_skills: false,
                credential: None,
                origin: McpAttachmentOrigin::Session,
            };
            let mut session = persisted("recovery-replacement", false, "idle");
            session.realization = Some(lease.clone());
            session.mcp = SessionMcpAttachmentSet::from_initial(
                vec![
                    draft("replace", "v1"),
                    draft("remove", "v1"),
                    draft("keep", "v1"),
                ],
                None,
            )
            .unwrap();
            for attachment in session.mcp.attachments.clone() {
                let generation = attachment.generation;
                let realization_id = format!("old-{}", attachment.name);
                session
                    .mcp
                    .claim_realization(
                        &generation,
                        McpRealizationClaim {
                            realization_id: realization_id.clone(),
                            runtime_incarnation: lease.runtime_incarnation.clone(),
                            lease_epoch: lease.epoch,
                            lease_expires_at_unix_ms: expiry,
                            stage_idempotency_key: realization_id.clone(),
                        },
                    )
                    .unwrap();
                session.mcp.activate(&generation, &realization_id).unwrap();
                session
                    .mcp
                    .acknowledge_publication(&generation, &realization_id)
                    .unwrap();
            }
            session
                .mcp
                .request_full_replacement(vec![draft("replace", "v2"), draft("keep", "v1")], None)
                .unwrap();
            create(repo.as_ref(), session).await;
            let staged = app
                .begin_session_realization(BeginSessionRealization {
                    session_id: "recovery-replacement".into(),
                    expected_resources: None,
                    target: SessionRealizationTarget {
                        owner: lease.owner.clone(),
                        runtime_incarnation: if recovery {
                            "new-boot".into()
                        } else {
                            lease.runtime_incarnation.clone()
                        },
                        lease_expires_at_unix_ms: expiry,
                        reassign_existing_lease: false,
                    },
                })
                .await
                .unwrap();
            let SessionRealizationAction::Stage { mcp_stages, .. } = &staged.action else {
                panic!("R1/R2 stage");
            };
            assert_eq!(mcp_stages.len(), if recovery { 4 } else { 1 });
            let receipts = mcp_stages
                .iter()
                .map(|request| McpRealizationReceipt {
                    generation: request.generation.clone(),
                    realization_id: request.realization_id.clone(),
                    selected_plaintext_holder: request.selected_plaintext_holder.clone(),
                    actual_realization_kind: None,
                    receipt_fingerprint: request.fingerprint(),
                })
                .collect::<Vec<_>>();
            let command = ActivateSessionRealization {
                session_id: "recovery-replacement".into(),
                lease: staged.authority.lease.clone(),
                prepared_resource_revision: None,
                mcp_receipts: receipts.clone(),
            };
            if renewal {
                app.renew_session_realization(RenewSessionRealization {
                    session_id: command.session_id.clone(),
                    asserted_lease: command.lease.clone(),
                    requested_expires_at_unix_ms: expiry + 60_000,
                })
                .await
                .unwrap();
            }
            if recovery {
                let before = repo.get(&command.session_id).await.unwrap();
                let mut corrupt = command.clone();
                corrupt.mcp_receipts[0]
                    .receipt_fingerprint
                    .push_str("-corrupt");
                assert!(
                    matches!(
                        app.activate_session_realization(corrupt).await,
                        Err(SessionRealizationControlFailure::Invalid(_))
                    ),
                    "R3 old receipt is not ignored"
                );
                assert_eq!(
                    repo.get(&command.session_id).await.unwrap(),
                    before,
                    "R3 no write"
                );
            }
            let activated = app
                .activate_session_realization(command.clone())
                .await
                .unwrap();
            let SessionRealizationAction::Publish { publish, drain } = &activated.action else {
                panic!("R1/R2 publish");
            };
            assert_eq!(
                publish.len(),
                if recovery { 2 } else { 1 },
                "E2 no obsolete publication"
            );
            assert_eq!(drain.len(), 2, "E3 replaced and removed names drain");
            let after = repo.get(&command.session_id).await.unwrap();
            for generation in publish {
                assert!(
                    receipts
                        .iter()
                        .any(|receipt| &receipt.generation == generation),
                    "E2 exact physical receipt"
                );
                assert_eq!(
                    generation.lease_expires_at_unix_ms(),
                    expiry,
                    "E2 no fabricated renewed receipt"
                );
                assert!(
                    after
                        .mcp
                        .attachments
                        .iter()
                        .any(|a| &a.generation == generation.generation()
                            && a.state == McpAttachmentState::Active),
                    "E2 active only"
                );
                assert!(
                    !drain
                        .iter()
                        .any(|old| old.generation() == generation.generation()),
                    "E3 disjoint sets"
                );
            }
            let replay = app
                .activate_session_realization(command.clone())
                .await
                .unwrap();
            assert!(
                replay.same_progress_as(&activated),
                "E4 response-loss replay"
            );
            assert_eq!(
                repo.get(&command.session_id).await.unwrap(),
                after,
                "E4 no replay write"
            );
            // In R2 the first receipt is now Draining: accepting its presence
            // on replay must not bypass any canonical receipt verification.
            for fault in [
                "foreign",
                "duplicate",
                "fingerprint",
                "incarnation",
                "future",
            ] {
                let mut invalid = command.clone();
                match fault {
                    "foreign" => {
                        let mut extra = invalid.mcp_receipts[0].clone();
                        let mut w = extra.generation.to_wire();
                        w.generation = w.generation.at_version(99);
                        extra.generation = w.try_into().unwrap();
                        invalid.mcp_receipts.push(extra);
                    }
                    "duplicate" => invalid.mcp_receipts.push(invalid.mcp_receipts[0].clone()),
                    "fingerprint" => invalid.mcp_receipts[0]
                        .receipt_fingerprint
                        .push_str("-corrupt"),
                    "incarnation" | "future" => {
                        let mut w = invalid.mcp_receipts[0].generation.to_wire();
                        match fault {
                            "incarnation" => w.runtime_incarnation.push_str("-foreign"),
                            _ => w.lease_expires_at_unix_ms = u64::MAX,
                        }
                        invalid.mcp_receipts[0].generation = w.try_into().unwrap();
                    }
                    _ => unreachable!(),
                }
                assert!(
                    matches!(
                        app.activate_session_realization(invalid).await,
                        Err(SessionRealizationControlFailure::Invalid(_))
                    ),
                    "R4 reject {fault}"
                );
                assert_eq!(
                    repo.get(&command.session_id).await.unwrap(),
                    after,
                    "R4 {fault} no write"
                );
            }
            let complete = app
                .acknowledge_session_realization(AcknowledgeSessionRealization {
                    session_id: command.session_id.clone(),
                    lease: activated.authority.lease,
                    published: publish.clone(),
                    drained: drain.clone(),
                })
                .await
                .unwrap();
            assert!(
                matches!(complete.action, SessionRealizationAction::Complete),
                "E5 convergence"
            );
            let complete = repo.get(&command.session_id).await.unwrap();
            assert_eq!(complete.execution, SessionExecutionState::Idle);
            assert!(
                complete
                    .mcp
                    .attachments
                    .iter()
                    .all(|a| a.state == McpAttachmentState::Removed
                        || (a.state == McpAttachmentState::Active && a.publication_acknowledged)),
                "E5 every generation is published or durably drained"
            );
        }
    }
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
