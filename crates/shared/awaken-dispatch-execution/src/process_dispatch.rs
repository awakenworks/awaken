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
//!
//! The pool's existing admission owner is constructed with this process, before
//! either service. Its absorbing drain fences a cold or still-constructing
//! service without waiting for its synchronous resolver factory.

use std::sync::{Arc, RwLockReadGuard};

use once_cell::sync::OnceCell;

use awaken_dispatch_erasure::{AnyCoordinatorDispatch, AnyDispatchStore, DispatchBinding};
use awaken_sync::LeafRwLock;
use awaken_time::WallClock;

use crate::pool::PoolAdmission;
use crate::session_reconciliation::TerminalReconciliationState;
use crate::{
    AdmissionError, DEFAULT_LEASE_MS, DispatchHint, DispatchPool, DispatchReconciler, Error,
    PoolDrainFence, WorkerResolver,
};
use awaken_deployment_config::{DispatchPoolSizing, ExecutionTopology};
use awaken_dispatch_contract::CompletionSink;

/// The dispatch pool, terminal reconciliation and wake hint of one process.
/// Consumers: the Session Run component (submission, cancellation and resume
/// publication), the executor's claimed-Run resolver construction, and the
/// process composition (startup, drain, readiness and admission control).
pub struct ProcessDispatch {
    topology: ExecutionTopology,
    clock: Arc<dyn WallClock>,
    hint: DispatchHint,
    publication: crate::DispatchPublication,
    admission: Arc<PoolAdmission>,
    pool: OnceCell<Arc<DispatchPool<AnyDispatchStore>>>,
    /// Configuration readers publish capabilities before either service may
    /// choose immutable construction ports. Service initialization owns the
    /// writer through OnceCell publication, including failure and unwind.
    service_configuration: LeafRwLock<()>,
    /// The scan state the claimed-Run resolver shares.
    terminal_reconciliation: Arc<TerminalReconciliationState>,
    /// The coordinator-side maintenance loop (rules T1-T4 above).
    terminal_reconciler: OnceCell<DispatchReconciler>,
    preparation: Arc<crate::PreparationPlacement>,
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
/// without linking the executor. Implementer: Run delivery's shared Session
/// dispatch lifecycle, which hands [`ProcessDispatch`] its selected resolver.
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

/// The process's owned dispatch tasks returned after its absorbing drain fence
/// was installed. Durable claim/attempt outcomes remain with their own owners.
#[must_use]
pub struct ClaimedRunsDrained {
    _fence: Option<PoolDrainFence>,
}

impl ProcessDispatch {
    /// The selected topology with no pool yet: [`Self::ensure_pool`] spawns it.
    #[must_use]
    pub fn new(topology: ExecutionTopology, clock: Arc<dyn WallClock>, hint: DispatchHint) -> Self {
        let admission = Arc::new(PoolAdmission::new(
            crate::DispatchServiceConfig::for_topology(&topology).max_attempts,
        ));
        let publication =
            crate::DispatchPublication::selected(topology.local.runs_local_pool(), hint.clone());
        Self {
            topology,
            clock,
            hint,
            publication,
            admission,
            pool: OnceCell::new(),
            service_configuration: LeafRwLock::new(()),
            terminal_reconciliation: Arc::default(),
            terminal_reconciler: OnceCell::new(),
            preparation: Arc::new(crate::PreparationPlacement),
        }
    }

    /// The execution topology the composition root selected.
    #[must_use]
    pub fn topology(&self) -> &ExecutionTopology {
        &self.topology
    }

