//! Delayed Outcome publication must extend the already issued event prefix.

use super::*;

#[tokio::test]
async fn late_outcome_terminal_uses_its_state_commit_and_preserves_an_issued_prefix() {
    // Cause/effect graph: C1 the accepted DefineOutcome and an ordinary Run
    // prefix are already listable; C2 the Outcome terminal is visible before
    // its recovery snapshot anchor; C3 the owner committed evaluation state at
    // cursor 15, before the already issued lifecycle cursor 20, but publishes
    // the terminal Outcome at cursor 30; C4 a cold peer
    // reads all final facts. E1 C1 remains an immutable prefix; E2 C2 is held
    // outside the public projection; E3 the evaluation trio appears only after
    // the issued cursor; E4 warm/cold full order and old-cursor suffix match.
    // Decision table: O1=C1=>E1; O2=C1+C2=>E1+E2;
    // O3=C1+C2+C3=>E1+E3; O4=C1+C3+C4=>E4. The evaluation command owns the
    // event time, while the terminal commit owns its first public position;
    // DefineOutcome admission is not reused as a fallback.
    let runtime = LifecycleRuntime::default();
    let repository = Arc::new(ephemeral_session_repo().await);
    let warm = {
        let session_fixture_runtime = runtime.clone();
        let session_fixture_clock = awaken_time::system_clock();
        crate::state::test_support::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            repository.clone(),
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    };
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let batch = warm
        .application
        .append_session_event_batch(
            &session.id,
            vec![SessionEventInput::DefineOutcome {
                description: "ship the result".into(),
                rubric: SessionOutcomeRubric::Text {
                    content: "correct".into(),
                },
                max_iterations: Some(1),
            }],
            None,
            None,
        )
        .await
        .unwrap();
    let SessionEventCommand::DefineOutcome { outcome_id, .. } = &batch.events[0].event else {
        panic!("O1 retained DefineOutcome")
    };
    let outcome_id = outcome_id.clone();
    persist_batch_projection_anchors(&warm, &session.id, &batch.batch_id, &[5]).await;
    let run_id = RunId("late-outcome-run".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            10,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            20,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    runtime.messages.lock().unwrap().push(Message::text(
        MessageId("late-outcome-output".into()),
        Role::Assistant,
        "visible before evaluation",
    ));
    runtime.message_cursors.lock().unwrap().push(19);
    warm.refresh_committed_events(&session.id).await.unwrap();
    let prefix = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let cursor = prefix.last().expect("O1 issued cursor").id.clone();

    runtime.outcome_projections.lock().unwrap().insert(
        outcome_id.clone(),
        awaken_session_execution_contract::CommittedOutcomeProjection::Completed(
            awaken_session_execution_contract::OutcomeReport {
                iterations: vec![awaken_session_execution_contract::OutcomeIteration {
                    messages: Vec::new(),
                    outcome_id: outcome_id.clone(),
                    description: "ship the result".into(),
                    iteration: 1,
                    result: "satisfied".into(),
                    explanation: "correct".into(),
                    usage: None,
                }],
            },
        ),
    );
    warm.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        warm.list_events(&session.id, None, None, false)
            .unwrap()
            .data,
        prefix,
        "O2/E1-E2 unanchored terminal is not public"
    );

    let state_command = |key| {
        awaken_agent_contract::agent::state::Command::set(
            awaken_agent_contract::agent::state::Scope::Thread,
            awaken_agent_contract::agent::state::MergePolicy::Disjoint,
            key,
            serde_json::json!({"committed": true}),
        )
    };
    *runtime.outcome_commit_on_read.lock().unwrap() = vec![
        (
            state_command(format!("outcome/{outcome_id}/evaluation/1")),
            15,
        ),
        (state_command(format!("outcome/{outcome_id}/state")), 30),
        (
            awaken_agent_contract::agent::state::Command::remove(
                awaken_agent_contract::agent::state::Scope::Thread,
                awaken_agent_contract::agent::state::MergePolicy::Disjoint,
                "outcome/active",
            ),
            30,
        ),
    ];
    warm.refresh_committed_events(&session.id).await.unwrap();
    let final_warm = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(&final_warm[..prefix.len()], prefix.as_slice(), "O3/E1");
    let warm_suffix = warm
        .list_events(&session.id, Some(&cursor), None, false)
        .unwrap();
    assert_eq!(
        warm_suffix
            .data
            .iter()
            .filter(|event| event.type_str().starts_with("span.outcome_evaluation"))
            .count(),
        3,
        "O3/E3"
    );

    let cold = {
        let session_fixture_runtime = runtime;
        let session_fixture_clock = awaken_time::system_clock();
        crate::state::test_support::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            repository,
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    };
    cold.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        cold.list_events(&session.id, None, None, false)
            .unwrap()
            .data,
        final_warm,
        "O4/E4 full order"
    );
    assert_eq!(
        cold.list_events(&session.id, Some(&cursor), None, false)
            .unwrap(),
        warm_suffix,
        "O4/E4 old cursor suffix"
    );
}

