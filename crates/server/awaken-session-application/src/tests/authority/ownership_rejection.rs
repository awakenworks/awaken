//! T06-C01 (归属拒绝): substitute the owner (Workspace) of a Session addressed
//! through every owner-scoped application entrypoint.
//!
//! Oracles (groups-1.json order):
//! - O0 reject before any unauthorized data read (`get`) or physical effect;
//! - O1 the error does not reveal whether another tenant's Session exists
//!   (foreign rejection is byte-equal to the missing-Session rejection).
//!
//! Negative control: validating ownership only at the HTTP layer while the
//! internal entrypoint trusts the caller must fail. It is executed by
//! re-running the SAME matrix against a repository whose `owner()` answers the
//! caller's own scope (what an HTTP-only check would hand the internals), and
//! asserting that the matrix checker rejects at least one row.
//!
//! | Row | Entrypoint | Foreign result | Missing result |
//! |---|---|---|---|
//! | E1 | `admit_session_run_for_owner` | `Session was not found` | create-on-missing (see reproducer) |
//! | E2 | `resume_session_run_for_owner` | `Session was not found` | identical |
//! | E3 | `archive_with_repository_publication` | `NotFound` | identical |
//! | E4 | `close_session_event_admission` | `session_not_found` | identical |
//! | E5 | `session_file_is_frozen_for_worker` | `false` | identical |
use super::*;
use crate::tests::effect_counting_runtime::EffectCountingRuntime;
use crate::{SessionArchiveWithRepositoryPublicationCommand, SessionRepositoryPublicationSelector};
use awaken_session_realization_contract::SessionFileReadAuthorization as _;

const INTRUDER: &str = "intruder-workspace";
const OWNED: &str = "t06-owned-session";
const MISSING: &str = "t06-missing-session";

/// HTTP-only validation mutant: the internal entrypoint is handed whatever
/// scope the outer layer authenticated, so `owner()` of an existing Session
/// answers the caller instead of the durable owner.
struct CallerScopedOwnerRepository {
    inner: Arc<FaultingSessionRepository>,
    caller: String,
}

#[async_trait::async_trait]
impl awaken_session_repository_contract::SessionRepositoryReads for CallerScopedOwnerRepository {
    async fn get(
        &self,
        session_id: &SessionId,
    ) -> Result<PersistedSession, awaken_session_state_contract::SessionRepositoryError> {
        self.inner.get(session_id).await
    }

    async fn owner(
        &self,
        session_id: &SessionId,
    ) -> Result<String, awaken_session_state_contract::SessionRepositoryError> {
        self.inner
            .owner(session_id)
            .await
            .map(|_| self.caller.clone())
    }

    async fn scan(
        &self,
        query: awaken_session_state_contract::SessionScanQuery<'_>,
    ) -> Result<
        awaken_session_state_contract::SessionScan,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        self.inner.scan(query).await
    }

    async fn deleted_event_admissions(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<
        Option<awaken_session_state_contract::DeletedSessionEventAdmissions>,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        self.inner
            .deleted_event_admissions(owner_scope, session_id)
            .await
    }
}

#[async_trait::async_trait]
impl SessionRepository for CallerScopedOwnerRepository {
    async fn create(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: awaken_session_state_contract::IdempotencyRecord,
        lifecycle_facts: Vec<awaken_session_state_contract::ManagedLifecycleFact>,
    ) -> Result<
        awaken_session_state_contract::SessionCreateResult,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        self.inner
            .create(owner_scope, session, idempotency, lifecycle_facts)
            .await
    }

    async fn replay_create(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        idempotency: &awaken_session_state_contract::IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, awaken_session_state_contract::SessionRepositoryError>
    {
        self.inner
            .replay_create(owner_scope, session_id, idempotency)
            .await
    }

    async fn commit(
        &self,
        owner_scope: &str,
        mutation: awaken_session_state_contract::SessionMutation,
    ) -> Result<
        awaken_session_state_contract::SessionMutationResult,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        self.inner.commit(owner_scope, mutation).await
    }

    async fn reconciliation_scan(
        &self,
        after: Option<&awaken_session_state_contract::SessionScanCursor>,
    ) -> Result<
        awaken_session_state_contract::SessionScan,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        self.inner.reconciliation_scan(after).await
    }

    async fn idempotency_receipt(
        &self,
        session_id: &SessionId,
        key: &str,
    ) -> Result<
        Option<awaken_session_state_contract::SessionIdempotencyReceipt>,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        self.inner.idempotency_receipt(session_id, key).await
    }

    async fn count_environment_phase(
        &self,
        phase: awaken_session_state_contract::SessionEnvironmentPhase,
    ) -> Result<u64, awaken_session_state_contract::SessionRepositoryError> {
        self.inner.count_environment_phase(phase).await
    }
}

