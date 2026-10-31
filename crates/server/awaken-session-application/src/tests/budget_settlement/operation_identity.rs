//! T13-C01 (结算回执): each budget settlement is checked by its OWN operation
//! receipt, not only by the cumulative total.
//!
//! Oracles (groups JSON order):
//! - O0 the original root receipt and usage agree;
//! - O1 nothing is charged twice;
//! - O2 a failure loses no ledger entry (B10a in the parent test).
//!
//! Negative control: "checking only the cumulative total is not enough; every
//! operation identity must be checked". It is executed: two Sessions reach the
//! SAME cumulative usage through different settlement histories (10k+20k vs
//! one 20k). A totals-only checker declares them equal; the per-operation
//! receipt checker used for O0 tells them apart.
//!
//! The production settlement keys its root CAS on the compiled snapshot
//! (`compile_session_snapshot_idempotency`, mutation.rs:155) under the constant
//! operation name `reconcile-managed-budget-usage` (lifecycle.rs:352), so an
//! operation is identified only by the cumulative cursor it carries. The
//! ignored reproducer records the consequence: a delayed exact retry of an
//! earlier settlement is refused instead of replaying its receipt.
use super::*;
use awaken_session_contract::SessionRepository as _;
use awaken_session_contract::{IdempotencyRecord, PersistedSession};
use std::time::Duration;

const OPERATION: &str = "reconcile-managed-budget-usage";
const WAIT: Duration = Duration::from_secs(5);
const LIMIT_MINOR: u64 = 10;

fn budgeted(id: &str) -> PersistedSession {
    let mut session = persisted(id, false, "idle");
    session.budget = SessionBudgetState::active(
        LIMIT_MINOR,
        ManagedListPriceSnapshot {
            snapshot_id: "operation-identity-prices-v1".into(),
            version: 1,
            effective_at_unix_ms: 1,
            arithmetic_version: 1,
            model_rates: BTreeMap::from([(
                "model".into(),
                ManagedTokenListRates {
                    input_micros_per_million: 1_000_000,
                    ..Default::default()
                },
            )]),
            runtime_rates: Default::default(),
            fingerprint: "operation-identity-prices-v1-fingerprint".into(),
        },
    );
    session
}

fn usage(tokens: u64) -> SessionUsage {
    SessionUsage {
        input_tokens: tokens,
        by_model: BTreeMap::from([(
            "model".into(),
            SessionModelUsage {
                input_tokens: tokens,
                ..Default::default()
            },
        )]),
        ..Default::default()
    }
}

struct Fixture {
    store: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    repository: Arc<FaultingSessionRepository>,
    app: crate::SessionApplication,
}

async fn fixture(sessions: &[&str]) -> Fixture {
    let store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("settlement repository"),
    );
    for id in sessions {
        create(store.as_ref(), budgeted(id)).await;
    }
    let repository = Arc::new(FaultingSessionRepository::new(store.clone()));
    let app = application(
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    Fixture {
        store,
        repository,
        app,
    }
}

/// One settlement whose exact root-CAS request is captured at the real
/// repository boundary. Returns the committed root and that operation's key.
async fn settle(
    fixture: &Fixture,
    id: &SessionId,
    tokens: u64,
) -> (PersistedSession, IdempotencyRecord) {
    let pause = fixture.repository.pause_next_mutation(OPERATION);
    let lifecycle = fixture.app.lifecycle();
    let session_id = id.clone();
    let settlement = tokio::spawn(async move {
        lifecycle
            .reconcile_managed_budget_usage(&session_id, usage(tokens))
            .await
    });
    tokio::time::timeout(WAIT, pause.entered.notified())
        .await
        .expect("settlement reaches its root CAS");
    let record = pause
        .candidate
        .lock()
        .unwrap()
        .as_ref()
        .expect("captured settlement CAS")
        .idempotency
        .clone();
    pause.release.notify_one();
    let outcome = tokio::time::timeout(WAIT, settlement)
        .await
        .expect("settlement completes")
        .expect("settlement task joins")
        .expect("settlement commits");
    (outcome.session, record)
}

