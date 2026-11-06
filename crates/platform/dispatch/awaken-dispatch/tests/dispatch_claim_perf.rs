//! Repeatable, opt-in PostgreSQL dispatch discovery measurement.
//!
//! Run on the same database and host before and after a candidate-query change.
//! Use --locked and alternate actual optimization revisions on one fixed fixture.
//! The printed timings are finite discovery observations, not Agent capacity.

mod harness;

use awaken_worker_contract::WorkerCurrentSource as _;
use std::time::Instant;

use awaken_dispatch::{PlacementRequirements, PostgresDispatchStore, RunDispatch};
use sqlx::types::Json;

// The schemas isolate rows, not CPU/IO/WAL. All scenarios in this binary share
// one admission so libtest's parallel execution cannot contaminate their loads.
static MEASUREMENT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn parameter(name: &str, default: usize, maximum: usize) -> usize {
    let value = std::env::var(name).unwrap_or_else(|_| default.to_string());
    let value = value
        .parse::<usize>()
        .expect("benchmark parameter must be an integer");
    assert!(
        (1..=maximum).contains(&value),
        "{name} must be in 1..={maximum}"
    );
    value
}

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

fn reported_percentile(samples: &[f64], percent: usize) -> String {
    // Reporting boundary, not statistical qualification: below one nominal
    // percentile interval (20 for p95, 100 for p99), print no numeric tail.
    let minimum = 100_usize.div_ceil(100 - percent);
    if samples.len() < minimum {
        return "insufficient_samples".to_owned();
    }
    format!("{:.3}", percentile(samples, percent))
}