#[async_trait::async_trait]
impl awaken_session_repository_contract::LifecycleOutbox for CallerScopedOwnerRepository {
    async fn append_lifecycle(
        &self,
        fact: awaken_session_state_contract::ManagedLifecycleFact,
    ) -> Result<(), awaken_session_state_contract::SessionRepositoryError> {
        self.inner.append_lifecycle(fact).await
    }

    async fn pending_lifecycle(
        &self,
    ) -> Result<
        Vec<awaken_session_state_contract::ManagedLifecycleFact>,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        self.inner.pending_lifecycle().await
    }

    async fn complete_lifecycle(
        &self,
        fact_id: &str,
    ) -> Result<(), awaken_session_state_contract::SessionRepositoryError> {
        self.inner.complete_lifecycle(fact_id).await
    }
}

struct OwnershipFixture {
    durable: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    faulting: Arc<FaultingSessionRepository>,
    runtime: Arc<EffectCountingRuntime>,
    app: SessionApplication,
}

/// Environment projection that lets create-on-missing complete: the shared
/// `RecordingEnvironmentSource` treats any creation-time resolution as
/// unreachable. Every creation resolution is counted, so the reproducer can
/// observe which ids reach the creation path.
#[derive(Default)]
struct CreationEnvironment {
    resolutions: AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_executable_environment_contract::SessionEnvironmentProjectionSource
    for CreationEnvironment
{
    type Snapshot = awaken_session_configuration_contract::EnvironmentSnapshot;

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
        _mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        let mut snapshot = persisted("t06-environment-template", false, "idle")
            .frozen_baseline()
            .expect("fixture baseline")
            .environment
            .clone();
        environment_id.clone_into(&mut snapshot.environment_id);
        Ok(Some(ResolvedSessionEnvironment { snapshot }))
    }

    async fn resolve_exact_for_session(
        &self,
        environment_id: &str,
        _revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
        self.resolve_current_for_session(environment_id, runtime, mcp_targets)
            .await
    }
}

async fn ownership_fixture(http_only_mutant: bool) -> OwnershipFixture {
    let recording = Arc::new(RecordingEnvironmentSource::default());
    ownership_fixture_with(http_only_mutant, recording.into()).await
}

async fn ownership_fixture_with(
    http_only_mutant: bool,
    environments: crate::SessionEnvironments,
) -> OwnershipFixture {
    let durable = authority_repository("T06 Session store").await;
    create(durable.as_ref(), persisted(OWNED, false, "idle")).await;
    let faulting = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let store: Arc<dyn awaken_session_repository_contract::SessionStore> = if http_only_mutant {
        Arc::new(CallerScopedOwnerRepository {
            inner: faulting.clone(),
            caller: INTRUDER.into(),
        })
    } else {
        faulting.clone()
    };
    let runtime = Arc::new(EffectCountingRuntime::default());
    let app = application_with_runtime(runtime.clone(), store, environments);
    OwnershipFixture {
        durable,
        faulting,
        runtime,
        app,
    }
}

fn admit_command(session: &str) -> awaken_session_execution_contract::AdmitSessionRun {
    let session_id = SessionId::from(session);
    awaken_session_execution_contract::AdmitSessionRun {
        session_id: session_id.clone(),
        agent_id: "agent".into(),
        operation_id: "t06-operation".into(),
        run_id: awaken_session_execution_contract::session_run_id(&session_id, "t06-operation"),
        messages: Vec::new(),
        data_subject_id: None,
        traceparent: None,
        execution_requirements: Default::default(),
        replacement: Default::default(),
    }
}

