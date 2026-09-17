use super::*;
use awaken_agent_contract::clock::ManualClock;
use awaken_session_contract::{IdempotencyRecord, SessionMutationPayload, SessionRepositoryError};
use awaken_session_store::SqliteManagedSessionRepository;

fn timed_application(repo: Arc<dyn ManagedSessionRepository>, now: u64) -> SessionApplication {
    application_with_configuration(
        repo,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    )
    .with_clock(Arc::new(ManualClock::new(now)))
}

fn command() -> CreateSessionCommand {
    let mut command = creation_command("time-assurance");
    command.intent.control.runtime_placement =
        awaken_session_contract::SessionRuntimePlacement::Worker;
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
        let repo =
            Arc::new(SqliteManagedSessionRepository::open(&std::env::var(DB).unwrap()).unwrap());
        let faulting = Arc::new(FaultingSessionRepository::new(repo));
        *faulting.create_crash_boundary.lock().unwrap() =
            Some((mode == "after", std::env::var(MARKER).unwrap().into()));
        timed_application(faulting, 100)
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
        let repo = Arc::new(SqliteManagedSessionRepository::open(db.to_str().unwrap()).unwrap());
        if mode == "before" {
            assert!(
                matches!(
                    repo.get(&awaken_session_contract::SessionId::from("time-assurance"))
                        .await,
                    Err(SessionRepositoryError::NotFound)
                ),
                "T5"
            );
        } else {
            let durable = repo
                .get(&awaken_session_contract::SessionId::from("time-assurance"))
                .await
                .unwrap();
            assert_eq!(durable.created_at_unix_ms, Some(100), "T7 creation");
            assert_eq!(durable.updated_at_unix_ms, Some(100), "T7 atomic root");
            assert!(durable.frozen_baseline().is_some(), "T7 complete root");
        }
        // Replay may complete later activation; it must never recreate the root
        // or replace the original creation time with the recovery clock.
        let prior = repo
            .get(&awaken_session_contract::SessionId::from("time-assurance"))
            .await
            .ok();
        let app = timed_application(repo.clone(), 2_000);
        let recovered = app.create_session(command()).await.unwrap();
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
            app.create_session(command()).await.unwrap(),
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
        SqliteManagedSessionRepository::open_in_memory().unwrap(),
    ))
    .await;
}

#[tokio::test]
async fn postgres_mutation_time_receipts_reject_conflicts_and_recover_lost_ack() {
    // The same T3/T4/T7 decision rules run through the production Postgres
    // transaction adapter. Explicitly configured database failure is fatal;
    // an unconfigured native run does not claim Postgres coverage.
    let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
        return;
    };
    let repo = awaken_session_store::PostgresManagedSessionRepository::connect(&url)
        .await
        .expect("configured time-assurance Postgres");
    mutation_time_receipt_table(Arc::new(repo)).await;
}

async fn mutation_time_receipt_table(repo: Arc<dyn ManagedSessionRepository>) {
    let mut create = command();
    // Keep the real Postgres case repeatable without deleting shared database
    // state: every test process owns a distinct Session identity.
    create.session_id = format!("time-receipt-{}", std::process::id());
    let faulting = Arc::new(FaultingSessionRepository::new(repo.clone()));
    let app = timed_application(faulting.clone(), 100);
    let original = app.create_session(create).await.unwrap();
    let app = timed_application(faulting.clone(), 200);
    let owner = app.owner(&original.session_id).await.unwrap();
    let mut desired = original.clone();
    desired.title = Some("changed".into());
    let key = receipt("lost-time-ack", &desired);
    faulting.commit_then_conflict_once("lost-time-ack");
    assert!(
        matches!(
            app.commit_session_snapshot_with_record(&owner, desired.clone(), key.clone(), vec![])
                .await,
            Err(SessionMutationError::Conflict)
        ),
        "T7 reply lost"
    );
    let committed = repo.get(&original.session_id).await.unwrap();
    let restarted = timed_application(repo.clone(), 999);
    let (replayed, applied) = restarted
        .commit_session_snapshot_with_record(&owner, desired.clone(), key.clone(), vec![])
        .await
        .unwrap();
    assert!(!applied);
    assert_eq!(replayed, committed, "T7");
    super::super::observation_refinement::root_step("root-lost-ack", &committed, &replayed, 999);
    desired.title = Some("conflicting payload".into());
    let conflicting = receipt(&key.key, &desired);
    assert!(
        matches!(
            restarted
                .commit_session_snapshot_with_record(&owner, desired.clone(), conflicting, vec![])
                .await,
            Err(SessionMutationError::IdempotencyMismatch)
        ),
        "T3"
    );
    let stale = receipt("stale-time", &desired);
    assert!(
        matches!(
            restarted
                .commit_session_snapshot_with_record(&owner, desired, stale, vec![])
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
    let repo = Arc::new(SqliteManagedSessionRepository::open_in_memory().unwrap());
    let initial = timed_application(repo.clone(), 100)
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
        async move {
            gate.wait().await;
            app.commit_session_snapshot_with_record(&owner, desired, key, vec![])
                .await
        }
    };
    let (left, right) = tokio::join!(write(200), write(300));
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    let winner = left.or(right).unwrap().0;
    assert_eq!(
        winner.updated_at_unix_ms,
        winner.title.as_ref().and_then(|s| s.parse().ok())
    );
    assert_eq!(winner.created_at_unix_ms, initial.created_at_unix_ms);
    assert_eq!(repo.get(&initial.session_id).await.unwrap(), winner);
}
