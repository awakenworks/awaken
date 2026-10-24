//! T34-C01: one WorkQueue behaviour table across Volatile, in-memory SQLite,
//! PostgreSQL (work_queue_conformance.rs) and real-file SQLite.
//!
//! | Row | Backend | Suite |
//! |---|---|---|
//! | P1 | Volatile / in-memory SQLite / PostgreSQL | `run_suite` (work_queue_conformance.rs) |
//! | P2 | file SQLite, fresh file per rule | `run_suite` |
//! | P3 | file SQLite, drop + reopen the same file | `reopen_preserves_authority` |
//! | P4 | PostgreSQL, drop pool + new independent pool on the same schema | `reopen_preserves_authority` |
//!
//! Negative controls, executed through the SAME suites (each must fail):
//! NC1 a divergent wrapper that rebuilds a refused ack/stop result from the
//! current row (reports `Accepted` instead of the owner fence's
//! `PreconditionFailed`); NC2 an in-memory "file" factory for the reopen step.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_environment_contract::EnvironmentRevision;
use awaken_executable_environment_contract::{
    EnvironmentWorkLifecycle, EnvironmentWorkRevisionError, EnvironmentWorkRevisionOperation,
    EnvironmentWorkRevisionOutcome, HeartbeatResult, LeaseHeartbeat, QueueStats,
    SessionAccessAuthenticator, SessionAccessDigest, SessionWorkLeaseControl, SessionWorkStore,
    SessionWorkerLeaseStore, WorkClaim, WorkConsumer, WorkItem, WorkMetadataPatch,
    WorkMutationResult, WorkQueries, WorkQueueError, WorkSessionAccess, WorkerLeaseObservation,
};
use awaken_session_types::{SessionId, SessionWorkLease};
use awaken_store_conformance::work_queue::{reopen_preserves_authority, run_suite};
use awaken_time::WallClock;
use awaken_work_store::{LEASE_TTL_MS, PostgresWorkQueue, SqliteWorkQueue};

#[path = "support/work_postgres.rs"]
mod work_postgres_fixture;

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

/// Run a suite on its own thread and report whether it panicked.
fn fails<F: std::future::Future<Output = ()> + 'static>(
    suite: impl FnOnce() -> F + Send + 'static,
) -> bool {
    std::thread::spawn(move || block(suite())).join().is_err()
}