fn archive_command(session: &str) -> SessionArchiveWithRepositoryPublicationCommand {
    SessionArchiveWithRepositoryPublicationCommand::new(
        INTRUDER.into(),
        SessionId::from(session),
        SessionRepositoryPublicationSelector {
            binding_id: awaken_resource_contract::BindingId::from("source"),
            expectation: awaken_provisioning_contract::RepositoryPublicationExpectation {
                branch: "awf/t06".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                expected_prior_commit: None,
            },
        },
        100,
    )
}

/// Execute one owner-scoped entrypoint as the intruder and render its full
/// observable answer (variant, message, kind and code) as bytes.
async fn invoke(row: &str, app: &SessionApplication, session: &str) -> String {
    let session_id = SessionId::from(session);
    match row {
        "E1" => format!(
            "{:?}",
            app.run_admissions
                .admit_session_run_for_owner(INTRUDER, admit_command(session))
                .await
                .map(|_| ())
        ),
        "E2" => format!(
            "{:?}",
            app.coordination
                .resume_session_run_for_owner(
                    INTRUDER,
                    "t06-resume",
                    &session_id,
                    "t06-tool-use",
                    awaken_session_application_contract::RunResume::Permission(
                        awaken_agent_contract::agent::awaiting::PermissionDecision::Allow {
                            note: None
                        },
                    ),
                )
                .await
                .map(|_| ())
        ),
        "E3" => format!(
            "{:?}",
            app.termination_commands
                .archive_with_repository_publication(archive_command(session))
                .await
                .map(|_| ())
        ),
        "E4" => format!(
            "{:?}",
            app.event_batches
                .close_session_event_admission(
                    INTRUDER,
                    &session_id,
                    "t06-close".into(),
                    "t06-close-fingerprint".into(),
                )
                .await
                .map(|_| ())
        ),
        "E5" => format!(
            "{:?}",
            app.queries()
                .session_file_is_frozen_for_worker(
                    &session_id,
                    INTRUDER,
                    &awaken_worker_contract::WorkerIdentity::new("worker", "incarnation", 1),
                    "file",
                    100,
                )
                .await
        ),
        _ => unreachable!("unknown T06 row {row}"),
    }
}

/// The oracle checker shared by the real application and the mutant.
async fn check_ownership_matrix(
    fixture: &OwnershipFixture,
) -> Vec<(&'static str, Result<(), String>)> {
    let owned = SessionId::from(OWNED);
    let mut results = Vec::new();
    for row in ["E1", "E2", "E3", "E4", "E5"] {
        let root_before = fixture.durable.get(&owned).await.unwrap();
        let outbox_before =
            serde_json::to_value(fixture.durable.pending_lifecycle().await.unwrap()).unwrap();
        let effects_before = fixture.runtime.counts();
        fixture.faulting.observed_gets.lock().unwrap().clear();
        let foreign = invoke(row, &fixture.app, OWNED).await;
        let foreign_reads = fixture.faulting.observed_gets.lock().unwrap().clone();
        let mut violations = Vec::new();
        if !foreign_reads.is_empty() {
            violations.push(format!("O0 foreign request read roots {foreign_reads:?}"));
        }
        if fixture.runtime.counts() != effects_before {
            violations.push(format!(
                "O0 foreign request reached Runtime effects {:?}",
                fixture.runtime.counts()
            ));
        }
        if fixture.durable.get(&owned).await.unwrap() != root_before {
            violations.push("O0 foreign request changed the root".into());
        }
        if serde_json::to_value(fixture.durable.pending_lifecycle().await.unwrap()).unwrap()
            != outbox_before
        {
            violations.push("O0 foreign request changed the outbox".into());
        }
        // E1 create-on-missing is the separately reproduced existence gap.
        if row != "E1" {
            let missing = invoke(row, &fixture.app, MISSING).await;
            if foreign != missing {
                violations.push(format!(
                    "O1 foreign answer {foreign} differs from missing answer {missing}"
                ));
            }
        } else if !foreign.contains("Session was not found") {
            violations.push(format!("O1 E1 foreign answer leaks: {foreign}"));
        }
        results.push((
            row,
            if violations.is_empty() {
                Ok(())
            } else {
                Err(violations.join("; "))
            },
        ));
    }
    results
}

