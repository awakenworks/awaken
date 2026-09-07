use super::*;
use awaken_agent_contract::agent::state::StateKey;
use awaken_runtime_contract::model_operation::{
    ActiveModelOperation, ModelExecutionEvidence, ModelOperation,
};
use awaken_runtime_contract::resolved::ModelBinding;

#[tokio::test]
async fn cancelled_model_evidence_projects_without_rewriting_logical_outcome() {
    // C1 Cancelled Run; C2 journal absent/Unknown/NotDispatched/Terminated;
    // C3 same/foreign Run; C4 receipt after terminal; C5 warm/repeated/cold read.
    // R1 C1+Unknown+same=>one stable terminal error before idle; R2 positive or
    // absent/foreign=>no invented error; R3 C4 cannot rewrite the earlier error.
    // R4 all rows preserve Cancelled and never execute or acknowledge an attempt.
    // This proves projection, not physical termination or provider receipt truth.
    for evidence in [
        None,
        Some(ModelExecutionEvidence::Unknown),
        Some(ModelExecutionEvidence::NotDispatched),
        Some(ModelExecutionEvidence::Terminated),
    ] {
        for foreign in [false, true] {
            let runtime = LifecycleRuntime::default();
            let repository = Arc::new(ephemeral_session_repo());
            let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
            let session = warm
                .create_session(
                    serde_json::from_value(serde_json::json!({
                        "agent":"coder", "environment_id":"env_local"
                    }))
                    .unwrap(),
                    None,
                )
                .await
                .unwrap();
            let run = RunId("cancelled-model".into());
            let activity = warm.application.begin_activity(&session.id).await.unwrap();
            runtime.lifecycle.lock().unwrap().extend([
                lifecycle(
                    1,
                    &session.id,
                    &run,
                    RunLifecycleEventKind::Running,
                    RunState::Running,
                ),
                lifecycle(
                    3,
                    &session.id,
                    &run,
                    RunLifecycleEventKind::Cancelled,
                    RunState::Ended(EndCause::Cancelled),
                ),
            ]);
            if let Some(evidence) = evidence {
                let mut operation =
                    ModelOperation::next(None, ModelBinding::new("p", "m", "b")).unwrap();
                operation.execution = evidence;
                let mut command = ActiveModelOperation::write(&Some(operation.clone()));
                let owner = if foreign {
                    RunId("foreign".into())
                } else {
                    run.clone()
                };
                command.bind_run(&owner);
                runtime.state.lock().unwrap().push(command);
                runtime.state_cursors.lock().unwrap().push(2);
                // A later durable terminal receipt is a separate fact. The same
                // terminal event must keep its original content under cold replay.
                operation.execution = ModelExecutionEvidence::Terminated;
                let mut later = ActiveModelOperation::write(&Some(operation));
                later.bind_run(&run);
                runtime.state.lock().unwrap().push(later);
                runtime.state_cursors.lock().unwrap().push(4);
            }
            warm.application
                .settle_activity_observed(
                    &session.id,
                    activity.activity_epoch,
                    Some(awaken_session_contract::SessionRuntimeIntervalObservation {
                        activity_epoch: activity.activity_epoch,
                        thread_id: ThreadId(session.id.clone()),
                        run_id: run.clone(),
                        lifecycle_cursor: lifecycle_cursor(3),
                        source_commit_cursor: 3,
                    }),
                )
                .await
                .unwrap();
            let cold = ManagedState::new(runtime.clone()).with_session_repo(repository);
            let mut previous = None;
            for state in [&warm, &warm, &cold] {
                state.refresh_committed_events(&session.id).await.unwrap();
                let events = state
                    .list_events(&session.id, None, None, false)
                    .unwrap()
                    .data;
                let errors: Vec<_> = events
                    .iter()
                    .filter(|event| matches!(event.kind, OutboundKind::SessionError { .. }))
                    .collect();
                let expected = evidence == Some(ModelExecutionEvidence::Unknown) && !foreign;
                assert_eq!(errors.len(), usize::from(expected));
                let wire = serde_json::to_value(&errors).unwrap();
                if expected {
                    assert_eq!(wire[0]["error"]["retry_status"]["type"], "terminal");
                    let error_index = events
                        .iter()
                        .position(|event| matches!(event.kind, OutboundKind::SessionError { .. }))
                        .unwrap();
                    let idle_index = events
                        .iter()
                        .position(|event| {
                            matches!(event.kind, OutboundKind::SessionStatusIdle { .. })
                        })
                        .unwrap();
                    assert!(error_index < idle_index);
                }
                if let Some(previous) = &previous {
                    assert_eq!(&wire, previous);
                }
                previous = Some(wire);
            }
            assert_eq!(
                runtime.lifecycle.lock().unwrap().last().unwrap().state,
                RunState::Ended(EndCause::Cancelled)
            );
        }
    }
}

