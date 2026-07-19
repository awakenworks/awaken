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

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;

use crate::dispatch::{
    CasOutcome, Claimed, CommitEpochGuard, Dispatch, DispatchError, DispatchOutcome, DispatchQueue,
    DispatchSummary, Inbox, Outbox, PendingInput, PendingRecord, RunClaim, SettleOutcome,
    SubmitOptions,
};
use crate::postgres::PostgresDispatchStore;
use crate::sqlite::SqliteDispatchStore;
use awaken_run_ingress_contract::RunDispatch;

/// The active durable-dispatch backend behind one concrete type. Both a SQLite
/// and a Postgres store satisfy the full `Dispatch` bundle; this holds whichever
/// was selected and forwards every call to it.
pub struct AnyDispatchStore {
    inner: Arc<dyn Dispatch>,
}

impl AnyDispatchStore {
    /// Open the SQLite backend at `path` (per-thread file queue; survives restart).
    pub fn open_sqlite(path: &str) -> Result<Self, String> {
        SqliteDispatchStore::open(path)
            .map(Self::from_store)
            .map_err(|e| e.to_string())
    }

    /// Open an in-memory SQLite backend (ephemeral; tests and no-store-dir mode).
    pub fn open_sqlite_in_memory() -> Result<Self, String> {
        SqliteDispatchStore::open_in_memory()
            .map(Self::from_store)
            .map_err(|e| e.to_string())
    }

    /// Connect the shared Postgres backend at `url`: one queue for a multi-node
    /// fleet, `FOR UPDATE SKIP LOCKED` gives distinct-claim across processes.
    pub async fn connect_postgres(url: &str) -> Result<Self, String> {
        PostgresDispatchStore::connect(url)
            .await
            .map(Self::from_store)
            .map_err(|e| e.to_string())
    }

    /// Connect the Postgres backend and, sharing its pool, a [`PgNotifyWake`] over
    /// `channel`: the served pool's wake fires `pg_notify` on the same database it
    /// enqueues into, so a peer node's `LISTEN` is nudged with no extra
    /// infrastructure (ADR-0019/0024). Returns the store plus the ready wake signal;
    /// keeps sqlx out of the host crate. Only Postgres carries a cross-node wake —
    /// SQLite is single-process, so it stays on the in-process `LocalWakeSignal`.
    pub async fn connect_postgres_with_wake(
        url: &str,
        channel: &str,
    ) -> Result<(Self, Arc<dyn crate::wake::WakeSignal>), String> {
        let store = PostgresDispatchStore::connect(url)
            .await
            .map_err(|e| e.to_string())?;
        let wake: Arc<dyn crate::wake::WakeSignal> =
            Arc::new(crate::wake::PgNotifyWake::new(store.wake_pool(), channel));
        Ok((Self::from_store(store), wake))
    }

    /// Connect the Postgres durable backend but pair it with a [`NatsWakeSignal`]
    /// (feature `nats`) instead of `pg_notify` for the cross-node wake hint. The
    /// durable STORE stays Postgres (the queue authority); only the best-effort wake
    /// fan-out moves to a NATS `subject`, for a fleet that already runs a NATS broker
    /// (ADR-0019/0028). Returns the store plus the ready wake signal. Like every
    /// [`WakeSignal`] the hint is non-authoritative — a lost NATS message only defers
    /// a drain to the poll fallback, so correctness never depends on it.
    #[cfg(feature = "nats")]
    pub async fn connect_postgres_with_nats_wake(
        db_url: &str,
        nats_url: &str,
        subject: &str,
    ) -> Result<(Self, Arc<dyn crate::wake::WakeSignal>), String> {
        let store = PostgresDispatchStore::connect(db_url)
            .await
            .map_err(|e| e.to_string())?;
        let wake: Arc<dyn crate::wake::WakeSignal> = Arc::new(
            crate::wake::NatsWakeSignal::connect(nats_url, subject.to_string())
                .await
                .map_err(|e| e.to_string())?,
        );
        Ok((Self::from_store(store), wake))
    }

    fn from_store(store: impl Dispatch + 'static) -> Self {
        Self {
            inner: Arc::new(store),
        }
    }

    /// Wrap an already-built [`Dispatch`] implementation behind this concrete type.
    /// The neutral seam for a composition root that assembles its own backend — e.g.
    /// a horizontal-scaling shard fan-out (`ShardedDispatchQueue`) that composes N
    /// per-shard stores — and injects it via
    /// [`init_shared_dispatch_store`](crate) so the host drives it like any other
    /// queue. Open mechanism; the sharding/tenant policy stays in the closed caller.
    pub fn from_dispatch(inner: Arc<dyn Dispatch>) -> Self {
        Self { inner }
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
    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        delegate!(self, lock_commit_epoch(claim))
    }

    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        delegate!(self, enqueue_with(request, options))
    }

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim_new_run(request, owner, lease_ms, now_ms))
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, deliver_and_claim(input, owner, lease_ms, now_ms))
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim(owner, lease_ms, now_ms))
    }

    async fn claim_run(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        delegate!(self, claim_run(run_id, owner, lease_ms, now_ms))
    }

    async fn renew_lease(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        delegate!(self, renew_lease(run_id, owner, lease_ms, now_ms))
    }

    async fn renew_owned_leases(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        delegate!(self, renew_owned_leases(owner, lease_ms, now_ms))
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

    async fn reap(&self, max_attempts: u64, now_ms: u64) -> Result<usize, DispatchError> {
        delegate!(self, reap(max_attempts, now_ms))
    }

    async fn bind_sandbox(&self, run_id: &RunId, sandbox_ref: &str) -> Result<(), DispatchError> {
        delegate!(self, bind_sandbox(run_id, sandbox_ref))
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

    async fn relay(&self) -> Result<usize, DispatchError> {
        delegate!(self, relay())
    }
}
