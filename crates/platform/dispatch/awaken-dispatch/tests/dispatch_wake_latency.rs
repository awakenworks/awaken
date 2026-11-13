//! Opt-in measurement over the real PostgreSQL queue and production wake transports.
//! Use a dedicated AWAKEN_TEST_DATABASE_URL with track_commit_timestamp enabled
//! and pg_stat_statements installed and preloaded, plus AWAKEN_TEST_NATS_URL.
//! cargo test -p awaken-dispatch --features test-support,nats --test dispatch_wake_latency -- --ignored --nocapture
//! AWAKEN_WAKE_BENCH_PROFILE=pressure selects paced arrivals with and without
//! notifications. Set RATE, CLAIMERS, PRODUCERS, POOL_MAX and REPEATS with the
//! AWAKEN_WAKE_BENCH_ prefix. Three repeats rotate all three transport positions.
//! EXECUTION_HOLD_MS keeps each claimant slot occupied for a fixed duration.

mod harness;

use std::future::pending;
use std::sync::Arc;
use std::time::{Duration, Instant};

use awaken_dispatch::{
    DispatchClaims, DispatchEnqueue, NatsWakeSignal, PgNotifyWake, PostgresDispatchStore,
    RunDispatch,
};
use awaken_dispatch_contract::WakeSignal;
use futures_util::{StreamExt, TryStreamExt, stream};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

const SCHEMA: &str = "t_pg_dispatch_wake_latency";
const BURST: usize = 32;
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Poll,
    PgNotify,
    Nats,
}

#[derive(Clone, Copy, Debug)]
enum Load {
    Idle,
    Burst,
    Interrupted,
    Sustained,
    SustainedInterrupted,
}

impl Load {
    fn paced(self) -> bool {
        matches!(self, Self::Sustained | Self::SustainedInterrupted)
    }

    fn interrupted(self) -> bool {
        matches!(self, Self::Interrupted | Self::SustainedInterrupted)
    }
}

struct Workload {
    samples: usize,
    poll_ms: u64,
    claimers: usize,
    producers: usize,
    pool_max: u32,
    rate: u64,
    repeats: usize,
    execution_hold: Duration,
}

#[derive(Clone, Copy, Default)]
struct Progress {
    claims: usize,
    idle_claimers: usize,
    occupied: usize,
    peak_occupied: usize,
}

async fn transport(
    mode: Mode,
    pool: &PgPool,
    nats_url: &str,
    channel: &str,
) -> Option<Arc<dyn WakeSignal>> {
    match mode {
        Mode::Poll => None,
        Mode::PgNotify => Some(Arc::new(PgNotifyWake::new(pool.clone(), channel))),
        Mode::Nats => Some(Arc::new(
            tokio::time::timeout(TIMEOUT, NatsWakeSignal::connect(nats_url, channel))
                .await
                .expect("NATS connect deadline")
                .expect("live NATS fixture"),
        )),
    }
}

async fn warm_transport(sender: &dyn WakeSignal, receiver: &dyn WakeSignal) {
    let retry = async {
        loop {
            sender.publish().await.expect("warm wake publish");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            _ = receiver.wait() => {}
            _ = retry => unreachable!(),
        }
    })
    .await
    .expect("observe one live wake before measuring");
}

#[derive(Default)]
struct Drain {
    claims: usize,
    empty_claims: usize,
    wakes: usize,
    polls: usize,
}

