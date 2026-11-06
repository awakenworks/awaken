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
//! The caller retains its operation identity. The original Session command
//! receipt binds that identity to the normalized input cursor before newer
//! Root-owned runtime time is joined; a delayed retry reads the same receipt.
use super::*;
use awaken_session_contract::SessionRepository as _;
use awaken_session_contract::{IdempotencyRecord, PersistedSession};
use std::sync::atomic::Ordering;
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
    path: String,
    _directory: tempfile::TempDir,
}

async fn fixture(sessions: &[&str]) -> Fixture {
    let directory = tempfile::tempdir().expect("settlement fixture directory");
    let path = directory
        .path()
        .join("settlements.db")
        .to_str()
        .unwrap()
        .to_owned();
    let store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(&path)
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
        path,
        _directory: directory,
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
            .reconcile_managed_budget_usage(&session_id, &format!("usage-{tokens}"), usage(tokens))
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
        .reconcile_managed_budget_usage(&incremental, "usage-20000", usage(20_000))
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
async fn a_delayed_retry_of_an_earlier_settlement_replays_its_original_receipt() {
    // DT-S: C1 U1 and U2 both committed; C2 U1 is retried after U2.
    // S1 C1+C2 => observe U1's existing receipt before rewind validation,
    // no new debit/reach, byte-equal current root and outbox; C3 cold rebuild
    // repeats S1 over the same file. The sibling
    // operation-ledger table independently distinguishes U1 from U2.
    let id = SessionId::from("settlement-ops-delayed");
    let fixture = fixture(&[id.as_str()]).await;
    let (_after_u1, _u1) = settle(&fixture, &id, 10_000).await;
    let (after_u2, _u2) = settle(&fixture, &id, 20_000).await;
    let facts = fixture.store.pending_lifecycle().await.unwrap();
    assert_delayed_replay(&fixture.app, fixture.store.as_ref(), &id, &after_u2, &facts).await;
    let Fixture {
        path,
        _directory,
        store,
        repository,
        app,
    } = fixture;
    drop(app);
    drop(repository);
    drop(store);
    let reopened = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(&path)
            .await
            .unwrap(),
    );
    let rebuilt = application(
        reopened.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert_delayed_replay(&rebuilt, reopened.as_ref(), &id, &after_u2, &facts).await;
    drop(_directory);
}

async fn assert_delayed_replay(
    app: &crate::SessionApplication,
    store: &dyn crate::SessionStore,
    id: &SessionId,
    before: &PersistedSession,
    facts: &[awaken_session_contract::ManagedLifecycleFact],
) {
    // Shared SQLite/PostgreSQL S1 oracle. An already committed input is found
    // by its original operation, while current Root/outbox are preserved.
    let replay = app
        .lifecycle()
        .reconcile_managed_budget_usage(id, "usage-10000", usage(10_000))
        .await
        .expect("an exact retry of U1 replays U1's committed settlement");
    assert!(!replay.reached_now, "the replay reaches nothing new");
    assert_eq!(
        &store.get(id).await.unwrap(),
        before,
        "S1 no second charge/revision"
    );
    assert_eq!(store.pending_lifecycle().await.unwrap(), facts);
}

#[tokio::test]
async fn postgres_delayed_settlement_replays_after_new_connections() {
    // Same S1 cause/effect over required real PostgreSQL: U1 -> U2 -> close
    // both application and pool handles -> reconnect to the SAME owned schema
    // -> exact U1 replay. Fixture absence is never treated as configured PASS.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let id = SessionId::from("postgres-delayed-settlement");
    let store = Arc::new(
        awaken_session_store::PostgresManagedSessionRepository::connect(&url)
            .await
            .unwrap(),
    );
    create(store.as_ref(), budgeted(id.as_str())).await;
    let app = application(
        store.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    app.lifecycle()
        .reconcile_managed_budget_usage(&id, "usage-10000", usage(10_000))
        .await
        .unwrap();
    let after = app
        .lifecycle()
        .reconcile_managed_budget_usage(&id, "usage-20000", usage(20_000))
        .await
        .unwrap();
    let facts = store.pending_lifecycle().await.unwrap();
    drop(app);
    drop(store);
    let reopened = Arc::new(
        awaken_session_store::PostgresManagedSessionRepository::connect(&url)
            .await
            .unwrap(),
    );
    let rebuilt = application(
        reopened.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert_delayed_replay(&rebuilt, reopened.as_ref(), &id, &after.session, &facts).await;
}

#[test]
fn postgres_delayed_settlement_fixture_is_required_when_configured() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "tests::budget_settlement::operation_identity::postgres_delayed_settlement_replays_after_new_connections",
    );
}

#[tokio::test]
async fn settlement_identity_refuses_changed_payload_unknown_rewind_and_unavailable_proof() {
    // DT-I: C1 an operation committed; C2 same id + different cursor; C3 a new
    // id attempts an older, never-admitted cursor; C4 receipt read fails; C5
    // empty id. I1 C2=>IdempotencyMismatch; I2 C3=>rewind rejection; I3 C4=>
    // retryable unavailable, never inferred non-admission; I4 C5=>refuse before
    // IO. Every error preserves Root/outbox; exact original identity recovers.
    let id = SessionId::from("settlement-identity-errors");
    let fixture = fixture(&[id.as_str()]).await;
    let (before, original) = settle(&fixture, &id, 10_000).await;
    let facts = fixture.store.pending_lifecycle().await.unwrap();
    assert_eq!(
        fixture
            .app
            .lifecycle()
            .reconcile_managed_budget_usage(&id, "usage-10000", usage(20_000))
            .await
            .unwrap_err(),
        SessionMutationError::IdempotencyMismatch
    );
    assert!(
        fixture
            .app
            .lifecycle()
            .reconcile_managed_budget_usage(&id, "unseen-older", usage(5_000))
            .await
            .is_err(),
        "I2 no invented prior receipt"
    );
    assert!(
        fixture
            .app
            .lifecycle()
            .reconcile_managed_budget_usage(&id, "", usage(10_000))
            .await
            .is_err(),
        "I4"
    );
    fixture
        .repository
        .fail_idempotency_receipt_once
        .store(true, Ordering::SeqCst);
    assert!(
        matches!(
            fixture
                .app
                .lifecycle()
                .reconcile_managed_budget_usage(&id, "usage-10000", usage(10_000))
                .await,
            Err(SessionMutationError::Unavailable(_))
        ),
        "I3"
    );
    let replay = fixture
        .app
        .lifecycle()
        .reconcile_managed_budget_usage(&id, "usage-10000", usage(10_000))
        .await
        .unwrap();
    assert!(!replay.reached_now);
    assert_eq!(replay.session, before);
    assert_eq!(fixture.store.get(&id).await.unwrap(), before);
    assert_eq!(fixture.store.pending_lifecycle().await.unwrap(), facts);
    assert_eq!(receipts(&fixture, &id, &[original]).await.len(), 1);
}

#[tokio::test]
async fn a_losing_exact_settlement_replays_across_independent_pools_without_reaching_twice() {
    // DT-C: C1 two independently built applications/pools share one file DB;
    // C2 A pauses immediately before its actual CAS; C3 B commits the SAME
    // operation and crosses the cap; C4 A resumes its original request. C1-C4
    // => one reach fact and receipt, winner reports reached_now, loser does not;
    // the complete Root remains the winner's. A process-local mutex cannot
    // satisfy this schedule: B must finish while A is held.
    let id = SessionId::from("settlement-independent-race");
    let fixture = fixture(&[id.as_str()]).await;
    let peer_store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(&fixture.path)
            .await
            .unwrap(),
    );
    let peer = application(peer_store, Arc::new(RecordingEnvironmentSource::default()));
    let pause = fixture.repository.pause_next_mutation(OPERATION);
    let lifecycle = fixture.app.lifecycle();
    let pending_id = id.clone();
    let pending = tokio::spawn(async move {
        lifecycle
            .reconcile_managed_budget_usage(&pending_id, "reach-once", usage(100_000))
            .await
    });
    tokio::time::timeout(WAIT, pause.entered.notified())
        .await
        .expect("C2 actual CAS cut");
    let winner = tokio::time::timeout(
        WAIT,
        peer.lifecycle()
            .reconcile_managed_budget_usage(&id, "reach-once", usage(100_000)),
    )
    .await
    .expect("C3 peer does not share an application lock")
    .unwrap();
    assert!(winner.reached_now, "C3 actual cap crossing");
    let facts = fixture.store.pending_lifecycle().await.unwrap();
    pause.release.notify_one();
    let replay = tokio::time::timeout(WAIT, pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!replay.reached_now, "C4 a replay is not a second crossing");
    assert_eq!(replay.session, winner.session);
    assert_eq!(fixture.store.pending_lifecycle().await.unwrap(), facts);
}

#[test]
fn observation_identity_tracks_runtime_time_and_usage_without_following_unrelated_root_writes() {
    // DT-O: C1 identical source observation => same id; C2 title/revision only
    // changes => same id; C3 new Root-owned runtime seconds with unchanged
    // Runtime token totals => new id; C4 another Thread/cursor => new id. This
    // keeps a new runtime charge from being hidden behind a prior token receipt.
    let before = budgeted("observed-usage");
    let input = usage(10_000);
    let observe = |root: &PersistedSession, thread: &str, input: &SessionUsage| {
        crate::SessionLifecycle::budget_usage_observation_id(root, thread, input).unwrap()
    };
    let original = observe(&before, "source-thread", &input);
    assert_eq!(original, observe(&before.clone(), "source-thread", &input));
    let mut unrelated = before.clone();
    unrelated.revision.0 += 1;
    unrelated.title = Some("new title".into());
    assert_eq!(original, observe(&unrelated, "source-thread", &input));
    let mut later = before.clone();
    later.usage_cursor.active_seconds = 10;
    assert_ne!(original, observe(&later, "source-thread", &input));
    assert_ne!(original, observe(&before, "other-thread", &input));
    assert_ne!(original, observe(&before, "source-thread", &usage(20_000)));
}
