//! T12-C03 (暂停与事务失败), application rows: a budget-paused Session
//! receives interleaved ordinary and interrupt-only batches through the
//! production `append_session_event_batch_idempotent` root-CAS admission.
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
        .append_session_event_batch_idempotent(
            session,
            vec![input],
            None,
            None,
            Some((key.into(), format!("fingerprint-{key}"))),
        )
        .await
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
    let id = "t12-paused-interrupt";
    let session = SessionId::from(id);
    let fixture = fixture(id).await;
    append(&fixture.app, &session, user("held"), "p1")
        .await
        .expect("P1 ordinary batch");
    let before = fixture.store.get(&session).await.unwrap();
    let admitted = fixture
        .app
        .event_batches
        .admit_session_event_batch_idempotent(
            &session,
            vec![interrupt_only()],
            None,
            None,
            Some(("p2".into(), "fingerprint-p2".into())),
        )
        .await
        .expect("P2/O0 the interrupt-only batch is accepted");
    assert_eq!(admitted, None, "P2/O0 accepted as a no-op, with no batch");
    let after = fixture.store.get(&session).await.unwrap();
    assert_eq!(
        after.event_batches.len(),
        before.event_batches.len(),
        "P2/O0 an interrupt-only batch on a budget-paused Session is an accepted no-op"
    );
    assert_eq!(after.revision, before.revision, "P2/O0 no root fact");
}
