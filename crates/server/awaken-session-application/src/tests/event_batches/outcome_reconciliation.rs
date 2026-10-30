use super::*;

#[test]
fn outcome_crosses_awaiting_user_while_next_user_stays_queued_after_restart() {
    // Coverage rationale: the async case below owns Q1-Q3. The shared executor
    // changes stack placement only and does not add another recovery oracle.
    run_composed_async_test(
        outcome_crosses_awaiting_user_while_next_user_stays_queued_after_restart_case,
    );
}

#[test]
fn queued_user_before_awaiting_boundary_must_not_hide_required_tool_action() {
    run_composed_async_test(queued_user_before_awaiting_boundary_case);
}

async fn queued_user_before_awaiting_boundary_case() {
    for legacy in [false, true] {
        queued_input_before_awaiting_boundary(false, legacy).await;
        queued_input_before_awaiting_boundary(true, legacy).await;
    }
}

async fn queued_input_before_awaiting_boundary(with_system: bool, legacy: bool) {
    // Live Classroom regression: C1 first User is executing; C2 a later User
    // is accepted BEFORE first Awaiting settlement; C3 first Dispatch settles;
    // C4 a cold supervisor reconciles the same root. Effects: E1 first input
    // processed with no active epoch; E2 later User unprocessed/unreserved;
    // E3 Session Idle exposes the exact required action instead of withholding
    // it until that blocked later User executes. Rule AQ1 C1+C2+C3+C4=>E1-E3.
    // C5 later User carries/does not carry its paired System input; both remain
    // unprocessed together. A System after a ToolReply is not this partition.
    // The existing outcome-crosses-awaiting test covers the opposite order
    // (C2 AFTER C3); both orders must retain the same actionability and queue.
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("AQ1 repository"),
    );
    let id = "queued-before-awaiting";
    create(repository.as_ref(), persisted(id, false, "idle")).await;
    let runtime = Arc::new(EventBatchRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    for (key, content) in [("first", "first"), ("later", "queued")] {
        let mut inputs = vec![SessionEventInput::UserMessage {
            content: vec![ContentBlock::text(content)],
        }];
        if key == "later" && with_system {
            inputs.push(SessionEventInput::SystemMessage {
                content: vec![ContentBlock::text("paired later context")],
            });
        }
        app.event_batches
            .append_session_event_batch(id, inputs, Some(key.into()), None)
            .await
            .expect("AQ1 append");
        assert!(
            app.event_batches
                .reconcile_event_batches()
                .await
                .failures
                .is_empty()
        );
    }
    let run = runtime
        .latest_run
        .lock()
        .unwrap()
        .clone()
        .expect("AQ1 first Run");
    assert_eq!(
        runtime.reserved.lock().unwrap().len(),
        1,
        "AQ1 later stays queued"
    );
    runtime.await_run(&run);
    if legacy {
        // AQ2 reproduces the old policy (no Awaiting-specific closure) through
        // its conservative Completed branch, then reopens the SQLite-backed
        // application against the real fixture's Awaiting Run. The exact
        // Dispatch receipt/interval remain durable; no SQL repair is used.
        let epoch = runtime.activity_epoch(&run);
        app.lifecycle()
            .settle_user_event_dispatch_observed(
                &SessionId::from(id),
                epoch,
                awaken_session_contract::SessionRuntimeIntervalObservation {
                    activity_epoch: epoch,
                    thread_id: ThreadId(id.into()),
                    run_id: run.clone(),
                    lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor(epoch),
                    source_commit_cursor: 1,
                },
                &run,
                awaken_session_execution_contract::SessionAgentRunBoundary::Completed,
            )
            .await
            .expect("AQ2 old conservative settlement");
        assert_eq!(
            repository
                .get(&SessionId::from(id))
                .await
                .unwrap()
                .execution,
            awaken_session_contract::SessionExecutionState::Running,
            "AQ2 stranded legacy shape"
        );
    } else {
        finalize_applied_user_dispatch(&app, &runtime, id, &run).await;
    }
    let cold = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(
        cold.event_batches
            .reconcile_event_batches()
            .await
            .failures
            .is_empty()
    );
    let root = repository
        .get(&SessionId::from(id))
        .await
        .expect("AQ1 durable root");
    assert!(root.event_batches[0].events[0].processed, "AQ1/E1");
    assert!(root.active_activity_epochs.is_empty(), "AQ1/E1");
    assert!(!root.event_batches[1].events[0].processed, "AQ1/E2");
    assert!(
        root.event_batches[1]
            .events
            .iter()
            .all(|entry| !entry.processed),
        "AQ1/E2 paired System stays queued"
    );
    assert_eq!(runtime.reserved.lock().unwrap().len(), 1, "AQ1/E2");
    assert_eq!(
        root.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "AQ1/E3 queued successor cannot hide the first Run's required action"
    );
    // AQ3 exact repeated recovery is a complete root stutter; AQ4 a later
    // active epoch must survive an old callback. Compare the whole serialized
    // aggregate, not merely status, so receipts and timestamps cannot drift.
    let stable = serde_json::to_value(&root).unwrap();
    assert!(
        cold.event_batches
            .reconcile_event_batches()
            .await
            .failures
            .is_empty()
    );
    assert_eq!(
        serde_json::to_value(repository.get(&SessionId::from(id)).await.unwrap()).unwrap(),
        stable,
        "AQ3"
    );
    let (_, newer_epoch) = cold
        .activities
        .begin_activity_for_operation(&SessionId::from(id), "newer-execution")
        .await
        .unwrap();
    assert!(newer_epoch > runtime.activity_epoch(&run));
    let newer = serde_json::to_value(repository.get(&SessionId::from(id)).await.unwrap()).unwrap();
    finalize_applied_user_dispatch(&cold, &runtime, id, &run).await;
    assert_eq!(
        serde_json::to_value(repository.get(&SessionId::from(id)).await.unwrap()).unwrap(),
        newer,
        "AQ4 old callback cannot close a newer activity"
    );
}

