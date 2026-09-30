//! The root lifecycle owns primary Thread Running across Session intervals.

use super::*;
use awaken_managed_session_events::{FactRole, IntervalStep, plan_interval_aggregate};

#[test]
fn hitl_interval_close_cannot_insert_running_before_an_issued_tool_use() {
    // Causes: H1 the root Run opened at commit 3; H2 another child closes a
    // Session interval at 4 without closing that root Run; H3 the root ToolUse
    // and answerable Awaiting commit at 7, and its interval closes later;
    // H4 a reader already issued the ToolUse at cursor 7/phase 50. Effects:
    // E1 root lifecycle emits exactly its real Running3; E2 interval planning
    // emits Usage/Idle but no second primary Running7 at phase 20; E3 H4's old
    // cursor stays a prefix on warm/cold rebuild. Decision row R1=H1-H4=>
    // E1+E2+E3. Boundary-only H1 absent is owned by the root lifecycle test's
    // synthetic Running row, not by this interval planner.
    let session_id = "hitl-root";
    let run = RunId("outcome-worker".into());
    let child_run = RunId("child-earlier".into());
    let mut awaiting = lifecycle(
        7,
        session_id,
        &run,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    );
    awaiting.await_reason =
        Some(awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission);
    let accepted = [
        lifecycle(
            3,
            session_id,
            &run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            4,
            "child-earlier",
            &child_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
        awaiting.clone(),
    ];
    let observation = |close, thread_id: &str, run_id: &RunId| {
        awaken_session_state_contract::SessionRuntimeIntervalObservation {
            activity_epoch: close,
            thread_id: ThreadId(thread_id.into()),
            run_id: run_id.clone(),
            lifecycle_cursor: lifecycle_cursor(close),
            source_commit_cursor: close,
        }
    };
    let interval = |id: &str, close, thread_id: &str, run_id: &RunId| {
        awaken_session_state_contract::SessionRuntimeInterval {
            interval_id: id.into(),
            activity_epoch: close,
            started_at_unix_ms: 0,
            ended_at_unix_ms: close,
            opened_revision: awaken_session_types::SessionRevision(close),
            closed_revision: awaken_session_types::SessionRevision(close + 1),
            admitted_after_commit_cursor: None,
            observations: vec![observation(close, thread_id, run_id)],
            usage: Default::default(),
            max_list_cost_minor: None,
        }
    };
    let mut persisted = crate::state::tests::sample_persisted(session_id);
    persisted.closed_runtime_intervals = vec![
        interval("child-close", 4, "child-earlier", &child_run),
        interval("hitl-close", 7, session_id, &run),
    ];
    let pending = std::collections::HashMap::from([(
        awaiting.cursor,
        Pending {
            tool_use_id: "outcome-write-1".into(),
            name: "write".into(),
            input: serde_json::json!({"path":"deliverable"}),
            client_executed: false,
        },
    )]);
    let opening = awaken_managed_session_events::root_lifecycle_opening_facts(session_id, &accepted);
    assert_eq!(
        opening.iter().map(|fact| fact.key.role).collect::<Vec<_>>(),
        [FactRole::RootLifecycleStatusRunning],
        "R1/E1 real Running3 owns the Thread"
    );
    let plan = plan_interval_aggregate(&persisted, &accepted, &pending).unwrap();
    assert!(
        plan.steps
            .iter()
            .any(|step| matches!(step, IntervalStep::Close { .. })),
        "R1/E2 Session interval still closes with its Usage/Idle pair"
    );
    assert!(
        !plan.steps.iter().any(|step| matches!(
            step,
            IntervalStep::AppendOnce(fact)
                if fact.key.role == FactRole::RootTerminalStatusRunning
        )),
        "R1/E2-E3 no phase-20 Running7 may be inserted before ToolUse7"
    );
}

#[tokio::test]
async fn late_hitl_activity_cannot_rewrite_issued_usage_or_old_cursors() {
    // Causes: H5 root Running3 and an answerable ToolUse/Awaiting7 are already
    // public; H6 a later Session activity opens with admitted_after=7, then
    // settles from that same earlier boundary; H7 an independent replica
    // rebuilds after the empty interval close. Effects: E4 H5's exact IDs,
    // payload, order and every cursor survive H6; E5 the interval records no
    // pre-admission observation or second Usage/Idle pair; E6 H7 returns the
    // same full history and every old-cursor suffix. Decision rows R2=H5+H6
    // =>E4+E5; R3=R2+H7=>E6. The committed ticket audit, not a live cache,
    // classifies the old ToolUse while Session CAS owns activity settlement.
    let runtime = LifecycleRuntime::default();
    let repository = Arc::new(ephemeral_session_repo());
    let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder", "environment_id": "env_local"
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let id = session.id.as_str();
    let run = RunId("outcome-worker".into());
    let call = "outcome-write-1";
    let input = serde_json::json!({"path":"deliverable"});
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        3,
        id,
        &run,
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    runtime.push_committed_message(
        7,
        Message::new(
            MessageId::assistant(&run, 0),
            Role::Assistant,
            vec![ContentBlock::tool_use(call, "write", input.clone())],
        ),
    );
    let target = awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall {
        reason: awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission,
        call_id: call.into(),
        tool: awaken_agent_contract::agent::awaiting::PendingTool {
            tool_id: "write".into(),
            arguments: input.clone(),
        },
        authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
    };
    let draft: awaken_agent_contract::audit::draft::Draft =
        awaken_agent_contract::audit::run_event::RunEvent::RunStateChanged {
            state: RunState::Awaiting,
            await_reason: Some(awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission),
            await_target: Some(target),
        }
        .into();
    runtime
        .audit_events
        .lock()
        .unwrap()
        .push(awaken_agent_contract::audit::record::Record {
            sequence: lifecycle_cursor(7).0,
            run_id: run.clone(),
            kind: draft.kind,
            payload: draft.payload,
        });
    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: call.into(),
        name: "write".into(),
        input,
        client_executed: false,
    });
    let mut awaiting = lifecycle(
        7,
        id,
        &run,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    );
    awaiting.await_reason =
        Some(awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission);
    runtime.lifecycle.lock().unwrap().push(awaiting.clone());
    warm.refresh_committed_events(id).await.unwrap();
    let prior = warm.list_events(id, None, None, false).unwrap().data;
    assert!(
        prior
            .iter()
            .any(|event| event.type_str() == "agent.tool_use")
    );
    assert!(
        prior
            .iter()
            .any(|event| event.type_str() == "session.usage")
    );
    let before = serde_json::to_value(&prior).unwrap();

    let admitted = warm
        .application
        .begin_activity(&SessionId::from(id))
        .await
        .unwrap();
    assert_eq!(
        admitted
            .running_interval
            .as_ref()
            .unwrap()
            .admitted_after_commit_cursor,
        Some(7),
        "R2 late Session activity cannot own prior Runtime commit"
    );
    let settled = warm
        .application
        .settle_activity_observed(
            &SessionId::from(id),
            admitted.activity_epoch,
            Some(
                awaken_session_state_contract::SessionRuntimeIntervalObservation {
                    activity_epoch: admitted.activity_epoch,
                    thread_id: ThreadId(id.into()),
                    run_id: run,
                    lifecycle_cursor: awaiting.cursor,
                    source_commit_cursor: 7,
                },
            ),
        )
        .await
        .unwrap();
    assert_eq!(settled.closed_runtime_intervals.len(), 1);
    assert!(
        settled.closed_runtime_intervals[0].observations.is_empty(),
        "R2/E5"
    );
    warm.refresh_committed_events(id).await.unwrap();
    assert_eq!(
        serde_json::to_value(warm.list_events(id, None, None, false).unwrap().data).unwrap(),
        before,
        "R2/E4-E5 complete previously issued prefix"
    );

    let cold = ManagedState::new(runtime).with_session_repo(repository);
    cold.refresh_committed_events(id).await.unwrap();
    assert_eq!(
        serde_json::to_value(cold.list_events(id, None, None, false).unwrap().data).unwrap(),
        before,
        "R3/E6 full cold replay"
    );
    for cursor in prior.iter().map(|event| event.id.as_str()) {
        assert_eq!(
            serde_json::to_value(cold.list_events(id, Some(cursor), None, false).unwrap()).unwrap(),
            serde_json::to_value(warm.list_events(id, Some(cursor), None, false).unwrap()).unwrap(),
            "R3/E6 old cursor {cursor}"
        );
    }
}