#[test]
fn file_sqlite_backend_conforms_with_a_fresh_file_per_rule() {
    let dir = tempfile::tempdir().unwrap();
    let next = std::sync::atomic::AtomicUsize::new(0);
    block(run_suite(
        awaken_time::system_clock(),
        |clock| {
            let path = dir.path().join(format!(
                "rule-{}.db",
                next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            async move {
                SqliteWorkQueue::open(path.to_str().unwrap(), clock)
                    .await
                    .expect("file SQLite queue")
            }
        },
        LEASE_TTL_MS,
    ));
    assert!(
        next.load(std::sync::atomic::Ordering::SeqCst) >= 17,
        "P2 every rule received its own file"
    );
}

#[test]
fn file_sqlite_reopen_preserves_authority_and_in_memory_reopen_is_detected() {
    block(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reopen.db").to_str().unwrap().to_string();
        let first = SqliteWorkQueue::open(&path, awaken_time::system_clock())
            .await
            .unwrap();
        reopen_preserves_authority(first, || {
            let path = path.clone();
            async move {
                SqliteWorkQueue::open(&path, awaken_time::system_clock())
                    .await
                    .unwrap()
            }
        })
        .await;
    });
    // NC2: an in-memory "file" factory cannot pass the reopen row.
    assert!(
        fails(|| async {
            let first = SqliteWorkQueue::open_in_memory(awaken_time::system_clock())
                .await
                .unwrap();
            reopen_preserves_authority(first, || async {
                SqliteWorkQueue::open_in_memory(awaken_time::system_clock())
                    .await
                    .unwrap()
            })
            .await;
        }),
        "NC2 in-memory reopen must fail the reopen row"
    );
}

/// NC1: a deliberately divergent backend over the SQLite reference. A refused
/// ack/stop is rebuilt from the current row and reported as accepted.
struct RebuildsMutationResult {
    inner: SqliteWorkQueue,
}

impl RebuildsMutationResult {
    async fn rebuilt(
        &self,
        env_id: &str,
        wid: &str,
        result: WorkMutationResult,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        match result {
            WorkMutationResult::PreconditionFailed => {
                Ok(match self.inner.get(env_id, wid).await? {
                    Some(current) => WorkMutationResult::accepted(current),
                    None => WorkMutationResult::PreconditionFailed,
                })
            }
            other => Ok(other),
        }
    }
}

#[async_trait]
impl WorkQueries for RebuildsMutationResult {
    async fn list(&self, env_id: &str) -> Result<Vec<WorkItem>, WorkQueueError> {
        self.inner.list(env_id).await
    }
    async fn get(&self, env_id: &str, wid: &str) -> Result<Option<WorkItem>, WorkQueueError> {
        self.inner.get(env_id, wid).await
    }
    async fn stats(&self, env_id: &str, now_ms: u64) -> Result<QueueStats, WorkQueueError> {
        self.inner.stats(env_id, now_ms).await
    }
}

#[async_trait]
impl WorkConsumer for RebuildsMutationResult {
    async fn claim(
        &self,
        env_id: &str,
        worker_id: &str,
        now_ms: u64,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.inner.claim(env_id, worker_id, now_ms).await
    }
    async fn claim_with_reclaim(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        now_ms: u64,
        reclaim_older_than_ms: Option<u64>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.inner
            .claim_with_reclaim(
                env_id,
                lease_owner,
                poller_id,
                now_ms,
                reclaim_older_than_ms,
            )
            .await
    }
    async fn claim_with_session_access(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        now_ms: u64,
        reclaim_older_than_ms: Option<u64>,
        session_access: &SessionAccessDigest,
    ) -> Result<Option<WorkClaim>, WorkQueueError> {
        self.inner
            .claim_with_session_access(
                env_id,
                lease_owner,
                poller_id,
                now_ms,
                reclaim_older_than_ms,
                session_access,
            )
            .await
    }
    async fn ack(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let result = self.inner.ack(env_id, wid, worker_id).await?;
        self.rebuilt(env_id, wid, result).await
    }
    async fn heartbeat(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        self.inner
            .heartbeat(env_id, wid, worker_id, now_ms, heartbeat)
            .await
    }
    async fn stop(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let result = self.inner.stop(env_id, wid, worker_id).await?;
        self.rebuilt(env_id, wid, result).await
    }
}

#[async_trait]
impl SessionAccessAuthenticator for RebuildsMutationResult {
    async fn authenticate_session_access(
        &self,
        presented: &SessionAccessDigest,
        now_ms: u64,
    ) -> Result<Option<WorkSessionAccess>, WorkQueueError> {
        self.inner
            .authenticate_session_access(presented, now_ms)
            .await
    }
}

#[async_trait]
impl SessionWorkLeaseControl for RebuildsMutationResult {
    async fn ack_with_session_access(
        &self,
        access: &WorkSessionAccess,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        self.inner.ack_with_session_access(access).await
    }
    async fn heartbeat_with_session_access(
        &self,
        access: &WorkSessionAccess,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        self.inner
            .heartbeat_with_session_access(access, now_ms, heartbeat)
            .await
    }
    async fn stop_with_session_access(
        &self,
        access: &WorkSessionAccess,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        self.inner.stop_with_session_access(access).await
    }
}

#[async_trait]
impl SessionWorkStore for RebuildsMutationResult {
    async fn enqueue_session(
        &self,
        env_id: &str,
        session_id: &SessionId,
    ) -> Result<String, WorkQueueError> {
        self.inner.enqueue_session(env_id, session_id).await
    }
    async fn wake_session(
        &self,
        env_id: &str,
        session_id: &SessionId,
    ) -> Result<String, WorkQueueError> {
        self.inner.wake_session(env_id, session_id).await
    }
    async fn retire_session(
        &self,
        env_id: &str,
        session_id: &SessionId,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.inner.retire_session(env_id, session_id).await
    }
}

#[async_trait]
impl SessionWorkerLeaseStore for RebuildsMutationResult {
    async fn release_owner(&self, worker_owner: &str) -> Result<usize, WorkQueueError> {
        self.inner.release_owner(worker_owner).await
    }
    async fn release_session(&self, lease: &SessionWorkLease) -> Result<bool, WorkQueueError> {
        self.inner.release_session(lease).await
    }
    async fn acquire_session(
        &self,
        env_id: &str,
        session_id: &SessionId,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<SessionWorkLease>, WorkQueueError> {
        self.inner
            .acquire_session(env_id, session_id, worker_owner, now_ms)
            .await
    }
}

#[async_trait]
impl WorkerLeaseObservation for RebuildsMutationResult {
    async fn has_owner_claims(&self, worker_owner: &str) -> Result<bool, WorkQueueError> {
        self.inner.has_owner_claims(worker_owner).await
    }
}

#[async_trait]
impl WorkMetadataPatch for RebuildsMutationResult {
    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, Option<String>>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.inner.update_metadata(env_id, wid, patch).await
    }
}

#[async_trait]
impl EnvironmentWorkLifecycle for RebuildsMutationResult {
    async fn apply_environment_revision(
        &self,
        env_id: &str,
        revision: EnvironmentRevision,
        operation: EnvironmentWorkRevisionOperation,
    ) -> Result<EnvironmentWorkRevisionOutcome, EnvironmentWorkRevisionError> {
        self.inner
            .apply_environment_revision(env_id, revision, operation)
            .await
    }
    async fn enqueue_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
        self.inner.enqueue_healthcheck(env_id).await
    }
    async fn ensure_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
        self.inner.ensure_healthcheck(env_id).await
    }
    async fn remove_env(&self, env_id: &str) -> Result<(), WorkQueueError> {
        self.inner.remove_env(env_id).await
    }
}

