// @formal entrypoint remote_worker.maintenance_scheduler_respects_adapter_ownership
// @formal entrypoint run_ingress.pool_retry_gate_admits_one_probe_and_uses_bounded_monotonic_backoff
// @formal entrypoint run_ingress.request_local_resolution_retry_honors_persisted_deadline
// @formal entrypoint session.agent_message_missing_lifecycle_projection_relinquishes_without_settlement
// @formal entrypoint session_run.recovery_claim_is_admission_only
// @formal entrypoint worker_drain.closed_generation_is_absorbing
// @formal entrypoint worker_drain.no_claim_after_acknowledgement
// @formal entrypoint worker_drain.no_claim_attempt_after_registry_drain
// @formal entrypoint worker_drain.registry_transition_follows_local_fence
//! The process-level dispatch pool.
//!
//! [`DispatchService`](crate::DispatchService) binds one drain loop to one
//! runtime — fine for a single session, but a host with many sessions would spawn
//! one loop per session, so hundreds of thousands of sessions become hundreds of
//! thousands of pollers hammering the queue. [`DispatchPool`] instead runs a fixed
//! set of drain tasks for the *whole process* over one shared queue: each task
//! claims a runnable dispatch, asks a [`WorkerResolver`] for the worker that owns
//! that run's thread (its runtime carries the thread's model/tools/config), and
//! drives the run there via [`DispatchWorker::drive_claimed`]. Claim is decoupled
//! from drive precisely so a run always executes on its own session's runtime, not
//! on whichever task happened to claim it.
//!
//! Each claimed task owns one exact renewal guard, transferred across resolver to
//! Worker drive; one maintenance loop (GC aged manual quarantines, relay the
//! cross-thread outbox) runs per process rather than per Session. Correctness
//! rests on the same durable-claim invariants as the per-Session service: `claim`
//! is owner-scoped with `FOR UPDATE SKIP LOCKED`, so N tasks take distinct Runs; a
//! dropped wake only defers work to the poll fallback; a crash drops the guard and
//! leaves the lease to expire and be reclaimed.

use awaken_lease_contract::fence::SlotToken;
use awaken_dispatch_contract::ControlDispatch;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::metrics::MetricsRecorder;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use awaken_runtime_contract::activation::RunActivation;

use crate::Error;
use crate::dispatch::{Claimed, PendingInput};
use crate::execution_capacity::ExecutionCapacity;
use crate::service::{
    DispatchServiceConfig, LocalMaintenanceSchedule, spawn_dispatch_depth_sampler,
    wait_for_terminal_reconciliation,
};
use crate::wake::{LocalWakeSignal, WakeSignal, publish_dispatch_hint};
use crate::worker::{DispatchWorker, renew_claim_while_active};
use awaken_dispatch_contract::RunDispatch;
use awaken_time::WallClock;
use awaken_time::WallClockExt;

/// Resolves the worker that owns a thread's runtime. The pool claims from the one
/// shared queue, then asks the resolver for the session worker carrying the
/// claimed run's thread config and drives the run there.
///
/// The resolver MUST return workers that share the pool's store and claim owner,
/// so the Pool's claim, exact renewal guard, and drive agree on lease ownership
/// (the host wires every session worker with the process's `dispatch_owner()` and
/// the one shared store, which satisfies this).
#[async_trait]
pub trait WorkerResolver<S>: Send + Sync {
    /// Retry infrastructure retention through this same maintenance loop.
    /// It never drives a Run or creates a second scheduler.
    async fn reconcile_worker_evidence(&self) -> Result<(), Error> {
        Ok(())
    }
    /// Exact process-local realization evidence used before the pool freezes a
    /// local claim. Registered remote workers use their immutable manifest at
    /// the server and do not route through this local pool seam.
    fn credential_realization_capabilities(
        &self,
    ) -> awaken_credential_contract::CredentialRealizationCapabilities {
        Default::default()
    }

    /// The worker whose runtime owns `thread_id`, opening the session if needed.
    /// `agent_id` is the claimed run's own agent (from its activation snapshot), so a
    /// cold worker opens the session bound to THAT agent's published config — its own
    /// catalog, so the run resolves against a matching fingerprint — resolved from the
    /// worker's config service. `None`/empty opens the built-in default agent.
    async fn worker_for(
        &self,
        thread_id: &ThreadId,
        agent_id: Option<&str>,
    ) -> Result<Arc<DispatchWorker<S>>, Error>;

    /// Resolve from the complete durable claim. The default preserves existing
    /// resolvers while allowing recovery-aware adapters to consume opaque claim
    /// metadata such as the sandbox binding without teaching the pool its shape.
    async fn worker_for_claimed(&self, claimed: &Claimed) -> Result<Arc<DispatchWorker<S>>, Error> {
        let thread_id = claimed.request.session_thread_id();
        let agent_id = claimed.request.activation.snapshot.root_agent_id.0.as_str();
        self.worker_for(thread_id, (!agent_id.is_empty()).then_some(agent_id))
            .await
    }

    /// Convert a returned pre-execution resolution failure into committed Run
    /// truth while the exact claim is still owned. The default preserves retry
    /// semantics for resolvers that cannot author commits; recovery-aware
    /// resolvers override this and use the ordinary claim-fenced failure commit
    /// plus settlement path. A process crash never calls this method and remains
    /// governed by lease expiry/reclaim.
    async fn settle_claimed_resolution_failure(
        &self,
        _claimed: &Claimed,
        error: Error,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        Err(error)
    }

    /// Resolve an exact retry-exhaustion claim through the canonical Worker
    /// terminal path. The default reuses the ordinary claimed Worker; hosts that
    /// can construct an environment-free boundary Worker override only that
    /// resolution detail. No resolver may author terminal truth itself.
    async fn terminalize_retry_exhausted(
        &self,
        claimed: &Claimed,
    ) -> Result<Option<(RunId, RunState)>, Error>
    where
        S: ControlDispatch + 'static,
    {
        self.worker_for_claimed(claimed)
            .await?
            .terminalize_retry_exhausted(claimed)
            .await
    }

    /// Reconcile up to `limit` quiescent or expired dispatches against the
    /// resolver's authoritative committed Run readers. Generic resolvers have no
    /// global reader registry and do nothing; a Runtime Host overrides this once
    /// for its Session/commit ownership boundary.
    async fn reconcile_committed_terminals(
        &self,
        _limit: usize,
    ) -> Result<Vec<(RunId, RunState)>, Error> {
        Ok(Vec::new())
    }
}

/// Notified the instant the pool settles a run, so a foreground submitter can await
/// its own run's completion by **event** rather than polling committed truth —
/// removing the poll-interval latency floor from the durable foreground path.
pub trait CompletionSink: Send + Sync {
    /// The pool drove `run_id` to a settled `state` (`Ended` or `Awaiting`).
    fn settled(&self, run_id: &RunId, state: &RunState);
}

/// A running pool of drain tasks over one shared dispatch queue.
pub struct DispatchPool<S> {
    store: Arc<S>,
    wake: Arc<dyn WakeSignal>,
    shutdown: CancellationToken,
    drains: Vec<JoinHandle<()>>,
    wake_coordinator: JoinHandle<()>,
    reconciler: JoinHandle<()>,
    depth_sampler: Option<JoinHandle<()>>,
    admission: Arc<PoolAdmission>,
}

fn spawn_observed_depth<S: ControlDispatch + 'static>(
    store: Arc<S>,
    shutdown: CancellationToken,
    enabled: bool,
) -> Option<JoinHandle<()>> {
    let metrics = Arc::new(awaken_observability::OtelMetricsRecorder::new());
    let report = Arc::new(move |depth| metrics.record_dispatch_queue_depth(depth));
    spawn_dispatch_depth_sampler(store, report, shutdown, enabled)
}

/// Reconciles durable Dispatch state with already committed facts when a
/// deployment owns the queue but intentionally runs no local claim drainers.
///
/// Coordinator-only deployments use this handle instead of reproducing the
/// pool's relay/reconciliation scheduler in their Runtime Host. With no drainer,
/// retry-exhausted rows remain durable until a Worker returns and claims them.
/// It relays staged input, reconciles committed terminals and expires aged
/// manual quarantines. It never executes Runs or certifies physical termination;
/// all mutations still go through the existing Dispatch aggregate.
pub struct DispatchReconciler {
    shutdown: CancellationToken,
    task: Option<JoinHandle<()>>,
    depth_sampler: Option<JoinHandle<()>>,
}

