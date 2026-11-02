//! T12-C03 (暂停与事务失败), application rows: a budget-paused Session
//! receives interleaved ordinary and interrupt-only batches through the
//! production `admit_session_event_batch_idempotent` root-CAS admission.
//!
//! Oracles (groups-2.json order):
//! - O0 only actions the existing policy allows are admitted;
//! - O1 records written in one transaction roll back together (the SQL
//!   trigger rows live in `awaken-session-store/tests/pause_transaction.rs`,
//!   which owns raw SQL access to both backends).
//!
//! Existing policy (protocol `send_event_batch`): while the Session is budget
//! paused, an ordinary batch is retained and waits for the allowance; an
//! interrupt-only batch has nothing to stop and is an accepted no-op.
//!
//! | Row | Root | Batch | Expected |
//! |---|---|---|---|
//! | P1 | budget paused, idle | ordinary | retained; drive activates no Run and opens no epoch |
//! | P2 | paused, P1 retained | interrupt-only | accepted no-op: no batch, no root fact |
//! | P3 | paused, P1 retained | ordinary | retained after P1, order kept, still no activation |
//! | P4 | file reopen | - | P1/P3 retained exactly once, still unactivated |
//!
//! Negative control: keeping the pause rule only in the protocol parser must
//! fail. P2 goes through the application admission (the root-CAS boundary a
//! second protocol or a racing pause transition reaches), not the parser.
use super::*;

const CONSUMED_MINOR: u64 = 2;

fn paused_session(id: &str) -> PersistedSession {
    let legacy = persisted(id, false, "idle");
    let mut session = PersistedSession::frozen_with_budget(
        id,
        legacy.frozen_baseline().unwrap().clone(),
        legacy.resources,
        legacy.mcp,
        legacy.title,
        legacy.metadata,
        legacy.tools,
        legacy.budget,
    );
    session.execution = legacy.execution;
    session.budget = awaken_session_contract::SessionBudgetState::Active {
        max_list_cost_minor: 1,
        consumed_numerator: u128::from(CONSUMED_MINOR)
            * awaken_session_contract::SessionBudgetState::MICROS_PER_MINOR_USD
            * awaken_session_contract::SessionBudgetState::COST_DENOMINATOR,
        usage_cursor: Default::default(),
        snapshot: awaken_session_contract::ManagedListPriceSnapshot {
            snapshot_id: "t12-prices-v1".into(),
            version: 1,
            effective_at_unix_ms: 1,
            arithmetic_version: 1,
            model_rates: Default::default(),
            runtime_rates: Default::default(),
            fingerprint: "t12-prices-v1-fingerprint".into(),
        },
        reach_transitions: Vec::new(),
    };
    assert!(
        awaken_session_contract::session_is_budget_paused(&session),
        "fixture is budget paused"
    );
    session
}

fn user(text: &str) -> SessionEventInput {
    SessionEventInput::UserMessage {
        content: vec![ContentBlock::text(text)],
    }
}

fn interrupt_only() -> SessionEventInput {
    SessionEventInput::Interrupt(SessionEventInterrupt {
        requested_target: None,
        targets: vec![SessionThreadTarget::Primary],
    })
}

struct Fixture {
    _directory: tempfile::TempDir,
    path: String,
    store: Arc<dyn crate::SessionStore>,
    runtime: Arc<EventBatchRuntime>,
    app: SessionApplication,
}

async fn fixture(id: &str) -> Fixture {
    let directory = tempfile::tempdir().expect("T12 SQLite directory");
    let path = directory
        .path()
        .join("t12-pause.db")
        .to_str()
        .unwrap()
        .to_owned();
    let store: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(&path)
            .await
            .expect("T12 file SQLite"),
    );
    create(store.as_ref(), paused_session(id)).await;
    let runtime = Arc::new(EventBatchRuntime::default());
    // This table models one primary Thread, with no coordinated child Run.
    // The shared reply fixture otherwise advertises an unrelated child Run.
    *runtime.coordinated_links.lock().unwrap() = Some(Vec::new());
    let app = application_with_runtime(
        runtime.clone(),
        store.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    Fixture {
        _directory: directory,
        path,
        store,
        runtime,
        app,
    }
}

async fn append(
    app: &SessionApplication,
    session: &SessionId,
    input: SessionEventInput,
    key: &str,
) -> Result<awaken_session_contract::SessionEventBatch, RunError> {
    app.event_batches
        .admit_session_event_batch_idempotent(
            session,
            vec![input],
            None,
            None,
            Some((key.into(), format!("fingerprint-{key}"))),
        )
        .await?
        .into_batch()
        .ok_or_else(|| {
            RunError::classified(
                "test_expected_batch",
                "ordinary input was acknowledged without a batch",
            )
        })
}

