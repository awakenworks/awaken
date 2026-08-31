//! A runtime-selectable dispatch backend: one type that carries either the SQLite
//! or the Postgres store, chosen at assembly time (single-machine vs a multi-node
//! fleet sharing one Postgres queue, ADR-0019). It holds the active backend as an
//! `Arc<dyn Dispatch>` and re-implements the `DispatchQueue`/`Inbox`/`Outbox`
//! bundle by delegating through that trait object, so
//! `DurableRunIngress<AnyDispatchStore>` and `DispatchService<AnyDispatchStore>`
//! carry either backend behind one concrete, non-generic type — the host holds a
//! non-generic field and picks the variant from configuration.
//!
//! The trait-object indirection is deliberate: delegating through `dyn Dispatch`
//! erases the Postgres backend's sqlx `Executor` future to a `dyn Future + Send`
//! at the trait boundary, so composing it into the daemon's spawned task stays
//! `Send` (a direct enum match over the concrete sqlx store trips the sqlx
//! higher-ranked "implementation of Send is not general enough" limitation).

use std::sync::Arc;

use crate::{PlacementPolicy, WorkerSnapshot};
use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_runtime_contract::resume::ResumeResult;

use crate::dispatch::{
    CasOutcome, Claimed, CommitEpochGuard, ContinuationAdmission, CredentialRealizationReceipt,
    Dispatch, DispatchCompletion, DispatchError, DispatchOutcome, DispatchQueue, DispatchSummary,
    Inbox, Outbox, PendingInput, PendingRecord, RunClaim, SessionChildAdmission,
    SessionRunReservationActivation, SessionRunReservationOutcome, SessionRunReservationResolution,
    SettleOutcome, SubmitOptions,
};
#[cfg(feature = "durable")]
use crate::postgres::PostgresDispatchStore;
#[cfg(feature = "durable")]
use crate::sqlite::SqliteDispatchStore;
use crate::{DispatchCursor, DispatchOperationalFeed, DispatchPage};
use awaken_run_ingress_contract::RunDispatch;

/// The active durable-dispatch backend behind one concrete type. Both a SQLite
/// and a Postgres store satisfy the full `Dispatch` bundle; this holds whichever
/// was selected and forwards every call to it.
pub struct AnyDispatchStore {
    inner: Arc<dyn Dispatch>,
    operational: Option<Arc<dyn DispatchOperationalFeed>>,
    stream_checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
}

impl AnyDispatchStore {
    /// Open the SQLite backend at `path` (per-thread file queue; survives restart).
    #[cfg(feature = "durable")]
    pub fn open_sqlite(path: &str) -> Result<Self, String> {
        SqliteDispatchStore::open(path)
            .map(Self::from_store)
            .map_err(|e| e.to_string())
    }

