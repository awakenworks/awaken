use super::*;

#[tokio::test]
async fn wire_tool_identity_and_permission_use_the_exact_selected_message_or_pending_run() {
    // Causes: C1 root/child reuse one call id and Message id; C2 two Messages
    // on the root reuse that call; C3 an audited pending Run has no Message;
    // C4 a tool-use DTO arrives without its selected source. Effects: E1 each
    // C1-C3 public id retains its exact Thread+Message/Run+call coordinates;
    // E2 the recorded permission comes from that same tuple; E3 C4 keeps the
    // existing internal refusal. Rules: Q1 ordinary root/child Message => E1+E2;
    // Q2 synthetic pending Run => E1+E2 without inheriting a Message decision;
    // Q3 missing source => E3. These wire inputs are exactly the two production
    // cases: one selected Message with encoder pending=None, or no Message and
    // one index-selected historical target. The unchanged canonical index owns
    // ambiguous/missing committed evidence, not another qualifier-side fold.
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
    let run = RunId("shared-run".into());
    let messages = [1, 2].map(|step| {
        Message::new(
            MessageId::assistant(&run, step),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "same",
                "read",
                serde_json::json!({}),
            )],
        )
    });
    let audit =
        |cursor, run_id: RunId, decision: &str| awaken_agent_contract::audit::record::Record {
            sequence: lifecycle_cursor(cursor).0,
            run_id,
            kind: awaken_agent_contract::audit::kind::Kind::PermissionDecided,
            payload: serde_json::json!({"call_id": "same", "decision": decision}),
        };
    let prefix = |thread_id: &str, messages: Vec<Message>, cursors, events| {
        awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
            thread_id: ThreadId(thread_id.into()),
            claimed_run_id: run.clone(),
            runs: vec![],
            latest_run_id: None,
            messages,
            message_commit_cursors: cursors,
            commit_timestamps: Default::default(),
            state: vec![],
            state_commit_cursors: vec![],
            events,
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 40,
            next_commit_ordinal: 0,
        }
    };
    let pending_run = RunId("pending-run".into());
    let root = prefix(
        &session.id,
        messages.to_vec(),
        vec![10, 20],
        vec![
            audit(10, run.clone(), "deny"),
            audit(20, run.clone(), "allow"),
            audit(30, pending_run.clone(), "ask"),
        ],
    );
    let children = std::collections::HashMap::from([(
        "child".into(),
        prefix(
            "child",
            vec![messages[0].clone()],
            vec![10],
            vec![audit(10, run.clone(), "ask")],
        ),
    )]);
    let historical = Default::default();
    let index = awaken_managed_session_events::OrderIndex::new(
        awaken_managed_session_events::OrderEvidence {
            updates: None,
            session_id: &session.id,
            batches: &[],
            closed_intervals: &[],
            budget_reach: &[],
            parent_terminal: Default::default(),
            lifecycle: &[],
            root: Some(&root),
            children: &children,
            links: &[],
            outcomes: &[],
            historical_pending: &historical,
        },
    );
    let mut record = state
        .sessions
        .lock()
        .unwrap()
        .get(&session.id)
        .unwrap()
        .clone();
    record.project_recorded_permissions(&index).unwrap();
    // P4 the complete reducer rejects a historical source-kind collision before
    // replacing the disposable cache. Every prior unambiguous wire decision
    // must remain available after this failed refresh.
    let retained_permissions = record.permission_decisions.clone();
    let alias_run = RunId("legacy-alias".into());
    let mut ambiguous = root.clone();
    ambiguous.messages = vec![Message::new(
        MessageId(alias_run.0.clone()),
        Role::Assistant,
        vec![ContentBlock::tool_use(
            "same",
            "read",
            serde_json::json!({}),
        )],
    )];
    ambiguous.message_commit_cursors = vec![20];
    ambiguous.events = vec![
        audit(10, alias_run.clone(), "allow"),
        audit(20, alias_run, "deny"),
    ];
    let empty_children = Default::default();
    let ambiguous_index = awaken_managed_session_events::OrderIndex::new(
        awaken_managed_session_events::OrderEvidence {
            updates: None,
            session_id: &session.id,
            batches: &[],
            closed_intervals: &[],
            budget_reach: &[],
            parent_terminal: Default::default(),
            lifecycle: &[],
            root: Some(&ambiguous),
            children: &empty_children,
            links: &[],
            outcomes: &[],
            historical_pending: &historical,
        },
    );
    assert!(
        record
            .project_recorded_permissions(&ambiguous_index)
            .is_err()
    );
    assert_eq!(record.permission_decisions, retained_permissions);
    let pending = Pending {
        tool_use_id: "same".into(),
        name: "read".into(),
        input: serde_json::json!({}),
        client_executed: false,
    };
    let cases = [
        (
            None,
            Some(&messages[0]),
            messages[0].id.0.as_str(),
            EvaluatedPermission::Deny,
        ),
        (
            None,
            Some(&messages[1]),
            messages[1].id.0.as_str(),
            EvaluatedPermission::Allow,
        ),
        (
            Some("child"),
            Some(&messages[0]),
            messages[0].id.0.as_str(),
            EvaluatedPermission::Ask,
        ),
        (None, None, pending_run.0.as_str(), EvaluatedPermission::Ask),
    ];
    let mut ids = std::collections::HashSet::new();
    for (owner, message, source, expected) in cases {
        let mut events = project_messages_with_mcp_ids(
            message.map_or(&[], std::slice::from_ref),
            message.is_none().then_some(&pending),
            &Default::default(),
            Default::default(),
            &Default::default(),
            &[],
        );
        ManagedState::qualify_tool_projection(&record, owner, Some(source), &[], &mut events)
            .unwrap();
        let tool = events
            .iter()
            .find(|event| ProjectedToolUseFamily::from_kind(&event.kind).is_some())
            .unwrap();
        let public_thread = ManagedState::public_tool_thread_id(&session.id, owner);
        let expected_id = managed_tool_event_id(&public_thread, source, "same");
        assert_eq!(
            tool.id.as_deref(),
            Some(expected_id.as_str()),
            "Q1/Q2 exact wire identity"
        );
        assert!(
            ids.insert(expected_id),
            "Q1/Q2 all occurrence identities stay distinct"
        );
        assert!(
            matches!(&tool.kind, OutboundKind::AgentToolUse {
            evaluated_permission: Some(actual), ..
        } if *actual == expected),
            "Q1/Q2 exact permission: {expected:?}"
        );
    }
    let mut missing = project_messages_with_mcp_ids(
        &messages[..1],
        None,
        &Default::default(),
        Default::default(),
        &Default::default(),
        &[],
    );
    let error =
        ManagedState::qualify_tool_projection(&record, None, None, &[], &mut missing).unwrap_err();
    assert_eq!(
        error.to_string(),
        RunError::internal("Managed tool call `same` has no committed message or Run identity")
            .to_string(),
        "Q3 original refusal, never a raw-call fallback"
    );
}

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
            let cold = {
                let session_fixture_runtime = runtime.clone();
                let session_fixture_clock = awaken_time::system_clock();
                crate::state::test_support::managed_state_from_parts(
                    std::sync::Arc::new(session_fixture_runtime),
                    None,
                    repository.clone(),
                    awaken_protocol_managed_testkit::environment_fixture(
                        session_fixture_clock.clone(),
                    )
                    .execution,
                    session_fixture_clock,
                    std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
                )
            };
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