fn assert_nothing_activated(fixture: &Fixture, root: &PersistedSession, row: &str) {
    assert!(
        fixture.runtime.activated.lock().unwrap().is_empty(),
        "{row}/O0 a budget-paused Session activates no Run"
    );
    assert!(
        root.active_activity_epochs.is_empty(),
        "{row}/O0 no activity epoch while paused"
    );
    assert_eq!(
        root.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "{row}/O0 still idle"
    );
}

#[tokio::test]
async fn budget_paused_session_retains_ordinary_batches_without_execution() {
    let id = "t12-paused-ordinary";
    let session = SessionId::from(id);
    let fixture = fixture(id).await;

    // P1: an ordinary batch is retained and waits for the allowance.
    let first = append(&fixture.app, &session, user("first"), "p1")
        .await
        .expect("P1 ordinary batch is admitted while paused");
    let _ = fixture
        .app
        .event_batches
        .drive_session_event_batches(&session, None)
        .await;
    let root = fixture.store.get(&session).await.unwrap();
    assert_eq!(
        root.event_batches
            .iter()
            .map(|batch| batch.batch_id.clone())
            .collect::<Vec<_>>(),
        vec![first.batch_id.clone()],
        "P1 one retained batch"
    );
    assert!(!root.event_batches[0].is_complete(), "P1 waits for budget");
    assert_nothing_activated(&fixture, &root, "P1");

    // P3: a second ordinary batch is appended after P1; order is kept.
    let third = append(&fixture.app, &session, user("third"), "p3")
        .await
        .expect("P3 ordinary batch is admitted while paused");
    let _ = fixture
        .app
        .event_batches
        .drive_session_event_batches(&session, None)
        .await;
    let root = fixture.store.get(&session).await.unwrap();
    assert_eq!(
        root.event_batches
            .iter()
            .map(|batch| batch.batch_id.clone())
            .collect::<Vec<_>>(),
        vec![first.batch_id.clone(), third.batch_id.clone()],
        "P3 retained in admission order"
    );
    assert_nothing_activated(&fixture, &root, "P3");

    // Exact replays return the original receipts and append nothing.
    assert_eq!(
        append(&fixture.app, &session, user("first"), "p1")
            .await
            .unwrap(),
        first,
        "P1 exact replay"
    );
    assert_eq!(
        fixture.store.get(&session).await.unwrap().revision,
        root.revision,
        "P1 replay appends no fact"
    );

    // P4: the file database reopens with exactly the same retained batches.
    let reopened = awaken_session_store::SqliteManagedSessionRepository::open(&fixture.path)
        .await
        .expect("P4 reopen");
    use awaken_session_contract::SessionRepositoryReads as _;
    assert_eq!(
        reopened.get(&session).await.unwrap(),
        root,
        "P4 durable root"
    );
}