async fn outcome_crosses_awaiting_user_while_next_user_stays_queued_after_restart_case() {
    // Eligibility/recovery decision table. C1 first User is Running/Awaiting;
    // C2 a later batch contains User then DefineOutcome; C3 one/two restarted
    // supervisors race. Effects: E1 Running User remains unprocessed; E2
    // Awaiting proves that User processed and its activity settles; E3 Outcome
    // exact prepare crosses the queued User and is processed on receipt; E4 the
    // later User remains queued with no reservation/activity. Rules Q1 Running
    // =>E1; Q2 Awaiting+later Outcome=>E2+E3+E4; Q3 active-active replay=>one
    // Outcome aggregate and the same retained flags.
    // Causes: C1-C3 select Running/Awaiting, later User+Outcome, and restarted
    // supervisor races. Constraint/Invariant: Outcome may cross only proven
    // Awaiting User truth; the later User keeps queue order. Decision rule:
    // execute Q1-Q3 and require one Outcome plus retained later User.
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    create(
        repository.as_ref(),
        persisted("ordinary-eligibility", false, "idle"),
    )
    .await;
    let runtime = Arc::new(EventBatchRuntime::default());
    let warm = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    warm.event_batches
        .append_session_event_batch(
            "ordinary-eligibility",
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("first")],
            }],
            Some("subject-first".into()),
            None,
        )
        .await
        .unwrap();
    let running = warm.event_batches.reconcile_event_batches().await;
    assert!(running.failures.is_empty(), "Q1");
    let first_run = runtime.latest_run.lock().unwrap().clone().expect("Q1 Run");
    assert!(
        !repository
            .get(&SessionId::from("ordinary-eligibility"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "Q1/E1"
    );

    runtime.await_run(&first_run);
    finalize_applied_user_dispatch(&warm, &runtime, "ordinary-eligibility", &first_run).await;
    warm.event_batches
        .append_session_event_batch(
            "ordinary-eligibility",
            vec![
                SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("queued")],
                },
                SessionEventInput::DefineOutcome {
                    description: "ship".into(),
                    rubric: SessionOutcomeRubric::Text {
                        content: "correct".into(),
                    },
                    max_iterations: Some(2),
                },
            ],
            Some("subject-queued".into()),
            None,
        )
        .await
        .unwrap();

    let cold_one = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let cold_two = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let left = tokio::spawn(async move { cold_one.event_batches.reconcile_event_batches().await });
    let right = tokio::spawn(async move { cold_two.event_batches.reconcile_event_batches().await });
    let left = left.await.unwrap();
    let right = right.await.unwrap();
    assert!(left.failures.is_empty() && right.failures.is_empty(), "Q3");

    let durable = repository
        .get(&SessionId::from("ordinary-eligibility"))
        .await
        .unwrap();
    assert!(durable.event_batches[0].events[0].processed, "Q2/E2");
    assert!(!durable.event_batches[1].events[0].processed, "Q2/E4");
    assert!(durable.event_batches[1].events[1].processed, "Q2/E3");
    assert_eq!(runtime.outcomes.lock().unwrap().len(), 1, "Q3/E3");
    assert_eq!(runtime.reserved.lock().unwrap().len(), 1, "Q2/E4");
    assert!(durable.active_activity_epochs.is_empty(), "Q2/E4");
    assert_eq!(
        durable.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "Q2/E2+E4"
    );
}