    /// Open an in-memory SQLite backend for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_sqlite_in_memory() -> Result<Self, String> {
        SqliteDispatchStore::open_in_memory()
            .map(Self::from_store)
            .map_err(|e| e.to_string())
    }

    /// Connect the shared Postgres backend at `url`: one queue for a multi-node
    /// fleet, `FOR UPDATE SKIP LOCKED` gives distinct-claim across processes.
    #[cfg(feature = "durable")]
    pub async fn connect_postgres(url: &str, max_connections: u32) -> Result<Self, String> {
        PostgresDispatchStore::connect(url, max_connections)
            .await
            .map(Self::from_postgres_store)
            .map_err(|e| e.to_string())
    }

    /// Connect to a Postgres queue whose migration ledger was applied by the
    /// deployment migration phase. No DDL is executed.
    #[cfg(feature = "durable")]
    pub async fn connect_postgres_existing(
        url: &str,
        max_connections: u32,
    ) -> Result<Self, String> {
        PostgresDispatchStore::connect_existing(url, max_connections)
            .await
            .map(Self::from_postgres_store)
            .map_err(|e| e.to_string())
    }

    /// Build the migrated Postgres dispatch authority from the process-owned
    /// pool. Composition roots use this to share one reconnect/backpressure
    /// budget with commit and Worker-registry adapters for the same database.
    #[cfg(feature = "durable")]
    pub async fn with_postgres_pool(pool: sqlx::PgPool) -> Result<Self, String> {
        PostgresDispatchStore::with_pool(pool)
            .await
            .map(Self::from_postgres_store)
            .map_err(|error| error.to_string())
    }

    /// Verify an externally migrated dispatch schema over a process-owned pool.
    #[cfg(feature = "durable")]
    pub async fn with_existing_postgres_pool(pool: sqlx::PgPool) -> Result<Self, String> {
        PostgresDispatchStore::with_existing_pool(pool)
            .await
            .map(Self::from_postgres_store)
            .map_err(|error| error.to_string())
    }

    /// Pair a process-owned dispatch pool with its PG-NOTIFY hint. The listener
    /// clones the pool handle; it does not create a second connection pool.
    #[cfg(feature = "durable")]
    pub async fn with_postgres_pool_and_wake(
        pool: sqlx::PgPool,
        channel: &str,
    ) -> Result<(Self, Arc<dyn crate::wake::WakeSignal>), String> {
        Self::with_postgres_pool_and_wake_access(pool, channel, false).await
    }

    /// Pair an externally migrated process-owned dispatch pool with its
    /// PG-NOTIFY hint without executing DDL.
    #[cfg(feature = "durable")]
    pub async fn with_existing_postgres_pool_and_wake(
        pool: sqlx::PgPool,
        channel: &str,
    ) -> Result<(Self, Arc<dyn crate::wake::WakeSignal>), String> {
        Self::with_postgres_pool_and_wake_access(pool, channel, true).await
    }

    #[cfg(feature = "durable")]
    async fn with_postgres_pool_and_wake_access(
        pool: sqlx::PgPool,
        channel: &str,
        verify_only: bool,
    ) -> Result<(Self, Arc<dyn crate::wake::WakeSignal>), String> {
        let store = if verify_only {
            PostgresDispatchStore::with_existing_pool(pool).await
        } else {
            PostgresDispatchStore::with_pool(pool).await
        }
        .map_err(|error| error.to_string())?;
        let wake: Arc<dyn crate::wake::WakeSignal> =
            Arc::new(crate::wake::PgNotifyWake::new(store.wake_pool(), channel));
        Ok((Self::from_postgres_store(store), wake))
    }

    /// Connect the Postgres backend and, sharing its pool, a `PgNotifyWake` over
    /// `channel`: the served pool's wake fires `pg_notify` on the same database it
    /// enqueues into, so a peer node's `LISTEN` is nudged with no extra
    /// infrastructure (ADR-0019/0024). Returns the store plus the ready wake signal;
    /// keeps sqlx out of the host crate. Only Postgres carries a cross-node wake —
    /// SQLite is single-process, so it stays on the in-process `LocalWakeSignal`.
    #[cfg(feature = "durable")]
    pub async fn connect_postgres_with_wake(
        url: &str,
        channel: &str,
        max_connections: u32,
    ) -> Result<(Self, Arc<dyn crate::wake::WakeSignal>), String> {
        let store = PostgresDispatchStore::connect(url, max_connections)
            .await
            .map_err(|e| e.to_string())?;
        let wake: Arc<dyn crate::wake::WakeSignal> =
            Arc::new(crate::wake::PgNotifyWake::new(store.wake_pool(), channel));
        Ok((Self::from_postgres_store(store), wake))
    }

    /// Verify and connect an already-migrated Postgres queue with a shared
    /// `LISTEN`/`NOTIFY` wake adapter.
    #[cfg(feature = "durable")]
    pub async fn connect_postgres_existing_with_wake(
        url: &str,
        channel: &str,
        max_connections: u32,
    ) -> Result<(Self, Arc<dyn crate::wake::WakeSignal>), String> {
        let store = PostgresDispatchStore::connect_existing(url, max_connections)
            .await
            .map_err(|e| e.to_string())?;
        let wake: Arc<dyn crate::wake::WakeSignal> =
            Arc::new(crate::wake::PgNotifyWake::new(store.wake_pool(), channel));
        Ok((Self::from_postgres_store(store), wake))
    }

    /// Connect the Postgres durable backend but pair it with a
    /// [`NatsWakeSignal`](crate::wake::NatsWakeSignal)
    /// (feature `nats`) instead of `pg_notify` for the cross-node wake hint. The
    /// durable STORE stays Postgres (the queue authority); only the best-effort wake
    /// fan-out moves to a NATS `subject`, for a fleet that already runs a NATS broker
    /// (ADR-0019/0028). Returns the store plus the ready wake signal. Like every
    /// [`WakeSignal`](crate::wake::WakeSignal) the hint is non-authoritative — a lost NATS message only defers
    /// a drain to the poll fallback, so correctness never depends on it.
    #[cfg(feature = "nats")]
    pub async fn connect_postgres_with_nats_wake(
        db_url: &str,
        nats_url: &str,
        subject: &str,
        max_connections: u32,
    ) -> Result<(Self, Arc<dyn crate::wake::WakeSignal>), String> {
        let store = PostgresDispatchStore::connect(db_url, max_connections)
            .await
            .map_err(|e| e.to_string())?;
        let wake: Arc<dyn crate::wake::WakeSignal> = Arc::new(
            crate::wake::NatsWakeSignal::connect(nats_url, subject.to_string())
                .await
                .map_err(|e| e.to_string())?,
        );
        Ok((Self::from_postgres_store(store), wake))
    }

    /// Verify and connect an already-migrated Postgres queue with a NATS wake
    /// adapter. The NATS hint remains non-authoritative.
    #[cfg(feature = "nats")]
    pub async fn connect_postgres_existing_with_nats_wake(
        db_url: &str,
        nats_url: &str,
        subject: &str,
        max_connections: u32,
    ) -> Result<(Self, Arc<dyn crate::wake::WakeSignal>), String> {
        let store = PostgresDispatchStore::connect_existing(db_url, max_connections)
            .await
            .map_err(|e| e.to_string())?;
        let wake: Arc<dyn crate::wake::WakeSignal> = Arc::new(
            crate::wake::NatsWakeSignal::connect(nats_url, subject.to_string())
                .await
                .map_err(|e| e.to_string())?,
        );
        Ok((Self::from_postgres_store(store), wake))
    }

    #[cfg(feature = "durable")]
    fn from_store(store: impl Dispatch + DispatchOperationalFeed + 'static) -> Self {
        let store = Arc::new(store);
        Self {
            inner: store.clone(),
            operational: Some(store),
            stream_checkpoint: None,
        }
    }

    #[cfg(feature = "durable")]
    fn from_postgres_store(store: PostgresDispatchStore) -> Self {
        let stream_checkpoint: Arc<dyn StreamCheckpointStore> = Arc::new(store.checkpoint_store());
        let store = Arc::new(store);
        Self {
            inner: store.clone(),
            operational: Some(store),
            stream_checkpoint: Some(stream_checkpoint),
        }
    }

    /// Wrap an already-built [`Dispatch`] implementation behind this concrete type.
    /// The neutral seam for a composition root that assembles its own backend — e.g.
    /// a horizontal-scaling shard fan-out (`ShardedDispatchQueue`) that composes N
    /// per-shard stores. The composition injects this value into its host explicitly;
    /// the sharding/tenant policy stays in the closed caller.
    pub fn from_dispatch(inner: Arc<dyn Dispatch>) -> Self {
        Self {
            inner,
            operational: None,
            stream_checkpoint: None,
        }
    }

    /// Durable interrupted-stream storage paired with this dispatch authority.
    /// Present for Postgres; SQLite compositions use the host's durable FS adapter,
    /// and remote transports persist through their claim-bound HTTP operations.
    pub fn stream_checkpoint_store(&self) -> Option<Arc<dyn StreamCheckpointStore>> {
        self.stream_checkpoint.clone()
    }
}