/// Exercise only admission and first claim. Drain the actual queue until empty,
/// then race the production transport against the fallback timer. An optional
/// fixed hold consumes a claimant slot after each claim to model execution
/// capacity. There is no model, settlement, HTTP, or maintenance traffic.
async fn drain(
    store: Arc<PostgresDispatchStore>,
    wake: Option<Arc<dyn WakeSignal>>,
    interval: Duration,
    progress: watch::Sender<Progress>,
    stop: CancellationToken,
    owner: String,
    execution_hold: Duration,
) -> Drain {
    let mut result = Drain::default();
    // awaken-allow(wait): the fallback poll timer and wake transport are the mechanisms being measured.
    loop {
        let Some(claimed) = store
            .claim(&owner, 3_600_000, &Default::default())
            .await
            .expect("claim through PostgreSQL")
        else {
            result.empty_claims += 1;
            progress.send_modify(|current| current.idle_claimers += 1);
            tokio::select! {
                _ = stop.cancelled() => return result,
                _ = tokio::time::sleep(interval) => result.polls += 1,
                _ = async {
                    match wake.as_ref() {
                        Some(wake) => wake.wait().await,
                        None => pending::<()>().await,
                    }
                } => result.wakes += 1,
            }
            progress.send_modify(|current| current.idle_claimers -= 1);
            continue;
        };
        assert_eq!(claimed.lease.epoch(), 1, "only the first durable claim");
        result.claims += 1;
        progress.send_modify(|current| {
            current.occupied += 1;
            current.peak_occupied = current.peak_occupied.max(current.occupied);
        });
        if !execution_hold.is_zero() {
            tokio::time::sleep(execution_hold).await;
        }
        progress.send_modify(|current| {
            current.occupied -= 1;
            current.claims += 1;
        });
    }
}

async fn wait_progress(
    progress: &mut watch::Receiver<Progress>,
    count: usize,
    idle_claimers: usize,
    deadline: Duration,
) {
    let result = tokio::time::timeout(deadline, async {
        progress
            .wait_for(|seen| seen.claims == count && seen.idle_claimers >= idle_claimers)
            .await
            .expect("claim loop is alive");
    })
    .await;
    if result.is_err() {
        let seen = progress.borrow();
        panic!(
            "drain observed {}/{} claims and {}/{} idle claimers before {deadline:?}",
            seen.claims, count, seen.idle_claimers, idle_claimers,
        );
    }
}

async fn submit(
    store: &PostgresDispatchStore,
    wake: Option<&dyn WakeSignal>,
    index: usize,
    scheduled: tokio::time::Instant,
) -> Result<f64, awaken_dispatch_contract::DispatchError> {
    let id = format!("wake-bench-{index}");
    store
        .enqueue(RunDispatch::new(harness::activation_on(&id, &id)))
        .await?;
    let admission_ms = scheduled.elapsed().as_secs_f64() * 1_000.0;
    if let Some(wake) = wake {
        wake.publish().await?;
    }
    Ok(admission_ms)
}

async fn paced_arrivals(
    store: &PostgresDispatchStore,
    wake: Option<&dyn WakeSignal>,
    workload: &Workload,
) -> Vec<f64> {
    // The arrival scheduler never waits for a claim or a producer slot. The
    // bounded channel can hold the full prescribed schedule. Report the time
    // spent waiting for a slot separately from the commit-to-claim latency.
    let (arrivals, receiver) = mpsc::channel(workload.samples);
    let origin = tokio::time::Instant::now() + Duration::from_millis(10);
    let rate = workload.rate;
    let samples = workload.samples;
    let schedule = async move {
        for index in 0..samples {
            let deadline = origin + Duration::from_nanos(index as u64 * 1_000_000_000 / rate);
            tokio::time::sleep_until(deadline).await;
            arrivals
                .try_send((index, deadline))
                .expect("bounded arrival schedule");
        }
    };
    let publish = stream::unfold(receiver, |mut receiver| async {
        receiver.recv().await.map(|item| (item, receiver))
    })
    .map(|(index, deadline)| submit(store, wake, index, deadline))
    .buffer_unordered(workload.producers)
    .try_collect();
    let ((), admitted) = tokio::join!(schedule, publish);
    admitted.expect("enqueue the paced workload")
}

