//! T12-C01 protocol rows: batch boundaries, invalid targets, the legal
//! budget-paused no-op and the admission error precedence of
//! `ManagedState::send_events(_attributed)`. Every payload is first parsed as
//! a valid `SendEventsRequest` (the JSON grammar accepts it), so each refusal
//! below is business admission, never wire grammar.

use super::*;
use awaken_protocol_managed_testkit::CoordinatedRuntimeFake;
use awaken_runtime_contract::runs::RunErrorKind;

/// Negative control: the payload is grammatically valid Managed wire JSON.
fn grammar_valid(payload: serde_json::Value) -> SendEventsRequest {
    serde_json::from_value(payload).expect("NC: the payload is a valid SendEventsRequest")
}

fn interrupt(thread: Option<&str>) -> SendEventsRequest {
    grammar_valid(serde_json::json!({
        "events": [{"type": "user.interrupt", "session_thread_id": thread}]
    }))
}

async fn root(state: &ManagedState, session_id: &str) -> awaken_session_contract::PersistedSession {
    state
        .roles
        .roots
        .session(&SessionId::from(session_id))
        .await
        .expect("root is readable")
}

async fn root_json(state: &ManagedState, session_id: &str) -> serde_json::Value {
    serde_json::to_value(root(state, session_id).await).unwrap()
}

fn assert_bad_request(error: &StateError, message: &str, row: &str) {
    match error {
        StateError::Run(error) => {
            assert_eq!(error.kind, RunErrorKind::BadRequest, "{row}: {error:?}");
            assert_eq!(error.code, "invalid_request", "{row}");
            assert_eq!(error.message, message, "{row}");
        }
        other => panic!("{row}: expected a bad request, got {other:?}"),
    }
}

async fn coordinated_root() -> (CoordinatedRuntimeFake, Arc<ManagedState>, String) {
    let runtime = CoordinatedRuntimeFake::default();
    runtime.defer_child_completion();
    let state = Arc::new(ManagedState::new(runtime.clone()).await);
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
    (runtime, state, session.id)
}

async fn coordinated_session() -> (CoordinatedRuntimeFake, Arc<ManagedState>, String) {
    let (runtime, state, session_id) = Box::pin(coordinated_root()).await;
    <CoordinatedRuntimeFake as SessionRuns>::run(
        &runtime,
        "coder",
        &session_id,
        vec![ContentBlock::text("coordinate")],
    )
    .await
    .expect("committed coordinated child");
    (runtime, state, session_id)
}

#[tokio::test]
async fn ghost_and_archived_child_interrupt_targets_have_zero_effect() {
    // | Row | interrupt selector             | effect                                     |
    // | T1  | unknown child ("ghost")        | BadRequest, root unchanged, 0 interrupts   |
    // | T2  | canonically archived child     | BadRequest, root unchanged, 0 interrupts   |
    // | T3  | live child (positive control)  | one batch, exactly one child interrupt     |
    let (runtime, state, session_id) = Box::pin(coordinated_session()).await;
    let child_id = CoordinatedRuntimeFake::CHILD_THREAD_ID;

    for (row, ghost) in [("T1", "sthr_ghost"), ("T1-legacy", "ghost")] {
        let before = root_json(&state, &session_id).await;
        let error = state
            .send_events(&session_id, interrupt(Some(ghost)))
            .await
            .expect_err("T1 ghost child target is refused");
        assert_bad_request(
            &error,
            "session_thread_id does not name a thread in this session",
            row,
        );
        assert_eq!(
            root_json(&state, &session_id).await,
            before,
            "{row}: root unchanged"
        );
        assert!(
            runtime.take_interrupts().is_empty(),
            "{row}: zero interrupt effects"
        );
    }

    let batches = root(&state, &session_id).await.event_batches.len();
    state
        .send_events(&session_id, interrupt(Some(child_id)))
        .await
        .expect("T3 live child interrupt is admitted");
    assert_eq!(
        runtime.take_interrupts(),
        vec![format!("child:{session_id}:{child_id}")],
        "T3 exactly the selected child"
    );
    assert_eq!(
        root(&state, &session_id).await.event_batches.len(),
        batches + 1,
        "T3"
    );

    <CoordinatedRuntimeFake as awaken_session_execution_contract::ChildRunCoordination>::archive_session_thread(
        &runtime,
        &SessionId::from(&session_id),
        child_id,
    )
    .await
    .expect("T2 canonical archive transition");
    let before = root_json(&state, &session_id).await;
    let error = state
        .send_events_attributed(
            &session_id,
            interrupt(Some(child_id)),
            None,
            Some("t2".into()),
        )
        .await
        .expect_err("T2 archived child target is refused");
    assert_bad_request(
        &error,
        "an archived or terminated session thread cannot be interrupted",
        "T2",
    );
    assert_eq!(
        root_json(&state, &session_id).await,
        before,
        "T2 root unchanged"
    );
    assert!(
        runtime.take_interrupts().is_empty(),
        "T2 zero interrupt effects"
    );
}