/// Delegate one `&self` async method to the active backend through the trait
/// object (so the composed future stays `Send`, see the module doc).
macro_rules! delegate {
    ($self:ident, $method:ident ( $($arg:expr),* )) => {
        $self.inner.$method($($arg),*).await
    };
}

#[async_trait]
impl DispatchQueue for AnyDispatchStore {
    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        delegate!(self, reserve_session_run(request, reservation_ttl_ms))
    }

    async fn activate_session_run_reservation(
        &self,
        run_id: &RunId,
        session_thread_id: &ThreadId,
        session_activity_epoch: u64,
    ) -> Result<SessionRunReservationActivation, DispatchError> {
        delegate!(
            self,
            activate_session_run_reservation(run_id, session_thread_id, session_activity_epoch)
        )
    }

    async fn reject_session_run_reservation(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        delegate!(self, reject_session_run_reservation(run_id))
    }

    async fn resolve_claimed_session_run_reservation(
        &self,
        claim: &RunClaim,
        resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError> {
        delegate!(
            self,
            resolve_claimed_session_run_reservation(claim, resolution)
        )
    }

    async fn worker_owns_run(
        &self,
        identity: &crate::WorkerIdentity,
        run_id: &RunId,
        now_ms: u64,
    ) -> Result<Option<RunClaim>, DispatchError> {
        delegate!(self, worker_owns_run(identity, run_id, now_ms))
    }

    async fn claim_is_current(&self, claim: &RunClaim, now_ms: u64) -> Result<bool, DispatchError> {
        delegate!(self, claim_is_current(claim, now_ms))
    }

    async fn lock_session_run_reservation_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        delegate!(self, lock_session_run_reservation_epoch(claim))
    }

    async fn record_credential_realization(
        &self,
        claim: &RunClaim,
        receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, record_credential_realization(claim, receipt))
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        delegate!(self, lock_commit_epoch(claim))
    }

    async fn load_stream_checkpoint(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<StreamCheckpoint>, DispatchError> {
        delegate!(self, load_stream_checkpoint(claim))
    }

    async fn put_stream_checkpoint(
        &self,
        claim: &RunClaim,
        checkpoint: StreamCheckpoint,
    ) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, put_stream_checkpoint(claim, checkpoint))
    }

    async fn delete_stream_checkpoint(
        &self,
        claim: &RunClaim,
    ) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, delete_stream_checkpoint(claim))
    }

    async fn load_recovery_snapshot(
        &self,
        claim: &RunClaim,
    ) -> Result<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot, DispatchError>
    {
        delegate!(self, load_recovery_snapshot(claim))
    }

    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        delegate!(self, enqueue_with(request, options))
    }

    async fn enqueue_session_child(
        &self,
        request: RunDispatch,
        admission: SessionChildAdmission,
    ) -> Result<(), DispatchError> {
        delegate!(self, enqueue_session_child(request, admission))
    }

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(
            self,
            claim_new_run(request, owner, lease_ms, now_ms, capabilities)
        )
    }

    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(
            self,
            claim_new_run_compatible(request, worker, lease_ms, now_ms)
        )
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(
            self,
            deliver_and_claim(input, owner, lease_ms, now_ms, capabilities)
        )
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(
            self,
            deliver_and_claim_compatible(input, worker, lease_ms, now_ms)
        )
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim(owner, lease_ms, now_ms, capabilities))
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim_compatible(worker, lease_ms, now_ms))
    }

    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(
            self,
            claim_placed(requester, workers, policy, lease_ms, now_ms)
        )
    }

    async fn claim_run(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(
            self,
            claim_run(run_id, owner, lease_ms, now_ms, capabilities)
        )
    }

    async fn claim_for_terminal_recovery(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(
            self,
            claim_for_terminal_recovery(run_id, owner, lease_ms, now_ms)
        )
    }

    async fn claim_retry_exhausted(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(
            self,
            claim_retry_exhausted(owner, lease_ms, now_ms, max_attempts)
        )
    }

    async fn claim_run_compatible(
        &self,
        run_id: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim_run_compatible(run_id, worker, lease_ms, now_ms))
    }

    async fn renew_lease(
        &self,
        claim: &RunClaim,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        delegate!(self, renew_lease(claim, lease_ms, now_ms))
    }

    async fn renew_owned_leases(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        delegate!(self, renew_owned_leases(owner, lease_ms, now_ms))
    }

    async fn relinquish_claim(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, relinquish_claim(claim))
    }

    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, settle(run_id, epoch, outcome, consumed))
    }

    async fn completion_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<DispatchCompletion>, DispatchError> {
        delegate!(self, completion_events_after(after_sequence, limit))
    }

    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        delegate!(self, quarantine_retry_exhausted(max_attempts, now_ms))
    }

    async fn bind_sandbox(
        &self,
        claim: &RunClaim,
        sandbox_ref: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        delegate!(self, bind_sandbox(claim, sandbox_ref))
    }

    async fn runnable_depth(&self, now_ms: u64) -> Result<Option<u64>, DispatchError> {
        delegate!(self, runnable_depth(now_ms))
    }

    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        delegate!(self, dead_letters())
    }

    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        delegate!(self, requeue(run_id))
    }

    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        delegate!(self, cancel(run_id))
    }

    async fn awaiting_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
        delegate!(self, awaiting_run(thread_id))
    }

    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        delegate!(self, purge_dead_letters())
    }

    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError> {
        delegate!(self, purge_dead_letters_before(cutoff_ms))
    }

    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
        delegate!(self, superseded())
    }

    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
        delegate!(self, list_dispatches())
    }
}