impl DispatchReconciler {
    pub fn spawn<S: ControlDispatch + 'static>(
        store: Arc<S>,
        clock: Arc<dyn WallClock>,
        wake: Arc<dyn WakeSignal>,
        config: DispatchServiceConfig,
        resolver: Arc<dyn WorkerResolver<S>>,
        completion: Option<Arc<dyn CompletionSink>>,
    ) -> Self {
        let shutdown = CancellationToken::new();
        let depth_sampler = spawn_observed_depth(
            store.clone(),
            shutdown.clone(),
            config.sample_dispatch_queue_depth,
        );
        let task = tokio::spawn(reconciliation_loop(ReconciliationContext {
            store,
            clock,
            wake: wake.clone(),
            reconciliation_wake: wake,
            shutdown: shutdown.clone(),
            config,
            resolver,
            completion,
        }));
        Self {
            shutdown,
            task: Some(task),
            depth_sampler,
        }
    }

    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        if let Some(sampler) = self.depth_sampler.take() {
            let _ = sampler.await;
        }
    }
}

impl Drop for DispatchReconciler {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionState {
    Open,
    Suspended,
    Draining,
}

#[derive(Debug, Clone, Copy)]
enum AdmissionCommand {
    Suspend,
    Resume { shutdown_cancelled: bool },
    Drain,
}

/// Proof that this process's canonical claim-admission owner installed its
/// absorbing drain fence. The private field prevents callers from publishing a
/// remote drain transition without first passing through [`DispatchPool::begin_drain`].
#[derive(Debug)]
#[must_use]
pub struct PoolDrainFence {
    _private: (),
}

impl AdmissionState {
    /// Sole process-local claim-admission transition. Keeping temporary
    /// suspension and permanent drain in one closed table prevents any caller
    /// from inventing a resume edge out of `Draining`.
    const fn transition(self, command: AdmissionCommand) -> Self {
        match (self, command) {
            (Self::Draining, _) | (_, AdmissionCommand::Drain) => Self::Draining,
            (Self::Open, AdmissionCommand::Suspend) => Self::Suspended,
            (
                Self::Suspended,
                AdmissionCommand::Resume {
                    shutdown_cancelled: false,
                },
            ) => Self::Open,
            (Self::Open, AdmissionCommand::Resume { .. })
            | (Self::Suspended, AdmissionCommand::Suspend)
            | (
                Self::Suspended,
                AdmissionCommand::Resume {
                    shutdown_cancelled: true,
                },
            ) => self,
        }
    }

    const fn accepts_claim(self) -> bool {
        matches!(self, Self::Open)
    }

    const fn can_resume(self) -> bool {
        !matches!(self, Self::Draining)
    }

    /// Install the absorbing local fence and issue the only token that permits
    /// the Worker lifecycle to request the corresponding Registry transition.
    const fn begin_drain(self) -> (Self, PoolDrainFence) {
        (
            self.transition(AdmissionCommand::Drain),
            PoolDrainFence { _private: () },
        )
    }
}

struct PoolAdmission {
    gate: tokio::sync::RwLock<AdmissionState>,
    error_gate: DrainErrorGate,
    execution_capacity: ExecutionCapacity,
    wake: Notify,
    max_attempts: u64,
}

#[derive(Default)]
struct DrainErrorState {
    consecutive_errors: u32,
    retry_not_before: Option<tokio::time::Instant>,
    probe_in_flight: Option<SlotToken>,
}

impl DrainErrorState {
    /// Synchronous transition inside the pool-wide mutex. The caller owns clock
    /// comparison; this kernel owns the single recovery-probe authority.
    fn admit_cycle(&mut self, retry_deadline_reached: bool) -> bool {
        if self.consecutive_errors == 0 {
            return true;
        }
        if self.probe_in_flight.is_some() || !retry_deadline_reached {
            return false;
        }
        self.probe_in_flight = Some(SlotToken::mint());
        true
    }

    fn release_probe(&mut self, probe: &SlotToken) -> bool {
        if self
            .probe_in_flight
            .as_ref()
            .is_some_and(|current| current == probe)
        {
            self.probe_in_flight = None;
            true
        } else {
            false
        }
    }
}

#[derive(Default)]
struct DrainErrorGate {
    state: tokio::sync::Mutex<DrainErrorState>,
    changed: Notify,
}

enum DrainCycle {
    Healthy,
    Probe(SlotToken),
}

impl DrainErrorGate {
    /// During an outage, admit exactly one recovery probe for the whole pool.
    /// Pool width must remain execution concurrency, never retry concurrency.
    async fn wait_cycle(&self, shutdown: &CancellationToken) -> Option<DrainCycle> {
        self.wait_cycle_observed(shutdown, || {}).await
    }

    // The observer is synchronous and receives no authority. Tests use it to
    // interleave a completed probe exactly between observation and suspension.
    async fn wait_cycle_observed(
        &self,
        shutdown: &CancellationToken,
        mut after_observation: impl FnMut(),
    ) -> Option<DrainCycle> {
        loop {
            // notify_waiters does not retain a permit for a future waiter.
            // Register before reading state so a probe that returns after the
            // read cannot strand every drain behind an already-open gate.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let delay = {
                let mut state = self.state.lock().await;
                if state.consecutive_errors == 0 {
                    debug_assert!(state.admit_cycle(false));
                    return Some(DrainCycle::Healthy);
                }
                let retry_not_before = state
                    .retry_not_before
                    .expect("an error state owns a retry deadline");
                let now = awaken_time::mono::now();
                if state.admit_cycle(now >= retry_not_before) {
                    return Some(DrainCycle::Probe(
                        state
                            .probe_in_flight
                            .clone()
                            .expect("admitted recovery probe"),
                    ));
                }
                state
                    .probe_in_flight
                    .is_none()
                    .then(|| retry_not_before.duration_since(now))
            };
            after_observation();
            match delay {
                Some(delay) => tokio::select! {
                    _ = shutdown.cancelled() => return None,
                    _ = &mut changed => {},
                    _ = tokio::time::sleep(delay) => {},
                },
                None => tokio::select! {
                    _ = shutdown.cancelled() => return None,
                    _ = &mut changed => {},
                },
            }
        }
    }

    /// Queue recovery probes end before long-running execution. Keep the error
    /// count/deadline until settlement so repeated drive failures still back off.
    /// Arc identity cannot be reused while a delayed cycle retains its token.
    async fn release_probe(&self, cycle: &DrainCycle) {
        let DrainCycle::Probe(probe) = cycle else {
            return;
        };
        let mut state = self.state.lock().await;
        if state.release_probe(probe) {
            drop(state);
            self.changed.notify_waiters();
        }
    }

    async fn record_success(&self) {
        let mut state = self.state.lock().await;
        if state.consecutive_errors == 0 && state.probe_in_flight.is_none() {
            return;
        }
        *state = DrainErrorState::default();
        drop(state);
        self.changed.notify_waiters();
    }

    async fn record_failure(&self) -> (u32, Duration) {
        let mut state = self.state.lock().await;
        state.consecutive_errors = state.consecutive_errors.saturating_add(1);
        let delay = drain_error_backoff(state.consecutive_errors);
        state.retry_not_before = Some(awaken_time::mono::now() + delay);
        state.probe_in_flight = None;
        let consecutive_errors = state.consecutive_errors;
        drop(state);
        self.changed.notify_waiters();
        (consecutive_errors, delay)
    }
}

impl Default for PoolAdmission {
    fn default() -> Self {
        Self::new(DispatchServiceConfig::default().max_attempts)
    }
}

impl PoolAdmission {
    fn new(max_attempts: u64) -> Self {
        Self {
            gate: tokio::sync::RwLock::new(AdmissionState::Open),
            error_gate: DrainErrorGate::default(),
            execution_capacity: ExecutionCapacity::default(),
            wake: Notify::new(),
            max_attempts,
        }
    }
}