async fn query_calls(pool: &PgPool) -> (i64, i64, i64) {
    sqlx::query_as(
        "SELECT COALESCE(sum(calls), 0)::bigint,
                COALESCE(sum(calls) FILTER (WHERE query ILIKE '%runtime_dispatch%'), 0)::bigint,
                COALESCE(sum(calls) FILTER (WHERE query ILIKE '%pg_notify%'), 0)::bigint
         FROM public.pg_stat_statements
         WHERE toplevel AND dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
           AND query NOT ILIKE '%pg_stat_statements%'",
    )
    .fetch_one(pool)
    .await
    .expect("read database statement counters on the dedicated fixture")
}

async fn install_commit_observer(pool: &PgPool) {
    let track_commit_timestamp: String = sqlx::query_scalar("SHOW track_commit_timestamp")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        track_commit_timestamp, "on",
        "enable fixture commit timestamps"
    );
    sqlx::raw_sql(
        "CREATE TABLE wake_measurement (
            run_id text NOT NULL,
            boundary text NOT NULL,
            transaction_id xid NOT NULL,
            PRIMARY KEY (run_id, boundary)
         );
         CREATE FUNCTION observe_wake_commit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF TG_OP = 'INSERT' THEN
                 INSERT INTO wake_measurement VALUES (NEW.run_id, 'enqueue', pg_current_xact_id()::xid);
             ELSIF NEW.status = 'running' AND OLD.status <> 'running' THEN
                 INSERT INTO wake_measurement VALUES (NEW.run_id, 'claim', pg_current_xact_id()::xid);
             END IF;
             RETURN NEW;
         END $$;
         CREATE TRIGGER observe_wake_commit AFTER INSERT OR UPDATE ON runtime_dispatch
         FOR EACH ROW EXECUTE FUNCTION observe_wake_commit();",
    )
    .execute(pool)
    .await
    .expect("install transaction-ID observer in the benchmark schema");
}

async fn claim_latencies(pool: &PgPool, expected: usize) -> Vec<f64> {
    let values: Vec<Option<f64>> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM
            (pg_xact_commit_timestamp(c.transaction_id) - pg_xact_commit_timestamp(e.transaction_id))
         )::double precision * 1000
         FROM wake_measurement e
         JOIN wake_measurement c ON c.run_id = e.run_id AND c.boundary = 'claim'
         WHERE e.boundary = 'enqueue' ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .expect("read paired commit timestamps");
    assert_eq!(
        values.len(),
        expected,
        "every enqueued Run has a durable claim"
    );
    values
        .into_iter()
        .map(|sample| {
            let sample = sample.expect("both commits have timestamps");
            assert!(sample >= 0.0, "claim follows committed enqueue");
            sample
        })
        .collect()
}

async fn peak_pending(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "WITH events AS (
            SELECT pg_xact_commit_timestamp(transaction_id) AS committed,
                   CASE boundary WHEN 'enqueue' THEN 1 ELSE -1 END AS delta
            FROM wake_measurement
         ), pending AS (
            SELECT sum(delta) OVER (ORDER BY committed, delta DESC ROWS UNBOUNDED PRECEDING)
            FROM events
         ) SELECT max(sum) FROM pending",
    )
    .fetch_one(pool)
    .await
    .expect("read peak committed queue depth from the observed transaction boundaries")
}

