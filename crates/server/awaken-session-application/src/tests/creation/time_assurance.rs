// @formal evidence session_time.root_observations_are_revision_bound_and_replay_stable
use super::*;
use awaken_lease_testkit::ManualClock;
use awaken_session_state_contract::{
    IdempotencyRecord, SessionMutationPayload, SessionRepositoryError,
};
use awaken_session_store::SqliteManagedSessionRepository;

fn timed_application(
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
    now: u64,
) -> SessionApplication {
    SessionApplicationBuilder::fixture(
        Arc::new(NoopRuntime),
        Arc::new(NoopMcpRealizer),
        SessionPersistence::new(repo, Arc::new(ManualClock::new(now))),
        Arc::new(RecordingEnvironmentSource::default()),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .configuration(SessionApplicationConfiguration {
        execution_placement: SessionExecutionPlacement::RegisteredWorker,
        ..Default::default()
    })
    .build()
    .expect("valid fixture Store/Clock composition")
}

fn command() -> CreateSessionCommand {
    let mut command = creation_command("time-assurance");
    command.intent.control.runtime_placement =
        awaken_session_types::SessionRuntimePlacement::Worker;
    command.idempotency = None;
    command
}

fn receipt(key: &str, session: &PersistedSession) -> IdempotencyRecord {
    IdempotencyRecord {
        key: key.into(),
        payload_hash: SessionMutationPayload::Replace(session.clone()).stable_hash(),
    }
}

#[tokio::test]
async fn application_creation_time_survives_process_crash_boundaries() {
    // T5 before root commit => absent; T7 after root commit/before response =>
    // complete original timestamp survives SIGKILL. This drives real creation,
    // including receipt compilation and clock sampling, over file SQLite.
    // The decorator only suspends at a port boundary; it stores no product state.
    const MODE: &str = "AWAKEN_TIME_CRASH_MODE";
    const DB: &str = "AWAKEN_TIME_CRASH_DB";
    const MARKER: &str = "AWAKEN_TIME_CRASH_MARKER";
    if let Ok(mode) = std::env::var(MODE) {
        let repo = Arc::new(
            SqliteManagedSessionRepository::open(&std::env::var(DB).unwrap())
                .await
                .unwrap(),
        );
        let faulting = Arc::new(FaultingSessionRepository::new(repo));
        *faulting.create_crash_boundary.lock().unwrap() =
            Some((mode == "after", std::env::var(MARKER).unwrap().into()));
        timed_application(faulting, 100)
            .creation_commands
            .create_session(command())
            .await
            .unwrap();
        panic!("crash child unexpectedly returned");
    }
    let mut crash_traces = Vec::new();
    for mode in ["before", "after"] {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("time.sqlite");
        let marker = dir.path().join("boundary");
        let status = awaken_reliability_testkit::CrashProcess::new(
            "tests::creation::time_assurance::application_creation_time_survives_process_crash_boundaries", &marker)
            .env(MODE, mode).env(DB, &db).env(MARKER, &marker).run().unwrap();
        assert!(!status.success());
        let repo = Arc::new(
            SqliteManagedSessionRepository::open(db.to_str().unwrap())
                .await
                .unwrap(),
        );
        if mode == "before" {
            assert!(
                matches!(
                    repo.get(&awaken_session_types::SessionId::from("time-assurance"))
                        .await,
                    Err(SessionRepositoryError::NotFound)
                ),
                "T5"
            );
        } else {
            let durable = repo
                .get(&awaken_session_types::SessionId::from("time-assurance"))
                .await
                .unwrap();
            assert_eq!(durable.created_at_unix_ms, Some(100), "T7 creation");
            assert_eq!(durable.updated_at_unix_ms, Some(100), "T7 atomic root");
            assert!(durable.frozen_baseline().is_some(), "T7 complete root");
            assert!(
                durable.environment_materialization().unwrap().is_empty(),
                "T7 fresh fact belongs to same atomic insert"
            );
        }
        // Replay may complete later activation; it must never recreate the root
        // or replace the original creation time with the recovery clock.
        let prior = repo
            .get(&awaken_session_types::SessionId::from("time-assurance"))
            .await
            .ok();
        let app = timed_application(repo.clone(), 2_000);
        let recovered = app
            .creation_commands
            .create_session(command())
            .await
            .unwrap();
        crash_traces.push(super::super::observation_refinement::root_observation(
            prior.as_ref(),
            Some(&recovered),
            2_000,
            mode,
        ));
        assert_eq!(
            recovered.created_at_unix_ms,
            Some(if mode == "before" { 2_000 } else { 100 })
        );
        assert_eq!(
            app.creation_commands
                .create_session(command())
                .await
                .unwrap(),
            recovered,
            "T2 exact replay"
        );
    }
    awaken_reliability_testkit::write_observation_trace(
        "root-crash-recovery",
        "root",
        serde_json::json!(crash_traces),
    );
}

#[tokio::test]
async fn mutation_time_receipts_reject_conflicts_and_recover_lost_ack() {
    // T3 same key/different payload => unchanged; T4 stale CAS => unchanged;
    // T7 commit then lost acknowledgement => retry observes original time.
    // All assertions compare complete roots, so time-only or partial writes fail.
    mutation_time_receipt_table(Arc::new(
        SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    ))
    .await;
}

#[tokio::test]
async fn postgres_mutation_time_receipts_reject_conflicts_and_recover_lost_ack() {
    // The same T3/T4/T7 decision rules run through the production Postgres
    // transaction adapter. Explicitly configured database failure is fatal;
    // an unconfigured native run does not claim Postgres coverage.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let repo = awaken_session_store::PostgresManagedSessionRepository::connect(&url)
        .await
        .expect("configured time-assurance Postgres");
    mutation_time_receipt_table(Arc::new(repo)).await;
}

async fn mutation_time_receipt_table(
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
) {
    let mut create = command();
    // Keep the real Postgres case repeatable without deleting shared database
    // state: every test process owns a distinct Session identity.
    create.session_id = format!("time-receipt-{}", std::process::id());
    let faulting = Arc::new(FaultingSessionRepository::new(repo.clone()));
    let app = timed_application(faulting.clone(), 100);
    let original = app.creation_commands.create_session(create).await.unwrap();
    let app = timed_application(faulting.clone(), 200);
    let owner = app.queries().owner(&original.session_id).await.unwrap();
    let mut desired = original.clone();
    desired.title = Some("changed".into());
    let key = receipt("lost-time-ack", &desired);
    faulting.commit_then_conflict_once("lost-time-ack");
    assert!(
        matches!(
            app.mutations
                .commit_session_snapshot_with_record(
                    &owner,
                    // T7 must reach the real CAS/response-loss fault with one
                    // State authoring fact. Later replay/mismatch rows stay raw.
                    awaken_store_conformance::session_store::record_update(
                        &original,
                        desired.clone(),
                        0
                    ),
                    key.clone(),
                    vec![]
                )
                .await,
            Err(SessionMutationError::Conflict)
        ),
        "T7 reply lost"
    );
    let committed = repo.get(&original.session_id).await.unwrap();
    let restarted = timed_application(repo.clone(), 999);
    let commit = restarted
        .mutations
        .commit_session_snapshot_with_record(
            &owner,
            awaken_session_state_contract::SessionAggregate::try_from(desired.clone())
                .expect("valid test snapshot"),
            key.clone(),
            vec![],
        )
        .await
        .unwrap();
    assert!(!commit.transitioned());
    let replayed = commit.into_session();
    assert_eq!(replayed, committed, "T7");
    super::super::observation_refinement::root_step("root-lost-ack", &committed, &replayed, 999);
    desired.title = Some("conflicting payload".into());
    let conflicting = receipt(&key.key, &desired);
    assert!(
        matches!(
            restarted
                .mutations
                .commit_session_snapshot_with_record(
                    &owner,
                    awaken_session_state_contract::SessionAggregate::try_from(desired.clone())
                        .expect("valid test snapshot"),
                    conflicting,
                    vec![]
                )
                .await,
            Err(SessionMutationError::IdempotencyMismatch)
        ),
        "T3"
    );
    let stale = receipt("stale-time", &desired);
    assert!(
        matches!(
            restarted
                .mutations
                .commit_session_snapshot_with_record(
                    &owner,
                    awaken_session_state_contract::SessionAggregate::try_from(desired)
                        .expect("valid test snapshot"),
                    stale,
                    vec![]
                )
                .await,
            Err(SessionMutationError::Conflict)
        ),
        "T4"
    );
    assert_eq!(
        repo.get(&original.session_id).await.unwrap(),
        committed,
        "T3/T4 no partial write"
    );
    super::super::observation_refinement::root_step(
        "root-reject",
        &committed,
        &repo.get(&original.session_id).await.unwrap(),
        999,
    );
}

#[tokio::test]
async fn concurrent_root_cas_time_belongs_only_to_the_winner() {
    // T10 two writers prepared from one revision, distinct clocks and payloads.
    // Barrier releases both after preparation; exactly one CAS may persist.
    // The losing time/payload never contaminates the winner, including replay.
    let repo = Arc::new(
        SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let initial = timed_application(repo.clone(), 100)
        .creation_commands
        .create_session(command())
        .await
        .unwrap();
    let owner = repo.owner(&initial.session_id).await.unwrap();
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let write = |at: u64| {
        let app = timed_application(repo.clone(), at);
        let gate = gate.clone();
        let mut desired = initial.clone();
        let owner = owner.clone();
        desired.title = Some(at.to_string());
        let key = receipt(&format!("race-{at}"), &desired);
        // Each racing clock candidate is a legal State author; App stamps the
        // same next-revision fact at its actual clock after the barrier.
        let prepared = awaken_store_conformance::session_store::record_update(&initial, desired, 0);
        async move {
            gate.wait().await;
            app.mutations
                .commit_session_snapshot_with_record(&owner, prepared, key, vec![])
                .await
        }
    };
    let (left, right) = tokio::join!(write(200), write(300));
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    let winner = left.or(right).unwrap().into_session();
    assert_eq!(
        winner.updated_at_unix_ms,
        winner.title.as_ref().and_then(|s| s.parse().ok())
    );
    assert_eq!(winner.created_at_unix_ms, initial.created_at_unix_ms);
    assert_eq!(repo.get(&initial.session_id).await.unwrap(), winner);
}

#[tokio::test]
async fn first_archive_time_is_durable_and_legacy_archive_stays_unknown() {
    // Causes: A1 new Idle root + Application clock 100; A2 exact archive replay
    // after reopen with clock 900 and different v1 display time; A3 historical
    // Archived JSON without typed observation. Effects: A1 one committed typed
    // time; A2 original time/string survive restart and replay; A3 remains
    // unknown after file-SQLite readback. No read or retry supplies a new time.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive-time.sqlite");
    let repo = Arc::new(
        SqliteManagedSessionRepository::open(path.to_str().unwrap())
            .await
            .unwrap(),
    );
    let id = "archive-time-new";
    create(repo.as_ref(), persisted(id, false, "idle")).await;
    let fact = awaken_session_state_contract::ManagedLifecycleFact {
        id: format!("{id}:terminated"),
        object_id: id.into(),
        workspace_id: Some("workspace".into()),
        event_type: "session.terminated".into(),
        timestamp: 1,
        runtime_interval: None,
    };
    timed_application(repo.clone(), 100)
        .termination_commands
        .terminate_session(id, "2026-09-07T00:00:00Z", fact.clone())
        .await
        .unwrap();
    let first = repo.get(&SessionId::from(id)).await.unwrap();
    assert_eq!(first.archived_at_unix_ms(), Some(100), "A1");
    assert_eq!(first.archived_at(), Some("2026-09-07T00:00:00Z"), "A1 v1");

    let old_id = "archive-time-legacy";
    let mut old = persisted(old_id, false, "terminated");
    old.disposition = awaken_session_state_contract::SessionDisposition::Archived {
        archived_at: "2026-01-01T00:00:00Z".into(),
        archived_at_unix_ms: None,
    };
    create(repo.as_ref(), old).await;
    drop(repo);
    let cold_repo = Arc::new(
        SqliteManagedSessionRepository::open(path.to_str().unwrap())
            .await
            .unwrap(),
    );
    let cold = cold_repo.get(&SessionId::from(id)).await.unwrap();
    assert_eq!(cold.archived_at_unix_ms(), Some(100), "A1 cold");
    timed_application(cold_repo.clone(), 900)
        .termination_commands
        .terminate_session(id, "later-display", fact)
        .await
        .unwrap();
    assert_eq!(
        cold_repo.get(&SessionId::from(id)).await.unwrap(),
        cold,
        "A2"
    );
    let legacy = cold_repo.get(&SessionId::from(old_id)).await.unwrap();
    assert_eq!(legacy.archived_at_unix_ms(), None, "A3");
    assert_eq!(legacy.archived_at(), Some("2026-01-01T00:00:00Z"), "A3 v1");
    let old_fact = awaken_session_state_contract::ManagedLifecycleFact {
        id: format!("{old_id}:terminated"),
        object_id: old_id.into(),
        workspace_id: Some("workspace".into()),
        event_type: "session.terminated".into(),
        timestamp: 2,
        runtime_interval: None,
    };
    timed_application(cold_repo.clone(), 900)
        .termination_commands
        .terminate_session(old_id, "later-display", old_fact)
        .await
        .unwrap();
    let old_replay = cold_repo.get(&SessionId::from(old_id)).await.unwrap();
    assert_eq!(
        old_replay.archived_at_unix_ms(),
        None,
        "A3 replay cannot backfill"
    );
    assert_eq!(
        old_replay.archived_at(),
        legacy.archived_at(),
        "A3 v1 replay"
    );
}

#[tokio::test]
async fn concurrent_archives_keep_only_the_winning_clock_sample() {
    // Causes: C1 two Application instances submit the same command from one
    // Idle root and race root CAS with clocks 100/200; C2 loser replays the
    // same command receipt after the winner archived.
    // Effects: exactly one transition commits; the durable typed time and v1
    // display belong to that winner, never the later replaying observation.
    let repo = Arc::new(
        SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let id = "archive-time-race";
    create(repo.as_ref(), persisted(id, false, "idle")).await;
    let fact = awaken_session_state_contract::ManagedLifecycleFact {
        id: format!("{id}:terminated"),
        object_id: id.into(),
        workspace_id: Some("workspace".into()),
        event_type: "session.terminated".into(),
        timestamp: 1,
        runtime_interval: None,
    };
    let left_app = timed_application(repo.clone(), 100);
    let right_app = timed_application(repo.clone(), 200);
    let (left, right) = tokio::join!(
        left_app
            .termination_commands
            .terminate_session(id, "2026-09-07T00:00:00Z", fact.clone()),
        right_app
            .termination_commands
            .terminate_session(id, "2026-09-07T00:00:00Z", fact),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_ne!(left.transitioned(), right.transitioned(), "C1 one winner");
    let time = if left.transitioned() { 100 } else { 200 };
    let durable = repo.get(&SessionId::from(id)).await.unwrap();
    assert_eq!(durable.archived_at_unix_ms(), Some(time), "C1-C2");
    assert_eq!(durable.archived_at(), Some("2026-09-07T00:00:00Z"), "C1-C2");
}

#[test]
fn postgres_mutation_time_receipts_reject_conflicts_and_recover_lost_ack_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "tests::creation::time_assurance::postgres_mutation_time_receipts_reject_conflicts_and_recover_lost_ack",
    );
}