impl<S: ControlDispatch + 'static> DispatchPool<S> {
    /// Spawn the pool with the single-process wake signal. `concurrency` is the
    /// number of runs driven in parallel (clamped to at least 1).
    pub fn spawn(
        store: Arc<S>,
        clock: Arc<dyn WallClock>,
        owner: impl Into<String>,
        lease_ms: u64,
        config: DispatchServiceConfig,
        resolver: Arc<dyn WorkerResolver<S>>,
        concurrency: usize,
    ) -> Self {
        Self::spawn_with_wake(
            store,
            clock,
            owner,
            lease_ms,
            config,
            resolver,
            concurrency,
            Arc::new(LocalWakeSignal::new()),
        )
    }

    /// Spawn the pool with a chosen [`WakeSignal`] — a `LocalWakeSignal` for one
    /// process, or a cross-node signal so a fleet need not busy-poll.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_wake(
        store: Arc<S>,
        clock: Arc<dyn WallClock>,
        owner: impl Into<String>,
        lease_ms: u64,
        config: DispatchServiceConfig,
        resolver: Arc<dyn WorkerResolver<S>>,
        concurrency: usize,
        wake: Arc<dyn WakeSignal>,
    ) -> Self {
        Self::spawn_inner(
            store,
            clock,
            owner,
            lease_ms,
            config,
            resolver,
            concurrency,
            wake,
            None,
        )
    }

    /// Spawn the pool with BOTH a chosen [`WakeSignal`] and a [`CompletionSink`] —
    /// the served durable path on Postgres wants both: a cross-node wake (so a peer's
    /// enqueue nudges this pool without busy-poll) and event-driven completion (so a
    /// foreground submitter waits by event). SQLite stays on `spawn_with_completion`
    /// (a `LocalWakeSignal` suffices in one process).
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_wake_and_completion(
        store: Arc<S>,
        clock: Arc<dyn WallClock>,
        owner: impl Into<String>,
        lease_ms: u64,
        config: DispatchServiceConfig,
        resolver: Arc<dyn WorkerResolver<S>>,
        concurrency: usize,
        wake: Arc<dyn WakeSignal>,
        completion: Arc<dyn CompletionSink>,
    ) -> Self {
        Self::spawn_inner(
            store,
            clock,
            owner,
            lease_ms,
            config,
            resolver,
            concurrency,
            wake,
            Some(completion),
        )
    }

    /// Spawn the pool with a [`CompletionSink`] notified the instant each run
    /// settles, so a foreground submitter waits for its run by event, not by poll.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_completion(
        store: Arc<S>,
        clock: Arc<dyn WallClock>,
        owner: impl Into<String>,
        lease_ms: u64,
        config: DispatchServiceConfig,
        resolver: Arc<dyn WorkerResolver<S>>,
        concurrency: usize,
        completion: Arc<dyn CompletionSink>,
    ) -> Self {
        Self::spawn_inner(
            store,
            clock,
            owner,
            lease_ms,
            config,
            resolver,
            concurrency,
            Arc::new(LocalWakeSignal::new()),
            Some(completion),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_inner(
        store: Arc<S>,
        clock: Arc<dyn WallClock>,
        owner: impl Into<String>,
        lease_ms: u64,
        config: DispatchServiceConfig,
        resolver: Arc<dyn WorkerResolver<S>>,
        concurrency: usize,
        wake: Arc<dyn WakeSignal>,
        completion: Option<Arc<dyn CompletionSink>>,
    ) -> Self {
        let owner = owner.into();
        let shutdown = CancellationToken::new();
        let depth_sampler = spawn_observed_depth(
            store.clone(),
            shutdown.clone(),
            config.sample_dispatch_queue_depth,
        );
        let admission = Arc::new(PoolAdmission::new(config.max_attempts));
        let reconciliation_wake = Arc::new(LocalWakeSignal::new());
        let drains = (0..concurrency.max(1))
            .map(|_| {
                tokio::spawn(drain_loop(
                    store.clone(),
                    clock.clone(),
                    shutdown.clone(),
                    owner.clone(),
                    lease_ms,
                    resolver.clone(),
                    completion.clone(),
                    admission.clone(),
                ))
            })
            .collect();
        let wake_coordinator = tokio::spawn(wake_coordinator_loop(
            wake.clone(),
            admission.clone(),
            reconciliation_wake.clone(),
            shutdown.clone(),
            config.poll_interval,
        ));
        let reconciler = tokio::spawn(reconciliation_loop(ReconciliationContext {
            store: store.clone(),
            clock: clock.clone(),
            wake: wake.clone(),
            reconciliation_wake,
            shutdown: shutdown.clone(),
            config,
            resolver: resolver.clone(),
            completion: completion.clone(),
        }));
        Self {
            store,
            wake,
            shutdown,
            drains,
            wake_coordinator,
            reconciler,
            depth_sampler,
            admission,
        }
    }

    /// Durably enqueue a run and nudge the pool to pick it up.
    pub async fn submit(&self, activation: RunActivation) -> Result<(), Error> {
        self.submit_dispatch(RunDispatch::new(activation)).await
    }

    /// Enqueue an already-resolved durable execution envelope. Composition roots
    /// use this when model access and placement were pinned at admission.
    pub async fn submit_dispatch(&self, mut request: RunDispatch) -> Result<(), Error> {
        if request.traceparent.is_none() {
            request.traceparent = awaken_observability::current_traceparent();
        }
        self.store.enqueue(request).await?;
        publish_dispatch_hint(self.wake.as_ref(), &self.shutdown).await;
        Ok(())
    }

    /// Durably deliver pending input and nudge the pool to resume the run.
    pub async fn deliver(&self, input: PendingInput) -> Result<(), Error> {
        self.store.append(input).await?;
        publish_dispatch_hint(self.wake.as_ref(), &self.shutdown).await;
        Ok(())
    }

    /// Stage a cross-thread delivery and nudge the pool to relay it.
    pub async fn send(&self, input: PendingInput) -> Result<(), Error> {
        self.store.stage(input).await?;
        publish_dispatch_hint(self.wake.as_ref(), &self.shutdown).await;
        Ok(())
    }

    /// Wake the pool to drain immediately.
    pub async fn notify(&self) {
        publish_dispatch_hint(self.wake.as_ref(), &self.shutdown).await;
    }

    /// Persist cancellation and wake the pool's one claim/resolver path.
    ///
    /// Acceptance ends at the durable dispatch-row boundary. A drainer owns the
    /// resulting claim, Runtime cancellation commit, and settlement; keeping
    /// those effects out of the caller prevents an HTTP/control request from
    /// becoming a second synchronous dispatch driver. The retained intent remains
    /// authoritative across process replacement.
    pub async fn cancel(&self, run_id: &RunId) -> Result<bool, Error> {
        if self.store.cancel(run_id).await?.is_none() {
            return Ok(false);
        }
        // The signal is only a latency hint. A dropped signal is covered by the
        // pool's poll fallback and the durable cancellation bit.
        publish_dispatch_hint(self.wake.as_ref(), &self.shutdown).await;
        Ok(true)
    }

    /// Temporarily stop new claims while retaining in-flight execution and every
    /// durable Run lease. This is the reversible fail-closed state used while the
    /// Worker cannot refresh its registry authority inside the local proof window.
    pub async fn suspend_admission(&self) {
        let mut admission = self.admission.gate.write().await;
        *admission = admission.transition(AdmissionCommand::Suspend);
    }

    /// Reopen claims after registry authority is proven again. A graceful drain
    /// is absorbing: no heartbeat can resurrect admission after shutdown begins.
    #[must_use]
    pub async fn resume_admission(&self) -> bool {
        let mut admission = self.admission.gate.write().await;
        let previous = *admission;
        *admission = admission.transition(AdmissionCommand::Resume {
            shutdown_cancelled: self.shutdown.is_cancelled(),
        });
        let accepting = admission.accepts_claim();
        if previous != *admission && accepting {
            drop(admission);
            publish_dispatch_hint(self.wake.as_ref(), &self.shutdown).await;
        }
        accepting
    }

    /// Whether the pool currently accepts new claims.
    #[must_use]
    pub async fn is_accepting(&self) -> bool {
        self.admission.gate.read().await.accepts_claim()
    }

    /// Whether a fresh authority proof may reopen this pool. This remains true
    /// while temporarily suspended and false once graceful drain is absorbing.
    #[must_use]
    pub async fn can_resume(&self) -> bool {
        self.admission.gate.read().await.can_resume()
    }

    /// Begin a graceful drain WITHOUT consuming the pool: cancel the drain tasks so
    /// each finishes the run it is currently driving and then stops claiming, and
    /// nudge them so they notice immediately rather than at the next poll. The
    /// scale-in half of cloud-native lifecycle — a `preStop`/SIGTERM asks the worker
    /// to stop taking new work while its in-flight runs complete. Idempotent: a
    /// second call is a no-op (the token is already cancelled). Unlike
    /// [`shutdown`](Self::shutdown) it does not await the tasks (the caller owns the
    /// grace period), and it takes `&self` so a live handle (e.g. behind an `Arc` in
    /// the host) can trigger it.
    pub async fn begin_drain(&self) -> PoolDrainFence {
        // The write lock linearizes drain against the short read-side claim
        // critical section. Once this returns, every claim either completed
        // before the state became absorbing (and is now in-flight), or saw a
        // non-open state and did not touch the store.
        let mut admission = self.admission.gate.write().await;
        let (draining, fence) = admission.begin_drain();
        *admission = draining;
        self.shutdown.cancel();
        drop(admission);
        fence
    }

    /// Whether a drain has been requested (the drain tasks are stopping/stopped).
    pub fn is_draining(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// Actual number of execution slots installed in this process. This observes
    /// the pool itself; the registry remains the placement-capacity authority.
    #[must_use]
    pub fn concurrency(&self) -> usize {
        self.drains.len()
    }

    /// Exact process-local number of physically admitted executions. The worker
    /// heartbeat reports this value; capacity enforcement remains server-side.
    #[must_use]
    pub fn in_flight(&self) -> u32 {
        self.admission.execution_capacity.in_flight()
    }

    /// Stop every task and wait for the in-flight drains to finish.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        for drain in self.drains {
            let _ = drain.await;
        }
        let _ = self.wake_coordinator.await;
        let _ = self.reconciler.await;
        if let Some(sampler) = self.depth_sampler {
            let _ = sampler.await;
        }
    }
}

