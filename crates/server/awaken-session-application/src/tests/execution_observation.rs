use super::*;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::kind::Kind as AuditKind;
use awaken_agent_contract::audit::record::Record as AuditRecord;
use awaken_runtime_contract::llm::{ModelRequestObservation, TokenUsage};
use awaken_session_contract::SessionId;

fn observed_session(id: &str) -> PersistedSession {
    let mut session = persisted(id, false, "idle");
    let environment = session
        .frozen_baseline()
        .expect("fixture baseline")
        .environment
        .clone();
    session.baseline = awaken_session_contract::SessionBaselineState::Frozen(
        awaken_session_contract::SessionBaseline::compile_with_execution_model_ref(
            awaken_session_contract::SessionBaselineInputs {
                environment,
                runtime_placement: SessionRuntimePlacement::Local,
                mcp_authoring: Default::default(),
                agent_id: "courseware-agent".into(),
                agent_revision: Some(17),
                model: "deepseek-v4-pro".into(),
                model_override: None,
                runtime: Some("acp:claude".into()),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
            },
            "deepseek-v4-pro-execution".into(),
        ),
    );
    session
}

fn bind_user_command(session: &mut PersistedSession, idempotency_key: &str) -> RunId {
    let mut batch = awaken_session_contract::SessionEventBatch::compile(
        &session.session_id,
        format!("batch:{idempotency_key}"),
        vec![awaken_session_contract::SessionEventInput::UserMessage {
            content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                "生成当前课件",
            )],
        }],
    )
    .expect("valid User Event batch");
    batch
        .bind_idempotency(idempotency_key, format!("fingerprint:{idempotency_key}"))
        .expect("valid idempotency coordinate");
    let run_id = match &batch.events[0].event {
        awaken_session_contract::SessionEventCommand::UserMessage { run_id, .. } => run_id.clone(),
        _ => unreachable!("fixture contains one User Message"),
    };
    session
        .install_initial_event_plan(awaken_session_contract::SessionInitialEventPlan { batch })
        .expect("install Event batch");
    run_id
}

fn model_audit(sequence: u64, run_id: &RunId, is_error: bool, retry_count: u32) -> AuditRecord {
    AuditRecord {
        sequence,
        run_id: run_id.clone(),
        kind: AuditKind::ModelRequestCompleted,
        payload: serde_json::to_value(ModelRequestObservation {
            is_error,
            usage: TokenUsage::default(),
            retry_count,
        })
        .expect("model observation"),
    }
}

fn recovery_snapshot(
    session_id: &str,
    runs: Vec<RunRecord>,
    messages: Vec<Message>,
    events: Vec<AuditRecord>,
) -> awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
    let latest_run_id = runs.last().map(|run| run.id.clone());
    awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
        thread_id: ThreadId(session_id.into()),
        claimed_run_id: latest_run_id
            .clone()
            .unwrap_or_else(|| RunId("unclaimed".into())),
        runs,
        latest_run_id,
        message_commit_cursors: (1..=messages.len() as u64).collect(),
        messages,
        commit_timestamps: Default::default(),
        state: Vec::new(),
        state_commit_cursors: Vec::new(),
        events,
        resume_tickets: Vec::new(),
        thread_version: 4,
        store_cursor: 9,
        next_commit_ordinal: 2,
    }
}

#[test]
fn user_run_retry_identity_has_one_aggregate_owner_and_fails_closed_on_ambiguity() {
    // Cause/effect decision table: C1 one admitted HTTP batch owns the Run and
    // a retry key => return that exact key; C2 one create-time batch owns the
    // Run without a retry key => return historical unknown; C3 two retained
    // batches claim the same Run => reject. Effects E1-E3 preserve the Session
    // root as the only mapping authority. Rules C1=>E1, C2=>E2, C3=>E3.
    let mut session = observed_session("session-keyed");
    let run_id = bind_user_command(&mut session, "command-keyed");
    assert_eq!(
        session.user_run_idempotency_key(&run_id).unwrap(),
        Some("command-keyed"),
        "C1/E1"
    );

    let mut initial = observed_session("session-initial");
    let batch = awaken_session_contract::SessionEventBatch::compile(
        &initial.session_id,
        "initial-batch",
        vec![awaken_session_contract::SessionEventInput::UserMessage {
            content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                "生成初始课件",
            )],
        }],
    )
    .expect("valid initial batch");
    let initial_run_id = match &batch.events[0].event {
        awaken_session_contract::SessionEventCommand::UserMessage { run_id, .. } => run_id.clone(),
        _ => unreachable!("fixture contains one User Message"),
    };
    initial
        .install_initial_event_plan(awaken_session_contract::SessionInitialEventPlan { batch })
        .expect("install initial batch");
    assert_eq!(
        initial.user_run_idempotency_key(&initial_run_id).unwrap(),
        None,
        "C2/E2"
    );

    session.event_batches.push(session.event_batches[0].clone());
    assert!(session.user_run_idempotency_key(&run_id).is_err(), "C3/E3");
}

