//! Force the relevant PostgreSQL transaction order and assert the committed
//! result. HTTP requests run on independent Coordinator processes.

use super::{
    ChildGuard, WORKER_ID, activation, configured_worker, database_url_in_schema, reserve_port,
    schema_pool, wait_for_port,
};
use awaken_agent_contract::{agent::run::Id as RunId, agent::thread::Id as ThreadId};
use awaken_coordinator_client::HttpDispatchQueue;
use awaken_dispatch::{Claimed, PlacementRequirements, PostgresDispatchStore, RunDispatch};
use awaken_dispatch_contract::{DispatchEnqueue as _, Outbox as _};
use awaken_worker_contract::WorkerIdentity;
use serde::Deserialize;
use sqlx::{PgPool, Postgres, Transaction};
use std::time::Duration;
use tokio::task::JoinHandle;
use uuid::Uuid;

const LOCK_BUDGET: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
struct ClaimReply {
    claimed: Option<Claimed>,
}

struct Cluster {
    _controls: [ChildGuard; 2],
    ports: [u16; 2],
    pool: PgPool,
    application_name: String,
    client: reqwest::Client,
    worker: WorkerIdentity,
}

impl Cluster {
    async fn start(case: &str) -> Option<Self> {
        let schema = format!("t_claim_receipt_{case}_{}", std::process::id());
        let (pool, url) = schema_pool(&schema).await?;
        let url = format!(
            "{}&application_name={schema}",
            database_url_in_schema(&url, &schema)
        );
        let a = reserve_port().await;
        let mut first = ChildGuard::spawn(a, &url, false);
        wait_for_port(a, &mut first).await;
        let b = reserve_port().await;
        let mut second = ChildGuard::spawn(b, &url, false);
        wait_for_port(b, &mut second).await;
        Some(Self {
            _controls: [first, second],
            ports: [a, b],
            pool,
            application_name: schema,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(35))
                .build()
                .unwrap(),
            worker: configured_worker().0,
        })
    }

    async fn enqueue(&self, label: &str) -> RunId {
        let run = RunId(format!("run-{label}"));
        let thread = ThreadId(format!("thread-{label}"));
        let mut request = RunDispatch::new(activation(&run, &thread));
        request.placement = PlacementRequirements::remote_preferred();
        request.placement.runtime_protocol_version =
            awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
        HttpDispatchQueue::new(
            format!("http://127.0.0.1:{}", self.ports[0]),
            self.worker.clone(),
        )
        .enqueue(request.with_worker_codec_requirements())
        .await
        .expect("enqueue a claimable Run");
        run
    }

    fn claim(
        &self,
        coordinator: usize,
        id: Uuid,
        run_id: Option<&RunId>,
    ) -> JoinHandle<Option<Claimed>> {
        self.claim_wait(coordinator, id, run_id, None)
    }

    fn claim_wait(
        &self,
        coordinator: usize,
        id: Uuid,
        run_id: Option<&RunId>,
        wait_ms: Option<u64>,
    ) -> JoinHandle<Option<Claimed>> {
        let endpoint = format!(
            "http://127.0.0.1:{}/v1/worker/dispatch/claim_for_request",
            self.ports[coordinator]
        );
        let request = self
            .client
            .post(endpoint)
            .header("x-awaken-worker-id", WORKER_ID)
            .json(&serde_json::json!({
                "identity": self.worker,
                "request_id": id.to_string(),
                "run_id": run_id,
                "wait_ms": wait_ms,
            }));
        tokio::spawn(async move {
            request
                .send()
                .await
                .expect("Coordinator replied")
                .error_for_status()
                .expect("claim request was accepted")
                .json::<ClaimReply>()
                .await
                .expect("Coordinator replied with a claim envelope")
                .claimed
        })
    }

    async fn hold_retention_fence(&self) -> Transaction<'static, Postgres> {
        let mut tx = self.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM runtime_dispatch_claim_retention WHERE singleton FOR UPDATE")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx
    }

    async fn wait_for_receipt_inserts(&self, expected: i64) {
        awaken_reliability_testkit::wait_for_postgres_lock_waiters(
            &self.pool,
            &self.application_name,
            expected,
            Some("INSERT INTO runtime_dispatch_claim_request"),
            LOCK_BUDGET,
        )
        .await
        .expect("each request must reach receipt insertion before the fence opens");
    }

    async fn dispatch_state(&self, run: &RunId) -> (String, i64, i64) {
        sqlx::query_as(
            "SELECT status, lease_epoch, attempt_count FROM runtime_dispatch WHERE run_id = $1",
        )
        .bind(&run.0)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn receipt_count(&self, request: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM runtime_dispatch_claim_request WHERE request_id = $1",
        )
        .bind(request.to_string())
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_request_on_two_coordinators_commits_only_one_claim() {
    let Some(cluster) = Cluster::start("overlap").await else {
        return;
    };
    let first_run = cluster.enqueue("one").await;
    let second_run = cluster.enqueue("two").await;
    let fence = cluster.hold_retention_fence().await;
    let id = Uuid::now_v7();

    // Both can tentatively acquire different Runs. Wait for both writes, so a
    // fast completed replay cannot satisfy the test without hitting the race.
    let a = cluster.claim(0, id, None);
    let b = cluster.claim(1, id, None);
    cluster.wait_for_receipt_inserts(2).await;
    fence.commit().await.unwrap();

    let a = a.await.unwrap().expect("first Coordinator returns a claim");
    let b = b
        .await
        .unwrap()
        .expect("second Coordinator returns the winner");
    assert_eq!(a.lease.run_id(), b.lease.run_id());
    assert_eq!(a.lease.epoch(), b.lease.epoch());
    assert_eq!(a.lease.owner(), b.lease.owner());
    let (winner, loser) = if a.lease.run_id() == &first_run {
        (first_run, second_run)
    } else {
        assert_eq!(a.lease.run_id(), &second_run);
        (second_run, first_run)
    };
    assert_eq!(
        cluster.dispatch_state(&winner).await,
        ("running".into(), 1, 0)
    );
    assert_eq!(
        cluster.dispatch_state(&loser).await,
        ("pending".into(), 0, 0)
    );
    assert_eq!(cluster.receipt_count(id).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claim_waiting_to_commit_cannot_cross_the_retention_cutoff() {
    let Some(cluster) = Cluster::start("cutoff").await else {
        return;
    };
    let run = cluster.enqueue("cutoff").await;
    let mut fence = cluster.hold_retention_fence().await;
    let (seconds, nanos) = Uuid::now_v7().get_timestamp().unwrap().to_unix();
    let before = uuid::Timestamp::from_unix(uuid::NoContext, seconds - 60, nanos);
    let id = Uuid::new_v7(before);

    // The initial read sees an open window. Publish the same durable cutoff
    // that cleanup advances while the exact claim waits to insert its receipt.
    let waiting = cluster.claim(0, id, Some(&run));
    cluster.wait_for_receipt_inserts(1).await;
    let cutoff = i64::try_from((seconds - 60) * 1_000 + u64::from(nanos / 1_000_000)).unwrap()
        + 15 * 60 * 1_000;
    sqlx::query("UPDATE runtime_dispatch_claim_retention SET expired_through_ms = $1")
        .bind(cutoff)
        .execute(&mut *fence)
        .await
        .unwrap();
    fence.commit().await.unwrap();

    assert!(waiting.await.unwrap().is_none());
    assert!(cluster.claim(1, id, None).await.unwrap().is_none());
    assert_eq!(cluster.dispatch_state(&run).await, ("pending".into(), 0, 0));
    assert_eq!(cluster.receipt_count(id).await, 0);
    let fresh = cluster
        .claim(1, Uuid::now_v7(), Some(&run))
        .await
        .unwrap()
        .expect("a fresh request can still claim the Run");
    assert_eq!(fresh.lease.epoch(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receipt_sweep_is_bounded_and_skips_locked_rows() {
    let Some(cluster) = Cluster::start("sweep").await else {
        return;
    };
    let run = cluster.enqueue("live").await;
    let id = Uuid::now_v7();
    let live = cluster.claim(0, id, Some(&run)).await.unwrap().unwrap();
    sqlx::query(
        "INSERT INTO runtime_dispatch_claim_request (lease_owner, request_id, expires_at_ms)
         SELECT $1, 'seed-expired-' || n, n FROM generate_series(1, 514) n",
    )
    .bind(cluster.worker.lease_owner())
    .execute(&cluster.pool)
    .await
    .unwrap();
    let store = PostgresDispatchStore::with_pool(cluster.pool.clone())
        .await
        .unwrap();
    let mut lock = cluster.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT 1 FROM runtime_dispatch_claim_request
         WHERE lease_owner = $1 AND request_id = 'seed-expired-1' FOR UPDATE",
    )
    .bind(cluster.worker.lease_owner())
    .execute(&mut *lock)
    .await
    .unwrap();
    let remaining = || {
        sqlx::query_scalar::<_, String>(
            "SELECT request_id FROM runtime_dispatch_claim_request
             WHERE request_id LIKE 'seed-expired-%' ORDER BY expires_at_ms",
        )
        .fetch_all(&cluster.pool)
    };

    // A held oldest row must not stall maintenance or let the sweep exceed
    // its budget. Once released it becomes eligible on the next pass.
    assert_eq!(
        tokio::time::timeout(LOCK_BUDGET, store.relay())
            .await
            .expect("sweep must skip the held receipt")
            .unwrap(),
        0
    );
    assert_eq!(
        remaining().await.unwrap(),
        ["seed-expired-1", "seed-expired-514"]
    );
    lock.commit().await.unwrap();
    assert_eq!(store.relay().await.unwrap(), 0);
    assert!(remaining().await.unwrap().is_empty());
    let replay = cluster.claim(1, id, Some(&run)).await.unwrap().unwrap();
    assert_eq!(replay.lease, live.lease);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registered_wait_finds_work_from_another_coordinator_without_notification() {
    let Some(cluster) = Cluster::start("wait_shards").await else {
        return;
    };
    let id = Uuid::now_v7();
    let mut waiting = cluster.claim_wait(1, id, None, Some(2_000));
    tokio::select! {
        response = &mut waiting => panic!("idle claim returned before work arrived: {response:?}"),
        // awaken-allow(wait): keep the queue empty across a poll interval before publishing without a wake.
        () = tokio::time::sleep(Duration::from_millis(350)) => {}
    }
    let run = cluster.enqueue("wait-cross-node").await;
    let first = waiting
        .await
        .unwrap()
        .expect("fallback finds the committed shard");
    assert_eq!(first.lease.run_id(), &run);
    let stored: i16 =
        sqlx::query_scalar("SELECT dispatch_shard FROM runtime_dispatch WHERE run_id = $1")
            .bind(&run.0)
            .fetch_one(&cluster.pool)
            .await
            .unwrap();
    assert_eq!(
        stored,
        i16::from(awaken_dispatch_contract::dispatch_shard(
            "",
            "thread-wait-cross-node"
        ))
    );
    let replay = cluster
        .claim_wait(0, id, None, Some(2_000))
        .await
        .unwrap()
        .expect("a live receipt replays even though its shard has no new work");
    assert_eq!(replay.lease.run_id(), &run);
    assert_eq!(replay.lease.epoch(), first.lease.epoch());
    assert_eq!(cluster.receipt_count(id).await, 1);
}