/// One drain task: claim a runnable dispatch, route it to its owning session's
/// worker, drive it, repeat until idle, then await on a wake or the poll timer.
#[allow(clippy::too_many_arguments)]
async fn drain_loop<S: ControlDispatch + 'static>(
    store: Arc<S>,
    clock: Arc<dyn WallClock>,
    shutdown: CancellationToken,
    owner: String,
    lease_ms: u64,
    resolver: Arc<dyn WorkerResolver<S>>,
    completion: Option<Arc<dyn CompletionSink>>,
    admission: Arc<PoolAdmission>,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = admission.wake.notified() => {}
        }
        let Some(cycle) = admission.error_gate.wait_cycle(&shutdown).await else {
            break;
        };
        // Drain everything runnable now. A store error is transient — the next
        // tick retries — so swallow it rather than kill the task.
        match claim_and_drive(
            &store,
            &clock,
            &owner,
            lease_ms,
            resolver.as_ref(),
            &completion,
            &admission,
            &cycle,
        )
        .await
        {
            Ok(true) => {
                admission.error_gate.record_success().await;
                // A successful drive can make a same-Thread or same-Session
                // successor eligible only at settlement. Any permit published
                // while the predecessor was live may already have been consumed
                // by an idle peer that correctly found no claimable row. Re-arm
                // one permit after the settlement boundary so the pool actually
                // drains to idle instead of waiting for the coarse fallback poll.
                // Tokio Notify coalesces permits, so this cannot accumulate an
                // unbounded retry queue when a peer wake is still pending.
                tracing::debug!(owner = %owner, "dispatch drive settled; rearming drain permit");
                admission.wake.notify_one();
                continue;
            }
            Ok(false) => admission.error_gate.record_success().await,
            // A store/drive error is transient — the next tick retries — so log it
            // and back off rather than kill the task. A stale owner's rejected
            // commit (terminal-is-final fence) never reaches here: `drive_claimed`
            // absorbs it as an already-done settle, so this only fires on genuine
            // faults.
            Err(failure) => {
                let (error, retry_after, shared_fault) = match failure {
                    ClaimAndDriveFailure::ClaimDeferred(error, delay) => {
                        (error, Some(delay), false)
                    }
                    ClaimAndDriveFailure::FaultDeferred(error, delay) => (error, Some(delay), true),
                    ClaimAndDriveFailure::Fault(error) => (error, None, true),
                };
                if shared_fault {
                    let (consecutive_errors, delay) = admission.error_gate.record_failure().await;
                    tracing::warn!(owner = %owner, error = %error, consecutive_errors,
                        retry_delay_ms = delay.as_millis(), "drain tick failed; retrying");
                } else {
                    // Request-local pressure releases a prior recovery probe,
                    // so unrelated claims can run without a pool-wide delay.
                    admission.error_gate.record_success().await;
                    tracing::warn!(owner = %owner, error = %error,
                        retry_delay_ms = retry_after.map(|delay| delay.as_millis()),
                        "request-local dispatch retry deferred");
                }
                admission.wake.notify_one();
                if let Some(retry_after) = retry_after {
                    // The persisted deadline owns eligibility; this cancellable
                    // timer only wakes the existing pool when it becomes due.
                    let retry_admission = admission.clone();
                    let retry_shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = retry_shutdown.cancelled() => {},
                            _ = tokio::time::sleep(retry_after) => retry_admission.wake.notify_one(),
                        }
                    });
                }
            }
        }
    }
}

/// Internal scheduling predicate for a transiently unresolved request. The
/// first cancellation clears a prior deadline in the canonical row transition;
/// later failed cancellation attempts must respect their new deadline.
#[must_use]
#[allow(
    dead_code,
    reason = "used by optional stores, executable tests, and the Kani admission proof"
)]
pub const fn retry_deadline_is_due(retry_not_before_ms: Option<u64>, now_ms: u64) -> bool {
    match retry_not_before_ms {
        None => true,
        Some(deadline) => deadline <= now_ms,
    }
}

#[test]
fn retry_deadline_blocks_repeated_cancellation_until_due() {
    // Causes: C1 no deadline after the first cancellation; C2 a failed
    // cancelled attempt installs a future deadline; C3 the deadline is due.
    // Effects: E1 C1 is immediately runnable; E2 C2 cannot hot-loop; E3 C3
    // becomes runnable again. Rule C1=>E1, C2=>E2, C3=>E3. The transition
    // kernel owns clearing C1 on first cancellation.
    assert!(retry_deadline_is_due(None, 0));
    assert!(!retry_deadline_is_due(Some(11), 10));
    assert!(retry_deadline_is_due(Some(10), 10));
}

/// Private pool control-flow classification. Resolution deferral is deliberately
/// not added to the public Managed Agents or run-ingress error schema: callers
/// keep seeing the established API while the drain loop avoids global backoff.
enum ClaimAndDriveFailure {
    ClaimDeferred(Error, Duration),
    FaultDeferred(Error, Duration),
    Fault(Error),
}

impl From<Error> for ClaimAndDriveFailure {
    fn from(error: Error) -> Self {
        Self::Fault(error)
    }
}

fn drain_error_backoff(consecutive_errors: u32) -> Duration {
    bounded_exponential_backoff(
        Duration::from_millis(100),
        Duration::from_secs(5),
        u64::from(consecutive_errors),
    )
}

/// The claim epoch is already the durable count of claims of this exact Run.
/// Reuse its fence rather than adding a second retry counter or queue.
pub(crate) fn claim_failure_backoff(epoch: u64) -> Duration {
    bounded_exponential_backoff(Duration::from_millis(250), Duration::from_secs(30), epoch)
}

fn claim_failure_is_request_local(error: &Error) -> bool {
    error.is_dispatch_conflict() || error.is_settlement_observation_pending()
}

fn bounded_exponential_backoff(minimum: Duration, maximum: Duration, ordinal: u64) -> Duration {
    let exponent = ordinal.saturating_sub(1).min(31) as u32;
    minimum.saturating_mul(1_u32 << exponent).min(maximum)
}

#[cfg(kani)]
mod drain_admission_proofs {
    use super::{AdmissionCommand, AdmissionState};