#[tokio::test]
async fn budget_paused_interrupt_only_batch_is_a_no_op_at_root_admission() {
    // DT-N: C1 budget paused/resumed; C2 original/changed fingerprint; C3 no
    // admitted activity and a stable, non-Awaiting Thread prefix. N1 new+C1
    // paused+C3 => NoOp plus one canonical receipt, byte-equal root/outbox;
    // N2 exact+C1 resumed => replay NoOp before current admission policy;
    // N3 changed fingerprint => conflict, preserving the original receipt.
    // C4 the allowance update commits but Runtime budget discovery is unavailable:
    // N2 must still replay NoOp without consulting Runtime or opening an epoch.
    // Constraint: no empty batch, Running epoch or terminal Run is fabricated.
    let id = "t12-paused-interrupt";
    let session = SessionId::from(id);
    let fixture = fixture(id).await;
    append(&fixture.app, &session, user("held"), "p1")
        .await
        .expect("P1 ordinary batch");
    let before = fixture.store.get(&session).await.unwrap();
    let outbox = fixture.store.pending_lifecycle().await.unwrap();
    let submit = |fingerprint: &str| {
        fixture
            .app
            .event_batches
            .admit_session_event_batch_idempotent(
                &session,
                vec![interrupt_only()],
                None,
                None,
                Some(("p2".into(), fingerprint.to_owned())),
            )
    };
    assert!(matches!(
        submit("fingerprint-p2").await.unwrap(),
        crate::SessionEventBatchResult::NoOp
    ));
    let after = fixture.store.get(&session).await.unwrap();
    assert_eq!(
        after.event_batches.len(),
        before.event_batches.len(),
        "P2/O0 an interrupt-only batch on a budget-paused Session is an accepted no-op"
    );
    assert_eq!(after.revision, before.revision, "P2/O0 no root fact");
    assert_eq!(after, before, "N1 every root field is unchanged");
    assert_eq!(
        fixture.store.pending_lifecycle().await.unwrap(),
        outbox,
        "N1 no outbox fact"
    );
    let key = awaken_session_contract::IdempotencyRecord::event_admission("p2", "fingerprint-p2");
    let receipt = fixture
        .store
        .idempotency_receipt(&session, &key.key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        receipt.kind(),
        awaken_session_contract::SessionCommandReceiptKind::EventNoOp
    );
    assert_eq!(receipt.committed_revision, before.revision);
    assert_eq!(
        submit("changed").await.unwrap_err().code,
        "idempotency_conflict",
        "N3"
    );

    // N2 changes allowance through the canonical update owner, including its
    // immutable configuration history; raw replacement is intentionally illegal.
    let update = fixture
        .app
        .update_commands()
        .update_session(
            &session,
            crate::SessionUpdateCommand {
                title: None,
                metadata: None,
                budget: Some(awaken_session_contract::SessionFieldUpdate::Replace(
                    CONSUMED_MINOR + 1,
                )),
                tools: None,
                mcp_update: None,
                idempotency_key: Some("resume-budget".into()),
                request_fingerprint: "resume-budget-fingerprint".into(),
                if_match: Some(after.revision),
            },
        )
        .await
        .expect_err("C4 the fixture deliberately has no budget-discovery service");
    let crate::SessionUpdateError::ProjectionAfterCommit { outcome, source } = update else {
        panic!("C4 requires a committed allowance update, not a rejected update");
    };
    assert!(outcome.command_applied && outcome.changes.budget());
    assert_eq!(source.kind, awaken_session_types::RunErrorKind::Unavailable);
    let resumed = fixture.store.get(&session).await.unwrap();
    assert!(!awaken_session_contract::session_is_budget_paused(&resumed));
    *fixture.runtime.recovery_read_error.lock().unwrap() = Some(RunError::unavailable(
        "N2 must replay before Runtime observations",
    ));
    assert!(
        matches!(
            submit("fingerprint-p2").await.unwrap(),
            crate::SessionEventBatchResult::NoOp
        ),
        "N2"
    );
    assert!(
        fixture
            .runtime
            .recovery_read_error
            .lock()
            .unwrap()
            .is_some(),
        "N2 exact receipt needs no fresh Runtime evidence"
    );
    assert_eq!(fixture.store.get(&session).await.unwrap(), resumed);
    assert_nothing_activated(&fixture, &resumed, "N1-N3");
}

#[tokio::test]
async fn awaiting_unknown_or_moving_prefix_never_becomes_a_budget_noop() {
    // DT-P: C1 Awaiting Run with no answerable ticket; C2 snapshot unavailable;
    // C3 an ended prefix changes; C4 a child link claims a Run without a
    // snapshot. P1 C1/C4 => retain Interrupt,
    // not NoOp (reply authority is unnecessary); P2 C2 => pending error and no
    // receipt/root change; P3 C3 => ordinary Interrupt admission, never NoOp.
    // Runtime is the existing source fixture; Root/receipt writes use real SQLite.
    for cause in ["awaiting", "unknown", "moving", "child_unknown"] {
        let id = format!("t12-noop-{cause}");
        let session = SessionId::from(id.as_str());
        let fixture = fixture(&id).await;
        let run = awaken_agent_contract::RunId("observed-run".into());
        if cause == "child_unknown" {
            *fixture.runtime.coordinated_links.lock().unwrap() =
                Some(vec![CoordinatedThreadLink {
                    session_id: session.clone(),
                    thread_id: ThreadId("unknown-child".into()),
                    target: CoordinatedThreadTarget::Agent {
                        agent_id: "child-agent".into(),
                    },
                    created_by_operation_id: "known-child-link".into(),
                    latest_run_id: Some(RunId("unobserved-child-run".into())),
                }]);
        } else if cause == "unknown" {
            *fixture.runtime.recovery_read_error.lock().unwrap() =
                Some(RunError::unavailable("snapshot offline"));
        } else {
            *fixture.runtime.latest_run.lock().unwrap() = Some(run.clone());
            fixture.runtime.states.lock().unwrap().insert(
                run.0,
                if cause == "awaiting" {
                    RunState::Awaiting
                } else {
                    RunState::Ended(awaken_agent_contract::agent::run::EndCause::NaturalEnd)
                },
            );
            if cause == "moving" {
                *fixture.runtime.recovery_versions.lock().unwrap() = VecDeque::from([1, 2, 2]);
            }
        }
        let before = fixture.store.get(&session).await.unwrap();
        let result = fixture
            .app
            .event_batches
            .admit_session_event_batch_idempotent(
                &session,
                vec![interrupt_only()],
                None,
                None,
                Some(("control".into(), "control-fp".into())),
            )
            .await;
        if cause == "unknown" {
            assert_eq!(
                result.unwrap_err().kind,
                awaken_session_types::RunErrorKind::Unavailable,
                "P2"
            );
            assert_eq!(fixture.store.get(&session).await.unwrap(), before);
            let record = awaken_session_contract::IdempotencyRecord::event_admission(
                "control",
                "control-fp",
            );
            assert!(
                fixture
                    .store
                    .idempotency_receipt(&session, &record.key)
                    .await
                    .unwrap()
                    .is_none()
            );
        } else {
            assert!(
                matches!(result.unwrap(), crate::SessionEventBatchResult::Batch(_)),
                "{cause}"
            );
            assert_eq!(
                fixture
                    .store
                    .get(&session)
                    .await
                    .unwrap()
                    .event_batches
                    .len(),
                before.event_batches.len() + 1
            );
        }
        assert_nothing_activated(&fixture, &fixture.store.get(&session).await.unwrap(), cause);
    }
}