#[test]
fn divergent_backend_fails_the_shared_behaviour_table() {
    assert!(
        fails(|| run_suite(
            awaken_time::system_clock(),
            async |clock: Arc<dyn WallClock>| RebuildsMutationResult {
                inner: SqliteWorkQueue::open_in_memory(clock).await.unwrap(),
            },
            LEASE_TTL_MS,
        )),
        "NC1 a rebuilt ack/stop result must fail the shared suite"
    );
    // The same wrapper also fails the reopen row (R3 foreign ack is fenced).
    assert!(
        fails(|| async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("nc.db").to_str().unwrap().to_string();
            let open = || {
                let path = path.clone();
                async move {
                    RebuildsMutationResult {
                        inner: SqliteWorkQueue::open(&path, awaken_time::system_clock())
                            .await
                            .unwrap(),
                    }
                }
            };
            reopen_preserves_authority(open().await, open).await;
        }),
        "NC1 a rebuilt ack/stop result must fail the reopen row"
    );
}

#[tokio::test]
async fn postgres_reopen_preserves_authority_through_an_independent_pool() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = work_postgres_fixture::WorkPostgresFixture::new(&url, 1).await;
    let scoped_url = fixture.scoped_url.clone();
    let connect = move || {
        let scoped_url = scoped_url.clone();
        async move {
            awaken_sql_storage::postgres_pool_options()
                .max_connections(2)
                .connect(&scoped_url)
                .await
                .expect("independent pool on the fixture schema")
        }
    };
    // `first` owns its own pool, which is dropped with it before the reopen.
    let first = PostgresWorkQueue::with_pool(connect().await, awaken_time::system_clock())
        .await
        .expect("queue schema migrations");
    reopen_preserves_authority(first, || {
        let connect = connect.clone();
        async move {
            PostgresWorkQueue::with_existing_pool(connect().await, awaken_time::system_clock())
                .await
                .expect("existing schema verified")
        }
    })
    .await;
    fixture.cleanup().await;
}

#[test]
fn postgres_reopen_preserves_authority_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_reopen_preserves_authority_through_an_independent_pool",
    );
}