    /// The same pure preparation compiler supplied to application admission
    /// and Worker transport. Retaining this value never retains the services.
    ///
    /// ```compile_fail,E0277
    /// use std::sync::Arc;
    /// use awaken_dispatch_execution::ProcessDispatch;
    /// use awaken_session_execution_contract::SessionPreparationPlacement;
    /// fn service_is_not_a_compiler(process: Arc<ProcessDispatch>) -> Arc<dyn SessionPreparationPlacement> {
    ///     process
    /// }
    /// ```
    #[must_use]
    pub fn preparation_placement(&self) -> Arc<crate::PreparationPlacement> {
        self.preparation.clone()
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

    /// Whether a service is selecting, or has selected, its immutable ports.
    /// Observation never waits for a resolver factory. A failed initializer
    /// releases its selection; a stopped published service still counts.
    #[must_use]
    pub fn has_started_services(&self) -> bool {
        let Some(_configuration) = self.read_service_configuration() else {
            return true;
        };
        self.service_ports_selected()
    }

    /// Atomically publish configuration before service port selection begins.
    /// Concurrent configuration publishers may arbitrate their own immutable
    /// value. An in-flight or published service refuses the callback entirely.
    /// Only publish capabilities here; never start a service in this callback.
    #[must_use = "None means service construction already selected its ports"]
    pub fn configure_before_services_start<T>(&self, publish: impl FnOnce() -> T) -> Option<T> {
        let _configuration = self.read_service_configuration()?;
        if self.service_ports_selected() {
            None
        } else {
            Some(publish())
        }
    }

    fn read_service_configuration(&self) -> Option<RwLockReadGuard<'_, ()>> {
        self.service_configuration.try_read()
    }

    fn service_ports_selected(&self) -> bool {
        self.pool.get().is_some() || self.terminal_reconciler.get().is_some()
    }

    /// Spawn the one process-level pool, once, when this process executes Runs
    /// and its dispatch authority is bound. `resolver` is built only when the
    /// pool is actually spawned. Concurrent callers share one fallible
    /// initializer; a failed factory leaves the slot empty for a later retry.
    /// Idempotent.
    pub fn ensure_pool(
        &self,
        binding: Option<DispatchBinding>,
        completion: Arc<dyn CompletionSink>,
        resolver: impl FnOnce() -> Result<Arc<dyn WorkerResolver<AnyDispatchStore>>, Error>,
    ) -> Result<(), Error> {
        let sizing = self.topology.local.pool_sizing();
        let PoolStart::Spawn(sizing) =
            pool_start(sizing, self.pool.get().is_some(), binding.is_some())
        else {
            return Ok(());
        };
        let binding = binding.expect("a spawn decision requires the bound authority");
        let _construction = self.service_configuration.write();
        self.pool
            .get_or_try_init(|| {
                self.admission.require_startable()?;
                let resolver = resolver()?;
                self.admission.require_startable()?;
                let pool = DispatchPool::spawn_bound_admitted(
                    binding,
                    self.clock.clone(),
                    self.topology.lease_owner.as_str().to_owned(),
                    DEFAULT_LEASE_MS,
                    crate::DispatchServiceConfig::for_topology(&self.topology),
                    resolver,
                    sizing.slots().get(),
                    self.hint.wake(),
                    Some(completion),
                    self.admission.clone(),
                )?;
                Ok(Arc::new(pool))
            })
            .map(|_| ())
    }

    /// Start the coordinator-side terminal reconciliation loop when this
    /// process owns durable dispatch and committed truth but runs no pool.
    /// Returns whether this call started it. Only the winning initializer
    /// constructs a resolver; an absent owner or resolver leaves it retryable.
    pub fn ensure_terminal_reconciliation(
        &self,
        scans_committed_terminals: bool,
        owner: Option<Arc<AnyCoordinatorDispatch>>,
        completion: Arc<dyn CompletionSink>,
        resolver: impl FnOnce() -> Option<Arc<dyn WorkerResolver<AnyDispatchStore>>>,
    ) -> bool {
        if !starts_terminal_reconciliation(
            self.topology.foreground_delivery.is_durable(),
            self.topology.local.runs_local_pool(),
            scans_committed_terminals,
            self.terminal_reconciler.get().is_some(),
        ) {
            return false;
        }
        let _construction = self.service_configuration.write();
        let mut started = false;
        self.terminal_reconciler
            .get_or_try_init(|| {
                self.admission.require_startable().map_err(|_| ())?;
                let owner = owner.ok_or(())?;
                let resolver = resolver().ok_or(())?;
                self.admission.require_startable().map_err(|_| ())?;
                let reconciler = DispatchReconciler::spawn_with_shutdown(
                    owner,
                    self.clock.clone(),
                    self.hint.wake(),
                    crate::DispatchServiceConfig::for_topology(&self.topology),
                    resolver,
                    Some(completion),
                    self.admission.shutdown(),
                );
                started = true;
                Ok::<_, ()>(reconciler)
            })
            .is_ok()
            && started
    }