#[tokio::test]
async fn exact_activity_receipt_precedes_fresh_budget_policy() {
    // Causes: C1-C3 cover committed reservation/receipt, lost response, and a
    // newly reached budget at exact retry.
    // Receipt/policy replay graph: C1 a reservation and activity receipt both
    // committed; C2 the response was lost; C3 the Session budget is reached
    // before exact retry. Effects: E1 retry does not evaluate fresh projection
    // or activity policy; E2 Runtime classifies the existing reservation; E3
    // the original epoch is returned with no Session root mutation.
    //
    // | Rule | Receipt | Current budget | Reservation truth | Effect |
    // |---|---|---|---|---|
    // | P1 | absent | open | absent | fresh recover -> reserve -> activity |
    // | P2 | exact | reached | Reserved | E1+E2+E3 |
    // Constraint/Invariant: an exact committed receipt dominates fresh budget
    // policy and cannot mutate the Session root. Decision rule: use P1 as setup,
    // then execute P2 after budget closure and require E1-E3.
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("receipt-policy repository"),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    let snapshot = awaken_session_contract::ManagedListPriceSnapshot {
        snapshot_id: "receipt-policy-price-v1".into(),
        version: 1,
        effective_at_unix_ms: 1,
        arithmetic_version: 1,
        model_rates: BTreeMap::from([(
            "model".into(),
            awaken_session_contract::ManagedTokenListRates {
                input_micros_per_million: 1_000_000,
                ..Default::default()
            },
        )]),
        runtime_rates: Default::default(),
        fingerprint: "receipt-policy-price-v1-fingerprint".into(),
    };
    let mut session = persisted("receipt-before-policy", false, "idle");
    session.budget = awaken_session_contract::SessionBudgetState::active(1, snapshot);
    create(repository.as_ref(), session).await;
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let command = SessionUserRunCommand {
        session_id: "receipt-before-policy".into(),
        agent_id: "agent".into(),
        operation_id: "receipt-operation".into(),
        run_id: RunId("receipt-run".into()),
        content: vec![ContentBlock::text("one admitted command")],
        accompanying_system: None,
        data_subject_id: None,
        traceparent: None,
    };
    let first = app
        .event_batches
        .admit_session_user_run(command.clone())
        .await
        .expect("P1 fresh admission");
    let first_epoch = first
        .delivery()
        .expect("P1 delivery")
        .session_activity_epoch;
    app.lifecycle()
        .reconcile_managed_budget_usage(
            &SessionId::from("receipt-before-policy"),
            "receipt-policy-settlement",
            awaken_session_contract::SessionUsage {
                input_tokens: 10_000,
                by_model: BTreeMap::from([(
                    "model".into(),
                    awaken_session_contract::SessionModelUsage {
                        input_tokens: 10_000,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("P2 budget settlement");
    let before = repository
        .get(&SessionId::from("receipt-before-policy"))
        .await
        .unwrap()
        .revision;
    let projections_before = runtime
        .trace
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.starts_with("project:"))
        .count();

    let replay = app
        .event_batches
        .admit_session_user_run(command)
        .await
        .expect("P2 exact response-loss replay");
    assert!(
        matches!(replay, AdmittedSessionRun::AlreadyReserved(_)),
        "P2/E2"
    );
    assert_eq!(
        replay
            .delivery()
            .expect("P2/E3 delivery")
            .session_activity_epoch,
        first_epoch,
        "P2/E3 original epoch"
    );
    assert_eq!(
        repository
            .get(&SessionId::from("receipt-before-policy"))
            .await
            .unwrap()
            .revision,
        before,
        "P2/E3 no root CAS"
    );
    assert_eq!(
        runtime
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.starts_with("project:"))
            .count(),
        projections_before,
        "P2/E1 no fresh projection policy"
    );
}
