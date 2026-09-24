//! Repeatable, opt-in PostgreSQL dispatch discovery measurement.
//!
//! Run on the same database and host before and after a candidate-query change.
//! The printed timings are observations, not an acceptance threshold.

mod harness;

use std::time::Instant;

use awaken_dispatch::{ControlDispatchQueue, DispatchQueue, PostgresDispatchStore, RunDispatch};
use sqlx::types::Json;

#[tokio::test]
#[ignore = "requires a live PostgreSQL fixture and intentionally creates a large queue"]
async fn broad_pending_queue_claim_latency() {
    // Cause/effect design: C1 a large, entirely runnable Pending queue; C2 the
    // first claim warms caches, then repeated exact claims remove one candidate
    // at a time; C3 the exact operational depth query is repeated separately.
    // E1 every call claims a distinct Run; E2 print median and p95 claim
    // latency; E3 print the cost of one exact depth read. Constraint:
    // all rows have distinct Threads, no physical slot, and identical priority,
    // so the measurement isolates discovery/claim rather than placement or a
    // same-Thread fence. R1=C1+C2 -> E1+E2; R2=C1+C3 -> E3. The depth
    // measurement is separate so it can be combined with the old per-drive
    // call frequency or the new per-process sampling frequency. This is an
    // observation, not an acceptance threshold;
    // safety and incompatible-candidate rules belong to the conformance tests.
    let pool = harness::schema_pool("t_pg_claim_perf")
        .await
        .expect("explicit performance run requires PostgreSQL");
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("migrated dispatch store");
    let rows: i64 = std::env::var("AWAKEN_DISPATCH_PERF_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20_000);
    assert!(rows >= 100, "benchmark needs a meaningful pending queue");
    let rounds: usize = std::env::var("AWAKEN_DISPATCH_PERF_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    let depth_rounds: usize = std::env::var("AWAKEN_DISPATCH_PERF_DEPTH_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    assert!((1..=100).contains(&rounds) && (1..=100).contains(&depth_rounds));
    assert!(rows > (rounds + 1) as i64);
    let template = serde_json::to_value(RunDispatch::new(harness::activation("template")))
        .expect("serializable dispatch");
    sqlx::query(
        "INSERT INTO runtime_dispatch (run_id, thread_id, request, status, priority) \
         SELECT 'bench-run-' || g, 'bench-thread-' || g, \
                jsonb_set(jsonb_set($1::jsonb, '{activation,run_id}', \
                    to_jsonb('bench-run-' || g)), '{activation,thread_id}', \
                    to_jsonb('bench-thread-' || g)), 'pending', 0 \
         FROM generate_series(1, $2::bigint) AS g",
    )
    .bind(Json(&template))
    .bind(rows)
    .execute(&pool)
    .await
    .expect("seed identical eligible dispatches");
    let mut samples = Vec::new();
    for iteration in 0..=rounds {
        let started = Instant::now();
        let claimed = store
            .claim("perf-owner", 60_000, &Default::default())
            .await
            .expect("claim succeeds")
            .expect("runnable row remains");
        assert_eq!(claimed.lease.epoch, 1, "fresh exact claim");
        if iteration > 0 {
            samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "dispatch_claim_perf rows={rows} n={} median_ms={:.3} p95_ms={:.3} min_ms={:.3} max_ms={:.3}",
        samples.len(),
        samples[samples.len() / 2],
        samples[samples.len() * 95 / 100],
        samples[0],
        samples[samples.len() - 1],
    );
    let mut depth_samples = Vec::new();
    for iteration in 0..=depth_rounds {
        let started = Instant::now();
        let depth = store
            .runnable_depth()
            .await
            .expect("exact operational count")
            .expect("native PostgreSQL count");
        assert_eq!(
            depth,
            (rows - rounds as i64 - 1) as u64,
            "R2 exact query result"
        );
        if iteration > 0 {
            depth_samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }
    depth_samples.sort_by(f64::total_cmp);
    println!(
        "dispatch_depth_perf rows={rows} n={} median_ms={:.3} p95_ms={:.3}",
        depth_samples.len(),
        depth_samples[depth_samples.len() / 2],
        depth_samples[depth_samples.len() * 95 / 100],
    );
}

#[tokio::test]
#[ignore = "requires a live PostgreSQL fixture and intentionally creates incompatible work"]
async fn incompatible_front_queue_claim_latency() {
    // Causes: C1 a high-priority front of pending rows has exhausted claim
    // epochs; C2 ten ordinary lower-priority Runs remain; C3 each claim must
    // inspect past C1 without silently returning None. Effects: E1 only C2
    // runs are claimed; E2 print warm median/p95 latency with the same fixture
    // before and after the query change. Rule I1=C1+C2+C3 -> E1+E2. This probes
    // the worst-case cursor path that a broad all-compatible benchmark misses.
    let pool = harness::schema_pool("t_pg_claim_incompat_perf")
        .await
        .expect("explicit performance run requires PostgreSQL");
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("migrated dispatch store");
    let rows: i64 = std::env::var("AWAKEN_DISPATCH_PERF_INCOMPATIBLE_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5_000);
    let template = serde_json::to_value(RunDispatch::new(harness::activation("template")))
        .expect("serializable dispatch");
    sqlx::query(
        "INSERT INTO runtime_dispatch \
         (run_id, thread_id, request, status, priority, lease_epoch) \
         SELECT 'incompatible-' || g, 'incompatible-thread-' || g, \
                jsonb_set(jsonb_set($1::jsonb, '{activation,run_id}', \
                    to_jsonb('incompatible-' || g)), '{activation,thread_id}', \
                    to_jsonb('incompatible-thread-' || g)), 'pending', 100, \
                9223372036854775807 \
         FROM generate_series(1, $2::bigint) AS g",
    )
    .bind(Json(&template))
    .bind(rows)
    .execute(&pool)
    .await
    .expect("seed unclaimable high-priority rows");
    for index in 0..10 {
        store
            .enqueue(RunDispatch::new(harness::activation_on(
                &format!("valid-{index}"),
                &format!("valid-thread-{index}"),
            )))
            .await
            .unwrap();
    }
    let mut samples = Vec::new();
    for iteration in 0..10 {
        let started = Instant::now();
        let claimed = store
            .claim("perf-owner", 60_000, &Default::default())
            .await
            .expect("claim succeeds")
            .expect("lower-priority runnable row remains");
        assert!(claimed.lease.run_id.0.starts_with("valid-"), "I1/E1");
        if iteration > 0 {
            samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "dispatch_incompatible_perf rows={rows} n={} median_ms={:.3} p95_ms={:.3} min_ms={:.3} max_ms={:.3}",
        samples.len(),
        samples[samples.len() / 2],
        samples[samples.len() * 95 / 100],
        samples[0],
        samples[samples.len() - 1],
    );
}