async fn plain_session() -> (RehydrateFake, Arc<ManagedState>, String) {
    let runtime = RehydrateFake::default();
    let state = Arc::new(ManagedState::new(runtime.clone()).await);
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
    (runtime, state, session.id)
}

fn user_messages(count: usize) -> SendEventsRequest {
    grammar_valid(serde_json::json!({
        "events": (0..count)
            .map(|index| serde_json::json!({
                "type": "user.message",
                "content": [{"type": "text", "text": format!("message {index}")}]
            }))
            .collect::<Vec<_>>()
    }))
}

fn file_documents(per_event: &[usize]) -> SendEventsRequest {
    let mut next = 0usize;
    grammar_valid(serde_json::json!({
        "events": per_event
            .iter()
            .map(|count| {
                let content = (0..*count)
                    .map(|_| {
                        next += 1;
                        serde_json::json!({
                            "type": "document",
                            "source": {"type": "file", "file_id": format!("file-{next}")}
                        })
                    })
                    .collect::<Vec<_>>();
                serde_json::json!({"type": "user.message", "content": content})
            })
            .collect::<Vec<_>>()
    }))
}

#[tokio::test]
async fn ordinary_batch_of_51_entries_is_one_batch_and_file_documents_cap_at_100() {
    // | Row | batch                                       | effect                       |
    // | B1  | 51 ordinary user.message entries            | accepted as exactly one batch |
    // | B2  | 101 file-sourced documents over two events  | BadRequest, zero facts        |
    // | B3  | exactly 100 file-sourced documents          | passes the cap                |
    let (_runtime, state, session_id) = Box::pin(plain_session()).await;
    let batches = root(&state, &session_id).await.event_batches.len();
    let receipt = state
        .send_events(&session_id, user_messages(51))
        .await
        .expect("B1 a 51-entry ordinary batch is admitted");
    assert_eq!(receipt.data.len(), 51, "B1 one receipt per entry");
    let persisted = root(&state, &session_id).await;
    assert_eq!(
        persisted.event_batches.len(),
        batches + 1,
        "B1 exactly one batch"
    );
    assert_eq!(
        persisted.event_batches.last().unwrap().events.len(),
        51,
        "B1 the batch retains every entry in one admission"
    );

    let before = root_json(&state, &session_id).await;
    let error = state
        .send_events_attributed(
            &session_id,
            file_documents(&[51, 50]),
            None,
            Some("b2".into()),
        )
        .await
        .expect_err("B2 101 file documents exceed the batch cap");
    assert_bad_request(
        &error,
        "event batch supports at most 100 file-sourced document blocks",
        "B2",
    );
    assert_eq!(
        root_json(&state, &session_id).await,
        before,
        "B2 zero facts"
    );

    if let Err(StateError::Run(error)) = state
        .send_events(&session_id, file_documents(&[50, 50]))
        .await
    {
        assert_ne!(
            error.message, "event batch supports at most 100 file-sourced document blocks",
            "B3 exactly 100 documents are within the cap"
        );
    }
}

fn budget_paused_root(session_id: &str) -> awaken_session_contract::PersistedSession {
    let mut persisted = crate::state::tests::sample_persisted(session_id);
    persisted.budget = awaken_session_contract::SessionBudgetState::Active {
        max_list_cost_minor: 1,
        consumed_numerator: awaken_session_contract::SessionBudgetState::MICROS_PER_MINOR_USD
            * awaken_session_contract::SessionBudgetState::COST_DENOMINATOR,
        usage_cursor: Default::default(),
        snapshot: awaken_session_contract::ManagedListPriceSnapshot {
            snapshot_id: "budget-snapshot".into(),
            version: 1,
            effective_at_unix_ms: 0,
            arithmetic_version: 1,
            model_rates: Default::default(),
            runtime_rates: Default::default(),
            fingerprint: "budget-snapshot".into(),
        },
        reach_transitions: Vec::new(),
    };
    persisted
}

