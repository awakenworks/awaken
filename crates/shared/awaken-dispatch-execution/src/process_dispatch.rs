// @formal entrypoint worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal entrypoint worker_drain.grace_expiry_retains_cancellation_observer
//! [`ProcessDispatch`] — this process's part in dispatch.
//!
//! It owns the execution topology the composition root selected, the one
//! process-level [`DispatchPool`] that drains the queue here, the
//! coordinator-side terminal reconciliation loop, and the wake hint every
//! durable command publisher nudges. The Session executor hands it the claimed-
//! Run resolver when the pool or reconciler starts; nothing it holds refers to
//! the executor.
//!
//! The reconciliation loop is owned here, beside the scan state its resolver
//! shares, never inside that state: the loop's task holds the resolver and the
//! resolver holds the scan state, so a loop stored in the state would keep
//! itself, the resolver and every executor component the resolver reads alive
//! after the process dropped its dispatch.
//!
//! Lifetime of the terminal reconciliation loop:
//!
//! | Rule | Cause | Effect |
//! |---|---|---|
//! | T1 | started, this dispatch alive, not stopped | the loop runs and holds its resolver |
//! | T2 | this dispatch dropped (the executor that owns it dropped) | the loop is cancelled and aborted; its resolver and the scan state are freed |
//! | T3 | the process begins draining ([`ProcessDispatch::begin_drain`]) | the loop ends at its next await and frees its resolver; this dispatch stays usable |
//! | T4 | start after T3 | refused: a stopped loop is never restarted |

use std::sync::{Arc, OnceLock};

use awaken_dispatch_erasure::{AnyCoordinatorDispatch, AnyDispatchStore, DispatchBinding};
use awaken_time::WallClock;

use crate::execution_topology::{DispatchPoolSizing, ExecutionTopology};
use crate::session_reconciliation::TerminalReconciliationState;
use crate::{
    AdmissionError, CompletionSink, DEFAULT_LEASE_MS, DispatchHint, DispatchPool,
    DispatchReconciler, Error, PoolDrainFence, WorkerResolver,
};

/// The dispatch pool, terminal reconciliation and wake hint of one process.
/// Consumers: the Session Run component (submission, cancellation and resume
/// publication), the executor's claimed-Run resolver construction, and the
/// process composition (startup, drain, readiness and admission control).
pub struct ProcessDispatch {
    topology: ExecutionTopology,
    clock: Arc<dyn WallClock>,
    completion: Arc<dyn CompletionSink>,
    hint: DispatchHint,
    pool: OnceLock<Arc<DispatchPool<AnyDispatchStore>>>,
    /// The scan state the claimed-Run resolver shares.
    terminal_reconciliation: Arc<TerminalReconciliationState>,
    /// The coordinator-side maintenance loop (rules T1-T4 above).
    terminal_reconciler: OnceLock<DispatchReconciler>,
}

/// What starting the local pool does for one topology and pool state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolStart {
    /// The process admits Runs only; registered remote Workers drain them.
    RemoteWorkersOnly,
    /// The pool already runs; starting again keeps it.
    AlreadyRunning,
    /// No dispatch authority is bound yet; a later start after the
    /// Coordinator supplies it spawns the pool.
    AwaitingBinding,
    /// Spawn the pool with this sizing.
    Spawn(DispatchPoolSizing),
}

fn pool_start(sizing: Option<DispatchPoolSizing>, running: bool, bound: bool) -> PoolStart {
    match (sizing, running, bound) {
        (None, _, _) => PoolStart::RemoteWorkersOnly,
        (Some(_), true, _) => PoolStart::AlreadyRunning,
        (Some(_), false, false) => PoolStart::AwaitingBinding,
        (Some(sizing), false, true) => PoolStart::Spawn(sizing),
    }
}

/// Whether this process starts the coordinator-side terminal reconciliation
/// loop: only a durable-ingress process that owns committed truth, runs no
/// local pool (a pool performs the same maintenance) and has not started it.
fn starts_terminal_reconciliation(
    durable_ingress: bool,
    runs_local_pool: bool,
    scans_committed_terminals: bool,
    started: bool,
) -> bool {
    durable_ingress && !runs_local_pool && scans_committed_terminals && !started
}

/// Starting and draining this process's dispatch work over the executor that
/// resolves its claimed Runs. Consumer: the Coordinator component, which
/// starts the work once its composition validates and drains it at shutdown,
/// without linking the executor. Implementer: the Session executor's
/// Coordinator role set, which hands [`ProcessDispatch`] its resolver.
#[async_trait::async_trait]
pub trait ProcessDispatchLifecycle: Send + Sync {
    /// Spawn the co-located pool when the topology runs one, and the
    /// committed-terminal reconciliation when this process owns committed
    /// truth but runs no pool. Idempotent.
    fn start(&self) -> Result<(), Error>;