    /// Bind the exact Worker directory and terminal-reconciliation source the
    /// maintenance loop retains Worker evidence with.
    pub fn bind_worker_evidence(
        &self,
        directory: Arc<dyn awaken_worker_contract::WorkerDirectory>,
        source: Arc<dyn crate::WorkerTerminalReconciliation>,
    ) -> Result<(), AdmissionError> {
        self.terminal_reconciliation
            .bind_worker_evidence(directory, source)
    }

    /// The immutable publication capability over the same original hint. Its
    /// callers never retain this process, its service handles or resolver.
    #[must_use]
    pub fn publication(&self) -> crate::DispatchPublication {
        self.publication.clone()
    }

    /// Begin a graceful drain (scale-in / SIGTERM): stop claiming while the
    /// in-flight Runs finish, and stop the coordinator-side terminal
    /// reconciliation loop, which claims maintenance work too (rule T3).
    /// The local topology issues its fence even before its pool is constructed;
    /// a topology without a local pool returns `None`. A resolver still being
    /// constructed does not delay this fence. Idempotent.
    pub async fn begin_drain(&self) -> Option<PoolDrainFence> {
        let fence = self.admission.begin_drain().await;
        self.topology.local.runs_local_pool().then_some(fence)
    }

    /// Install the drain fence and await the original dispatch tasks, including
    /// claim resolution and terminal observation, before `deadline`. Physical
    /// execution capacity alone deliberately excludes those control tasks.
    /// `None` when the deadline passed first; no task is aborted on timeout.
    pub async fn drain_claimed_runs(
        &self,
        deadline: &awaken_time::mono::MonoDeadline,
    ) -> Option<ClaimedRunsDrained> {
        let fence = drain_fence_before(deadline.instant(), self.begin_drain()).await?;
        while !drain_complete(
            self.in_flight(),
            self.pool
                .get()
                .into_iter()
                .flat_map(|pool| pool.task_completions())
                .chain(
                    self.terminal_reconciler
                        .get()
                        .into_iter()
                        .flat_map(DispatchReconciler::task_completions),
                ),
        ) {
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
        self.admission.in_flight()
    }
}

/// Fold primitive completion observations from the existing owned task handles.
/// Derive completion here, rather than accepting an already-aggregated flag.
fn drain_complete(physical: u32, mut tasks: impl Iterator<Item = bool>) -> bool {
    physical == 0 && tasks.all(std::convert::identity)
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

#[cfg(kani)]
mod drain_completion_proofs {
    use super::drain_complete;

    /// @obligation worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
    /// @obligation worker_drain.grace_expiry_retains_cancellation_observer
    /// @kani default-features=false
    #[kani::proof]
    #[kani::unwind(8)]
    fn drain_completion_requires_owned_tasks_and_physical_quiescence() {
        let physical: u32 = kani::any();
        // Primitive task-return observations, before production's all-task
        // reduction; the selected prefix includes empty/cold ownership.
        let tasks: [bool; 6] = kani::any();
        let selected = usize::from(kani::any::<u8>() % 7);
        let complete = drain_complete(physical, tasks.into_iter().take(selected));
        if physical != 0 {
            assert!(!complete);
        }
        for (index, finished) in tasks.into_iter().enumerate() {
            if index < selected && !finished {
                assert!(!complete);
            }
        }
        assert!(drain_complete(0, [].into_iter()));
        assert!(drain_complete(0, [true; 6].into_iter().take(selected)));
        // Native held-resolution/scan cases witness the actual Tokio handle
        // observations. This proves the shared fold through six observations,
        // not scheduling, eventual task return or external physical termination.
    }
}