#[tokio::test]
async fn historical_idle_keeps_exact_tool_sources_across_two_runs() {
    // Causes: C1 Run one commits ToolUse(same) at4 and its audited Awaiting5;
    // C2 a replacement cancels it, then Run two commits another ToolUse(same)
    // at9 and Awaiting10; C3 a cold replica reads the complete same prefix.
    // Effects: E1 the first idle keeps its original id AND first public tool
    // occurrence; E2 the second idle names only the second occurrence; E3 the
    // complete warm/cold histories agree with no rejected refresh. Rules:
    // R1=C1=>E1; R2=C1+C2=>E1+E2; R3=C1+C2+C3=>E1+E2+E3. Constraints: the
    // raw call id is deliberately reused, both sources are real committed
    // Messages from different Runs, and each historical target is fixed at
    // its own audit cursor. Existing LifecycleRuntime/SQLite/from_parts and
    // durable_history own the fixture and oracle; no sleep, new store, fake
    // projection or compatibility id. Whole-record latest_id aliases R2's
    // old idle to the new call and the real delivery guard must expose it.
    let runtime = LifecycleRuntime::default();
    let repository = Arc::new(ephemeral_session_repo().await);
    let clock = awaken_time::system_clock();
    let environments =
        awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution;
    let compiler = Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler);
    let warm = crate::state::test_support::managed_state_from_parts(
        Arc::new(runtime.clone()),
        None,
        repository.clone(),
        environments.clone(),
        clock.clone(),
        compiler.clone(),
    );
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
    let first_run = RunId("first-awaiting-run".into());
    let second_run = RunId("replacement-awaiting-run".into());
    let pending = Pending {
        tool_use_id: "same".into(),
        name: "read".into(),
        input: serde_json::json!({"path":"same-path"}),
        client_executed: false,
    };
    let first_message = Message::new(
        MessageId::assistant(&first_run, 0),
        Role::Assistant,
        vec![ContentBlock::tool_use(
            "same",
            "read",
            pending.input.clone(),
        )],
    );
    let target = awaken_protocol_managed_testkit::tool_call_target(
        pending.clone(),
        awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission,
    );
    let mut first_awaiting = lifecycle(
        5,
        &session.id,
        &first_run,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    );
    first_awaiting.await_reason = Some(target.reason());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            &session.id,
            &first_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        first_awaiting.clone(),
    ]);
    runtime.push_committed_message(4, first_message.clone());
    runtime
        .await_targets
        .lock()
        .unwrap()
        .insert(first_awaiting.cursor, target.clone());
    *runtime.pending.lock().unwrap() = Some(pending.clone());
    warm.refresh_committed_events(&session.id).await.unwrap();
    let first_history = super::timeline_scenarios::durable_history(&warm, &session.id);
    let first_idle_id = fact_event_id(
        &session.id,
        &awaken_managed_session_events::root_terminal_idle_key(&session.id, &first_awaiting),
    );
    let first_idle = first_history
        .iter()
        .find(|event| event.0 == first_idle_id)
        .expect("R1 the original Awaiting has its stable idle fact")
        .clone();
    let public_thread = ManagedState::public_tool_thread_id(&session.id, None);
    let first_tool = managed_tool_event_id(&public_thread, &first_message.id.0, "same");
    assert_eq!(
        first_idle.1["stop_reason"]["event_ids"],
        serde_json::json!([first_tool]),
        "R1 first idle owns first Message occurrence"
    );

    let second_message = Message::new(
        MessageId::assistant(&second_run, 0),
        Role::Assistant,
        vec![ContentBlock::tool_use(
            "same",
            "read",
            pending.input.clone(),
        )],
    );
    let mut second_awaiting = lifecycle(
        10,
        &session.id,
        &second_run,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    );
    second_awaiting.await_reason = Some(target.reason());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            6,
            &session.id,
            &first_run,
            RunLifecycleEventKind::Cancelled,
            RunState::Ended(awaken_agent_contract::agent::run::EndCause::Cancelled),
        ),
        lifecycle(
            7,
            &session.id,
            &second_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        second_awaiting.clone(),
    ]);
    runtime.push_committed_message(9, second_message.clone());
    runtime
        .await_targets
        .lock()
        .unwrap()
        .insert(second_awaiting.cursor, target);
    warm.refresh_committed_events(&session.id)
        .await
        .expect("R2 valid later occurrence cannot rewrite the historical idle payload");
    let complete = super::timeline_scenarios::durable_history(&warm, &session.id);
    assert!(
        complete.starts_with(&first_history),
        "R2 immutable published prefix"
    );
    assert_eq!(
        complete.iter().find(|event| event.0 == first_idle_id),
        Some(&first_idle),
        "R2 first stable id retains its complete original payload and time"
    );
    let second_idle_id = fact_event_id(
        &session.id,
        &awaken_managed_session_events::root_terminal_idle_key(&session.id, &second_awaiting),
    );
    let second_idle = complete
        .iter()
        .find(|event| event.0 == second_idle_id)
        .expect("R2 replacement Awaiting has its own stable idle fact");
    let second_tool = managed_tool_event_id(&public_thread, &second_message.id.0, "same");
    assert_ne!(first_tool, second_tool, "R2 distinct committed occurrences");
    assert_eq!(
        second_idle.1["stop_reason"]["event_ids"],
        serde_json::json!([second_tool]),
        "R2 second idle owns only the replacement occurrence"
    );
    let cold = crate::state::test_support::managed_state_from_parts(
        Arc::new(runtime),
        None,
        repository,
        environments,
        clock,
        compiler,
    );
    cold.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        super::timeline_scenarios::durable_history(&cold, &session.id),
        complete,
        "R3 full cold history equals the complete warm history"
    );
}