/// The per-operation ledger: each operation key with its durable receipt.
async fn receipts(
    fixture: &Fixture,
    id: &SessionId,
    records: &[IdempotencyRecord],
) -> Vec<(String, u64)> {
    let mut ledger = Vec::new();
    for record in records {
        let receipt = fixture
            .store
            .idempotency_receipt(id, &record.key)
            .await
            .expect("receipt read")
            .expect("every settlement owns a durable receipt");
        assert_eq!(
            receipt.payload_hash, record.payload_hash,
            "receipt owns its request"
        );
        ledger.push((record.key.clone(), receipt.committed_revision.0));
    }
    ledger
}

fn totals(session: &PersistedSession) -> u64 {
    session.usage_cursor.by_model["model"].input_tokens
}

#[tokio::test]
async fn every_settlement_owns_one_receipt_and_totals_alone_cannot_identify_it() {
    let incremental = SessionId::from("settlement-ops-incremental");
    let single = SessionId::from("settlement-ops-single");
    let fixture = fixture(&[incremental.as_str(), single.as_str()]).await;

    // U1 then U2 on one Session.
    let (after_u1, u1) = settle(&fixture, &incremental, 10_000).await;
    let (after_u2, u2) = settle(&fixture, &incremental, 20_000).await;
    assert_ne!(u1.key, u2.key, "O0 two operations, two identities");
    let ledger = receipts(&fixture, &incremental, &[u1.clone(), u2.clone()]).await;
    assert_eq!(
        ledger,
        vec![
            (u1.key.clone(), after_u1.revision.0),
            (u2.key.clone(), after_u2.revision.0),
        ],
        "O0 each receipt is its own committing revision"
    );
    assert_eq!(totals(&after_u1), 10_000, "O0 U1 usage at its receipt");
    assert_eq!(totals(&after_u2), 20_000, "O0 U2 usage at its receipt");

    // O1: the exact latest settlement is a no-op; nothing is charged twice.
    let replay = fixture
        .app
        .lifecycle()
        .reconcile_managed_budget_usage(&incremental, usage(20_000))
        .await
        .expect("O1 exact latest replay");
    assert!(!replay.reached_now, "O1");
    assert_eq!(replay.session, after_u2, "O1 no second debit or revision");
    assert_eq!(
        receipts(&fixture, &incremental, &[u1.clone(), u2.clone()]).await,
        ledger,
        "O0 receipts unchanged by the replay"
    );

    // NC: the same total through a different history.
    let (single_root, only) = settle(&fixture, &single, 20_000).await;
    let single_ledger = receipts(&fixture, &single, &[only]).await;
    let totals_only_equal = totals(&single_root) == totals(&after_u2)
        && single_root.budget.can_admit_model_request()
            == after_u2.budget.can_admit_model_request();
    assert!(
        totals_only_equal,
        "NC a totals-only checker cannot tell the two histories apart"
    );
    assert_ne!(
        single_ledger.len(),
        ledger.len(),
        "NC the per-operation checker distinguishes them"
    );
}

#[tokio::test]
#[ignore = "product gap: budget settlement has no per-operation identity; a delayed exact retry of an earlier settlement is refused as 'cumulative usage cannot move backwards' instead of replaying its original receipt"]
async fn a_delayed_retry_of_an_earlier_settlement_replays_its_original_receipt() {
    let id = SessionId::from("settlement-ops-delayed");
    let fixture = fixture(&[id.as_str()]).await;
    let (_after_u1, _u1) = settle(&fixture, &id, 10_000).await;
    let (after_u2, _u2) = settle(&fixture, &id, 20_000).await;
    let facts = fixture.store.pending_lifecycle().await.unwrap();
    let delayed = fixture
        .app
        .lifecycle()
        .reconcile_managed_budget_usage(&id, usage(10_000))
        .await;
    let replay = delayed.expect("an exact retry of U1 replays U1's committed settlement");
    assert!(!replay.reached_now, "the replay reaches nothing new");
    assert_eq!(
        fixture.store.get(&id).await.unwrap(),
        after_u2,
        "the replay charges nothing and moves no revision"
    );
    assert_eq!(fixture.store.pending_lifecycle().await.unwrap(), facts);
}