    fn symbolic_state() -> AdmissionState {
        match kani::any::<u8>() % 3 {
            0 => AdmissionState::Open,
            1 => AdmissionState::Suspended,
            _ => AdmissionState::Draining,
        }
    }

    fn symbolic_command() -> AdmissionCommand {
        match kani::any::<u8>() % 3 {
            0 => AdmissionCommand::Suspend,
            1 => AdmissionCommand::Resume {
                shutdown_cancelled: kani::any(),
            },
            _ => AdmissionCommand::Drain,
        }
    }

    /// @obligation worker_drain.closed_generation_is_absorbing
    /// @obligation worker_drain.no_claim_after_acknowledgement
    /// @obligation worker_drain.no_claim_attempt_after_registry_drain
    /// @obligation worker_drain.registry_transition_follows_local_fence
    /// @kani default-features=false
    #[kani::proof]
    fn pool_draining_state_is_absorbing_and_claim_closed() {
        let initial = symbolic_state();
        let command = symbolic_command();
        let next = initial.transition(command);

        assert_eq!(next.accepts_claim(), matches!(next, AdmissionState::Open));
        assert_eq!(next.can_resume(), !matches!(next, AdmissionState::Draining));

        if matches!(initial, AdmissionState::Draining) {
            assert_eq!(next, AdmissionState::Draining);
            assert!(!next.accepts_claim());
            assert!(!next.can_resume());
        }

        let (drained, _fence) = initial.begin_drain();
        assert_eq!(drained, AdmissionState::Draining);
        assert!(!drained.accepts_claim());
        assert!(!drained.can_resume());
        assert_eq!(drained.transition(command), drained);
    }
}

#[cfg(kani)]
mod drain_error_proofs {
    use super::{DrainErrorState, SlotToken, drain_error_backoff, retry_deadline_is_due};
    use std::time::Duration;

    /// @obligation run_ingress.request_local_resolution_retry_honors_persisted_deadline
    /// @kani default-features=false
    #[kani::proof]
    fn resolution_retry_admission_matches_only_the_persisted_deadline() {
        let deadline_present: bool = kani::any();
        let deadline: u64 = kani::any();
        let now_ms: u64 = kani::any();
        let retry_not_before_ms = deadline_present.then_some(deadline);
        assert_eq!(
            retry_deadline_is_due(retry_not_before_ms, now_ms),
            !deadline_present || deadline <= now_ms
        );
    }

    /// @obligation run_ingress.pool_retry_gate_admits_one_probe_and_uses_bounded_monotonic_backoff
    /// @kani default-features=false
    #[kani::proof]
    fn shared_error_gate_admits_exactly_one_probe_after_the_deadline() {
        let consecutive_errors: u32 = kani::any();
        kani::assume(consecutive_errors > 0);
        let mut state = DrainErrorState {
            consecutive_errors,
            retry_not_before: None,
            probe_in_flight: None,
        };

        assert!(!state.admit_cycle(false));
        assert!(state.probe_in_flight.is_none());
        assert!(state.admit_cycle(true));
        assert!(state.probe_in_flight.is_some());
        assert!(!state.admit_cycle(true));
    }

    /// @obligation run_ingress.pool_retry_gate_admits_one_probe_and_uses_bounded_monotonic_backoff
    /// @kani default-features=false
    #[kani::proof]
    fn probe_release_is_exact_and_preserves_backoff() {
        // P1 exact identity releases the gate, P2 stale identity changes nothing;
        // both preserve fault history. A retained token cannot equal the next
        // allocation, including after the previous probe has been released.
        let mut state = DrainErrorState {
            consecutive_errors: kani::any(),
            ..Default::default()
        };
        kani::assume(state.consecutive_errors > 0);
        let errors = state.consecutive_errors;
        assert!(state.admit_cycle(true));
        let old = state.probe_in_flight.clone().unwrap();
        let foreign = SlotToken::mint();
        let exact: bool = kani::any();
        assert_eq!(
            state.release_probe(if exact { &old } else { &foreign }),
            exact
        );
        assert_eq!(state.probe_in_flight.is_none(), exact);
        assert_eq!(state.consecutive_errors, errors);
        state.release_probe(&old);
        assert!(state.admit_cycle(true));
        assert!(!state.release_probe(&old));
        assert!(state.probe_in_flight.is_some());
        assert!(!state.admit_cycle(true));
    }

    /// @obligation run_ingress.pool_retry_gate_admits_one_probe_and_uses_bounded_monotonic_backoff
    /// @kani default-features=false
    #[kani::proof]
    fn drain_error_backoff_is_bounded_monotonic_and_saturating() {
        let consecutive_errors: u32 = kani::any();
        let delay = drain_error_backoff(consecutive_errors);
        let successor = drain_error_backoff(consecutive_errors.saturating_add(1));

        assert!(delay >= Duration::from_millis(100));
        assert!(delay <= Duration::from_secs(5));
        assert!(successor >= delay);
        if consecutive_errors >= 7 {
            assert_eq!(delay, Duration::from_secs(5));
        }
    }
}

/// Convert the external/cross-node wake plus the fallback timer into one
/// process-local claim permit. The fixed-size drain pool therefore preserves
/// execution concurrency without multiplying idle queue polls by that capacity.
async fn wake_coordinator_loop(
    wake: Arc<dyn WakeSignal>,
    admission: Arc<PoolAdmission>,
    reconciliation_wake: Arc<LocalWakeSignal>,
    shutdown: CancellationToken,
    poll_interval: Duration,
) {
    // One initial authoritative poll recovers work that predated this process.
    admission.wake.notify_one();
    let _ = reconciliation_wake.publish().await;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = wake.wait() => {
                admission.wake.notify_one();
                let _ = reconciliation_wake.publish().await;
            },
            _ = tokio::time::sleep(poll_interval) => {
                admission.wake.notify_one();
                let _ = reconciliation_wake.publish().await;
            },
        }
    }
}

/// Claim one runnable dispatch from the shared queue and drive it on the worker
/// that owns its thread. Returns whether a run was claimed. On settle, notifies the
/// [`CompletionSink`] so a foreground waiter wakes by event, not by poll.
#[allow(clippy::too_many_arguments)]
async fn claim_and_drive<S: ControlDispatch + 'static>(
    store: &Arc<S>,
    clock: &Arc<dyn WallClock>,
    owner: &str,
    lease_ms: u64,
    resolver: &dyn WorkerResolver<S>,
    completion: &Option<Arc<dyn CompletionSink>>,
    admission: &Arc<PoolAdmission>,
    cycle: &DrainCycle,
) -> Result<bool, ClaimAndDriveFailure> {
    let (claimed, retry_exhausted) = {
        let gate = admission.gate.read().await;
        if !gate.accepts_claim() {
            return Ok(false);
        }
        if let Some(claimed) = store
            .claim_retry_exhausted(owner, lease_ms, admission.max_attempts)
            .await
            .map_err(Error::from)?
        {
            (Some(claimed), true)
        } else {
            (
                store
                    .claim(
                        owner,
                        lease_ms,
                        &resolver.credential_realization_capabilities(),
                    )
                    .await
                    .map_err(Error::from)?,
                false,
            )
        }
    };
    let Some(claimed) = claimed else {
        return Ok(false);
    };
    // Session resolution can create/adopt a sandbox and materialize credentials
    // before `drive_claimed` installs its guard. Renew from queue exit onward.
    let claim = crate::RunClaim::from(&claimed.lease);
    let retry_after = claim_failure_backoff(claim.epoch);
    let retry_after_ms = retry_after.as_millis() as u64;
    let claim_renewal = renew_claim_while_active(store.clone(), &claim, lease_ms);
    // Route to the runtime that owns this run's thread, then drive+settle there.
    // The resolved worker shares this store and owner, so the settle it performs
    // acts on the same row this task just claimed.
    // Give the adapter the complete claim: the neutral pool does not interpret
    // sandbox/provider metadata, while a recovery-aware host can adopt it before
    // constructing the session runtime. Legacy resolvers use the default method,
    // which derives the same thread/agent arguments as before.
    let settled = if retry_exhausted {
        // Retry exhaustion is never execution resolution. Every pool and remote
        // path calls the same Worker terminal method through this resolver seam;
        // a failure leaves the exact claim leased for later re-claim.
        admission.error_gate.release_probe(cycle).await;
        admission.wake.notify_one();
        resolver.terminalize_retry_exhausted(&claimed).await
    } else {
        match resolver.worker_for_claimed(&claimed).await {
            Ok(worker) => {
                // Resolution succeeded, so this claim will make forward progress.
                // Only now hand a permit to a peer: notifying before resolution
                // would let a temporarily inadmissible head item hot-loop and starve
                // later runnable work.
                admission.error_gate.release_probe(cycle).await;
                admission.wake.notify_one();
                let result = worker
                    .drive_claimed_with_cancellation(
                        claimed,
                        clock.clone(),
                        claim_renewal.attempt_cancellation(),
                        Some(admission.execution_capacity.clone()),
                    )
                    .await;
                drop(claim_renewal);
                result
            }
            Err(error) if error.is_terminal_resolution() => {
                resolver
                    .settle_claimed_resolution_failure(&claimed, error)
                    .await
            }
            Err(error) => {
                defer_failed_claim(store.as_ref(), &claim, retry_after_ms).await?;
                return Err(ClaimAndDriveFailure::ClaimDeferred(error, retry_after));
            }
        }
    };
    let settled = match settled {
        Ok(settled) => settled,
        Err(error) => {
            // Exact-claim deferral releases the mutation lease and retains an
            // independent physical-attempt fence. Conflict, pending Session
            // observation, and other claim-local failures therefore share one
            // retry path; a stale claim is fenced by the same store operation.
            let request_local = claim_failure_is_request_local(&error);
            defer_failed_claim(store.as_ref(), &claim, retry_after_ms).await?;
            return Err(if request_local {
                ClaimAndDriveFailure::ClaimDeferred(error, retry_after)
            } else {
                ClaimAndDriveFailure::FaultDeferred(error, retry_after)
            });
        }
    };
    if let Some((run_id, state)) = settled
        && let Some(sink) = completion
    {
        // Signal the foreground waiter (if any) the instant the run settles.
        sink.settled(&run_id, &state);
    }
    Ok(true)
}