    /// Drain claimed Runs, then the auxiliary work they created, by
    /// `timeout`; whether everything settled in time.
    async fn drain(&self, timeout: std::time::Duration) -> bool;
}

/// Every claimed Run of the local pool settled while its absorbing drain fence
/// was held; the fence stays installed while this value lives.
#[must_use]
pub struct ClaimedRunsDrained {
    _fence: Option<PoolDrainFence>,
}

impl ProcessDispatch {
    /// The selected topology with no pool yet: [`Self::ensure_pool`] spawns it.
    #[must_use]
    pub fn new(
        topology: ExecutionTopology,
        clock: Arc<dyn WallClock>,
        completion: Arc<dyn CompletionSink>,
        hint: DispatchHint,
    ) -> Self {
        Self {
            topology,
            clock,
            completion,
            hint,
            pool: OnceLock::new(),
            terminal_reconciliation: Arc::default(),
            terminal_reconciler: OnceLock::new(),
        }
    }

    /// The execution topology the composition root selected.
    #[must_use]
    pub fn topology(&self) -> &ExecutionTopology {
        &self.topology
    }

    /// The terminal reconciliation state the claimed-Run resolver scans with.
    #[must_use]
    pub fn terminal_reconciliation(&self) -> &Arc<TerminalReconciliationState> {
        &self.terminal_reconciliation
    }

    /// The running local pool, when this process spawned one.
    #[must_use]
    pub fn pool(&self) -> Option<&Arc<DispatchPool<AnyDispatchStore>>> {
        self.pool.get()
    }

    /// Spawn the one process-level pool, once, when this process executes Runs
    /// and its dispatch authority is bound. `resolver` is built only when the
    /// pool is actually spawned. Idempotent.
    pub fn ensure_pool(
        &self,
        binding: Option<DispatchBinding>,
        resolver: impl FnOnce() -> Result<Arc<dyn WorkerResolver<AnyDispatchStore>>, Error>,
    ) -> Result<(), Error> {
        let sizing = self.topology.local.pool_sizing();
        let PoolStart::Spawn(sizing) =
            pool_start(sizing, self.pool.get().is_some(), binding.is_some())
        else {
            return Ok(());
        };
        let binding = binding.expect("a spawn decision requires the bound authority");
        let pool = DispatchPool::spawn_bound(
            binding,
            self.clock.clone(),
            self.topology.lease_owner.as_str().to_owned(),
            DEFAULT_LEASE_MS,
            self.topology.dispatch_service_config(),
            resolver()?,
            sizing.slots().get(),
            self.hint.wake(),
            Some(self.completion.clone()),
        )?;
        let _ = self.pool.set(Arc::new(pool));
        Ok(())
    }

    /// Start the coordinator-side terminal reconciliation loop when this
    /// process owns durable dispatch and committed truth but runs no pool.
    /// Returns whether this call started it.
    pub fn ensure_terminal_reconciliation(
        &self,
        scans_committed_terminals: bool,
        owner: Option<Arc<AnyCoordinatorDispatch>>,
        resolver: impl FnOnce() -> Option<Arc<dyn WorkerResolver<AnyDispatchStore>>>,
    ) -> bool {
        if !starts_terminal_reconciliation(
            self.topology.ingress.is_durable(),
            self.topology.local.runs_local_pool(),
            scans_committed_terminals,
            self.terminal_reconciler.get().is_some(),
        ) {
            return false;
        }
        let (Some(owner), Some(resolver)) = (owner, resolver()) else {
            return false;
        };
        let reconciler = DispatchReconciler::spawn(
            owner,
            self.clock.clone(),
            self.hint.wake(),
            self.topology.dispatch_service_config(),
            resolver,
            Some(self.completion.clone()),
        );
        self.terminal_reconciler.set(reconciler).is_ok()
    }

    /// Stop the coordinator-side terminal reconciliation loop, when it runs
    /// (rule T3). The process lifecycle ends the loop, not the last reference
    /// to this dispatch: a composed process keeps the dispatch shared until it
    /// exits, so the loop and the resolver it holds end when the process
    /// begins draining. A stopped loop is never restarted (T4). Idempotent.
    fn stop_terminal_reconciliation(&self) {
        if let Some(reconciler) = self.terminal_reconciler.get() {
            reconciler.stop();
        }
    }

