//! Real PostgreSQL decisions for bounded candidate discovery.

mod harness;

use awaken_dispatch::{
    DispatchClaims as _, DispatchEnqueue as _, DispatchError, PlacementRequirements,
    PostgresDispatchStore, RunDispatch,
};
use awaken_worker_contract::WorkerCurrentSource as _;
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn incompatible_pages_preserve_priority_time_and_id_with_one_connection() {
    // Causes: C1 three full pages of genuinely GPU-incompatible requests;
    // C2 priority uses both i64 extremes; C3 timestamp/id ties cross pages;
    // C4 the pool has exactly one connection; C5 eligible work appears after
    // an exhausted scan. Effects: E1 no false empty result for a legal tail;
    // E2 each exact claim is unique and in original order; E3 a fresh call
    // reconsiders the full prefix. Rules K1=C1+!C5=>None only after EOF;
    // K2=C1+C2+C3+C4+C5=>E1+E2; K3 new high-priority work=>E3.
    // C6 a corrupt request sorts AFTER legal candidates. K5 C6=>earlier legal
    // work still claims in order; K6 no earlier winner=>the decoding failure
    // remains Unavailable, never false EOF. Read-ahead cannot reorder errors.
    // Exact claim remains the original mutation/credential/epoch authority.
    const SCHEMA: &str = "t_pg_discovery_keyset";
    let Some(initial) = harness::schema_pool(SCHEMA).await else {
        return;
    };
    initial.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&harness::database_url_in_schema(SCHEMA))
        .await
        .unwrap();
    // K4 non-default DateStyle/timezone must not become a cursor codec.
    // PostgreSQL's typed timestamp stays exact through every page boundary.
    sqlx::query("SET DateStyle TO 'SQL, DMY'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("SET TIME ZONE 'Asia/Shanghai'")
        .execute(&pool)
        .await
        .unwrap();
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    let (directory, identity) =
        awaken_dispatch_testkit::worker_http::ready_worker("keyset-worker").await;
    let worker = directory
        .current(&identity.worker_id)
        .await
        .unwrap()
        .unwrap()
        .snapshot;
    let mut gpu = PlacementRequirements::remote_required();
    gpu.required_capabilities.insert("gpu".into());
    harness::seed_placement_candidate_prefix(
        &pool,
        RunDispatch::new(harness::activation_on("template", "template-thread"))
            .with_placement(gpu)
            .with_worker_codec_requirements(),
        384,
        "gpu-",
        i64::MAX,
    )
    .await;
    sqlx::query("UPDATE runtime_dispatch SET created_at='2026-01-01T00:00:00.123456Z'::timestamptz WHERE run_id LIKE 'gpu-%'")
        .execute(&pool).await.unwrap();
    assert!(
        store
            .claim_compatible(&worker, 60_000)
            .await
            .unwrap()
            .is_none(),
        "K1"
    );
    for id in ["tail-c", "tail-a", "tail-b", "tail-z-broken"] {
        store
            .enqueue(
                RunDispatch::new(harness::activation_on(id, &format!("thread-{id}")))
                    .with_placement(PlacementRequirements::remote_required())
                    .with_worker_codec_requirements(),
            )
            .await
            .unwrap();
    }
    sqlx::query("UPDATE runtime_dispatch SET priority=$1, created_at='2026-01-02T00:00:00.000001Z'::timestamptz WHERE run_id LIKE 'tail-%'")
        .bind(i64::MIN).execute(&pool).await.unwrap();
    sqlx::query("UPDATE runtime_dispatch SET request='{}'::jsonb WHERE run_id='tail-z-broken'")
        .execute(&pool)
        .await
        .unwrap();
    for expected in ["tail-a", "tail-b", "tail-c"] {
        let claim = store
            .claim_compatible(&worker, 60_000)
            .await
            .unwrap()
            .expect("K2 legal tail");
        assert_eq!(claim.request.run_id().0, expected, "K2 timestamp/id order");
    }
    // One candidate now precedes the last scan's cursor. No process-local
    // continuation may survive the completed call and hide this new work.
    store
        .enqueue(
            RunDispatch::new(harness::activation_on("new-front", "new-thread"))
                .with_placement(PlacementRequirements::remote_required())
                .with_worker_codec_requirements(),
        )
        .await
        .unwrap();
    let claim = store
        .claim_compatible(&worker, 60_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.request.run_id().0, "new-front", "K3");
    assert!(
        matches!(store.claim_compatible(&worker, 60_000).await,
        Err(DispatchError::Unavailable(detail)) if detail.contains("request")),
        "K6 the original corrupt request remains visible after earlier winners"
    );
}

#[tokio::test]
async fn exhausted_discovery_deadline_is_unavailable_without_a_claim() {
    // C1 the sole connection is retained by unrelated work; C2 the discovery
    // deadline expires before its first query can run. D1=>Unavailable, never
    // None or a lease. D2 release connection and retry=>the original pending
    // Run is claimable. The paused clock is armed after real fixture setup;
    // an explicit pending poll establishes C1 before advancing the deadline.
    const SCHEMA: &str = "t_pg_discovery_deadline";
    let Some(initial) = harness::schema_pool(SCHEMA).await else {
        return;
    };
    initial.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(300))
        .connect(&harness::database_url_in_schema(SCHEMA))
        .await
        .unwrap();
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(harness::activation_on(
            "deferred",
            "deferred-thread",
        )))
        .await
        .unwrap();
    let held = pool.acquire().await.unwrap();
    tokio::time::pause();
    let capabilities = Default::default();
    let mut discovery = Box::pin(store.claim("deadline-owner", 60_000, &capabilities));
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(discovery.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        }),
    )
    .await
    .expect("D1 initial pending observation is bounded");
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    assert!(
        matches!(tokio::time::timeout(std::time::Duration::from_secs(1), discovery).await.expect("D1 original discovery deadline must finish the waiter"),
            Err(DispatchError::Unavailable(message)) if message == "candidate discovery deadline exceeded"
        ),
        "D1 no false empty result"
    );
    tokio::time::resume();
    drop(held);
    assert_eq!(
        store
            .claim("deadline-owner", 60_000, &capabilities)
            .await
            .unwrap()
            .unwrap()
            .request
            .run_id()
            .0,
        "deferred",
        "D2"
    );
}
