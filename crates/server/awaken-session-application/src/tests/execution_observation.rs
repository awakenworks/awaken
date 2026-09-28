use super::*;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::kind::Kind as AuditKind;
use awaken_agent_contract::audit::record::Record as AuditRecord;
use awaken_runtime_contract::llm::{ModelRequestObservation, TokenUsage};
use awaken_session_types::SessionId;

fn observed_session(id: &str) -> PersistedSession {
    let mut session = persisted(id, false, "idle");
    let environment = session
        .frozen_baseline()
        .expect("fixture baseline")
        .environment
        .clone();
    session.baseline = awaken_session_configuration_contract::SessionBaselineState::Frozen(
        awaken_session_configuration_contract::SessionBaseline::compile_with_execution_model_ref(
            awaken_session_configuration_contract::SessionBaselineInputs {
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

fn profiled_observed_session(id: &str) -> PersistedSession {
    use awaken_session_configuration_contract::{
        ControlSessionCreationInputs, SessionCreationIntent,
    };
    let mut session = observed_session(id);
    let source = session.frozen_baseline().unwrap().clone();
    let compiled = SessionCreationIntent {
        control: ControlSessionCreationInputs {
            mutation_policy: awaken_session_resource_contract::SessionMutationPolicy::Frozen,
            environment: source.environment,
            runtime_placement: source.runtime_placement,
            agent_id: source.agent_id,
            agent_revision: source.agent_revision,
            model: source.model,
            execution_model_ref: source.execution_model_ref,
            model_override: source.model_override,
            system_prompt: *source.system_prompt,
            runtime: source.runtime,
            mcp_authoring: source.mcp_authoring,
            delegate_ids: source.delegate_ids,
            toolsets: source.toolsets,
            mounts: source.mounts,
            env: source.env,
            prompts: source.prompts,
            transcript_prefix: source.transcript_prefix,
            resources: Default::default(),
            initial_mcp: Vec::new(),
        },
    }
    .finalize()
    .unwrap();
    session.baseline =
        awaken_session_configuration_contract::SessionBaselineState::Frozen(compiled.baseline);
    session
}

struct ObservedDispatchFeed {
    events: Vec<awaken_dispatch_contract::DispatchOperationalEvent>,
    calls: std::sync::atomic::AtomicUsize,
    unavailable: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl awaken_dispatch_contract::DispatchOperationalFeed for ObservedDispatchFeed {
    async fn events_after(
        &self,
        after: awaken_dispatch_contract::DispatchCursor,
        limit: usize,
    ) -> Result<awaken_dispatch_contract::DispatchPage, awaken_dispatch_contract::DispatchError>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(awaken_dispatch_contract::DispatchError::Unavailable(
                "test source outage".into(),
            ));
        }
        let events = self
            .events
            .iter()
            .filter(|event| event.cursor.0 > after.0)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let next_cursor = events.last().map_or(after, |event| event.cursor);
        Ok(awaken_dispatch_contract::DispatchPage {
            events,
            next_cursor,
        })
    }
}

#[tokio::test]
async fn native_run_pages_need_no_managed_input_history_and_preserve_exact_cursors() {
    // Causes: C1 profiled owner/Run match; C2 Session has NO Managed input batch;
    // C3 another Run interleaves facts/output; C4 page limit/cursors; C5 Ended
    // truth is visible before the dispatch Done row. Effects: E1 exact Run data
    // is readable despite C2; E2 other Run facts/output never leak; E3 cursors
    // advance across inspected foreign facts; E4 separate pages preserve the
    // original Done cursor; E5 zero limit preserves cursors without reading the
    // feed; E6 the query performs no Session write. Terminal projection belongs
    // to the Flow consumer and must wait for that Done fact.
    // R1=C1+C2+C3+limit2 => E1/E2/E3/E6; R2=R1+next => E4;
    // R3=R1+limit0 => E5/E6; R4=exact replay => byte-identical page.
    use awaken_dispatch_contract::{
        DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOutcome, RunClaim,
    };
    let repository =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let session = profiled_observed_session("native-observed");
    assert!(
        session.event_batches.is_empty(),
        "C2 is the native admission gap"
    );
    create(repository.as_ref(), session).await;
    let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
    let run = RunId("native-a".into());
    let other = RunId("native-b".into());
    let ended = RunState::Ended(EndCause::Indeterminate);
    runtime.set_recovery_snapshot(
        "native-observed",
        &ThreadId("native-observed".into()),
        recovery_snapshot(
            "native-observed",
            vec![
                RunRecord {
                    id: run.clone(),
                    thread_id: ThreadId("native-observed".into()),
                    state: ended.clone(),
                },
                RunRecord {
                    id: other.clone(),
                    thread_id: ThreadId("native-observed".into()),
                    state: RunState::Ended(EndCause::NaturalEnd),
                },
            ],
            vec![
                Message::text(
                    MessageId::assistant(&run, 0),
                    Role::Assistant,
                    "exact native output",
                ),
                Message::text(
                    MessageId::assistant(&other, 0),
                    Role::Assistant,
                    "foreign later output",
                ),
            ],
            vec![
                AuditRecord {
                    sequence: 1_000,
                    run_id: run.clone(),
                    kind: AuditKind::RunStateChanged,
                    payload: serde_json::json!({"state":ended}),
                },
                AuditRecord {
                    sequence: 2_000,
                    run_id: other.clone(),
                    kind: AuditKind::RunStateChanged,
                    payload: serde_json::json!({"state":RunState::Ended(EndCause::NaturalEnd)}),
                },
            ],
        ),
    );
    let claim = |id: &RunId| RunClaim {
        run_id: id.clone(),
        owner: "worker".into(),
        epoch: 3,
    };
    let feed = Arc::new(ObservedDispatchFeed {
        events: vec![
            DispatchOperationalEvent {
                cursor: DispatchCursor(7),
                recorded_at_ms: None,
                operation: DispatchOperation::Claimed { claim: claim(&run) },
            },
            DispatchOperationalEvent {
                cursor: DispatchCursor(8),
                recorded_at_ms: None,
                operation: DispatchOperation::Claimed {
                    claim: claim(&other),
                },
            },
            DispatchOperationalEvent {
                cursor: DispatchCursor(9),
                recorded_at_ms: None,
                operation: DispatchOperation::Settled {
                    claim: claim(&run),
                    outcome: DispatchOutcome::Done,
                },
            },
        ],
        calls: 0.into(),
        unavailable: false.into(),
    });
    let application = application_with_runtime(
        runtime,
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    application
        .set_dispatch_observation_source(feed.clone())
        .unwrap();
    assert!(
        application
            .set_dispatch_observation_source(feed.clone())
            .is_err(),
        "one source, no replacement"
    );
    let id = SessionId::from("native-observed");
    let before = repository.get(&id).await.unwrap();
    let page = application
        .session_run_events_after(
            "workspace",
            &id,
            &run,
            DispatchCursor(0),
            awaken_agent_contract::RunLifecycleCursor(0),
            2,
        )
        .await
        .unwrap();
    assert_eq!(page.state, Some(ended), "E1/C5");
    assert_eq!(page.response, "exact native output", "E2");
    assert_eq!(page.dispatch.events.len(), 1, "E2");
    assert_eq!(page.dispatch.events[0].cursor.0, 7);
    assert_eq!(page.dispatch.next_cursor.0, 8, "E3");
    assert_eq!(page.lifecycle.events.len(), 1, "E2");
    assert_eq!(page.lifecycle.events[0].cursor.0, 1_000);
    assert_eq!(page.lifecycle.next_cursor.0, 2_000, "E3");
    let replay = application
        .session_run_events_after(
            "workspace",
            &id,
            &run,
            DispatchCursor(0),
            awaken_agent_contract::RunLifecycleCursor(0),
            2,
        )
        .await
        .unwrap();
    assert_eq!(replay, page, "R4");
    let next = application
        .session_run_events_after(
            "workspace",
            &id,
            &run,
            page.dispatch.next_cursor,
            page.lifecycle.next_cursor,
            2,
        )
        .await
        .unwrap();
    assert_eq!(next.dispatch.events[0].cursor.0, 9, "E4");
    assert!(next.lifecycle.events.is_empty());
    let calls = feed.calls.load(Ordering::SeqCst);
    let empty = application
        .session_run_events_after(
            "workspace",
            &id,
            &run,
            next.dispatch.next_cursor,
            next.lifecycle.next_cursor,
            0,
        )
        .await
        .unwrap();
    assert!(
        empty.dispatch.events.is_empty() && empty.lifecycle.events.is_empty(),
        "E5"
    );
    assert_eq!(empty.dispatch.next_cursor, next.dispatch.next_cursor);
    assert_eq!(empty.lifecycle.next_cursor, next.lifecycle.next_cursor);
    assert_eq!(feed.calls.load(Ordering::SeqCst), calls, "E5");
    assert_eq!(repository.get(&id).await.unwrap(), before, "E6");
}

#[tokio::test]
async fn native_run_observation_refuses_foreign_scope_missing_source_and_invalid_page() {
    // R1 foreign owner => NotFound before feed/read effects; R2 Managed policy
    // => NotFound before feed; R3 profiled owner + missing feed => Unavailable;
    // R4 limit>1024 => Rejected before feed; R5 exact source outage => Unavailable;
    // R6 unknown Run => NotFound, never reuse the latest Run. These cover scope,
    // mode, dependency, input and membership causes; all leave root unchanged.
    use awaken_dispatch_contract::DispatchCursor;
    let repository =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        repository.as_ref(),
        profiled_observed_session("native-refusals"),
    )
    .await;
    create(repository.as_ref(), observed_session("managed-refusals")).await;
    let application = application_with_runtime(
        Arc::new(RecordingAgentAdmissionRuntime::new([])),
        repository,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let id = SessionId::from("native-refusals");
    let run = RunId("missing".into());
    let read = |scope: &'static str, session: SessionId, limit| {
        let application = &application;
        let run = &run;
        async move {
            application
                .session_run_events_after(
                    scope,
                    &session,
                    run,
                    DispatchCursor(0),
                    awaken_agent_contract::RunLifecycleCursor(0),
                    limit,
                )
                .await
        }
    };
    assert!(
        matches!(
            read("foreign", id.clone(), 1).await,
            Err(crate::SessionProjectionRecoveryError::NotFound)
        ),
        "R1"
    );
    assert!(
        matches!(
            read("workspace", SessionId::from("managed-refusals"), 1).await,
            Err(crate::SessionProjectionRecoveryError::NotFound)
        ),
        "R2"
    );
    assert!(
        matches!(
            read("workspace", id.clone(), 1).await,
            Err(crate::SessionProjectionRecoveryError::Unavailable(_))
        ),
        "R3"
    );
    assert!(
        matches!(
            read("workspace", id.clone(), 1_025).await,
            Err(crate::SessionProjectionRecoveryError::Rejected(_))
        ),
        "R4"
    );
    let feed = Arc::new(ObservedDispatchFeed {
        events: Vec::new(),
        calls: 0.into(),
        unavailable: true.into(),
    });
    application
        .set_dispatch_observation_source(feed.clone())
        .unwrap();
    assert!(
        matches!(
            read("workspace", id.clone(), 1).await,
            Err(crate::SessionProjectionRecoveryError::Unavailable(_))
        ),
        "R5"
    );
    feed.unavailable.store(false, Ordering::SeqCst);
    assert!(
        matches!(
            read("workspace", id, 1).await,
            Err(crate::SessionProjectionRecoveryError::NotFound)
        ),
        "R6"
    );
}

#[tokio::test]
async fn native_run_reply_rejects_foreign_owner_mode_and_nonprimary_or_unfenced_input() {
    // The authorization/admission graph is serial: owner -> profiled mode ->
    // primary target -> positive original version -> stable operation -> Runtime
    // ticket owner. R1 foreign owner and R2 Managed mode stop before Runtime;
    // R3 child, absent/zero version, empty operation or adjacent System reject
    // before activity transfer. E1 no root mutation or delivery on every row.
    use awaken_session_execution_contract::{
        SessionThreadTarget, SessionThreadToolReply, SessionThreadToolReplyCommand,
    };
    let repository =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        repository.as_ref(),
        profiled_observed_session("native-reply-owner"),
    )
    .await;
    create(repository.as_ref(), observed_session("managed-reply-owner")).await;
    let application = application_with_runtime(
        Arc::new(RecordingAgentAdmissionRuntime::new([])),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let id = SessionId::from("native-reply-owner");
    let before = repository.get(&id).await.unwrap();
    let command = SessionThreadToolReplyCommand {
        session_id: id.clone(),
        tool_request_event_id: Some("reply-1".into()),
        expected_thread_version: Some(17),
        target: SessionThreadTarget::Primary,
        expected_run_id: RunId("run-1".into()),
        expected_correlation_id: "await-1".into(),
        tool_use_id: String::new(),
        reply: SessionThreadToolReply::Input("answer".into()),
        accompanying_system: None,
    };
    assert!(
        matches!(
            application
                .reply_profiled_session_run_for_owner("foreign", command.clone())
                .await,
            Err(crate::SessionProjectionRecoveryError::NotFound)
        ),
        "R1"
    );
    let mut managed = command.clone();
    managed.session_id = SessionId::from("managed-reply-owner");
    assert!(
        matches!(
            application
                .reply_profiled_session_run_for_owner("workspace", managed)
                .await,
            Err(crate::SessionProjectionRecoveryError::NotFound)
        ),
        "R2"
    );
    for invalid in [
        SessionThreadToolReplyCommand {
            target: SessionThreadTarget::Child(ThreadId("child".into())),
            ..command.clone()
        },
        SessionThreadToolReplyCommand {
            expected_thread_version: None,
            ..command.clone()
        },
        SessionThreadToolReplyCommand {
            expected_thread_version: Some(0),
            ..command.clone()
        },
        SessionThreadToolReplyCommand {
            tool_request_event_id: Some(" ".into()),
            ..command.clone()
        },
        SessionThreadToolReplyCommand {
            accompanying_system: Some(awaken_session_event_contract::SessionUserRunSystemInput {
                operation_id: "system".into(),
                content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "extra",
                )],
            }),
            ..command
        },
    ] {
        assert!(
            matches!(
                application
                    .reply_profiled_session_run_for_owner("workspace", invalid)
                    .await,
                Err(crate::SessionProjectionRecoveryError::Rejected(_))
            ),
            "R3"
        );
    }
    assert_eq!(repository.get(&id).await.unwrap(), before, "E1");
}

fn bind_user_command(session: &mut PersistedSession, idempotency_key: &str) -> RunId {
    let mut batch = awaken_session_event_contract::SessionEventBatch::compile(
        &session.session_id,
        format!("batch:{idempotency_key}"),
        vec![
            awaken_session_event_contract::SessionEventInput::UserMessage {
                content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "生成当前课件",
                )],
            },
        ],
    )
    .expect("valid User Event batch");
    batch
        .bind_idempotency(idempotency_key, format!("fingerprint:{idempotency_key}"))
        .expect("valid idempotency coordinate");
    let run_id = match &batch.events[0].event {
        awaken_session_event_contract::SessionEventCommand::UserMessage { run_id, .. } => {
            run_id.clone()
        }
        _ => unreachable!("fixture contains one User Message"),
    };
    session
        .install_initial_event_plan(awaken_session_event_contract::SessionInitialEventPlan {
            batch,
        })
        .expect("install Event batch");
    run_id
}