#[tokio::test]
async fn cancelled_child_uses_the_same_committed_execution_classifier() {
    // Extend root rules across the coordinated child boundary: Unknown versus
    // positive evidence, Session versus Thread view, warm/repeated/cold read.
    // C1 Unknown => one child-owned error visible in both views; C2 Terminated
    // => no fabricated failure. Event ids stay identical; cancellation is never
    // rewritten and the child cannot authorize parent physical settlement.
    for execution in [
        ModelExecutionEvidence::Unknown,
        ModelExecutionEvidence::Terminated,
    ] {
        let runtime = RehydrateFake::default();
        runtime
            .delegate_ids
            .lock()
            .unwrap()
            .push("researcher".into());
        let repository = Arc::new(ephemeral_session_repo());
        let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
        let session = warm
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent":"coder", "environment_id":"env_local"
                }))
                .unwrap(),
                None,
            )
            .await
            .unwrap();
        let child_id = "cancelled-child";
        let run = RunId("child-run".into());
        runtime.install_agent_coordination_prefix(
            &session.id,
            RunId("root".into()),
            0,
            "child",
            awaken_session_contract::CoordinatedThreadLink {
                session_id: session.id.clone(),
                thread_id: ThreadId(child_id.into()),
                target: awaken_session_contract::CoordinatedThreadTarget::Agent {
                    agent_id: "researcher".into(),
                },
                created_by_operation_id: ToolBatch::operation_id_for_step(
                    &RunId("root".into()),
                    0,
                    "child",
                ),
                latest_run_id: Some(run.clone()),
            },
        );
        runtime.lifecycle.lock().unwrap().extend([
            lifecycle(
                1,
                child_id,
                &run,
                RunLifecycleEventKind::Running,
                RunState::Running,
            ),
            lifecycle(
                3,
                child_id,
                &run,
                RunLifecycleEventKind::Cancelled,
                RunState::Ended(EndCause::Cancelled),
            ),
        ]);
        let mut operation = ModelOperation::next(None, ModelBinding::new("p", "m", "b")).unwrap();
        operation.execution = execution;
        let mut command = ActiveModelOperation::write(&Some(operation));
        command.bind_run(&run);
        runtime
            .state_by_thread
            .lock()
            .unwrap()
            .insert(child_id.into(), vec![command]);
        runtime
            .state_cursors_by_thread
            .lock()
            .unwrap()
            .insert(child_id.into(), vec![2]);
        let cold = ManagedState::new(runtime).with_session_repo(repository);
        let mut previous = None;
        for state in [&warm, &warm, &cold] {
            state.refresh_committed_events(&session.id).await.unwrap();
            let events = state
                .list_events(&session.id, None, None, false)
                .unwrap()
                .data;
            let errors: Vec<_> = events
                .iter()
                .filter(|event| matches!(event.kind, OutboundKind::SessionError { .. }))
                .collect();
            assert_eq!(
                errors.len(),
                usize::from(execution == ModelExecutionEvidence::Unknown)
            );
            let wire = serde_json::to_value(errors).unwrap();
            let child_events = state
                .list_thread_events(&session.id, child_id, None, None)
                .unwrap()
                .data;
            let child_errors: Vec<_> = child_events
                .iter()
                .filter(|event| matches!(event.kind, OutboundKind::SessionError { .. }))
                .collect();
            assert_eq!(serde_json::to_value(child_errors).unwrap(), wire);
            if let Some(previous) = &previous {
                assert_eq!(&wire, previous);
            }
            previous = Some(wire);
        }
    }
}