pub(crate) async fn defer_failed_claim<S: ControlDispatch + 'static>(
    store: &S,
    claim: &crate::RunClaim,
    retry_after_ms: u64,
) -> Result<(), Error> {
    match store.defer_claim(claim, retry_after_ms).await? {
        crate::SettleOutcome::Applied => {
            tracing::debug!(
                run_id = %claim.run_id.0,
                owner = %claim.owner,
                epoch = claim.epoch,
                retry_after_ms,
                "deferred only the failed dispatch claim"
            );
            Ok(())
        }
        crate::SettleOutcome::Fenced => {
            tracing::debug!(
                run_id = %claim.run_id.0,
                owner = %claim.owner,
                epoch = claim.epoch,
                "failed dispatch claim was already fenced before deferral"
            );
            Ok(())
        }
    }
}

/// GC aged manual quarantines, relay the cross-thread outbox, and repair
/// already-committed terminals. Retry exhaustion belongs to the actual drainer,
/// so coordinator-only maintenance never competes with a remote Worker.
struct ReconciliationContext<S> {
    store: Arc<S>,
    clock: Arc<dyn WallClock>,
    wake: Arc<dyn WakeSignal>,
    // Standalone: the external port. Pool: its sole listener's local fan-out.
    // Both reuse WakeSignal; do not add a competing external subscription.
    reconciliation_wake: Arc<dyn WakeSignal>,
    shutdown: CancellationToken,
    config: DispatchServiceConfig,
    resolver: Arc<dyn WorkerResolver<S>>,
    completion: Option<Arc<dyn CompletionSink>>,
}

async fn reconciliation_loop<S: ControlDispatch + 'static>(context: ReconciliationContext<S>) {
    // The same pool also drives database-less Workers. Their upstream already
    // owns maintenance; do not poll unsupported server verbs or a second scan.
    // Claim/renewal/retry-exhaustion stay in the independent ordinary drainers.
    let Some(maintenance) = LocalMaintenanceSchedule::for_adapter(
        context.store.owns_local_maintenance(),
        context.config.dead_letter_ttl.is_some(),
        context.config.terminal_reconciliation_interval.is_some(),
    ) else {
        return;
    };
    let ReconciliationContext {
        store,
        clock,
        wake,
        reconciliation_wake,
        shutdown,
        config,
        resolver,
        completion,
    } = context;
    let mut next_terminal_reconciliation = maintenance
        .reconciles_terminals()
        .then(awaken_time::mono::now);
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let now = clock.now_ms();
        if maintenance.purges_dead_letters() {
            let ttl = config
                .dead_letter_ttl
                .expect("the maintenance schedule retained its GC interval");
            let cutoff = now.saturating_sub(ttl.as_millis() as u64);
            if let Err(error) = store.purge_dead_letters_before(cutoff).await {
                tracing::warn!(%error, "dispatch dead-letter purge failed; retrying");
            }
        }
        // A relay that moved staged deliveries into a thread's pending input made a
        // awaiting run wakeable — nudge the drain tasks so they pick it up now rather
        // than at the next poll.
        let relayed = match store.relay().await {
            Ok(relayed) => relayed,
            Err(error) => {
                tracing::warn!(%error, "dispatch relay failed; retrying");
                0
            }
        };
        if relayed > 0 {
            // Relay is already durable. Only its best-effort notification may
            // be abandoned on shutdown; storage and observation work is joined.
            publish_dispatch_hint(wake.as_ref(), &shutdown).await;
        }
        if shutdown.is_cancelled() {
            break;
        }
        if let Some(deadline) = next_terminal_reconciliation
            && awaken_time::mono::now() >= deadline
        {
            match tokio::time::timeout(Duration::from_secs(5), resolver.reconcile_worker_evidence())
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "Worker evidence reconciliation failed; retrying")
                }
                Err(_) => {
                    tracing::warn!("Worker evidence reconciliation budget exhausted; retrying")
                }
            }
            match resolver.reconcile_committed_terminals(256).await {
                Ok(reconciled) => {
                    if let Some(sink) = &completion {
                        for (run_id, state) in reconciled {
                            sink.settled(&run_id, &state);
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "terminal dispatch reconciliation failed; retrying")
                }
            }
            let interval = config
                .terminal_reconciliation_interval
                .expect("the maintenance schedule retained its scan interval");
            next_terminal_reconciliation = Some(awaken_time::mono::now() + interval);
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = reconciliation_wake.wait() => {},
            _ = wait_for_terminal_reconciliation(next_terminal_reconciliation) => {},
            _ = tokio::time::sleep(config.poll_interval) => {}
        }
    }
}