#[tokio::test]
async fn outcome_receipt_is_processed_on_stable_prepare_and_cold_replay_skips_it() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Outcome receipt cause/effect table. C1 the stable Outcome aggregate is
    // absent/present; C2 process is warm/restarted; C3 create wake is active.
    // Effects: E1 exact prepare once; E2 mark the retained entry processed on
    // prepare receipt with its exact durable cursor; E3 settle create wake immediately; E4 never hold receipt
    // completion on Outcome evaluation/continue. Rules O1 absent+warm=>E1-E4;
    // O2 present+processed+cold=>skip every effect and retain provenance.
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    let session = planned_session(
        "initial-outcome",
        vec![SessionEventInput::DefineOutcome {
            description: "ship".into(),
            rubric: SessionOutcomeRubric::Text {
                content: "FINAL".into(),
            },
            max_iterations: Some(2),
        }],
    );
    let (batch_epoch, expected_outcome_id) = {
        let batch = session.event_batches.first().expect("batch");
        let awaken_session_contract::SessionEventCommand::DefineOutcome { outcome_id, .. } =
            &batch.events[0].event
        else {
            panic!("Outcome Event")
        };
        (
            batch.wake_activity_epoch.expect("create wake"),
            outcome_id.clone(),
        )
    };
    create(repository.as_ref(), session).await;
    let warm = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let completed = warm.event_batches.reconcile_event_batches().await;
    assert!(completed.failures.is_empty(), "O1/E1");
    assert_eq!(completed.settled, 1, "O1/E2");
    assert_eq!(runtime.outcomes.lock().unwrap().len(), 1, "O1/E1");
    let durable = repository
        .get(&SessionId::from("initial-outcome"))
        .await
        .unwrap();
    assert!(durable.event_batches[0].events[0].processed, "O1/E2");
    assert_eq!(
        durable.event_batches[0].events[0]
            .projection_anchor
            .map(|anchor| anchor.source_commit_cursor),
        Some(1),
        "O1/E2 exact Outcome prepare cursor"
    );
    assert!(
        !durable.active_activity_epochs.contains(&batch_epoch),
        "O1/E3"
    );

    let cold = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let replay = cold.event_batches.reconcile_event_batches().await;
    assert!(replay.failures.is_empty(), "O2");
    assert_eq!(replay.settled, 0, "O2");
    assert_eq!(runtime.outcomes.lock().unwrap().len(), 1, "O2/E1");
    assert!(
        runtime
            .outcome_prepare_calls
            .lock()
            .unwrap()
            .iter()
            .all(|id| id == &expected_outcome_id),
        "O1/E1 stable id"
    );
}