#[async_trait]
impl DispatchOperationalFeed for AnyDispatchStore {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
    ) -> Result<DispatchPage, DispatchError> {
        let Some(feed) = &self.operational else {
            return Err(DispatchError::Rejected(
                "configured dispatch adapter does not expose an operational feed".to_string(),
            ));
        };
        feed.events_after(cursor, limit).await
    }
}

#[async_trait]
impl Inbox for AnyDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        delegate!(self, append(input))
    }

    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        delegate!(self, list(thread_id))
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        delegate!(self, retract(message_id, expected_revision))
    }

    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        delegate!(self, edit(message_id, expected_revision, result))
    }
}

#[async_trait]
impl Outbox for AnyDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        delegate!(self, stage(input))
    }

    async fn stage_session_resume(
        &self,
        input: PendingInput,
        session_thread_id: &ThreadId,
        prior_session_activity_epoch: Option<u64>,
        session_activity_epoch: u64,
    ) -> Result<bool, DispatchError> {
        delegate!(
            self,
            stage_session_resume(
                input,
                session_thread_id,
                prior_session_activity_epoch,
                session_activity_epoch
            )
        )
    }

    async fn relay(&self) -> Result<usize, DispatchError> {
        delegate!(self, relay())
    }

    async fn relay_and_enqueue(
        &self,
        input: PendingInput,
        request: RunDispatch,
        admission: ContinuationAdmission,
    ) -> Result<(), DispatchError> {
        delegate!(self, relay_and_enqueue(input, request, admission))
    }
}