#[cfg(test)]
mod in_flight_tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_agent_contract::agent::run::EndCause;
    use awaken_lease_testkit::ManualClock;
    use awaken_dispatch::MemoryDispatchStore;

    #[test]
    fn pool_admission_transition_is_total_and_drain_absorbing() {
        // Cause/effect graph: C1 current state is Open, Suspended, or Draining;
        // C2 command is Suspend, Resume, or Drain; C3 shutdown is cancelled for
        // Resume. Effects: E1 Open admits claims, E2 Suspended denies claims but
        // may resume, and E3 Draining denies claims and every later command.
        //
        // | Rule | Current | Command | Shutdown | Next/effect |
        // |---|---|---|---|---|
        // | A1 | Open | Suspend | any | Suspended/E2 |
        // | A2 | Open | Resume | any | Open/E1 |
        // | A3 | Suspended | Resume | false | Open/E1 |
        // | A4 | Suspended | Resume | true | Suspended/E2 |
        // | A5 | Open/Suspended | Drain | any | Draining/E3 |
        // | A6 | Draining | any | any | Draining/E3 |
        // Constraint: shutdown is consulted only by Resume; Drain is the sole
        // absorbing transition. Enumerating all 18 combinations covers every
        // closed enum input without mirroring the production match structure.
        for shutdown_cancelled in [false, true] {
            for (current, command, expected) in [
                (
                    AdmissionState::Open,
                    AdmissionCommand::Suspend,
                    AdmissionState::Suspended,
                ),
                (
                    AdmissionState::Open,
                    AdmissionCommand::Resume { shutdown_cancelled },
                    AdmissionState::Open,
                ),
                (
                    AdmissionState::Open,
                    AdmissionCommand::Drain,
                    AdmissionState::Draining,
                ),
                (
                    AdmissionState::Suspended,
                    AdmissionCommand::Suspend,
                    AdmissionState::Suspended,
                ),
                (
                    AdmissionState::Suspended,
                    AdmissionCommand::Resume { shutdown_cancelled },
                    if shutdown_cancelled {
                        AdmissionState::Suspended
                    } else {
                        AdmissionState::Open
                    },
                ),
                (
                    AdmissionState::Suspended,
                    AdmissionCommand::Drain,
                    AdmissionState::Draining,
                ),
                (
                    AdmissionState::Draining,
                    AdmissionCommand::Suspend,
                    AdmissionState::Draining,
                ),
                (
                    AdmissionState::Draining,
                    AdmissionCommand::Resume { shutdown_cancelled },
                    AdmissionState::Draining,
                ),
                (
                    AdmissionState::Draining,
                    AdmissionCommand::Drain,
                    AdmissionState::Draining,
                ),
            ] {
                let next = current.transition(command);
                assert_eq!(next, expected, "current={current:?} command={command:?}");
                assert_eq!(next.accepts_claim(), next == AdmissionState::Open);
                assert_eq!(next.can_resume(), next != AdmissionState::Draining);
            }
        }
        for current in [
            AdmissionState::Open,
            AdmissionState::Suspended,
            AdmissionState::Draining,
        ] {
            let (drained, _fence) = current.begin_drain();
            assert_eq!(drained, AdmissionState::Draining, "A5-A6 token state");
            assert!(!drained.accepts_claim(), "A5-A6 token closes claims");
            assert!(!drained.can_resume(), "A5-A6 token is absorbing");
        }
    }

    #[test]
    fn drain_failures_back_off_exponentially_and_cap() {
        // Cause/effect table: E1 first transient fault -> 100ms; E2 repeated
        // faults -> exponential delay; E3 sustained outage -> 5s cap. A later
        // successful tick resets the counter in `drain_loop` before this helper
        // is consulted again.
        assert_eq!(drain_error_backoff(1), Duration::from_millis(100), "E1");
        assert_eq!(drain_error_backoff(2), Duration::from_millis(200), "E2");
        assert_eq!(drain_error_backoff(7), Duration::from_secs(5), "E3");
        assert_eq!(drain_error_backoff(u32::MAX), Duration::from_secs(5), "E3");
    }

    #[test]
    fn failed_claims_back_off_by_the_existing_fence_epoch() {
        // Causes: C1 first claim fails; C2 the same Run fails again; C3 its
        // epoch is very large after prolonged unresolved physical quiescence.
        // Effects: E1 first retry remains prompt; E2 only this Run backs off
        // exponentially; E3 delay caps without overflow. The existing
        // request-local defer store preserves cancellation and unrelated work.
        // Rules F1=C1=>250ms, F2=C2=>500ms/1s, F3=C3=>30s.
        for (epoch, expected) in [
            (0, Duration::from_millis(250)),
            (1, Duration::from_millis(250)),
            (2, Duration::from_millis(500)),
            (3, Duration::from_secs(1)),
            (8, Duration::from_secs(30)),
            (u64::MAX, Duration::from_secs(30)),
        ] {
            assert_eq!(claim_failure_backoff(epoch), expected, "epoch={epoch}");
        }
    }

    #[test]
    fn failed_claims_preserve_request_local_and_shared_fault_boundaries() {
        // Causes: C1 exact stale claim; C2 pending Session projection; C3
        // unavailable shared store; C4 general execution fault. Effects: E1
        // C1/C2 defer only their claim and release the pool probe; E2 C3/C4
        // defer that claim while retaining the shared fault gate. Rules:
        // F4=C1|C2=>E1 and F5=C3|C4=>E2.
        let stale = Error::Dispatch(crate::DispatchError::Conflict("stale".into()));
        let pending = Error::SettlementObservationPending("pending".into());
        let store = Error::Dispatch(crate::DispatchError::Unavailable("store".into()));
        let execution = Error::Execution(awaken_runtime_contract::execution::Error::Execution(
            "attempt".into(),
        ));
        for (error, request_local) in [
            (stale, true),
            (pending, true),
            (store, false),
            (execution, false),
        ] {
            assert_eq!(claim_failure_is_request_local(&error), request_local);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn one_shared_recovery_probe_crosses_the_error_gate() {
        use std::future::{Future, poll_fn};
        use std::task::Poll;

        // Causes: C1 one outage; C2 two waiting drain cycles; C3 retry deadline;
        // C4 shutdown. Rules: before C3 both wait; at C3 exactly one gets the
        // shared probe; its sibling still waits; C4 releases that sibling with
        // false. Poll both futures and advance virtual time: scheduler load and
        // fixed wall-clock sleeps must not decide the test's outcome.
        let gate = DrainErrorGate::default();
        let shutdown = CancellationToken::new();
        let (errors, delay) = gate.record_failure().await;
        assert_eq!(errors, 1);
        let first = gate.wait_cycle(&shutdown);
        let second = gate.wait_cycle(&shutdown);
        tokio::pin!(first, second);
        poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            assert!(second.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::advance(delay).await;
        assert!(
            first.await.is_some(),
            "one pool-wide probe owns recovery authority"
        );
        poll_fn(|cx| {
            assert!(second.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        shutdown.cancel();
        assert!(
            second.await.is_none(),
            "shutdown releases the blocked sibling"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn execution_handoff_releases_only_its_probe_without_resetting_backoff() {
        // Causes: C1 failure deadline, C2 exact probe reaches execution, C3
        // duplicate/late handoff, C4 a later failure. Rules: C2 opens admission
        // without clearing the deadline/count; C3 cannot clear the next probe;
        // C4 retains exponential backoff rather than restarting at 100ms.
        let gate = DrainErrorGate::default();
        let shutdown = CancellationToken::new();
        let (_, delay) = gate.record_failure().await;
        tokio::time::advance(delay).await;
        let first = gate.wait_cycle(&shutdown).await.unwrap();
        let deadline = gate.state.lock().await.retry_not_before;
        gate.release_probe(&first).await;
        {
            let state = gate.state.lock().await;
            assert!(state.probe_in_flight.is_none());
            assert_eq!(state.consecutive_errors, 1);
            assert_eq!(state.retry_not_before, deadline);
        }
        let second = gate.wait_cycle(&shutdown).await.unwrap();
        gate.release_probe(&DrainCycle::Healthy).await;
        gate.release_probe(&first).await;
        assert!(gate.state.lock().await.probe_in_flight.is_some());
        gate.release_probe(&second).await;
        assert_eq!(gate.record_failure().await, (2, Duration::from_millis(200)));
        tokio::time::advance(Duration::from_millis(200)).await;
        let third = gate.wait_cycle(&shutdown).await.unwrap();
        gate.release_probe(&second).await;
        assert!(gate.state.lock().await.probe_in_flight.is_some());
        gate.release_probe(&third).await;
        gate.record_success().await;
        assert_eq!(gate.state.lock().await.consecutive_errors, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn completed_probe_between_observation_and_wait_cannot_strand_drains() {
        // C1 the only recovery probe is in flight; C2 its successful completion
        // occurs after a sibling observes C1, before that sibling awaits Notify.
        // E1 the sibling must re-check the now-open gate without another wake,
        // wall-clock polling, cancellation, model call, or new dispatch input.
        let gate = DrainErrorGate::default();
        let shutdown = CancellationToken::new();
        gate.record_failure().await;
        gate.state.lock().await.probe_in_flight = Some(SlotToken::mint());
        let mut observed = false;
        let waiter = gate.wait_cycle_observed(&shutdown, || {
            assert!(!observed, "the reopened gate must admit on the next check");
            observed = true;
            // Same state transition and broadcast as record_success, performed
            // synchronously at the otherwise cross-thread race boundary.
            *gate
                .state
                .try_lock()
                .expect("observation released the mutex") = DrainErrorState::default();
            gate.changed.notify_waiters();
        });
        assert!(
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("completion notification must survive before suspension")
                .is_some()
        );
        assert!(observed);
    }

    struct ReconciliationResolver {
        calls: AtomicUsize,
        evidence_calls: AtomicUsize,
        fail_first: bool,
    }

    #[async_trait]
    impl WorkerResolver<MemoryDispatchStore> for ReconciliationResolver {
        async fn reconcile_worker_evidence(&self) -> Result<(), Error> {
            let call = self.evidence_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_first && call == 0 {
                return Err(Error::Dispatch(crate::DispatchError::Unavailable(
                    "retention API unavailable".into(),
                )));
            }
            Ok(())
        }
        async fn worker_for(
            &self,
            _thread_id: &ThreadId,
            _agent_id: Option<&str>,
        ) -> Result<Arc<DispatchWorker<MemoryDispatchStore>>, Error> {
            unreachable!("maintenance reconciliation does not resolve an execution worker")
        }

        async fn reconcile_committed_terminals(
            &self,
            _limit: usize,
        ) -> Result<Vec<(RunId, RunState)>, Error> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_first && call == 0 {
                return Err(Error::Execution(
                    awaken_runtime_contract::execution::Error::Execution(
                        "injected reconciliation failure".to_string(),
                    ),
                ));
            }
            Ok(vec![(
                RunId("run-reconciled".to_string()),
                RunState::Ended(EndCause::NaturalEnd),
            )])
        }
    }

    #[derive(Default)]
    struct RecordingCompletion {
        values: Mutex<Vec<(RunId, RunState)>>,
        notified: Notify,
    }

    impl CompletionSink for RecordingCompletion {
        fn settled(&self, run_id: &RunId, state: &RunState) {
            self.values
                .lock()
                .expect("completion lock")
                .push((run_id.clone(), state.clone()));
            self.notified.notify_one();
        }
    }

    /// Terminal observation has its own cadence, independent of queue polling.
    #[tokio::test(start_paused = true)]
    async fn terminal_reconciliation_deadline_is_not_delayed_by_queue_poll() {
        // T1 terminal interval 5s < queue poll 60s; T2 first observation fails.
        // E1 startup observes once; E2 no early retry; E3 the terminal deadline
        // retries at 5s even without any wake. Advancing virtual time proves
        // cadence independently of the ordinary 60s fallback.
        let resolver = Arc::new(ReconciliationResolver {
            calls: AtomicUsize::new(0),
            evidence_calls: AtomicUsize::new(0),
            fail_first: true,
        });
        let completion = Arc::new(RecordingCompletion::default());
        let clock = Arc::new(ManualClock::new(0));
        let reconciler = DispatchReconciler::spawn(
            Arc::new(MemoryDispatchStore::new(clock.clone())),
            clock,
            Arc::new(LocalWakeSignal::new()),
            DispatchServiceConfig {
                poll_interval: Duration::from_secs(60),
                terminal_reconciliation_interval: Some(Duration::from_secs(5)),
                ..Default::default()
            },
            resolver.clone(),
            Some(completion.clone()),
        );
        tokio::task::yield_now().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "E1");
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "E2");
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2, "E3");
        assert_eq!(completion.values.lock().unwrap().len(), 1, "E3");
        reconciler.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn reconciler_shutdown_joins_in_flight_observation_without_cancelling_it() {
        // J1 observation entered before shutdown; J2 external response delayed.
        // E1 shutdown must not drop the observation Future or claim success;
        // J3 exact response arrives => E2 join completes, no next observation.
        struct BlockedObservation {
            entered: Notify,
            release: Notify,
            calls: AtomicUsize,
        }
        #[async_trait]
        impl WorkerResolver<MemoryDispatchStore> for BlockedObservation {
            async fn worker_for(
                &self,
                _: &ThreadId,
                _: Option<&str>,
            ) -> Result<Arc<DispatchWorker<MemoryDispatchStore>>, Error> {
                unreachable!()
            }
            async fn reconcile_committed_terminals(
                &self,
                _: usize,
            ) -> Result<Vec<(RunId, RunState)>, Error> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.notify_one();
                self.release.notified().await;
                Ok(Vec::new())
            }
        }
        let resolver = Arc::new(BlockedObservation {
            entered: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
        });
        let clock = Arc::new(ManualClock::new(0));
        let reconciler = DispatchReconciler::spawn(
            Arc::new(MemoryDispatchStore::new(clock.clone())),
            clock,
            Arc::new(LocalWakeSignal::new()),
            DispatchServiceConfig::default(),
            resolver.clone(),
            None,
        );
        resolver.entered.notified().await;
        let stopped = tokio::spawn(reconciler.shutdown());
        tokio::task::yield_now().await;
        assert!(
            !stopped.is_finished(),
            "E1 must retain the in-flight Future"
        );
        resolver.release.notify_one();
        tokio::time::timeout(Duration::from_millis(1), stopped)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "E2");
    }

    /// Cause/effect decision table for process-level terminal maintenance:
    ///
    /// | Rule | Interval | Resolver result | Expected effect |
    /// |------|----------|-----------------|-----------------|
    /// | R1 | None | n/a | no reconciliation call |
    /// | R2 | Some | transient error | loop survives and retries |
    /// | R3 | Some | terminal rows | forward each row to CompletionSink |
    ///
    /// Constraint: this cadence orchestrates the resolver only; it never infers
    /// terminal truth or mutates dispatch rows itself.
    #[tokio::test(start_paused = true)]
    async fn maintenance_reconciliation_is_optional_retryable_and_event_driven() {
        let clock = Arc::new(ManualClock::new(42));
        let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
        let wake = Arc::new(LocalWakeSignal::new());

        let disabled_resolver = Arc::new(ReconciliationResolver {
            calls: AtomicUsize::new(0),
            evidence_calls: AtomicUsize::new(0),
            fail_first: false,
        });
        let disabled_shutdown = CancellationToken::new();
        let disabled = tokio::spawn(reconciliation_loop(ReconciliationContext {
            store: store.clone(),
            clock: clock.clone(),
            wake: wake.clone(),
            reconciliation_wake: Arc::new(LocalWakeSignal::new()),
            shutdown: disabled_shutdown.clone(),
            config: DispatchServiceConfig {
                poll_interval: Duration::from_millis(2),
                terminal_reconciliation_interval: None,
                ..Default::default()
            },
            resolver: disabled_resolver.clone(),
            completion: None,
        }));
        // R1 advances five configured poll cadences under virtual time. The
        // disabled loop has entered before that window; no terminal or
        // retention call may be fabricated at any of those opportunities.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
        disabled_shutdown.cancel();
        disabled.await.expect("disabled maintenance exits");
        assert_eq!(disabled_resolver.calls.load(Ordering::SeqCst), 0, "R1");
        assert_eq!(
            disabled_resolver.evidence_calls.load(Ordering::SeqCst),
            0,
            "R1 retention shares schedule"
        );
        tokio::time::resume();

        let retrying_resolver = Arc::new(ReconciliationResolver {
            calls: AtomicUsize::new(0),
            evidence_calls: AtomicUsize::new(0),
            fail_first: true,
        });
        let completion = Arc::new(RecordingCompletion::default());
        let enabled_shutdown = CancellationToken::new();
        let enabled = tokio::spawn(reconciliation_loop(ReconciliationContext {
            store,
            clock,
            wake,
            reconciliation_wake: Arc::new(LocalWakeSignal::new()),
            shutdown: enabled_shutdown.clone(),
            config: DispatchServiceConfig {
                poll_interval: Duration::from_millis(2),
                terminal_reconciliation_interval: Some(Duration::from_millis(2)),
                ..Default::default()
            },
            resolver: retrying_resolver.clone(),
            completion: Some(completion.clone()),
        }));
        tokio::time::timeout(Duration::from_secs(1), completion.notified.notified())
            .await
            .expect("R2 retry reaches R3 completion");
        enabled_shutdown.cancel();
        enabled.await.expect("enabled maintenance exits");

        assert!(retrying_resolver.calls.load(Ordering::SeqCst) >= 2, "R2");
        assert!(
            retrying_resolver.evidence_calls.load(Ordering::SeqCst) >= 2,
            "R2 retention failure neither disables terminal repair nor creates a second timer"
        );
        assert_eq!(
            completion
                .values
                .lock()
                .expect("completion lock")
                .as_slice(),
            &[(
                RunId("run-reconciled".to_string()),
                RunState::Ended(EndCause::NaturalEnd),
            )],
            "R3"
        );
    }
}