#[tokio::test]
async fn outcome_busy_is_retryable_and_cannot_mark_the_root_entry_processed() {
    // Shared suffix boundary: B1 must return Pending before entering
    // observe_processed_event_after_wake. B2 alone supplies the stable prepare
    // cursor, then settles wake before the existing processed-observation CAS.
    // This helper grants no Outcome ownership or independent retry policy.
    // Cause/effect graph: C1 the stable DefineOutcome command is unprocessed;
    // C2 another Outcome owns the Thread aggregate; C3 that owner later clears.
    // Effects: E1 Busy is retryable rather than a reconciliation failure; E2
    // root provenance remains unprocessed during C2; E3 C3 prepares exactly the
    // retained id and only then commits processed=true.
    //
    // | Rule | Root command | Aggregate | Effect |
    // | B1 | unprocessed | busy | E1 + E2 |
    // | B2 | same retained command | available | E3 |
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.outcome_busy.store(true, Ordering::SeqCst);
    create(
        repository.as_ref(),
        planned_session(
            "outcome-busy",
            vec![SessionEventInput::DefineOutcome {
                description: "ship".into(),
                rubric: SessionOutcomeRubric::Text {
                    content: "correct".into(),
                },
                max_iterations: None,
            }],
        ),
    )
    .await;
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let busy = app.event_batches.reconcile_event_batches().await;
    assert!(busy.failures.is_empty(), "B1/E1");
    assert_eq!(busy.pending, 1, "B1/E1");
    assert!(
        !repository
            .get(&SessionId::from("outcome-busy"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "B1/E2"
    );

    runtime.outcome_busy.store(false, Ordering::SeqCst);
    let prepared = app.event_batches.reconcile_event_batches().await;
    assert!(prepared.failures.is_empty(), "B2/E3");
    assert!(
        repository
            .get(&SessionId::from("outcome-busy"))
            .await
            .unwrap()
            .event_batches[0]
            .events[0]
            .processed,
        "B2/E3"
    );
}

#[test]
fn initial_outcome_reconciliation_does_not_wait_for_its_own_first_run() {
    // Cause/effect graph: C1 retained DefineOutcome is processed; C2 Session is
    // Idle or awaiting initial realization; C3 an activity still owns a Run.
    // R1 C1+C2+!C3 -> select the existing Outcome continuation, whose normal Run
    // admission owns realization. R2 !C1 -> no drive before durable preparation.
    // R3 C3 -> no duplicate Worker Run. R4 Running/Rescheduling/terminal -> no
    // competing driver. The live Managed initial_events probe exposed R1 at
    // Preparing: no first Run could be created while the selector required Idle.
    use awaken_session_contract::SessionExecutionState as State;
    for state in [
        State::Preparing,
        State::Activating,
        State::Idle,
        State::Running,
        State::Rescheduling,
        State::ActivationFailed,
        State::Terminated,
    ] {
        for processed in [false, true] {
            for active in [false, true] {
                let mut session = planned_session(
                    "initial-outcome-selection",
                    vec![SessionEventInput::DefineOutcome {
                        description: "ship".into(),
                        rubric: SessionOutcomeRubric::Text {
                            content: "verified artifact".into(),
                        },
                        max_iterations: Some(3),
                    }],
                );
                session.execution = state;
                session.active_activity_epochs.clear();
                if active {
                    session.active_activity_epochs.insert(1);
                }
                session.event_batches[0].events[0].processed = processed;
                let expected = processed
                    && !active
                    && matches!(state, State::Preparing | State::Activating | State::Idle);
                assert_eq!(
                    session.needs_outcome_reconciliation(),
                    expected,
                    "state={state:?}, processed={processed}, active={active}"
                );
            }
        }
    }
}

#[tokio::test]
async fn cold_supervisor_continues_only_the_thread_owned_outcome_truth() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Crash/recovery cause/effect graph: C1 root prepare is durably processed;
    // C2 the process stops before Outcome continuation; C3 the Thread aggregate
    // is active/terminal/inactive. Effects: E1 a cold supervisor selects the
    // Thread from retained root provenance; E2 Awaiting remains pending; E3 a
    // completed aggregate settles once; E4 inactive truth performs no effect.
    // The conservative root selector stores no active/terminal shadow.
    //
    // | Rule | Root provenance | Thread truth | Effect |
    // | C1 | retained+processed | active | E1 + E2 |
    // | C2 | retained+processed | terminal transition | E3 |
    // | C3 | retained+processed | inactive | E4 |
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    create(
        repository.as_ref(),
        planned_session(
            "outcome-cold-continuation",
            vec![SessionEventInput::DefineOutcome {
                description: "ship".into(),
                rubric: SessionOutcomeRubric::File {
                    file_id: "file_rubric".into(),
                },
                max_iterations: None,
            }],
        ),
    )
    .await;
    let warm = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let prepared = warm.event_batches.reconcile_event_batches().await;
    assert!(prepared.failures.is_empty(), "C1 prepare");
    let durable = repository
        .get(&SessionId::from("outcome-cold-continuation"))
        .await
        .unwrap();
    assert!(
        matches!(
            &durable.event_batches[0].events[0].event,
            awaken_session_contract::SessionEventCommand::DefineOutcome {
                rubric: SessionOutcomeRubric::File { file_id },
                max_iterations: None,
                ..
            } if file_id == "file_rubric"
        ),
        "C1/E1 exact public provenance"
    );
    assert_eq!(
        runtime.outcomes.lock().unwrap().values().next().cloned(),
        Some(("ship".into(), "file_rubric".into(), 3)),
        "C1/E1 execution lowering applies the existing default only at effect time"
    );
    let mut in_flight = durable.clone();
    in_flight.execution = awaken_session_contract::SessionExecutionState::Running;
    assert!(
        !in_flight.needs_outcome_reconciliation(),
        "C1: a retained Outcome cannot replay its stable Worker Run while Session activity is in flight"
    );
    drop(warm);

    let cold = application_with_runtime(
        runtime.clone(),
        repository,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    runtime.dispatch_projection_installs.lock().unwrap().clear();
    let awaiting = cold.reconcile_outcome_continuations().await;
    assert_eq!(awaiting.pending, 1, "C1/E1+E2");
    assert_eq!(runtime.outcome_continue_calls.load(Ordering::SeqCst), 1);
    // C4/E5 a replacement process must restore this Session's exact frozen
    // dispatch projection before continuing its Outcome. Without it Session
    // admission would fall back to the flat-protocol `assistant` identity and
    // the Outcome would pin and grade an Agent that was never published.
    assert_eq!(
        runtime
            .dispatch_projection_installs
            .lock()
            .unwrap()
            .as_slice(),
        ["outcome-cold-continuation"],
        "C4/E5 exactly one restore of the continued Session precedes its Outcome"
    );

    runtime.outcome_completed.store(true, Ordering::SeqCst);
    let completed = cold.reconcile_outcome_continuations().await;
    assert_eq!(completed.settled, 1, "C2/E3");
    let inactive = cold.reconcile_outcome_continuations().await;
    assert_eq!(
        inactive,
        super::super::outcome_reconciliation::OutcomeReconciliation::default(),
        "C3/E4"
    );
    assert_eq!(runtime.outcome_continue_calls.load(Ordering::SeqCst), 3);
}