    /// Bind the exact Worker directory and terminal-reconciliation source the
    /// maintenance loop retains Worker evidence with.
    pub fn bind_worker_evidence(
        &self,
        directory: Arc<dyn awaken_dispatch_contract::WorkerDirectory>,
        source: Arc<dyn crate::WorkerTerminalReconciliation>,
    ) -> Result<(), AdmissionError> {
        self.terminal_reconciliation
            .bind_worker_evidence(directory, source)
    }

    /// Hint the local pool after a durable dispatch change, when one runs.
    pub async fn hint_local_pool(&self) {
        if self.pool.get().is_some() {
            self.hint.publish().await;
        }
    }

    /// Make a staged durable change visible to its drainer: the local pool is
    /// hinted; without one the owner relays its outbox so a remote Worker sees it.
    pub async fn publish_staged(
        &self,
        owner: &AnyCoordinatorDispatch,
    ) -> Result<(), awaken_dispatch_contract::DispatchError> {
        if self.pool.get().is_some() {
            self.hint.publish().await;
            return Ok(());
        }
        awaken_dispatch_contract::Outbox::relay(owner).await?;
        Ok(())
    }

    /// The durable Run control over `owner`, hinting the local pool when one runs.
    #[must_use]
    pub fn durable_run_control(
        &self,
        owner: Arc<AnyCoordinatorDispatch>,
    ) -> crate::durable_control::DurableRunControl {
        crate::durable_control::DurableRunControl::new(
            owner,
            self.pool.get().map(|_| self.hint.clone()),
        )
    }

    /// Begin a graceful drain (scale-in / SIGTERM): stop claiming while the
    /// in-flight Runs finish, and stop the coordinator-side terminal
    /// reconciliation loop, which claims maintenance work too (rule T3).
    /// `None` when no pool runs. Idempotent.
    pub async fn begin_drain(&self) -> Option<PoolDrainFence> {
        self.stop_terminal_reconciliation();
        match self.pool.get() {
            Some(pool) => Some(pool.begin_drain().await),
            None => None,
        }
    }

    /// Install the drain fence and wait until every claimed Run settled, both
    /// before `deadline`. `None` when the deadline passed first.
    pub async fn drain_claimed_runs(
        &self,
        deadline: &awaken_time::mono::MonoDeadline,
    ) -> Option<ClaimedRunsDrained> {
        let fence = drain_fence_before(deadline.instant(), self.begin_drain()).await?;
        while self.in_flight() != 0 {
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return None;
            }
            tokio::time::sleep(remaining.min(std::time::Duration::from_millis(5))).await;
        }
        Some(ClaimedRunsDrained { _fence: fence })
    }

    /// Pause only new claim admission while retaining all in-flight Run work.
    pub async fn suspend_admission(&self) {
        if let Some(pool) = self.pool.get() {
            pool.suspend_admission().await;
        }
    }

    /// Resume a temporary authority suspension. False when no pool runs or it
    /// entered the absorbing graceful-drain state.
    #[must_use]
    pub async fn resume_admission(&self) -> bool {
        match self.pool.get() {
            Some(pool) => pool.resume_admission().await,
            None => false,
        }
    }

    /// Whether the pool is up and still claiming work — the Worker's readiness.
    #[must_use]
    pub async fn accepting_work(&self) -> bool {
        match self.pool.get() {
            Some(pool) => pool.is_accepting().await,
            None => false,
        }
    }

    /// Whether a successful registry heartbeat may make this Worker ready.
    #[must_use]
    pub async fn can_resume_work(&self) -> bool {
        match self.pool.get() {
            Some(pool) => pool.can_resume().await,
            None => false,
        }
    }

    /// Physically admitted executions of the local pool; zero without one.
    #[must_use]
    pub fn in_flight(&self) -> u32 {
        self.pool.get().map_or(0, |pool| pool.in_flight())
    }
}

impl awaken_session_realization_contract::SessionPreparationPlacement for ProcessDispatch {
    fn requirements(
        &self,
        session: &awaken_session_state_contract::PersistedSession,
    ) -> Result<
        awaken_dispatch_contract::PlacementRequirements,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        crate::placement::session_preparation_requirements(session)
    }
}

/// Retain the pool-drain fence future until the caller's absolute deadline.
/// `Some(None)` is the valid no-local-pool case; `None` alone means the fence
/// owner did not return before the deadline.
async fn drain_fence_before<F, T>(deadline: tokio::time::Instant, fence: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout_at(deadline, fence).await.ok()
}

#[cfg(test)]
#[path = "process_dispatch/tests.rs"]
mod tests;