#[tokio::test]
async fn native_run_claim_is_visible_before_the_first_commit_only_with_exact_admission_receipt() {
    // Causes: C1 no committed Thread prefix; C2 exact native admission/activity
    // receipt exists; C3 dispatch claim is visible; C4 another Run is requested.
    // Effects: E1 C1+C2+C3 returns that original claim with state/version NONE,
    // unchanged lifecycle cursor and no ticket/output/usage; E2 !C2 or C4 is
    // NotFound; E3 read/replay performs no Session write. This tests the existing
    // receipt owner; native reservation/admission tests own receipt creation.
    use awaken_dispatch_contract::{
        DispatchCursor, DispatchOperation, DispatchOperationalEvent, RunClaim,
    };
    let repository =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        repository.as_ref(),
        profiled_observed_session("native-precommit"),
    )
    .await;
    let app = application_with_runtime(
        Arc::new(RecordingAgentAdmissionRuntime::new([])),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let id = SessionId::from("native-precommit");
    let run = RunId("native-claimed".into());
    let feed = Arc::new(ObservedDispatchFeed {
        events: vec![DispatchOperationalEvent {
            cursor: DispatchCursor(7),
            recorded_at_ms: None,
            operation: DispatchOperation::Claimed {
                claim: RunClaim {
                    run_id: run.clone(),
                    owner: "worker".into(),
                    epoch: 3,
                },
            },
        }],
        calls: 0.into(),
        unavailable: false.into(),
    });
    app.set_dispatch_observation_source(feed).unwrap();
    assert!(
        matches!(
            app.session_run_events_after(
                "workspace",
                &id,
                &run,
                DispatchCursor(0),
                awaken_agent_contract::RunLifecycleCursor(11),
                2
            )
            .await,
            Err(crate::SessionProjectionRecoveryError::NotFound)
        ),
        "E2 no receipt"
    );
    app.begin_activity_for_operation(
        &id,
        &awaken_session_execution_contract::session_run_activity_operation_id(&id, &run),
    )
    .await
    .unwrap();
    let before = repository.get(&id).await.unwrap();
    let page = app
        .session_run_events_after(
            "workspace",
            &id,
            &run,
            DispatchCursor(0),
            awaken_agent_contract::RunLifecycleCursor(11),
            2,
        )
        .await
        .unwrap();
    assert_eq!(page.dispatch.events[0].cursor.0, 7, "E1 original claim");
    assert_eq!(page.state, None, "E1 no fabricated Running truth");
    assert_eq!(page.thread_version, None);
    assert_eq!(page.lifecycle.next_cursor.0, 11);
    assert!(page.lifecycle.events.is_empty());
    assert!(page.resume_ticket.is_none() && page.usage.is_none() && page.response.is_empty());
    let replay = app
        .session_run_events_after(
            "workspace",
            &id,
            &run,
            DispatchCursor(0),
            awaken_agent_contract::RunLifecycleCursor(11),
            2,
        )
        .await
        .unwrap();
    assert_eq!(page, replay, "E3 exact read replay");
    assert!(
        matches!(
            app.session_run_events_after(
                "workspace",
                &id,
                &RunId("foreign".into()),
                DispatchCursor(0),
                awaken_agent_contract::RunLifecycleCursor(11),
                2
            )
            .await,
            Err(crate::SessionProjectionRecoveryError::NotFound)
        ),
        "E2 foreign Run"
    );
    assert_eq!(repository.get(&id).await.unwrap(), before, "E3");
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
    let batch = awaken_session_event_contract::SessionEventBatch::compile(
        &initial.session_id,
        "initial-batch",
        vec![
            awaken_session_event_contract::SessionEventInput::UserMessage {
                content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "生成初始课件",
                )],
            },
        ],
    )
    .expect("valid initial batch");
    let initial_run_id = match &batch.events[0].event {
        awaken_session_event_contract::SessionEventCommand::UserMessage { run_id, .. } => {
            run_id.clone()
        }
        _ => unreachable!("fixture contains one User Message"),
    };
    initial
        .install_initial_event_plan(awaken_session_event_contract::SessionInitialEventPlan {
            batch,
        })
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
        awaken_session_error_contract::RunErrorKind::Internal,
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
        awaken_session_error_contract::RunErrorKind::Internal,
        "R8"
    );
}