#[tokio::test]
async fn budget_paused_interrupt_only_batch_is_a_legal_no_op() {
    // N1: a pure interrupt on an idle, budget-exhausted root is accepted with
    // no receipt, no appended batch, an unchanged revision and zero interrupt
    // calls. NC: the identical payload is grammatically valid and, on a root
    // that is not budget paused, is admitted (a batch is appended), so the
    // no-op is the business rule rather than a parse outcome.
    let session_id = "session-budget-paused-interrupt";
    let persisted = budget_paused_root(session_id);
    assert!(
        awaken_session_contract::session_is_budget_paused(&persisted),
        "N1 precondition: the root is budget paused"
    );
    let repository = Arc::new(ephemeral_session_repo().await);
    crate::state::test_support::create_session_fixture(
        repository.as_ref(),
        awaken_tenancy::DEFAULT_WORKSPACE_ID,
        persisted,
    )
    .await;
    let runtime = CoordinatedRuntimeFake::default();
    let clock = awaken_time::system_clock();
    let state = Arc::new(crate::state::test_support::managed_state_from_parts(
        Arc::new(runtime.clone()),
        None,
        repository.clone(),
        awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
        clock,
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    ));
    let before = root(&state, session_id).await;
    let response = state
        .send_events_attributed(session_id, interrupt(None), None, Some("paused".into()))
        .await
        .expect("N1 budget-paused interrupt is a legal no-op");
    assert!(response.data.is_empty(), "N1 no receipt");
    let after = root(&state, session_id).await;
    assert_eq!(after.revision, before.revision, "N1 revision unchanged");
    assert_eq!(
        after.event_batches.len(),
        before.event_batches.len(),
        "N1 no batch appended"
    );
    assert!(
        runtime.take_interrupts().is_empty(),
        "N1 zero interrupt calls"
    );

    // NC: the same payload on a live (not budget-paused) Session is admitted.
    let (live_runtime, live, live_id) = Box::pin(coordinated_session()).await;
    let batches = root(&live, &live_id).await.event_batches.len();
    live.send_events(&live_id, interrupt(None))
        .await
        .expect("NC the same interrupt is admitted when not paused");
    assert_eq!(
        root(&live, &live_id).await.event_batches.len(),
        batches + 1,
        "NC"
    );
    assert!(
        !live_runtime.take_interrupts().is_empty(),
        "NC interrupts reach Runtime"
    );
}

#[tokio::test]
async fn admission_error_precedence_follows_the_entry_table() {
    // Precedence table (first matching row wins), all over an archived root:
    // | Row | idempotency key       | payload               | effect             |
    // | P1  | none                  | invalid placement     | Archived           |
    // | P2  | retained, other body  | any                   | IdempotencyMismatch |
    // | P3  | retained, exact body  | the retained batch    | original receipt   |
    // The archived root is seeded with one settled batch retained under key
    // `kept`, exactly as admission stores it (key + request fingerprint).
    let session_id = "session-archived-precedence";
    let accepted = user_messages(1);
    let mut persisted = crate::state::tests::sample_persisted(session_id);
    let mut batch = awaken_session_contract::SessionEventBatch::compile(
        &SessionId::from(session_id),
        "kept-batch",
        vec![SessionEventInput::UserMessage {
            content: vec![ContentBlock::text("message 0")],
        }],
    )
    .expect("retained batch compiles");
    batch.admitted_revision = awaken_session_types::SessionRevision(1);
    batch.idempotency_key = Some("kept".into());
    batch.request_fingerprint = Some(awaken_agent_contract::stable_fingerprint(&(
        &accepted,
        &None::<String>,
    )));
    batch
        .anchor_admission_projection(awaken_session_contract::SessionEventProjectionAnchor {
            source_commit_cursor: 0,
        })
        .unwrap();
    batch.events[0].processed = true;
    let receipt = SendEventsResponse {
        data: accepted_inbound_receipts(session_id, &batch)
            .into_iter()
            .map(|projection| projection.event)
            .collect(),
    };
    persisted.event_batches.push(batch);
    persisted
        .archive("2026-10-24T00:00:00Z", 1_792_800_000_000)
        .expect("archive the seeded root");
    let repository = Arc::new(ephemeral_session_repo().await);
    crate::state::test_support::create_session_fixture(
        repository.as_ref(),
        awaken_tenancy::DEFAULT_WORKSPACE_ID,
        persisted,
    )
    .await;
    let clock = awaken_time::system_clock();
    let state = Arc::new(crate::state::test_support::managed_state_from_parts(
        Arc::new(RehydrateFake::default()),
        None,
        repository.clone(),
        awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
        clock,
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    ));
    let session_id = session_id.to_owned();
    assert!(
        root(&state, &session_id).await.archived_at().is_some(),
        "archived root"
    );
    let before = root_json(&state, &session_id).await;

    // A system.message first is an invalid placement the validator rejects.
    let misplaced = grammar_valid(serde_json::json!({
        "events": [
            {"type": "system.message", "content": [{"type": "text", "text": "context"}]},
            {"type": "user.message", "content": [{"type": "text", "text": "after"}]}
        ]
    }));
    assert!(
        matches!(
            state.send_events(&session_id, misplaced.clone()).await,
            Err(StateError::Archived)
        ),
        "P1 archived precedes placement validation"
    );
    assert!(
        matches!(
            state
                .send_events_attributed(&session_id, misplaced, None, Some("kept".into()))
                .await,
            Err(StateError::IdempotencyMismatch)
        ),
        "P2 a reused key with another body is a mismatch before archived"
    );
    let replay = state
        .send_events_attributed(&session_id, accepted, None, Some("kept".into()))
        .await
        .expect("P3 exact replay on an archived root returns its receipt");
    assert_eq!(
        replay
            .data
            .iter()
            .map(|event| (event.id.clone(), event.type_str()))
            .collect::<Vec<_>>(),
        receipt
            .data
            .iter()
            .map(|event| (event.id.clone(), event.type_str()))
            .collect::<Vec<_>>(),
        "P3 the original receipt identity"
    );
    assert_eq!(
        root_json(&state, &session_id).await,
        before,
        "P1-P3 root unchanged"
    );
}

