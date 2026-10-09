use super::*;

#[tokio::test]
async fn client_result_index_is_occurrence_exact_when_runtime_call_ids_repeat() {
    // Cause/effect graph: C1 one custom tool occurrence has a qualified public
    // id and an admitted client result; C2 a later server-executed occurrence
    // reuses the same Runtime-local call id but has a different source id.
    // Effects: E1 C1's neutral Runtime result is suppressed; E2 C2's result is
    // retained and binds to C2, never C1. Decision table: R1=C1+!C2=>E1;
    // R2=C1+C2=>E2. This is the alias boundary omitted by family-only tests:
    // settlement is indexed by full public occurrence identity, not raw call id.
    let state = ManagedState::new(RehydrateFake::default()).await;
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
    let public_thread = ManagedState::public_tool_thread_id(&session.id, None);
    let first = managed_tool_event_id(&public_thread, "source-1", "same-call");
    let second = managed_tool_event_id(&public_thread, "source-2", "same-call");
    let mut record = {
        let mut records = state.sessions.lock().unwrap();
        let record = records.get_mut(&session.id).unwrap();
        record.events.extend([
            Event {
                id: first.clone(),
                kind: OutboundKind::AgentCustomToolUse {
                    name: "client_tool".into(),
                    input: serde_json::json!({}),
                    session_thread_id: None,
                },
                processed_at: Some(LEGACY_UNRECORDED_AT.into()),
            },
            Event {
                id: "client-result".into(),
                kind: OutboundKind::UserCustomToolResult {
                    custom_tool_use_id: first.clone(),
                    content: Some(vec![ContentBlock::text("client result")]),
                    is_error: false,
                    session_thread_id: None,
                },
                processed_at: Some(LEGACY_UNRECORDED_AT.into()),
            },
        ]);
        record.clone()
    };
    let result = || ProjectedEvent {
        id: None,
        kind: OutboundKind::AgentToolResult {
            tool_use_id: "same-call".into(),
            content: vec![ContentBlock::text("result")],
            is_error: Some(false),
        },
        // A result is not a call, so it carries no call identity of its own.
        source_tool_call_id: None,
    };
    let mut first_result = vec![result()];
    ManagedState::qualify_tool_projection(
        &record,
        None,
        &[],
        None,
        &[awaken_managed_session_events::ToolResultSource {
            source_id: "source-1".into(),
            call_id: "same-call".into(),
            name: "client_tool".into(),
        }],
        &mut first_result,
    )
    .unwrap();
    assert!(
        first_result.is_empty(),
        "R1/E1 exact client result owns the wire"
    );

    record.events.push(Event {
        id: second.clone(),
        kind: OutboundKind::AgentToolUse {
            name: "bash".into(),
            input: serde_json::json!({}),
            evaluated_permission: Some(EvaluatedPermission::Allow),
            evaluation: None,
            session_thread_id: None,
        },
        processed_at: Some(LEGACY_UNRECORDED_AT.into()),
    });
    let mut second_result = vec![result()];
    ManagedState::qualify_tool_projection(
        &record,
        None,
        &[],
        None,
        &[awaken_managed_session_events::ToolResultSource {
            source_id: "source-2".into(),
            call_id: "same-call".into(),
            name: "bash".into(),
        }],
        &mut second_result,
    )
    .unwrap();
    assert!(
        matches!(
            &second_result[..],
            [ProjectedEvent {
                kind: OutboundKind::AgentToolResult { tool_use_id, .. },
                ..
            }] if tool_use_id == &second
        ),
        "R2/E2 reused Runtime id binds only to its latest public occurrence"
    );
    // R3 a delayed replay of C1 follows C2: its committed source is still C1,
    // so C1's client receipt suppresses it despite the newer latest index.
    let mut delayed = vec![result()];
    ManagedState::qualify_tool_projection(
        &record,
        None,
        &[],
        None,
        &[awaken_managed_session_events::ToolResultSource {
            source_id: "source-1".into(),
            call_id: "same-call".into(),
            name: "client_tool".into(),
        }],
        &mut delayed,
    )
    .unwrap();
    assert!(
        delayed.is_empty(),
        "R3 delayed first occurrence remains settled"
    );
}

#[tokio::test]
async fn overlapping_reused_calls_keep_exact_result_sources_and_families_after_restart() {
    // C1 two same-id calls precede both results; C2 builtin/MCP order reversed;
    // C3 incremental prefix versus cold complete prefix. R1 C1+C2 => every
    // result points at its own source and family, never latest raw id; R2 C3
    // => equal complete wire histories at every observed prefix.
    for names in [["read", "mcp__srv__echo"], ["mcp__srv__echo", "read"]] {
        let runtime = LifecycleRuntime::default();
        let repository = Arc::new(ephemeral_session_repo().await);
        let warm = ManagedState::new(runtime.clone())
            .await
            .with_session_repo(repository.clone());
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
        let run = RunId("overlapping-run".into());
        runtime.lifecycle.lock().unwrap().push(lifecycle(
            1,
            &session.id,
            &run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ));
        let calls = names
            .iter()
            .enumerate()
            .map(|(step, name)| {
                Message::new(
                    MessageId::assistant(&run, step),
                    Role::Assistant,
                    vec![ContentBlock::tool_use("same", *name, serde_json::json!({}))],
                )
            })
            .collect::<Vec<_>>();
        let entries = [
            (2, calls[0].clone()),
            (3, calls[1].clone()),
            (
                4,
                Message::new(
                    MessageId("first-result".into()),
                    Role::Tool,
                    vec![ContentBlock::tool_result(
                        "same",
                        vec![ContentBlock::text("first")],
                    )],
                ),
            ),
            (
                5,
                Message::new(
                    MessageId("second-result".into()),
                    Role::Tool,
                    vec![ContentBlock::tool_result(
                        "same",
                        vec![ContentBlock::text("second")],
                    )],
                ),
            ),
        ];
        for length in 1..=entries.len() {
            runtime.replace_committed_messages(entries[..length].to_vec());
            warm.refresh_committed_events(&session.id).await.unwrap();
            let events = warm
                .list_events(&session.id, None, None, false)
                .unwrap()
                .data;
            let wire = serde_json::to_value(&events).unwrap();
            for index in 0..length.saturating_sub(2) {
                let content = if index == 0 { "first" } else { "second" };
                let result = wire
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|event| event["content"][0]["text"] == content)
                    .unwrap();
                let expected = managed_tool_event_id(
                    &ManagedState::public_tool_thread_id(&session.id, None),
                    &calls[index].id.0,
                    "same",
                );
                let mcp = awaken_agent_contract::McpToolId::claims_namespace(names[index]);
                assert_eq!(
                    result["type"],
                    if mcp {
                        "agent.mcp_tool_result"
                    } else {
                        "agent.tool_result"
                    },
                    "R1 family"
                );
                assert_eq!(
                    result[if mcp {
                        "mcp_tool_use_id"
                    } else {
                        "tool_use_id"
                    }],
                    expected,
                    "R1 exact source"
                );
            }
            let cold = ManagedState::new(runtime.clone())
                .await
                .with_session_repo(repository.clone());
            cold.refresh_committed_events(&session.id).await.unwrap();
            assert_eq!(
                serde_json::to_value(
                    cold.list_events(&session.id, None, None, false)
                        .unwrap()
                        .data
                )
                .unwrap(),
                wire,
                "R2 prefix {length}"
            );
        }
    }
}