#[tokio::test]
async fn terminal_committing_between_outcome_query_and_root_read_cannot_backfill_spans() {
    // Causes: O5 a retained DefineOutcome and completed Run prefix are already
    // public; O6 the Outcome query observes None, then one terminal state
    // commit and a later admitted root Running commit become visible before
    // the root recovery read; O7 another replica rebuilds that full prefix.
    // Effects: E5 the candidate retries or publishes all three terminal spans
    // before the later Running, never a partial later frontier; E6 the old
    // prefix and every cursor remain stable; E7 warm/cold full/suffix replay
    // agree; O8 the terminal query temporarily lags an already published
    // terminal root, then recovers. E8 that read serves the prior complete
    // frame without CAS/broadcast and exact recovery is inert. Rules
    // R5=O5+O6=>E5+E6; R6=R5+O7=>E7; R7=R5+O8=>E8. The existing recovery pause
    // is the query/read race; no test-only Outcome cache or second source.
    let runtime = LifecycleRuntime::default();
    let repository = Arc::new(ephemeral_session_repo().await);
    let warm = Arc::new({
        let session_fixture_runtime = runtime.clone();
        let session_fixture_clock = awaken_time::system_clock();
        crate::state::test_support::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            repository.clone(),
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    });
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let session_id = session.id.clone();
    let batch = warm
        .application
        .append_session_event_batch(
            &session_id,
            vec![SessionEventInput::DefineOutcome {
                description: "ship the result".into(),
                rubric: SessionOutcomeRubric::Text {
                    content: "correct".into(),
                },
                max_iterations: Some(1),
            }],
            None,
            None,
        )
        .await
        .unwrap();
    let SessionEventCommand::DefineOutcome { outcome_id, .. } = &batch.events[0].event else {
        panic!("O5 retained DefineOutcome")
    };
    let outcome_id = outcome_id.clone();
    persist_batch_projection_anchors(&warm, &session_id, &batch.batch_id, &[5]).await;
    let first_run = RunId("race-first".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            10,
            &session_id,
            &first_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            20,
            &session_id,
            &first_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    runtime.push_committed_message(
        19,
        Message::text(
            MessageId("race-first-output".into()),
            Role::Assistant,
            "draft",
        ),
    );
    warm.refresh_committed_events(&session_id).await.unwrap();
    let prior = warm
        .list_events(&session_id, None, None, false)
        .unwrap()
        .data;
    let old_cursor = prior.last().expect("O5 issued prefix").id.clone();
    // Reserve the next Session interval before its Runtime effect; the later
    // Run opening is valid only after this root admission fence.
    warm.application
        .activities()
        .begin_activity(&SessionId::from(&session_id))
        .await
        .unwrap();
    runtime.reads.reset();
    let (entered, release) = runtime.reads.pause_next("recovery");
    let reader = warm.clone();
    let id = session_id.clone();
    let pending = tokio::spawn(async move { reader.refresh_committed_events(&id).await });
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
        .await
        .expect("O6 root recovery begins after the old Outcome query");

    let state_command = |key| {
        awaken_agent_contract::agent::state::Command::set(
            awaken_agent_contract::agent::state::Scope::Thread,
            awaken_agent_contract::agent::state::MergePolicy::Disjoint,
            key,
            serde_json::json!({"committed": true}),
        )
    };
    runtime.state.lock().unwrap().extend([
        state_command(format!("outcome/{outcome_id}/evaluation/1")),
        state_command(format!("outcome/{outcome_id}/state")),
        awaken_agent_contract::agent::state::Command::remove(
            awaken_agent_contract::agent::state::Scope::Thread,
            awaken_agent_contract::agent::state::MergePolicy::Disjoint,
            "outcome/active",
        ),
    ]);
    runtime.state_cursors.lock().unwrap().extend([29, 30, 30]);
    runtime.outcome_projections.lock().unwrap().insert(
        outcome_id.clone(),
        awaken_session_execution_contract::CommittedOutcomeProjection::Completed(
            awaken_session_execution_contract::OutcomeReport {
                iterations: vec![awaken_session_execution_contract::OutcomeIteration {
                    messages: Vec::new(),
                    outcome_id: outcome_id.clone(),
                    description: "ship the result".into(),
                    iteration: 1,
                    result: "satisfied".into(),
                    explanation: "correct".into(),
                    usage: None,
                }],
            },
        ),
    );
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        32,
        &session_id,
        &RunId("race-later".into()),
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(10), pending)
        .await
        .expect("O6 bounded refresh")
        .unwrap()
        .expect("R5 complete candidate or source retry succeeds");
    let after = warm
        .list_events(&session_id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(
        serde_json::to_value(&after[..prior.len()]).unwrap(),
        serde_json::to_value(&prior).unwrap(),
        "R5/E6 prior prefix unchanged"
    );
    let types = after.iter().map(Event::type_str).collect::<Vec<_>>();
    let span = types
        .iter()
        .position(|kind| *kind == "span.outcome_evaluation_start")
        .expect("R5/E5 terminal spans cannot be omitted behind later facts");
    let later_running = types
        .iter()
        .rposition(|kind| *kind == "session.thread_status_running")
        .expect("R5 later admitted Run opening");
    assert!(span < later_running, "R5/E5 terminal30 before Running32");
    let terminal = runtime
        .outcome_projections
        .lock()
        .unwrap()
        .remove(&outcome_id)
        .expect("R7 committed terminal query can lag its root");
    let cache_revision = warm
        .sessions
        .lock()
        .unwrap()
        .get(&session_id)
        .unwrap()
        .cache_revision;
    let (_snapshot, mut receiver) = warm.stream_subscribe(&session_id).unwrap();
    let decision = warm
        .refresh_committed_events_for_observation(&session_id, None)
        .await
        .unwrap();
    assert_eq!(
        decision,
        super::super::committed_projection::ProjectionObservationDecision::ServePrior,
        "R7/E8 root terminal with absent query stutters"
    );
    assert_eq!(
        warm.sessions
            .lock()
            .unwrap()
            .get(&session_id)
            .unwrap()
            .cache_revision,
        cache_revision,
        "R7/E8 no partial CAS"
    );
    assert!(
        matches!(
            receiver.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "R7/E8 no partial broadcast"
    );
    runtime
        .outcome_projections
        .lock()
        .unwrap()
        .insert(outcome_id, terminal);
    warm.refresh_committed_events(&session_id).await.unwrap();
    assert_eq!(
        serde_json::to_value(
            warm.list_events(&session_id, None, None, false)
                .unwrap()
                .data
        )
        .unwrap(),
        serde_json::to_value(&after).unwrap(),
        "R7/E8 exact terminal query recovery"
    );
    let cold = {
        let session_fixture_runtime = runtime;
        let session_fixture_clock = awaken_time::system_clock();
        crate::state::test_support::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            repository,
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    };
    cold.refresh_committed_events(&session_id).await.unwrap();
    assert_eq!(
        serde_json::to_value(
            cold.list_events(&session_id, None, None, false)
                .unwrap()
                .data
        )
        .unwrap(),
        serde_json::to_value(&after).unwrap(),
        "R6/E7 full cold history"
    );
    for cursor in prior.iter().map(|event| event.id.as_str()) {
        assert_eq!(
            serde_json::to_value(
                cold.list_events(&session_id, Some(cursor), None, false)
                    .unwrap()
            )
            .unwrap(),
            serde_json::to_value(
                warm.list_events(&session_id, Some(cursor), None, false)
                    .unwrap()
            )
            .unwrap(),
            "R6/E7 old cursor {cursor}"
        );
    }
    assert!(
        !warm
            .list_events(&session_id, Some(&old_cursor), None, false)
            .unwrap()
            .data
            .is_empty(),
        "R5/E5 terminal and later facts extend old cursor"
    );
}