#[tokio::test]
async fn budget_paused_mixed_batch_interrupt_follows_the_interrupt_only_target_law() {
    // Regression input: a budget-paused, idle root with no pending tool reply
    // receives [user.interrupt(session_thread_id = "sthr_ghost"),
    // user.message]. The parser used to drop that Interrupt before lowering
    // its target, so the batch was accepted while [user.interrupt("sthr_ghost")]
    // on the same root is refused. The parser now lowers every Interrupt and
    // the application decides the budget pause on the root it commits against.
    // | Row | batch | effect |
    // |---|---|---|
    // | G1 | [interrupt(ghost)] | BadRequest, root unchanged |
    // | G2 | [interrupt(ghost), message] | BadRequest, root unchanged |
    // | A1 | [interrupt, message], no live Run | message retained, interrupt absorbed |
    let session_id = "session-budget-paused-mixed";
    let repository = Arc::new(ephemeral_session_repo().await);
    crate::state::test_support::create_session_fixture(
        repository.as_ref(),
        awaken_tenancy::DEFAULT_WORKSPACE_ID,
        budget_paused_root(session_id),
    )
    .await;
    let runtime = CoordinatedRuntimeFake::default();
    let clock = awaken_time::system_clock();
    let state = Arc::new(crate::state::test_support::managed_state_from_parts(
        Arc::new(runtime.clone()),
        None,
        repository.clone(),
        awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
        clock,
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    ));
    let ghost = serde_json::json!({"type": "user.interrupt", "session_thread_id": "sthr_ghost"});
    let message = serde_json::json!({
        "type": "user.message", "content": [{"type": "text", "text": "held"}]
    });
    for (row, events) in [
        ("G1", serde_json::json!([ghost.clone()])),
        ("G2", serde_json::json!([ghost, message.clone()])),
    ] {
        let before = root_json(&state, session_id).await;
        let error = state
            .send_events(
                session_id,
                grammar_valid(serde_json::json!({ "events": events })),
            )
            .await
            .expect_err("a ghost Interrupt target is refused while budget paused");
        assert_bad_request(
            &error,
            "session_thread_id does not name a thread in this session",
            row,
        );
        assert_eq!(root_json(&state, session_id).await, before, "{row}");
    }

    let batches = root(&state, session_id).await.event_batches.len();
    state
        .send_events(
            session_id,
            grammar_valid(serde_json::json!({
                "events": [{"type": "user.interrupt"}, message]
            })),
        )
        .await
        .expect("A1 the ordinary half of the batch is admitted");
    let after = root(&state, session_id).await;
    assert_eq!(after.event_batches.len(), batches + 1, "A1 one batch");
    assert!(
        after.event_batches[batches]
            .events
            .iter()
            .all(|entry| !matches!(
                entry.event,
                awaken_session_contract::SessionEventCommand::Interrupt { .. }
            )),
        "A1 the budget pause absorbed the Interrupt"
    );
    assert!(
        runtime.take_interrupts().is_empty(),
        "A1 zero interrupt calls"
    );
}