#[tokio::test]
async fn foreign_owner_is_rejected_before_reads_or_effects_and_matches_missing() {
    let fixture = ownership_fixture(false).await;
    let results = check_ownership_matrix(&fixture).await;
    assert_eq!(results.len(), 5, "every owner-scoped row executed");
    for (row, result) in &results {
        assert!(result.is_ok(), "{row}: {result:?}");
    }
    // The missing-Session rows E2-E5 are the comparison baseline; they must
    // not create a Session for the intruder.
    assert!(
        fixture
            .durable
            .get(&SessionId::from(MISSING))
            .await
            .is_err(),
        "no missing Session was created by E2-E5"
    );
}

#[tokio::test]
async fn http_only_owner_validation_is_detected_by_the_matrix() {
    // NC: the internal entrypoints receive the HTTP-authenticated caller scope
    // as the Session owner. The same checker must reject at least one row.
    let fixture = ownership_fixture(true).await;
    let results = check_ownership_matrix(&fixture).await;
    let failed = results
        .iter()
        .filter(|(_, result)| result.is_err())
        .map(|(row, result)| format!("{row}: {}", result.as_ref().unwrap_err()))
        .collect::<Vec<_>>();
    assert!(
        !failed.is_empty(),
        "NC an HTTP-only owner check must make the matrix fail: {results:?}"
    );
    assert!(
        failed.iter().any(|row| row.contains("O0")),
        "NC at least one row performs an unauthorized read or effect: {failed:?}"
    );
}

#[tokio::test]
#[ignore = "product gap: admit_session_run_for_owner creates a Session when the id is missing but rejects a foreign one, leaking existence (run_admission.rs create-on-missing)"]
async fn foreign_run_admission_is_indistinguishable_from_missing() {
    // The projection resolves creation-time Environments so create-on-missing
    // completes; worker assignment/leases stay the shared recording fakes.
    let creation = Arc::new(CreationEnvironment::default());
    let recording = Arc::new(RecordingEnvironmentSource::default());
    let fixture = ownership_fixture_with(
        false,
        crate::SessionEnvironments {
            projection: creation.clone(),
            assignment: recording.clone(),
            leases: recording,
        },
    )
    .await;
    let foreign = invoke("E1", &fixture.app, OWNED).await;
    let foreign_resolutions = creation.resolutions.load(Ordering::SeqCst);
    let missing = invoke("E1", &fixture.app, MISSING).await;
    let missing_resolutions = creation.resolutions.load(Ordering::SeqCst) - foreign_resolutions;
    assert_eq!(
        foreign, missing,
        "a foreign Session id must produce the same answer as an unused id (creation resolutions: foreign {foreign_resolutions}, missing {missing_resolutions})"
    );
    assert_eq!(
        (foreign_resolutions, missing_resolutions),
        (0, 0),
        "neither id may reach the creation path for an intruder"
    );
}

#[tokio::test]
#[ignore = "product gap: append_session_event_batch_idempotent has no owner parameter, so an internal caller without owner proof mutates a foreign Session"]
async fn owner_less_event_append_refuses_without_owner_proof() {
    let fixture = ownership_fixture(false).await;
    let owned = SessionId::from(OWNED);
    let before = fixture.durable.get(&owned).await.unwrap();
    // Nothing identifies the caller; correct behaviour is a refusal without
    // a root mutation (an owner-scoped internal entrypoint).
    let result = fixture
        .app
        .event_batches
        .append_session_event_batch_idempotent(
            &owned,
            vec![
                awaken_session_event_contract::SessionEventInput::UserMessage {
                    content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                        "written by a foreign tenant",
                    )],
                },
            ],
            None,
            None,
            Some(("t06-foreign-key".into(), "t06-foreign-fingerprint".into())),
        )
        .await;
    assert!(result.is_err(), "owner-less append must refuse: {result:?}");
    assert_eq!(
        fixture.durable.get(&owned).await.unwrap(),
        before,
        "no foreign Event batch reaches the root"
    );
}
