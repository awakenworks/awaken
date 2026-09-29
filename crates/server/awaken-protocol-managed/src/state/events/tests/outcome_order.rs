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
    let repository = Arc::new(ephemeral_session_repo());
    let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
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

    let cold = ManagedState::new(runtime).with_session_repo(repository);
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
