//! Durable legacy corruption cannot starve healthy work or bypass Worker codecs.
//! L1: valid admission followed by persisted malformed Session projection.
//! L2: broad claim skips it; exact execution remains ineligible without mutation.
//! L3: cancellation intent persists, but cannot bypass the full request decoder.
//! These are backend recovery tests, not permission to enqueue invalid requests.
mod harness;

use awaken_dispatch::{
    DispatchOutcome, PostgresDispatchStore, RunDispatch, SessionRuntimeEnvelope,
    SqliteDispatchStore,
};
use awaken_dispatch_contract::SubmitOptions;
use awaken_dispatch_testkit::{ConformanceDispatch, LEASE_MS};

type PersistedFences = (
    i64,
    i64,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
);
fn assert_fences(actual: PersistedFences, mut original: PersistedFences) {
    assert_eq!((original.0, original.1, original.2), (0, 0, 0));
    assert!(
        original.3.is_none()
            && original.4.is_none()
            && original.5.is_none()
            && original.6.is_none()
    );
    original.2 = 1;
    assert_eq!(
        actual, original,
        "L2/L3 only original cancel intent changes; no epoch, attempt, lease, physical slot or credential mutation"
    );
}

async fn seed(store: &impl ConformanceDispatch) -> RunDispatch {
    let bad = RunDispatch::new(harness::activation_on("legacy-bad", "legacy-bad-thread"));
    store
        .enqueue_with(
            bad.clone(),
            SubmitOptions {
                priority: 100,
                ..SubmitOptions::default()
            },
        )
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(harness::activation_on(
            "healthy-peer",
            "healthy-thread",
        )))
        .await
        .unwrap();
    bad.with_session_runtime(SessionRuntimeEnvelope::new("{"))
}

async fn assert_legacy_recovery(store: &impl ConformanceDispatch, bad: &RunDispatch) {
    let healthy = store
        .claim("legacy-broad", LEASE_MS, &Default::default())
        .await
        .unwrap()
        .expect("L2 healthy peer remains claimable");
    assert_eq!(healthy.request.run_id().0, "healthy-peer");
    store
        .settle(
            healthy.request.run_id(),
            healthy.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();
    let before = store.list_dispatches().await.unwrap();
    assert!(
        store
            .claim_run(bad.run_id(), "legacy-exact", LEASE_MS, &Default::default())
            .await
            .expect("L2 invalid codec remains unclaimable")
            .is_none(),
        "L2 malformed execution never acquires a lease"
    );
    assert_eq!(
        store.list_dispatches().await.unwrap(),
        before,
        "L2 no lease, epoch or attempt mutation"
    );
    store.cancel(bad.run_id()).await.unwrap();
    let cancelled = store.list_dispatches().await.unwrap();
    let summary = store.dispatch_summary(bad.run_id()).await.unwrap().unwrap();
    assert!(
        summary.cancellation_requested,
        "L3 original cancellation intent retained"
    );
    assert_eq!(summary.attempt_count, 0);
    assert!(!summary.physical_attempt_active);
    assert!(
        store
            .claim_run(bad.run_id(), "legacy-cancel", LEASE_MS, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "L3 cancellation cannot bypass Worker codec admission"
    );
    assert_eq!(
        store.list_dispatches().await.unwrap(),
        cancelled,
        "L3 no lease or credential mutation"
    );
    let peer = RunDispatch::new(harness::activation_on(
        "after-cancel",
        "after-cancel-thread",
    ));
    store.enqueue(peer.clone()).await.unwrap();
    let claim = store
        .claim("legacy-after-cancel", LEASE_MS, &Default::default())
        .await
        .unwrap()
        .expect("L3 quarantined cancellation does not starve healthy work");
    assert_eq!(claim.request.run_id(), peer.run_id());
    store
        .settle(
            peer.run_id(),
            claim.lease.epoch(),
            DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        store.list_dispatches().await.unwrap(),
        cancelled,
        "L3 original bad row stays quarantined, never reported completed"
    );
}

#[tokio::test]
async fn sqlite_legacy_malformed_runtime_isolates_execution_and_retains_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy-runtime.sqlite");
    let store = SqliteDispatchStore::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    let bad = seed(&store).await;
    let raw = awaken_sqlite_pool::EmbeddedSqlite::file(path)
        .open()
        .await
        .unwrap();
    assert_eq!(
        sqlx::query("UPDATE runtime_dispatch SET request = ?1 WHERE run_id = ?2")
            .bind(serde_json::to_string(&bad).unwrap())
            .bind(&bad.run_id().0)
            .execute(&raw)
            .await
            .unwrap()
            .rows_affected(),
        1
    );
    let original: PersistedFences = sqlx::query_as("SELECT lease_epoch, attempt_count, cancel_requested, lease_owner, CAST(lease_until AS TEXT), active_attempt_owner, active_attempt_epoch, CAST(credential_bindings AS TEXT), CAST(credential_receipts AS TEXT) FROM runtime_dispatch WHERE run_id = ?1").bind(&bad.run_id().0).fetch_one(&raw).await.unwrap();
    assert_legacy_recovery(&store, &bad).await;
    let fences: PersistedFences = sqlx::query_as("SELECT lease_epoch, attempt_count, cancel_requested, lease_owner, CAST(lease_until AS TEXT), active_attempt_owner, active_attempt_epoch, CAST(credential_bindings AS TEXT), CAST(credential_receipts AS TEXT) FROM runtime_dispatch WHERE run_id = ?1").bind(&bad.run_id().0).fetch_one(&raw).await.unwrap();
    assert_fences(fences, original);
    raw.close().await;
}

#[tokio::test]
async fn postgres_legacy_malformed_runtime_isolates_execution_and_retains_cancellation() {
    let Some(pool) = harness::schema_pool("t_dispatch_legacy_runtime").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    let bad = seed(&store).await;
    assert_eq!(
        sqlx::query("UPDATE runtime_dispatch SET request = $1::TEXT::jsonb WHERE run_id = $2")
            .bind(serde_json::to_string(&bad).unwrap())
            .bind(&bad.run_id().0)
            .execute(&pool)
            .await
            .unwrap()
            .rows_affected(),
        1
    );
    let original: PersistedFences = sqlx::query_as("SELECT lease_epoch, attempt_count, cancel_requested, lease_owner, CAST(lease_until AS TEXT), active_attempt_owner, active_attempt_epoch, CAST(credential_bindings AS TEXT), CAST(credential_receipts AS TEXT) FROM runtime_dispatch WHERE run_id = $1").bind(&bad.run_id().0).fetch_one(&pool).await.unwrap();
    assert_legacy_recovery(&store, &bad).await;
    let fences: PersistedFences = sqlx::query_as("SELECT lease_epoch, attempt_count, cancel_requested, lease_owner, CAST(lease_until AS TEXT), active_attempt_owner, active_attempt_epoch, CAST(credential_bindings AS TEXT), CAST(credential_receipts AS TEXT) FROM runtime_dispatch WHERE run_id = $1").bind(&bad.run_id().0).fetch_one(&pool).await.unwrap();
    assert_fences(fences, original);
    pool.close().await;
}