#[tokio::test]
async fn execution_observation_joins_only_same_snapshot_run_and_model_facts() {
    // Cause/effect decision table:
    // R1 frozen Session + two Runs + matching model audits => exact selection,
    // Run state, sequence, error and retry count are projected.
    // R2 a non-model audit on a known Run => ignored.
    // R3 a model audit whose Run is absent => ignored.
    // R4 a committed Run without a model audit => retained with an empty list,
    // never promoted to executed. R5 two source messages reuse one batch-local
    // tool call id => retain both exact source/call coordinates. The projection
    // performs no write or recovery.
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let mut session = observed_session("session-observed");
    let run_a = bind_user_command(&mut session, "command-observed");
    create(repository.as_ref(), session).await;
    let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
    let run_b = RunId("run-b".into());
    let unknown_run = RunId("run-unknown".into());
    runtime.set_recovery_snapshot(
        "session-observed",
        &ThreadId("session-observed".into()),
        recovery_snapshot(
            "session-observed",
            vec![
                RunRecord {
                    id: run_a.clone(),
                    thread_id: ThreadId("session-observed".into()),
                    state: RunState::Running,
                },
                RunRecord {
                    id: run_b.clone(),
                    thread_id: ThreadId("session-observed".into()),
                    state: RunState::Ended(EndCause::NaturalEnd),
                },
            ],
            vec![
                Message::new(
                    MessageId::assistant(&run_a, 0),
                    Role::Assistant,
                    vec![
                        awaken_agent_contract::agent::content::ContentBlock::ToolUse {
                            id: "tool-submit".into(),
                            name: "design_submit_artifact".into(),
                            input: serde_json::json!({}),
                        },
                    ],
                ),
                Message::new(
                    MessageId::assistant(&run_a, 1),
                    Role::Assistant,
                    vec![
                        awaken_agent_contract::agent::content::ContentBlock::ToolUse {
                            id: "tool-submit".into(),
                            name: "design_submit_artifact".into(),
                            input: serde_json::json!({ "revision": 2 }),
                        },
                    ],
                ),
                Message::new(
                    MessageId::assistant(&unknown_run, 0),
                    Role::Assistant,
                    vec![
                        awaken_agent_contract::agent::content::ContentBlock::ToolUse {
                            id: "tool-foreign".into(),
                            name: "design_submit_artifact".into(),
                            input: serde_json::json!({}),
                        },
                    ],
                ),
            ],
            vec![
                model_audit(3, &run_a, false, 1),
                AuditRecord {
                    sequence: 4,
                    run_id: run_a.clone(),
                    kind: AuditKind::StateChanged,
                    payload: serde_json::json!({ "commands": 1 }),
                },
                model_audit(5, &unknown_run, false, 0),
            ],
        ),
    );
    let application = application_with_runtime(
        runtime,
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let before = repository
        .get(&SessionId::from("session-observed"))
        .await
        .expect("before projection");

    let observed = application
        .session_execution_observation("workspace", &SessionId::from("session-observed"))
        .await
        .expect("R1-R4 projection");
    let first_tool_source = MessageId::assistant(&run_a, 0).0;
    let second_tool_source = MessageId::assistant(&run_a, 1).0;

    assert_eq!(observed.session_revision, before.revision.0, "R1");
    assert_eq!(observed.agent_id, "courseware-agent", "R1");
    assert_eq!(observed.agent_revision, Some(17), "R1");
    assert_eq!(observed.model, "deepseek-v4-pro", "R1");
    assert_eq!(
        observed.execution_model_ref, "deepseek-v4-pro-execution",
        "R1"
    );
    assert_eq!(observed.backend_ref.as_deref(), Some("acp:claude"), "R1");
    assert_eq!(
        observed.runs,
        vec![
            SessionRunExecutionObservation {
                run_id: run_a,
                idempotency_key: Some("command-observed".into()),
                state: RunState::Running,
                tool_uses: vec![
                    crate::SessionToolUseObservation {
                        source_message_id: first_tool_source,
                        call_id: "tool-submit".into(),
                    },
                    crate::SessionToolUseObservation {
                        source_message_id: second_tool_source,
                        call_id: "tool-submit".into(),
                    },
                ],
                model_requests: vec![SessionModelRequestObservation {
                    sequence: 3,
                    is_error: false,
                    retry_count: 1,
                }],
            },
            SessionRunExecutionObservation {
                run_id: run_b,
                idempotency_key: None,
                state: RunState::Ended(EndCause::NaturalEnd),
                tool_uses: Vec::new(),
                model_requests: Vec::new(),
            },
        ],
        "R1-R5"
    );
    assert_eq!(
        repository
            .get(&SessionId::from("session-observed"))
            .await
            .expect("after projection"),
        before,
        "read-only projection"
    );
}

#[tokio::test]
async fn execution_observation_fails_closed_on_scope_or_corrupt_committed_evidence() {
    // Cause/effect decision table:
    // R5 a foreign Workspace reads an existing Session => NotFound.
    // R6 no committed Thread exists => valid frozen evidence with zero Runs.
    // R7 a malformed ModelRequestCompleted payload => typed internal rejection;
    // R8 duplicate tool-use identity in one source message => typed internal
    // rejection. The same batch-local call id in different messages is covered
    // as a valid case by R5 above.
    // Neither corrupt source can become fabricated successful evidence.
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repository.as_ref(), observed_session("session-empty")).await;
    create(repository.as_ref(), observed_session("session-corrupt")).await;
    create(
        repository.as_ref(),
        observed_session("session-corrupt-tool"),
    )
    .await;
    let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
    let run_id = RunId("run-corrupt".into());
    runtime.set_recovery_snapshot(
        "session-corrupt",
        &ThreadId("session-corrupt".into()),
        recovery_snapshot(
            "session-corrupt",
            vec![RunRecord {
                id: run_id.clone(),
                thread_id: ThreadId("session-corrupt".into()),
                state: RunState::Running,
            }],
            Vec::new(),
            vec![AuditRecord {
                sequence: 1,
                run_id,
                kind: AuditKind::ModelRequestCompleted,
                payload: serde_json::json!({ "is_error": "not-a-boolean" }),
            }],
        ),
    );
    let tool_run_id = RunId("run-corrupt-tool".into());
    runtime.set_recovery_snapshot(
        "session-corrupt-tool",
        &ThreadId("session-corrupt-tool".into()),
        recovery_snapshot(
            "session-corrupt-tool",
            vec![RunRecord {
                id: tool_run_id.clone(),
                thread_id: ThreadId("session-corrupt-tool".into()),
                state: RunState::Running,
            }],
            vec![Message::new(
                MessageId::assistant(&tool_run_id, 0),
                Role::Assistant,
                vec![
                    awaken_agent_contract::agent::content::ContentBlock::ToolUse {
                        id: "tool-duplicate".into(),
                        name: "design_submit_artifact".into(),
                        input: serde_json::json!({}),
                    },
                    awaken_agent_contract::agent::content::ContentBlock::ToolUse {
                        id: "tool-duplicate".into(),
                        name: "design_submit_artifact".into(),
                        input: serde_json::json!({}),
                    },
                ],
            )],
            vec![model_audit(1, &tool_run_id, false, 0)],
        ),
    );
    let application = application_with_runtime(
        runtime,
        repository,
        Arc::new(RecordingEnvironmentSource::default()),
    );

    assert!(
        matches!(
            application
                .session_execution_observation("foreign", &SessionId::from("session-empty"))
                .await,
            Err(SessionProjectionRecoveryError::NotFound)
        ),
        "R5"
    );
    assert!(
        application
            .session_execution_observation("workspace", &SessionId::from("session-empty"))
            .await
            .expect("R6")
            .runs
            .is_empty(),
        "R6"
    );
    let Err(SessionProjectionRecoveryError::Rejected(error)) = application
        .session_execution_observation("workspace", &SessionId::from("session-corrupt"))
        .await
    else {
        panic!("R7 malformed audit must fail closed");
    };
    assert_eq!(
        error.kind,
        awaken_session_contract::RunErrorKind::Internal,
        "R7"
    );
    let Err(SessionProjectionRecoveryError::Rejected(error)) = application
        .session_execution_observation("workspace", &SessionId::from("session-corrupt-tool"))
        .await
    else {
        panic!("R8 duplicate tool identity must fail closed");
    };
    assert_eq!(
        error.kind,
        awaken_session_contract::RunErrorKind::Internal,
        "R8"
    );
}
