//! Repeatable, opt-in PostgreSQL dispatch discovery measurement.
//!
//! Run on the same database and host before and after a candidate-query change.
//! The printed timings are observations, not an acceptance threshold.

mod harness;

use std::time::Instant;

use awaken_dispatch::{ControlDispatchQueue, DispatchQueue, PostgresDispatchStore, RunDispatch};
use sqlx::types::Json;

/// Nearest-rank percentile, ceil(N*p/100)-1, without width-dependent overflow.
/// All measurement callers sort their own completed samples before reporting.
fn percentile_index(len: usize, percent: usize) -> Option<usize> {
    if len == 0 || !(1..=100).contains(&percent) {
        return None;
    }
    (len / 100 * percent + ((len % 100) * percent).div_ceil(100)).checked_sub(1)
}

fn percentile(samples: &[f64], percent: usize) -> f64 {
    samples[percentile_index(samples.len(), percent)
        .expect("nonempty sorted measurement samples and percentile1..100")]
}

#[test]
fn percentile_reporting_covers_empty_bounds_and_exact_integer_ranks() {
    // Q1 empty/percent0/percent101 => no rank; Q2 N=1/9/20/32/100 => the
    // nearest rank; Q3 usizeMAX => representable exact index, no multiplication
    // overflow. N20,p95 formerly selected index19(max), not the correct18.
    for (n, p, expected) in [
        (0, 95, None),
        (20, 0, None),
        (20, 101, None),
        (1, 95, Some(0)),
        (9, 95, Some(8)),
        (20, 95, Some(18)),
        (32, 95, Some(30)),
        (100, 95, Some(94)),
        (20, 50, Some(9)),
        (20, 99, Some(19)),
    ] {
        assert_eq!(percentile_index(n, p), expected, "Q1/Q2 {n}/{p}");
    }
    for p in [1, 50, 95, 99, 100] {
        assert_eq!(
            percentile_index(usize::MAX, p),
            Some(((usize::MAX as u128 * p as u128).div_ceil(100) - 1) as usize),
            "Q3"
        );
    }
    let samples = (1..=20).map(f64::from).collect::<Vec<_>>();
    assert_eq!(percentile(&samples, 50), 10.0, "Q2 explicit p50 definition");
    assert_eq!(percentile(&samples, 95), 19.0, "Q2 exact p95");
    assert_eq!(percentile(&samples, 99), 20.0, "Q2 exact p99");
}

#[tokio::test]
#[ignore = "requires a live PostgreSQL fixture and intentionally creates a large queue"]
async fn broad_pending_queue_claim_latency() {
    // Cause/effect design: C1 a large, entirely runnable Pending queue; C2 the
    // first claim warms caches, then repeated exact claims remove one candidate
    // at a time; C3 the exact operational depth query is repeated separately.
    // E1 every call claims a distinct Run; E2 print nearest-rank p50, p95 and p99 claim
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
        "dispatch_claim_perf rows={rows} n={} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} min_ms={:.3} max_ms={:.3}",
        samples.len(),
        percentile(&samples, 50),
        percentile(&samples, 95),
        percentile(&samples, 99),
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
        "dispatch_depth_perf rows={rows} n={} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3}",
        depth_samples.len(),
        percentile(&depth_samples, 50),
        percentile(&depth_samples, 95),
        percentile(&depth_samples, 99),
    );
}

#[tokio::test]
#[ignore = "requires a live PostgreSQL fixture and intentionally creates incompatible work"]
async fn incompatible_front_queue_claim_latency() {
    // Causes: C1 a high-priority front of pending rows has exhausted claim
    // epochs; C2 ten ordinary lower-priority Runs remain; C3 each claim must
    // inspect past C1 without silently returning None. Effects: E1 only C2
    // runs are claimed; E2 print warm p50/p95/p99 latency with the same fixture
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
    harness::seed_exhausted_candidate_prefix(
        &pool,
        RunDispatch::new(harness::activation("template")),
        rows,
    )
    .await;
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
        "dispatch_incompatible_perf rows={rows} n={} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} min_ms={:.3} max_ms={:.3}",
        samples.len(),
        percentile(&samples, 50),
        percentile(&samples, 95),
        percentile(&samples, 99),
        samples[0],
        samples[samples.len() - 1],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a live PostgreSQL fixture and compares 1/8/32 simultaneous claimers"]
async fn concurrent_incompatible_front_claim_throughput() {
    // Causes: C1 the same exhausted high-priority prefix for every case;
    // C2 exactly N independent eligible Runs; C3 N claimers start behind one
    // barrier with N in {1,8,32}. Effects: E1 every Run gets one distinct
    // current claim; E2 print per-call p50/p95/p99 and whole-window claim/s under the
    // same database/pool. Rules T1=C1+C2+C3->E1+E2 for each N. No numeric
    // timing assertion is made in CI: old/new runs must alternate on one fixed
    // host, PostgreSQL image and pool before any performance claim is issued.
    let rows: i64 = std::env::var("AWAKEN_DISPATCH_PERF_INCOMPATIBLE_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5_000);
    assert!(
        rows >= 128,
        "deep comparison needs at least one full first page"
    );
    for (claimers, schema) in [
        (1, "t_pg_claim_concurrent_1"),
        (8, "t_pg_claim_concurrent_8"),
        (32, "t_pg_claim_concurrent_32"),
    ] {
        let pool = harness::schema_pool(schema)
            .await
            .expect("explicit performance run requires PostgreSQL");
        let store = std::sync::Arc::new(
            PostgresDispatchStore::with_pool(pool.clone())
                .await
                .expect("migrated dispatch store"),
        );
        harness::seed_exhausted_candidate_prefix(
            &pool,
            RunDispatch::new(harness::activation("template")),
            rows,
        )
        .await;
        for index in 0..claimers {
            store
                .enqueue(RunDispatch::new(harness::activation_on(
                    &format!("valid-{index}"),
                    &format!("valid-thread-{index}"),
                )))
                .await
                .unwrap();
        }
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(claimers + 1));
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..claimers {
            let store = store.clone();
            let barrier = barrier.clone();
            tasks.spawn(async move {
                barrier.wait().await;
                let started = Instant::now();
                let claimed = store
                    .claim(&format!("perf-owner-{index}"), 60_000, &Default::default())
                    .await
                    .expect("concurrent exact claim")
                    .expect("one eligible Run per claimant");
                (
                    claimed.lease.run_id.0,
                    started.elapsed().as_secs_f64() * 1_000.0,
                )
            });
        }
        let window = Instant::now();
        barrier.wait().await;
        let mut winners = std::collections::HashSet::new();
        let mut samples = Vec::new();
        while let Some(result) =
            tokio::time::timeout(std::time::Duration::from_secs(60), tasks.join_next())
                .await
                .expect("concurrent claim window terminates")
        {
            let (run_id, elapsed_ms) = result.unwrap();
            assert!(winners.insert(run_id), "T1/E1 no duplicate winner");
            samples.push(elapsed_ms);
        }
        let window_s = window.elapsed().as_secs_f64();
        assert_eq!(winners.len(), claimers, "T1/E1 every Run is claimed");
        samples.sort_by(f64::total_cmp);
        println!(
            "dispatch_claim_concurrent_perf rows={rows} claimers={claimers} \
             p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} claim_per_s={:.3}",
            percentile(&samples, 50),
            percentile(&samples, 95),
            percentile(&samples, 99),
            claimers as f64 / window_s,
        );
    }
}