async fn measure(
    pool: &PgPool,
    store: &Arc<PostgresDispatchStore>,
    nats_url: &str,
    mode: Mode,
    load: Load,
    workload: &Workload,
    round: usize,
) {
    let samples = workload.samples;
    let poll_ms = workload.poll_ms;
    sqlx::raw_sql("DELETE FROM runtime_dispatch; TRUNCATE wake_measurement")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("VACUUM (ANALYZE) runtime_dispatch")
        .execute(pool)
        .await
        .expect("reset the empty queue's statistics before the measurement");
    let channel = format!("awaken_wake_latency_{}", std::process::id());
    let sender = transport(mode, pool, nats_url, &channel).await;
    let receiver = transport(mode, pool, nats_url, &channel).await;
    if let (Some(sender), Some(receiver)) = (sender.as_deref(), receiver.as_deref()) {
        warm_transport(sender, receiver).await;
    }
    // Suppress delivery to the claimant for the entire case. The producer
    // still performs its real publish, including PostgreSQL's NOTIFY query.
    let interrupted = load.interrupted();
    let receive = if interrupted { None } else { receiver.clone() };
    let stop = CancellationToken::new();
    let _stop_on_unwind = stop.clone().drop_guard();
    let (progress, mut observed) = watch::channel(Progress::default());
    let before = query_calls(pool).await;
    let start = Instant::now();
    let workers = (0..workload.claimers)
        .map(|index| {
            tokio::spawn(drain(
                store.clone(),
                receive.clone(),
                Duration::from_millis(poll_ms),
                progress.clone(),
                stop.clone(),
                format!("wake-bench-worker-{index}"),
                workload.execution_hold,
            ))
        })
        .collect::<Vec<_>>();
    wait_progress(&mut observed, 0, workload.claimers, TIMEOUT).await;
    let admission_p99_ms = if load.paced() {
        let mut admitted = paced_arrivals(store, sender.as_deref(), workload).await;
        wait_progress(&mut observed, samples, 0, Duration::from_secs(120)).await;
        admitted.sort_unstable_by(f64::total_cmp);
        Some(percentile(&admitted, 990))
    } else {
        let batch = if matches!(load, Load::Burst) {
            BURST
        } else {
            1
        };
        let mut jitter = 0x8499_b1de_1250_f3ac_u64;
        for first in (0..samples).step_by(batch) {
            jitter = jitter.wrapping_mul(6364136223846793005).wrapping_add(1);
            tokio::time::sleep(Duration::from_millis((jitter >> 32) % poll_ms)).await;
            let end = (first + batch).min(samples);
            stream::iter(first..end)
                .map(|index| submit(store, sender.as_deref(), index, tokio::time::Instant::now()))
                .buffer_unordered(workload.producers)
                .try_for_each(|_| async { Ok(()) })
                .await
                .expect("enqueue and publish using the selected transport");
            wait_progress(&mut observed, end, workload.claimers, TIMEOUT).await;
        }
        None
    };
    stop.cancel();
    let mut drained = Drain::default();
    for worker in workers {
        let result = worker.await.expect("join the claimant");
        drained.claims += result.claims;
        drained.empty_claims += result.empty_claims;
        drained.wakes += result.wakes;
        drained.polls += result.polls;
    }
    let seconds = start.elapsed().as_secs_f64();
    let after = query_calls(pool).await;
    assert_eq!(drained.claims, samples);
    if mode == Mode::Poll || interrupted {
        assert_eq!(drained.wakes, 0, "case must rely on polling");
        assert!(drained.polls > 0);
    } else if !load.paced() {
        assert!(
            drained.wakes > 0,
            "healthy case exercised the real notification"
        );
    }
    let values = claim_latencies(pool, samples).await;
    let peak_occupied = observed.borrow().peak_occupied;
    if load.paced() && !workload.execution_hold.is_zero() {
        assert_eq!(
            peak_occupied, workload.claimers,
            "all claimant slots were occupied"
        );
    }
    let calls = after.0 - before.0;
    assert!(
        calls >= samples as i64,
        "statement counters covered the workload"
    );
    let notify_calls = after.2 - before.2;
    assert_eq!(
        notify_calls,
        if mode == Mode::PgNotify {
            samples as i64
        } else {
            0
        },
        "the dedicated database observes exactly this mode's NOTIFY calls",
    );
    println!(
        "WAKE_LATENCY {}",
        serde_json::json!({
            "mode": format!("{mode:?}"),
            "load": format!("{load:?}"),
            "round": round,
            "samples": samples,
            "poll_ms": poll_ms,
            "claimers": workload.claimers,
            "producers": workload.producers,
            "pool_max": workload.pool_max,
            "execution_hold_ms": workload.execution_hold.as_millis(),
            "peak_occupied": peak_occupied,
            "offered_per_second": load.paced().then_some(workload.rate),
            "admission_p99_ms": admission_p99_ms,
            "peak_pending": peak_pending(pool).await,
            "p99_ms": percentile(&values, 990),
            "p999_ms": percentile(&values, 999),
            "elapsed_seconds": seconds,
            "db_calls": calls,
            "db_qps": calls as f64 / seconds,
            "db_calls_per_run": calls as f64 / samples as f64,
            "dispatch_sql_calls": after.1 - before.1,
            "pg_notify_sql_calls": notify_calls,
            "empty_claims": drained.empty_claims,
            "poll_wins": drained.polls,
            "wake_wins": drained.wakes,
        })
    );
}

