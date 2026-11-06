//! T18-C02 (活跃来源保留): retiring the Worker of a LIVE Session with a real
//! running Run keeps every original fact.
//!
//! Oracles (groups JSON order):
//! - O0 the original Session, root, lease, Run and pending facts are kept;
//! - O1 no destructive acknowledgement manufactures an exit-ready state.
//!
//! Negative control: "cleaning up or terminating first to gain eligibility"
//! must fail. It is executed: the same root, but with terminal cleanup
//! requested first, is run through the same production read and the
//! `retained_without_destruction` checker rejects both its verdict and its
//! changed root.
//!
//! The parent B006 row uses `persisted()`, which carries no Run history, and
//! its Pending source observation is consulted only on the terminal branch;
//! this row keeps a positively recorded active Run admission instead.
use super::*;
use awaken_session_contract::LifecycleOutbox as _;

const WORKER: &str = "active-facts-worker";

fn running_root(
    id: &SessionId,
    identity: &awaken_worker_contract::WorkerIdentity,
) -> PersistedSession {
    let mut root = retained_root(id.as_str(), identity);
    let environment = root.environment.clone();
    let realization = root.realization.clone();
    root = PersistedSession::frozen_with_budget(
        id.as_str(),
        root.frozen_baseline().unwrap().clone(),
        root.resources,
        root.mcp,
        root.title,
        root.metadata,
        root.tools,
        root.budget,
    );
    root.environment = environment;
    root.realization = realization;
    root.execution = awaken_session_contract::SessionExecutionState::Running;
    let thread = awaken_agent_contract::ThreadId(id.as_str().into());
    let run = awaken_agent_contract::RunId("active-facts-run".into());
    let operation = awaken_session_execution_contract::session_run_activity_operation_id(id, &run);
    assert!(root.begin_activity_epoch_at(2, Some((&operation, &thread, &run))));
    root
}

/// O0/O1 checker: positive retention with every original fact untouched.
fn retained_without_destruction(
    observed: &crate::WorkerRealizationObservation,
    before: &PersistedSession,
    after: &PersistedSession,
) -> bool {
    observed.sources_retained
        && after == before
        && !after.is_terminal()
        && !after.terminal_cleanup.is_completed()
        && after.run_activities().map(|(records, _)| records.len()) == Some(1)
}

async fn observe(
    repo: &Arc<awaken_session_store::SqliteManagedSessionRepository>,
    identity: &awaken_worker_contract::WorkerIdentity,
) -> (
    Result<crate::WorkerRealizationObservation, RunError>,
    Vec<SourceRetentionSubject>,
    Arc<RecordingCleanupRuntime>,
    Arc<RecordingEnvironmentSource>,
) {
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    *runtime.source_observation.lock().unwrap() =
        awaken_session_execution_contract::SessionSourceDispatchObservation::Pending;
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let app = configured_application(
        runtime.clone(),
        crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
        environments.clone(),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
        None,
        {
            let probe = Arc::new(crate::ExecutionObservationProbe::default());
            crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe)
        },
    );
    // The Worker reports every subject it was given as retained.
    let (result, subjects) =
        qualify_retirement(&app, identity, |subjects| report(subjects, true)).await;
    (result, subjects, runtime, environments)
}

#[tokio::test]
async fn live_running_source_retirement_keeps_its_run_lease_and_pending_facts() {
    let identity = awaken_worker_contract::WorkerIdentity::new(WORKER, "boot", 1);
    let id = SessionId::from("active-facts-running");
    let repo = sqlite_session_repository("active source facts").await;
    create(repo.as_ref(), running_root(&id, &identity)).await;
    let before = repo.get(&id).await.unwrap();
    let outbox = repo.pending_lifecycle().await.unwrap();
    assert_eq!(
        before.run_activities().unwrap().0.len(),
        1,
        "fixture keeps a real Run"
    );

    let (result, subjects, runtime, environments) = observe(&repo, &identity).await;
    let observed = result.expect("live source retirement read");
    let after = repo.get(&id).await.unwrap();
    assert!(
        observed.occupied,
        "O0 the live Session still occupies the Worker"
    );
    assert!(
        retained_without_destruction(&observed, &before, &after),
        "O0/O1"
    );
    assert_eq!(after.realization, before.realization, "O0 exact lease");
    assert_eq!(
        after.active_activity_epochs, before.active_activity_epochs,
        "O0 Run"
    );
    assert_eq!(
        repo.pending_lifecycle().await.unwrap(),
        outbox,
        "O0 pending facts"
    );
    assert_eq!(subjects.len(), 1, "one exact subject");
    assert!(!subjects[0].source_prepared, "an unprepared live source");
    assert!(
        runtime.terminal_assignments.lock().unwrap().is_empty(),
        "O1 no slot"
    );
    assert!(
        runtime.intents.lock().unwrap().is_empty(),
        "O1 no preparation"
    );
    assert!(
        runtime.acknowledged_completed.lock().unwrap().is_empty(),
        "O1 no ACK"
    );
    assert!(
        runtime.acknowledged_effects.lock().unwrap().is_empty(),
        "O1 no ACK"
    );
    assert!(
        environments.retired.lock().unwrap().is_empty(),
        "O1 no Work retirement"
    );
    assert_eq!(
        environments.dispatch_calls.load(Ordering::SeqCst),
        0,
        "O1 no dispatch"
    );
}

#[tokio::test]
async fn terminating_first_to_gain_retirement_eligibility_is_detected() {
    // NC: request terminal cleanup before the retirement read.
    let identity = awaken_worker_contract::WorkerIdentity::new(WORKER, "boot", 1);
    let id = SessionId::from("active-facts-destroyed");
    let original = running_root(&id, &identity);
    let mut destroyed = retained_root(id.as_str(), &identity);
    destroyed.terminal_cleanup = Default::default();
    assert!(
        destroyed.terminal_cleanup.request(&id),
        "NC cleanup requested first"
    );
    let repo = sqlite_session_repository("destroyed source facts").await;
    create(repo.as_ref(), destroyed).await;
    let before = repo.get(&id).await.unwrap();
    let (result, _, runtime, _) = observe(&repo, &identity).await;
    let observed = result.expect("NC retirement read");
    assert!(
        !observed.sources_retained,
        "NC a destructive pre-step does not earn eligibility"
    );
    assert!(
        !retained_without_destruction(&observed, &original, &before),
        "NC the checker rejects the destroyed root"
    );
    assert!(
        runtime.acknowledged_completed.lock().unwrap().is_empty(),
        "NC no ACK"
    );
    assert_eq!(
        repo.get(&id).await.unwrap(),
        before,
        "NC root unchanged by the read"
    );
}