fn report(label: &str, rows: i64, samples: &mut [f64], max_connections: u32) {
    samples.sort_by(f64::total_cmp);
    println!(
        "{label} rows={rows} n={} pool_max_connections={max_connections} \
         p50_ms={} p95_ms={} p99_ms={} min_ms={:.3} max_ms={:.3}",
        samples.len(),
        reported_percentile(samples, 50),
        reported_percentile(samples, 95),
        reported_percentile(samples, 99),
        samples[0],
        samples[samples.len() - 1],
    );
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

#[test]
fn sparse_samples_never_report_a_qualified_numeric_tail() {
    // Causes: sample count crosses p95's 20 and p99's 100 boundary; empty and
    // singleton samples are independent sparse inputs. Effects: Q4 below the
    // boundary -> explicit insufficient_samples; Q5 at the boundary -> the
    // existing nearest rank. This prevents max-of-nine being presented as p99.
    for (n, p, expected) in [
        (0, 95, "insufficient_samples"),
        (1, 99, "insufficient_samples"),
        (9, 95, "insufficient_samples"),
        (19, 95, "insufficient_samples"),
        (20, 95, "19.000"),
        (99, 99, "insufficient_samples"),
        (100, 99, "99.000"),
    ] {
        let samples = (1..=n).map(|value| value as f64).collect::<Vec<_>>();
        assert_eq!(reported_percentile(&samples, p), expected, "Q4/Q5 {n}/{p}");
    }
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
    let _measurement = MEASUREMENT.lock().await;
    let pool = harness::schema_pool("t_pg_claim_perf")
        .await
        .expect("explicit performance run requires PostgreSQL");
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("migrated dispatch store");
    let rows = parameter("AWAKEN_DISPATCH_PERF_ROWS", 20_000, 10_000_000) as i64;
    assert!(rows >= 100, "benchmark needs a meaningful pending queue");
    let rounds = parameter("AWAKEN_DISPATCH_PERF_ROUNDS", 1_000, 100_000);
    let depth_rounds = parameter("AWAKEN_DISPATCH_PERF_DEPTH_ROUNDS", 100, 100_000);
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
    let mut winners = std::collections::HashSet::new();
    for iteration in 0..=rounds {
        let started = Instant::now();
        let claimed = store
            .claim("perf-owner", 600_000, &Default::default())
            .await
            .expect("claim succeeds")
            .expect("runnable row remains");
        assert_eq!(claimed.lease.epoch(), 1, "fresh exact claim");
        assert!(
            winners.insert(claimed.lease.run_id().clone()),
            "R1/E1 distinct Run"
        );
        if iteration > 0 {
            samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }
    report(
        "dispatch_claim_perf",
        rows,
        &mut samples,
        pool.options().get_max_connections(),
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
    report(
        "dispatch_depth_perf",
        rows,
        &mut depth_samples,
        pool.options().get_max_connections(),
    );
}

#[tokio::test]
#[ignore = "requires a live PostgreSQL fixture and intentionally creates incompatible work"]
async fn incompatible_front_queue_claim_latency() {
    // Causes: C1 a high-priority front of pending rows has exhausted claim
    // epochs; C2 configurable ordinary lower-priority Runs remain; C3 each claim must
    // inspect past C1 without silently returning None. Effects: E1 only C2
    // runs are claimed; E2 print warm p50/p95/p99 latency with the same fixture
    // before and after the query change. Rule I1=C1+C2+C3 -> E1+E2. This probes
    // the worst-case cursor path that a broad all-compatible benchmark misses.
    let _measurement = MEASUREMENT.lock().await;
    let pool = harness::schema_pool("t_pg_claim_incompat_perf")
        .await
        .expect("explicit performance run requires PostgreSQL");
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("migrated dispatch store");
    let rows = parameter("AWAKEN_DISPATCH_PERF_INCOMPATIBLE_ROWS", 5_000, 10_000_000) as i64;
    let rounds = parameter("AWAKEN_DISPATCH_PERF_ROUNDS", 1_000, 100_000);
    harness::seed_exhausted_candidate_prefix(
        &pool,
        RunDispatch::new(harness::activation("template")),
        rows,
    )
    .await;
    for index in 0..=rounds {
        store
            .enqueue(RunDispatch::new(harness::activation_on(
                &format!("valid-{index}"),
                &format!("valid-thread-{index}"),
            )))
            .await
            .unwrap();
    }
    let mut samples = Vec::new();
    let mut winners = std::collections::HashSet::new();
    for iteration in 0..=rounds {
        let started = Instant::now();
        let claimed = store
            .claim("perf-owner", 600_000, &Default::default())
            .await
            .expect("claim succeeds")
            .expect("lower-priority runnable row remains");
        assert!(claimed.lease.run_id().0.starts_with("valid-"), "I1/E1");
        assert!(
            winners.insert(claimed.lease.run_id().clone()),
            "I1/E1 distinct Run"
        );
        if iteration > 0 {
            samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }
    report(
        "dispatch_incompatible_perf",
        rows,
        &mut samples,
        pool.options().get_max_connections(),
    );
}

#[tokio::test]
#[ignore = "requires an explicit live PostgreSQL placement-prefix measurement"]
async fn capability_incompatible_front_queue_claim_latency() {
    // Causes: P1 N high-priority Pending Runs require a real GPU capability;
    // P2 the requester lacks it; P3 M independent legal lower-priority Runs.
    // Effects: E1 every claim crosses the actual compatibility prefix, E2
    // each legal Run is claimed once, E3 report finite warm latency. Rule
    // G1=P1+P2+P3=>E1+E2+E3. Epoch-only prefix measurements cannot establish
    // this property because SQL can discard their rows before discovery.
    // The whole loop has one300s budget and long fixture leases; a slow
    // candidate cannot pass by reclaiming an earlier measured Run. Timings
    // remain scoped observations; alternate baseline/candidate on one host.
    let _measurement = MEASUREMENT.lock().await;
    let pool = harness::schema_pool("t_pg_claim_capability_perf")
        .await
        .expect("explicit capability measurement requires PostgreSQL");
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    let rows = parameter("AWAKEN_DISPATCH_PERF_INCOMPATIBLE_ROWS", 5_000, 10_000_000) as i64;
    let rounds = parameter("AWAKEN_DISPATCH_PERF_ROUNDS", 100, 100_000);
    let (directory, identity) =
        awaken_dispatch_testkit::worker_http::ready_worker("perf-no-gpu").await;
    let worker = directory
        .current(&identity.worker_id)
        .await
        .unwrap()
        .unwrap()
        .snapshot;
    let mut placement = PlacementRequirements::remote_required();
    placement.required_capabilities.insert("gpu".into());
    harness::seed_placement_candidate_prefix(
        &pool,
        RunDispatch::new(harness::activation("gpu-template"))
            .with_placement(placement)
            .with_worker_codec_requirements(),
        rows,
        "gpu-measure-",
        100,
    )
    .await;
    for index in 0..=rounds {
        store
            .enqueue(
                RunDispatch::new(harness::activation_on(
                    &format!("legal-{index}"),
                    &format!("legal-thread-{index}"),
                ))
                .with_placement(PlacementRequirements::remote_required())
                .with_worker_codec_requirements(),
            )
            .await
            .unwrap();
    }
    let mut samples = Vec::new();
    let mut winners = std::collections::HashSet::new();
    tokio::time::timeout(std::time::Duration::from_secs(300), async {
        for iteration in 0..=rounds {
            let started = Instant::now();
            let claim = store
                .claim_compatible(&worker, 3_600_000)
                .await
                .unwrap()
                .expect("G1 legal tail");
            assert!(claim.request.run_id().0.starts_with("legal-"));
            assert!(
                winners.insert(claim.request.run_id().0.clone()),
                "G1 distinct current claim"
            );
            if iteration > 0 {
                samples.push(started.elapsed().as_secs_f64() * 1_000.0);
            }
        }
    })
    .await
    .expect("G1 whole placement measurement budget");
    report(
        "dispatch_capability_prefix_perf",
        rows,
        &mut samples,
        pool.options().get_max_connections(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a live PostgreSQL fixture and compares 1/8/32/128 persistent claimers"]
async fn concurrent_incompatible_front_claim_throughput() {
    // Causes: C1 the same exhausted high-priority prefix for every case;
    // C2 the same total of independent eligible Runs; C3 N claimers start behind
    // one barrier with N in {1,8,32,128} and each repeatedly claims total/N Runs.
    // C4 the pool is smaller than 128 persistent claimers: losing exact ids
    // must retain independent transactions without requeueing each checkout.
    // Effects: E1 every Run gets one distinct current claim; E2 report per-call
    // p50/p95/p99 and finite-window claim/s with the observed pool limit;
    // E3 one 300s window bounds the whole case, not each join separately. Under the
    // same database/pool. Rules T1=C1+C2+C3->E1+E2 for each N. No numeric
    // timing assertion is made in CI: old/new runs must alternate on one fixed
    // host, PostgreSQL image and pool before any performance claim is issued.
    let _measurement = MEASUREMENT.lock().await;
    // T2 exercises actual immutable placement incompatibility, not an SQL-
    // pruned epoch. Both fixture kinds use this same contention/identity table.
    // T3 an explicit registered Worker requires current protocol coordinates
    // on the legal tail; the legacy v0 decoder default is not eligible for
    // that Worker. The local epoch-only case retains its original default.
    let kind = std::env::var("AWAKEN_DISPATCH_PERF_PREFIX_KIND")
        .unwrap_or_else(|_| "exhausted_epoch".to_owned());
    assert!(
        matches!(kind.as_str(), "exhausted_epoch" | "capability"),
        "unknown prefix kind"
    );
    let rows = parameter("AWAKEN_DISPATCH_PERF_INCOMPATIBLE_ROWS", 5_000, 10_000_000) as i64;
    let total = parameter("AWAKEN_DISPATCH_PERF_CONCURRENT_CLAIMS", 512, 100_000);
    assert!(
        total.is_multiple_of(128),
        "the same claim total must divide every claimant count"
    );
    assert!(
        rows >= 128,
        "deep comparison needs at least one full first page"
    );
    for (claimers, schema) in [
        (1, "t_pg_claim_concurrent_1"),
        (8, "t_pg_claim_concurrent_8"),
        (32, "t_pg_claim_concurrent_32"),
        (128, "t_pg_claim_concurrent_128"),
    ] {
        let pool = harness::schema_pool(schema)
            .await
            .expect("explicit performance run requires PostgreSQL");
        let store = std::sync::Arc::new(
            PostgresDispatchStore::with_pool(pool.clone())
                .await
                .expect("migrated dispatch store"),
        );
        let worker = if kind == "capability" {
            let (directory, identity) =
                awaken_dispatch_testkit::worker_http::ready_worker("contention-no-gpu").await;
            let worker = directory
                .current(&identity.worker_id)
                .await
                .unwrap()
                .unwrap()
                .snapshot;
            let mut placement = PlacementRequirements::remote_required();
            placement.required_capabilities.insert("gpu".into());
            harness::seed_placement_candidate_prefix(
                &pool,
                RunDispatch::new(harness::activation("template"))
                    .with_placement(placement)
                    .with_worker_codec_requirements(),
                rows,
                "gpu-contention-",
                100,
            )
            .await;
            Some(worker)
        } else {
            harness::seed_exhausted_candidate_prefix(
                &pool,
                RunDispatch::new(harness::activation("template")),
                rows,
            )
            .await;
            None
        };
        for index in 0..=total {
            let mut request = RunDispatch::new(harness::activation_on(
                &format!("valid-{index}"),
                &format!("valid-thread-{index}"),
            ));
            if worker.is_some() {
                request = request
                    .with_placement(PlacementRequirements::remote_required())
                    .with_worker_codec_requirements();
            }
            store.enqueue(request).await.unwrap();
        }
        measurement_claim(&store, "perf-warmup", worker.as_ref()).await;
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(claimers + 1));
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..claimers {
            let store = store.clone();
            let barrier = barrier.clone();
            let worker = worker.clone();
            tasks.spawn(async move {
                barrier.wait().await;
                let mut observations = Vec::new();
                for _ in 0..total / claimers {
                    let started = Instant::now();
                    let claimed =
                        measurement_claim(&store, &format!("perf-owner-{index}"), worker.as_ref())
                            .await;
                    assert_eq!(claimed.lease.epoch(), 1, "T1/E1 fresh current claim");
                    observations.push((
                        claimed.lease.run_id().0.clone(),
                        started.elapsed().as_secs_f64() * 1_000.0,
                    ));
                }
                observations
            });
        }
        let window = Instant::now();
        barrier.wait().await;
        let mut winners = std::collections::HashSet::new();
        let mut samples = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(300), async {
            while let Some(result) = tasks.join_next().await {
                for (run_id, elapsed_ms) in result.unwrap() {
                    assert!(run_id.starts_with("valid-"), "T1/E1 eligible Run");
                    assert!(winners.insert(run_id), "T1/E1 no duplicate winner");
                    samples.push(elapsed_ms);
                }
            }
        })
        .await
        .expect("T1/E3 whole measurement window terminates");
        let window_s = window.elapsed().as_secs_f64();
        assert_eq!(winners.len(), total, "T1/E1 every Run is claimed");
        report(
            &format!(
                "dispatch_claim_concurrent_perf prefix_kind={kind} claimers={claimers} window_s={window_s:.3} claim_per_s={:.3}",
                total as f64 / window_s
            ),
            rows,
            &mut samples,
            pool.options().get_max_connections(),
        );
    }
}

use awaken_dispatch_contract::{
    DispatchClaims as _, DispatchControlReads as _, DispatchEnqueue as _,
};

// The same fixture drives both necessary-condition and true placement prefixes;
// only the authored caller role differs. No benchmark substitutes a queue model.
async fn measurement_claim(
    store: &PostgresDispatchStore,
    owner: &str,
    worker: Option<&awaken_dispatch::WorkerSnapshot>,
) -> awaken_dispatch::Claimed {
    match worker {
        Some(worker) => store.claim_compatible(worker, 3_600_000).await,
        None => store.claim(owner, 3_600_000, &Default::default()).await,
    }
    .expect("measured exact claim")
    .expect("eligible measured Run remains")
}