fn percentile(values: &[f64], permille: usize) -> f64 {
    values[(values.len() * permille).div_ceil(1000) - 1]
}

fn parameter(name: &str, default: usize, range: std::ops::RangeInclusive<usize>) -> usize {
    let value =
        std::env::var(name).map_or(default, |value| value.parse().expect("integer parameter"));
    assert!(range.contains(&value), "{name} must be within {range:?}");
    value
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live PostgreSQL and NATS latency benchmark"]
async fn dispatch_wake_latency() {
    let workload = Workload {
        samples: parameter("AWAKEN_WAKE_BENCH_SAMPLES", 2_000, 1_000..=100_000),
        poll_ms: parameter("AWAKEN_WAKE_BENCH_POLL_MS", 25, 1..=2_000) as u64,
        claimers: parameter("AWAKEN_WAKE_BENCH_CLAIMERS", 1, 1..=32),
        producers: parameter("AWAKEN_WAKE_BENCH_PRODUCERS", 4, 1..=64),
        pool_max: parameter("AWAKEN_WAKE_BENCH_POOL_MAX", 10, 3..=64) as u32,
        rate: parameter("AWAKEN_WAKE_BENCH_RATE", 1_000, 1..=100_000) as u64,
        repeats: parameter("AWAKEN_WAKE_BENCH_REPEATS", 1, 1..=12),
        execution_hold: Duration::from_millis(parameter(
            "AWAKEN_WAKE_BENCH_EXECUTION_HOLD_MS",
            0,
            0..=1000,
        ) as u64),
    };
    let profile = std::env::var("AWAKEN_WAKE_BENCH_PROFILE").unwrap_or_default();
    let loads: &[Load] = match profile.as_str() {
        "" | "baseline" => &[Load::Idle, Load::Burst, Load::Interrupted],
        "pressure" => &[Load::Sustained, Load::SustainedInterrupted],
        other => panic!("unknown AWAKEN_WAKE_BENCH_PROFILE {other:?}"),
    };
    let nats_url = std::env::var("AWAKEN_TEST_NATS_URL").expect("live NATS fixture is required");
    harness::schema_pool(SCHEMA)
        .await
        .expect("live dedicated PostgreSQL fixture is required")
        .close()
        .await;
    let pool = PgPoolOptions::new()
        .min_connections(workload.pool_max)
        .max_connections(workload.pool_max)
        .connect(&harness::database_url_in_schema(SCHEMA))
        .await
        .expect("pre-open the configured PostgreSQL connection budget");
    let store = Arc::new(
        PostgresDispatchStore::with_pool(pool.clone())
            .await
            .expect("migrate the benchmark dispatch store"),
    );
    install_commit_observer(&pool).await;
    for round in 1..=workload.repeats {
        for position in 0..loads.len() {
            let index = (position + round - 1) % loads.len();
            let mut modes = [Mode::Poll, Mode::PgNotify, Mode::Nats];
            let offset = (index + round - 1) % modes.len();
            modes.rotate_left(offset);
            for mode in modes {
                measure(
                    &pool,
                    &store,
                    &nats_url,
                    mode,
                    loads[index],
                    &workload,
                    round,
                )
                .await;
            }
        }
    }
}
