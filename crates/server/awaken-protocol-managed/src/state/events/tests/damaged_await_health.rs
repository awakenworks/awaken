//! Current-wait health and timeline admission consume one exact audit owner.

use super::*;

#[tokio::test]
async fn later_await_owns_its_target_without_repairing_an_earlier_missing_target() {
    // Causes: H1 the first Awaiting lifecycle commit has no target; H2 the
    // same Run writes another Awaiting audit with a target: the authoritative
    // classifier makes this a second lifecycle event; H3 a later Cancelled
    // commit closes that valid current wait. Effects: E1 H1 alone requires
    // recovery (caller-safe 400) with an unchanged memo/checkpoint; E2 H2
    // publishes one answerable call only at its own cursor, leaving H1's
    // historical pause without an answerable id; E3 H3 preserves every issued
    // id and emits no invented ToolResult. Rules W1=H1=>E1; W2=H1+H2=>E2;
    // W3=H1+H2+H3=>E3. Health and projection share exact audit ownership.
    // The lifecycle and audit rows share the production cursor encoder. An
    // extra committed message gives the second audit its own source commit.
    let runtime = LifecycleRuntime::default();
    let state = ManagedState::new(runtime.clone());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder", "environment_id": "env_local"
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let run = RunId("repeat-await-audit".into());
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        &session.id,
        &run,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    ));
    let before = state
        .sessions
        .lock()
        .unwrap()
        .get(&session.id)
        .cloned()
        .unwrap();
    let failure = state
        .refresh_committed_events(&session.id)
        .await
        .unwrap_err();
    assert!(
        failure.is_projection_recovery_required(),
        "W1/E1 first wait is damaged"
    );
    let after = state
        .sessions
        .lock()
        .unwrap()
        .get(&session.id)
        .cloned()
        .unwrap();
    assert_eq!(
        after.cache_revision, before.cache_revision,
        "W1/E1 no publication"
    );
    assert!(
        after.same_projected_content(&before).unwrap(),
        "W1/E1 no partial memo change"
    );
    let target = awaken_agent_contract::agent::awaiting::AwaitTarget::RemoteInput {
        reason: awaken_agent_contract::agent::awaiting::RemoteInputReason::UserInput,
        call_id: "later-only".into(),
    };
    let draft: awaken_agent_contract::audit::draft::Draft =
        awaken_agent_contract::audit::run_event::RunEvent::RunStateChanged {
            state: RunState::Awaiting,
            await_reason: Some(target.reason()),
            await_target: Some(target),
        }
        .into();
    runtime
        .audit_events
        .lock()
        .unwrap()
        .push(awaken_agent_contract::audit::record::Record {
            sequence: lifecycle_cursor(2).0,
            run_id: run.clone(),
            kind: draft.kind,
            payload: draft.payload,
        });
    runtime.push_committed_message(
        2,
        Message::text(MessageId::assistant(&run, 0), Role::Assistant, "progress"),
    );
    state.refresh_committed_events(&session.id).await.unwrap();
    let issued = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let calls = issued
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                OutboundKind::AgentToolUse { .. }
                    | OutboundKind::AgentCustomToolUse { .. }
                    | OutboundKind::AgentMcpToolUse { .. }
            )
        })
        .collect::<Vec<_>>();
    let pauses = issued
        .iter()
        .filter_map(|event| match &event.kind {
            OutboundKind::SessionThreadStatusIdle {
                stop_reason: StopReason::RequiresAction { event_ids },
                ..
            } => Some(event_ids),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1, "W2/E2 only the second wait owns a call");
    assert_eq!(pauses.len(), 2, "W2/E2 both committed waits have a status");
    assert!(
        pauses[0].is_empty(),
        "W2/E2 first wait cannot borrow later target"
    );
    assert_eq!(
        pauses[1],
        &vec![calls[0].id.clone()],
        "W2/E2 exact second wait"
    );

    runtime.lifecycle.lock().unwrap().push(lifecycle(
        3,
        &session.id,
        &run,
        RunLifecycleEventKind::Cancelled,
        RunState::Ended(EndCause::Cancelled),
    ));
    state.refresh_committed_events(&session.id).await.unwrap();
    let events = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(
        &events[..issued.len()],
        issued.as_slice(),
        "W3/E3 issued prefix stays intact"
    );
    assert!(
        events
            .iter()
            .all(|event| !event.kind.type_str().ends_with("tool_result")),
        "W3/E3 cancellation cannot invent a ToolResult"
    );
}
