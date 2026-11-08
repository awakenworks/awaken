//! Event-batch recovery of a refused User Run admission
//! (`event_batches::refused_run`): a permanent refusal settles the Session
//! terminally, a transient one stays queued for retry.

use super::*;

#[test]
fn rejected_user_reservation_cannot_orphan_an_accompanying_system() {
    // Coverage rationale: the async case below owns S1 and its E1-E4 oracle.
    // The shared executor changes stack placement only.
    run_composed_async_test(rejected_user_reservation_cannot_orphan_an_accompanying_system_case);
}

async fn rejected_user_reservation_cannot_orphan_an_accompanying_system_case() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Cause/effect graph: C1 the complete initial batch is valid; C2 durable Run
    // projection recovery succeeds; C3 reservation rejects permanently (a bad
    // request) before an activity receipt. Effects: E1 the refusal settles the
    // Session terminally (`refused_run_settlement` R2: ActivationFailed with
    // the refusal as its durable cause and no source Run) instead of reporting
    // a retryable failure; E2 projection precedes reservation but no operation
    // receipt is committed; E3 neither System nor User is reserved/committed;
    // E4 no activation occurs. Decision rule S1=C1+C2+C3=>E1+E2+E3+E4.
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.fail_reservation.store(true, Ordering::SeqCst);
    let session = planned_session(
        "system-after-admission",
        vec![
            SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("driver")],
            },
            SessionEventInput::SystemMessage {
                content: vec![ContentBlock::text("context")],
            },
        ],
    );
    let run_id = match &session.event_batches[0].events[0].event {
        awaken_session_contract::SessionEventCommand::UserMessage { run_id, .. } => run_id.clone(),
        _ => unreachable!("S1 User command"),
    };
    create(repository.as_ref(), session).await;
    let app = application_with_runtime(
        runtime.clone(),
        repository,
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let report = app.event_batches.reconcile_event_batches().await;
    assert!(
        report.failures.is_empty(),
        "S1/E1 no retry: {:?}",
        report.failures
    );
    let durable = app
        .mutations
        .repository()
        .get(&SessionId::from("system-after-admission"))
        .await
        .unwrap();
    assert_eq!(
        durable.execution,
        SessionExecutionState::ActivationFailed,
        "S1/E1 terminal"
    );
    assert!(
        durable
            .realization_progress()
            .last_error()
            .is_some_and(|cause| cause.contains("scripted reservation rejection")),
        "S1/E1 the refusal is the durable cause"
    );
    assert!(
        durable
            .realization_progress()
            .failure_source_run_id()
            .is_none(),
        "S1/E1 no Run was reserved to source the failure"
    );
    let trace = runtime.trace.lock().unwrap().clone();
    let projection = trace
        .iter()
        .position(|entry| entry.starts_with("project:system-after-admission:"))
        .expect("S1/E2 projection");
    let reservation = trace
        .iter()
        .position(|entry| entry == &format!("reserve:{}", run_id.0))
        .expect("S1/E2 rejected reservation");
    assert!(projection < reservation, "S1/E2");
    let operation = awaken_session_execution_contract::session_run_activity_operation_id(
        &awaken_session_types::SessionId::from("system-after-admission"),
        &run_id,
    );
    assert!(
        app.activities
            .recover_activity_for_operation(
                &SessionId::from("system-after-admission"),
                &operation,
                None
            )
            .await
            .expect("S1/E2 receipt query")
            .is_none(),
        "S1/E2 no orphan activity"
    );
    assert!(runtime.reserved.lock().unwrap().is_empty(), "S1/E3");
    assert!(runtime.committed.lock().unwrap().is_empty(), "S1/E3");
    assert!(
        runtime
            .trace
            .lock()
            .unwrap()
            .iter()
            .all(|entry| !entry.starts_with("activate:")),
        "S1/E4"
    );
}

#[test]
fn transiently_refused_user_reservation_stays_queued_for_retry() {
    run_composed_async_test(transiently_refused_user_reservation_stays_queued_for_retry_case);
}

async fn transiently_refused_user_reservation_stays_queued_for_retry_case() {
    // Negative control of `refused_run_settlement`: C1 a valid queued User
    // batch; C2 its reservation fails with a dependency outage (Unavailable).
    // Effects: E1 reconciliation reports the retryable failure (the supervisor
    // backs off and retries); E2 the Session keeps its live state and records
    // no terminal cause; E3 the batch stays unprocessed; E4 once the
    // dependency recovers, the same batch's Run is reserved and activated.
    // Decision rule R1=C1+C2=>E1+E2+E3; then !C2=>E4.
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime
        .fail_reservation_transiently
        .store(true, Ordering::SeqCst);
    let session = planned_session(
        "transient-reservation",
        vec![SessionEventInput::UserMessage {
            content: vec![ContentBlock::text("driver")],
        }],
    );
    let run_id = match &session.event_batches[0].events[0].event {
        awaken_session_contract::SessionEventCommand::UserMessage { run_id, .. } => run_id.clone(),
        _ => unreachable!("R1 User command"),
    };
    create(repository.as_ref(), session).await;
    let app = application_with_runtime(
        runtime.clone(),
        repository,
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let report = app.event_batches.reconcile_event_batches().await;
    assert_eq!(report.failures.len(), 1, "R1/E1 {:?}", report.failures);
    let durable = app
        .mutations
        .repository()
        .get(&SessionId::from("transient-reservation"))
        .await
        .unwrap();
    assert_eq!(durable.execution, SessionExecutionState::Running, "R1/E2");
    assert!(
        durable.realization_progress().last_error().is_none(),
        "R1/E2"
    );
    assert!(durable.has_incomplete_event_batches(), "R1/E3");
    assert!(runtime.reserved.lock().unwrap().is_empty(), "R1/E3");

    runtime
        .fail_reservation_transiently
        .store(false, Ordering::SeqCst);
    let report = app.event_batches.reconcile_event_batches().await;
    assert!(report.failures.is_empty(), "R1/E4 {:?}", report.failures);
    assert!(
        runtime.reserved.lock().unwrap().contains_key(&run_id.0),
        "R1/E4 the retried batch is reserved"
    );
    assert!(
        runtime
            .trace
            .lock()
            .unwrap()
            .contains(&format!("activate:{}", run_id.0)),
        "R1/E4 and activated"
    );
}
