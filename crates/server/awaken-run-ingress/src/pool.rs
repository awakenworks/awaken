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

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use awaken_runtime_contract::activation::RunActivation;

use crate::Error;
use crate::clock::Clock;
use crate::dispatch::{Claimed, Dispatch, PendingInput};
use crate::service::DispatchServiceConfig;
use crate::wake::{LocalWakeSignal, WakeSignal};
use crate::worker::{DispatchWorker, renew_claim_while_active};
use awaken_run_ingress_contract::RunDispatch;
use std::sync::atomic::{AtomicU32, Ordering};

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
    /// Exact process-local realization evidence used before the pool freezes a
    /// local claim. Registered remote workers use their immutable manifest at
    /// the server and do not route through this local pool seam.
    fn credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
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
        _clock: Arc<dyn Clock>,
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
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error>
    where
        S: Dispatch + 'static,
    {
        self.worker_for_claimed(claimed)
            .await?
            .terminalize_retry_exhausted(claimed, clock)
            .await
    }

    /// Reconcile up to `limit` quiescent or expired dispatches against the
    /// resolver's authoritative committed Run readers. Generic resolvers have no
    /// global reader registry and do nothing; a Runtime Host overrides this once
    /// for its Session/commit ownership boundary.
    async fn reconcile_committed_terminals(
        &self,
        _clock: Arc<dyn Clock>,
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
    maintenance: JoinHandle<()>,
    admission: Arc<PoolAdmission>,
}

/// The canonical process-level maintenance owner when a deployment owns the
/// durable queue but intentionally runs no local claim drainers.
///
/// Coordinator-only deployments use this handle instead of reproducing the
/// pool's relay/reconciliation scheduler in their Runtime Host. With no drainer,
/// retry-exhausted rows remain durable until a Worker returns and claims them.
pub struct DispatchMaintenance {
    shutdown: CancellationToken,
    wake: Arc<dyn WakeSignal>,
    task: Option<JoinHandle<()>>,
}

impl DispatchMaintenance {
    pub fn spawn<S: Dispatch + 'static>(
        store: Arc<S>,
        clock: Arc<dyn Clock>,
        wake: Arc<dyn WakeSignal>,
        config: DispatchServiceConfig,
        resolver: Arc<dyn WorkerResolver<S>>,
        completion: Option<Arc<dyn CompletionSink>>,
    ) -> Self {
        let shutdown = CancellationToken::new();
        let maintenance_wake = Arc::new(Notify::new());
        let task = tokio::spawn(maintenance_loop(MaintenanceContext {
            store,
            clock,
            wake: wake.clone(),
            maintenance_wake,
            shutdown: shutdown.clone(),
            config,
            resolver,
            completion,
        }));
        Self {
            shutdown,
            wake,
            task: Some(task),
        }
    }

    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        let _ = self.wake.publish().await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for DispatchMaintenance {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DrainAdmission {
    open: bool,
    generation: u64,
}

struct PoolAdmission {
    gate: tokio::sync::RwLock<DrainAdmission>,
    error_gate: DrainErrorGate,
    in_flight: Arc<AtomicU32>,
    wake: Notify,
    max_attempts: u64,
}

#[derive(Default)]
struct DrainErrorState {
    consecutive_errors: u32,
    retry_not_before: Option<tokio::time::Instant>,
    probe_in_flight: bool,
}

#[derive(Default)]
struct DrainErrorGate {
    state: tokio::sync::Mutex<DrainErrorState>,
    changed: Notify,
}

impl DrainErrorGate {
    /// During an outage, admit exactly one recovery probe for the whole pool.
    /// Pool width must remain execution concurrency, never retry concurrency.
    async fn wait_cycle(&self, shutdown: &CancellationToken) -> bool {
        loop {
            let delay = {
                let mut state = self.state.lock().await;
                if state.consecutive_errors == 0 {
                    return true;
                }
                if state.probe_in_flight {
                    None
                } else {
                    let retry_not_before = state
                        .retry_not_before
                        .expect("an error state owns a retry deadline");
                    let now = tokio::time::Instant::now();
                    if now >= retry_not_before {
                        state.probe_in_flight = true;
                        return true;
                    }
                    Some(retry_not_before.duration_since(now))
                }
            };
            match delay {
                Some(delay) => tokio::select! {
                    _ = shutdown.cancelled() => return false,
                    _ = self.changed.notified() => {},
                    _ = tokio::time::sleep(delay) => {},
                },
                None => tokio::select! {
                    _ = shutdown.cancelled() => return false,
                    _ = self.changed.notified() => {},
                },
            }
        }
    }

    async fn record_success(&self) {
        let mut state = self.state.lock().await;
        if state.consecutive_errors == 0 && !state.probe_in_flight {
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
        state.retry_not_before = Some(tokio::time::Instant::now() + delay);
        state.probe_in_flight = false;
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
            gate: tokio::sync::RwLock::new(DrainAdmission::default()),
            error_gate: DrainErrorGate::default(),
            in_flight: Arc::new(AtomicU32::new(0)),
            wake: Notify::new(),
            max_attempts,
        }
    }
}

impl Default for DrainAdmission {
    fn default() -> Self {
        Self {
            open: true,
            generation: 0,
        }
    }
}

impl<S: Dispatch + 'static> DispatchPool<S> {
    /// Spawn the pool with the single-process wake signal. `concurrency` is the
    /// number of runs driven in parallel (clamped to at least 1).
    pub fn spawn(
        store: Arc<S>,
        clock: Arc<dyn Clock>,
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
        clock: Arc<dyn Clock>,
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
        clock: Arc<dyn Clock>,
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
        clock: Arc<dyn Clock>,
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
        clock: Arc<dyn Clock>,
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
        let admission = Arc::new(PoolAdmission::new(config.max_attempts));
        let maintenance_wake = Arc::new(Notify::new());
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
                    config.poll_interval,
                ))
            })
            .collect();
        let wake_coordinator = tokio::spawn(wake_coordinator_loop(
            wake.clone(),
            admission.clone(),
            maintenance_wake.clone(),
            shutdown.clone(),
            config.poll_interval,
        ));
        let maintenance = tokio::spawn(maintenance_loop(MaintenanceContext {
            store: store.clone(),
            clock: clock.clone(),
            wake: wake.clone(),
            maintenance_wake,
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
            maintenance,
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
        let _ = self.wake.publish().await;
        Ok(())
    }

    /// Durably deliver pending input and nudge the pool to resume the run.
    pub async fn deliver(&self, input: PendingInput) -> Result<(), Error> {
        self.store.append(input).await?;
        let _ = self.wake.publish().await;
        Ok(())
    }

    /// Stage a cross-thread delivery and nudge the pool to relay it.
    pub async fn send(&self, input: PendingInput) -> Result<(), Error> {
        self.store.stage(input).await?;
        let _ = self.wake.publish().await;
        Ok(())
    }

    /// Wake the pool to drain immediately.
    pub async fn notify(&self) {
        let _ = self.wake.publish().await;
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
        let _ = self.wake.publish().await;
        Ok(true)
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
    pub async fn begin_drain(&self) {
        // The write lock linearizes drain against the short read-side claim
        // critical section. Once this returns, every claim either completed
        // before the drain generation advanced (and is now in-flight), or saw
        // `open = false` and did not touch the store.
        let mut admission = self.admission.gate.write().await;
        if admission.open {
            admission.open = false;
            admission.generation = admission.generation.saturating_add(1);
        }
        self.shutdown.cancel();
        drop(admission);
        let _ = self.wake.publish().await;
    }

    /// Whether a drain has been requested (the drain tasks are stopping/stopped).
    pub fn is_draining(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// Exact process-local number of claims currently being driven. The worker
    /// heartbeat reports this value; capacity enforcement remains server-side.
    #[must_use]
    pub fn in_flight(&self) -> u32 {
        self.admission.in_flight.load(Ordering::SeqCst)
    }

    /// Stop every task and wait for the in-flight drains to finish.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = self.wake.publish().await;
        for drain in self.drains {
            let _ = drain.await;
        }
        let _ = self.wake_coordinator.await;
        let _ = self.maintenance.await;
    }
}

/// One drain task: claim a runnable dispatch, route it to its owning session's
/// worker, drive it, repeat until idle, then await on a wake or the poll timer.
#[allow(clippy::too_many_arguments)]
async fn drain_loop<S: Dispatch + 'static>(
    store: Arc<S>,
    clock: Arc<dyn Clock>,
    shutdown: CancellationToken,
    owner: String,
    lease_ms: u64,
    resolver: Arc<dyn WorkerResolver<S>>,
    completion: Option<Arc<dyn CompletionSink>>,
    admission: Arc<PoolAdmission>,
    poll_interval: Duration,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = admission.wake.notified() => {}
        }
        if !admission.error_gate.wait_cycle(&shutdown).await {
            break;
        }
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
        )
        .await
        {
            Ok(true) => {
                admission.error_gate.record_success().await;
                continue;
            }
            Ok(false) => admission.error_gate.record_success().await,
            // A store/drive error is transient — the next tick retries — so log it
            // and back off rather than kill the task. A stale owner's rejected
            // commit (terminal-is-final fence) never reaches here: `drive_claimed`
            // absorbs it as an already-done settle, so this only fires on genuine
            // faults.
            Err(err) if err.is_resolution_not_ready() => {
                // WorkQueue serializes Runs that share one Environment. A
                // predecessor releasing that slot is normal backpressure, not a
                // fault; cap retries independently of a very small queue poll.
                tokio::time::sleep(poll_interval.max(Duration::from_millis(250))).await;
            }
            Err(err) => {
                let (consecutive_errors, delay) = admission.error_gate.record_failure().await;
                tracing::warn!(
                    owner = %owner,
                    error = %err,
                    consecutive_errors,
                    retry_delay_ms = delay.as_millis(),
                    "drain tick failed; retrying"
                );
            }
        }
    }
}

fn drain_error_backoff(consecutive_errors: u32) -> Duration {
    const MIN: Duration = Duration::from_millis(100);
    const MAX: Duration = Duration::from_secs(5);
    let exponent = consecutive_errors.saturating_sub(1).min(31);
    MIN.saturating_mul(1_u32 << exponent).min(MAX)
}

/// Convert the external/cross-node wake plus the fallback timer into one
/// process-local claim permit. The fixed-size drain pool therefore preserves
/// execution concurrency without multiplying idle queue polls by that capacity.
async fn wake_coordinator_loop(
    wake: Arc<dyn WakeSignal>,
    admission: Arc<PoolAdmission>,
    maintenance_wake: Arc<Notify>,
    shutdown: CancellationToken,
    poll_interval: Duration,
) {
    // One initial authoritative poll recovers work that predated this process.
    admission.wake.notify_one();
    maintenance_wake.notify_one();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = wake.wait() => {
                admission.wake.notify_one();
                maintenance_wake.notify_one();
            },
            _ = tokio::time::sleep(poll_interval) => {
                admission.wake.notify_one();
                maintenance_wake.notify_one();
            },
        }
    }
}

/// Claim one runnable dispatch from the shared queue and drive it on the worker
/// that owns its thread. Returns whether a run was claimed. On settle, notifies the
/// [`CompletionSink`] so a foreground waiter wakes by event, not by poll.
async fn claim_and_drive<S: Dispatch + 'static>(
    store: &Arc<S>,
    clock: &Arc<dyn Clock>,
    owner: &str,
    lease_ms: u64,
    resolver: &dyn WorkerResolver<S>,
    completion: &Option<Arc<dyn CompletionSink>>,
    admission: &Arc<PoolAdmission>,
) -> Result<bool, Error> {
    let now = clock.now_ms();
    let (claimed, retry_exhausted) = {
        let gate = admission.gate.read().await;
        if !gate.open {
            return Ok(false);
        }
        if let Some(claimed) = store
            .claim_retry_exhausted(owner, lease_ms, now, admission.max_attempts)
            .await?
        {
            (Some(claimed), true)
        } else {
            (
                store
                    .claim(
                        owner,
                        lease_ms,
                        now,
                        &resolver.credential_realization_capabilities(),
                    )
                    .await?,
                false,
            )
        }
    };
    let Some(claimed) = claimed else {
        return Ok(false);
    };
    let _in_flight = counts_toward_execution_capacity(
        retry_exhausted,
        claimed.session_activity_admission_required,
    )
    .then(|| InFlightGuard::new(admission.in_flight.clone()));
    // Session resolution can create/adopt a sandbox and materialize credentials
    // before `drive_claimed` installs its guard. Renew from queue exit onward.
    let claim = crate::RunClaim::from(&claimed.lease);
    let claim_renewal = renew_claim_while_active(store.clone(), &claim, lease_ms, clock.clone());
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
        admission.wake.notify_one();
        resolver
            .terminalize_retry_exhausted(&claimed, clock.clone())
            .await?
    } else {
        match resolver.worker_for_claimed(&claimed).await {
            Ok(worker) => {
                // Resolution succeeded, so this claim will make forward progress.
                // Only now hand a permit to a peer: notifying before resolution
                // would let a temporarily inadmissible head item hot-loop and starve
                // later runnable work.
                admission.wake.notify_one();
                worker
                    .drive_claimed_with_renewal(claimed, clock.clone(), claim_renewal)
                    .await?
            }
            Err(error) if error.is_terminal_resolution() => {
                resolver
                    .settle_claimed_resolution_failure(&claimed, error, clock.clone())
                    .await?
            }
            Err(error) => {
                relinquish_after_resolution_failure(store.as_ref(), &claimed).await;
                return Err(error);
            }
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

async fn relinquish_after_resolution_failure<S: Dispatch + 'static>(store: &S, claimed: &Claimed) {
    let claim = crate::RunClaim::from(&claimed.lease);
    match store.relinquish_claim(&claim).await {
        Ok(crate::SettleOutcome::Applied) => {}
        Ok(crate::SettleOutcome::Fenced) => tracing::debug!(
            run_id = %claim.run_id.0,
            owner = %claim.owner,
            epoch = claim.epoch,
            "resolution failure claim was already fenced before relinquish"
        ),
        Err(error) => tracing::warn!(
            run_id = %claim.run_id.0,
            owner = %claim.owner,
            epoch = claim.epoch,
            %error,
            "failed to relinquish claim after resolution failure"
        ),
    }
}

struct InFlightGuard(Arc<AtomicU32>);

/// Pool heartbeat capacity measures executable Run claims, not admission repair
/// or retry-exhaustion terminalization. Both maintenance claims still occupy a
/// drain task and retain lease renewal, but neither consumes model/tool/runtime
/// execution capacity.
const fn counts_toward_execution_capacity(
    retry_exhausted: bool,
    session_activity_admission_required: bool,
) -> bool {
    !retry_exhausted && !session_activity_admission_required
}

impl InFlightGuard {
    fn new(counter: Arc<AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// GC aged manual quarantines, relay the cross-thread outbox, and repair
/// already-committed terminals. Retry exhaustion belongs to the actual drainer,
/// so coordinator-only maintenance never competes with a remote Worker.
struct MaintenanceContext<S> {
    store: Arc<S>,
    clock: Arc<dyn Clock>,
    wake: Arc<dyn WakeSignal>,
    maintenance_wake: Arc<Notify>,
    shutdown: CancellationToken,
    config: DispatchServiceConfig,
    resolver: Arc<dyn WorkerResolver<S>>,
    completion: Option<Arc<dyn CompletionSink>>,
}

async fn maintenance_loop<S: Dispatch + 'static>(context: MaintenanceContext<S>) {
    let MaintenanceContext {
        store,
        clock,
        wake,
        maintenance_wake,
        shutdown,
        config,
        resolver,
        completion,
    } = context;
    let mut next_terminal_reconciliation = tokio::time::Instant::now();
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let now = clock.now_ms();
        if let Some(ttl) = config.dead_letter_ttl {
            let cutoff = now.saturating_sub(ttl.as_millis() as u64);
            let _ = store.purge_dead_letters_before(cutoff).await;
        }
        // A relay that moved staged deliveries into a thread's pending input made a
        // awaiting run wakeable — nudge the drain tasks so they pick it up now rather
        // than at the next poll.
        let relayed = store.relay().await.unwrap_or(0);
        if relayed > 0 {
            let _ = wake.publish().await;
        }
        if let Some(interval) = config.terminal_reconciliation_interval
            && tokio::time::Instant::now() >= next_terminal_reconciliation
        {
            match resolver
                .reconcile_committed_terminals(clock.clone(), 256)
                .await
            {
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
            next_terminal_reconciliation = tokio::time::Instant::now() + interval;
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = maintenance_wake.notified() => {},
            _ = tokio::time::sleep(config.poll_interval) => {}
        }
    }
}

#[cfg(test)]
mod in_flight_tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use crate::{ManualClock, MemoryDispatchStore};
    use awaken_agent_contract::agent::run::EndCause;

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

    #[tokio::test]
    async fn one_shared_recovery_probe_crosses_the_error_gate() {
        // Causes: C1 one queue outage; C2 multiple drain tasks. Effects: E1 one
        // probe crosses after the deadline; E2 every sibling remains blocked.
        // Constraint/Invariant: retry authority is pool-wide; per-task counters
        // must not rotate fresh first retries. Decision rule: with C1+C2, exactly
        // one waiter finishes before shutdown and its sibling does not.
        let gate = Arc::new(DrainErrorGate::default());
        let shutdown = CancellationToken::new();
        assert_eq!(gate.record_failure().await.0, 1);

        let first = {
            let gate = gate.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move { gate.wait_cycle(&shutdown).await })
        };
        let second = {
            let gate = gate.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move { gate.wait_cycle(&shutdown).await })
        };
        tokio::time::sleep(Duration::from_millis(130)).await;
        assert_eq!(
            u8::from(first.is_finished()) + u8::from(second.is_finished()),
            1,
            "one pool-wide probe owns recovery authority"
        );
        shutdown.cancel();
        let _ = first.await;
        let _ = second.await;
    }

    struct ReconciliationResolver {
        calls: AtomicUsize,
        fail_first: bool,
    }

    #[async_trait]
    impl WorkerResolver<MemoryDispatchStore> for ReconciliationResolver {
        async fn worker_for(
            &self,
            _thread_id: &ThreadId,
            _agent_id: Option<&str>,
        ) -> Result<Arc<DispatchWorker<MemoryDispatchStore>>, Error> {
            unreachable!("maintenance reconciliation does not resolve an execution worker")
        }

        async fn reconcile_committed_terminals(
            &self,
            _clock: Arc<dyn Clock>,
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

    #[test]
    fn guard_tracks_and_releases_exactly_once() {
        let counter = Arc::new(AtomicU32::new(0));
        {
            let _first = InFlightGuard::new(counter.clone());
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            {
                let _second = InFlightGuard::new(counter.clone());
                assert_eq!(counter.load(Ordering::SeqCst), 2);
            }
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        }
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn only_executable_claims_count_toward_worker_capacity() {
        // Cause/effect graph: C1 ordinary executable claim; C2 Session activity
        // repair claim; C3 retry-exhaustion terminal claim. Effects: E1 consumes
        // one Worker execution slot; E2/E3 keep capacity unchanged while their
        // existing drain task and lease renewal remain active.
        //
        // | Rule | Retry exhausted | Admission repair | Capacity effect |
        // |---|---|---|---|
        // | PC1 | false | false | E1 counted |
        // | PC2 | false | true | E2 not counted |
        // | PC3 | true | false | E3 not counted |
        // | PC4 | true | true | E2+E3 not counted (defensive overlap) |
        // Constraint/Invariant: only a claim that can enter the Run executor
        // consumes execution capacity. Decision rule: PC1-PC4 exhaust the two
        // boolean causes, including their defensive overlap.
        assert!(counts_toward_execution_capacity(false, false), "PC1/E1");
        assert!(!counts_toward_execution_capacity(false, true), "PC2/E2");
        assert!(!counts_toward_execution_capacity(true, false), "PC3/E3");
        assert!(!counts_toward_execution_capacity(true, true), "PC4/E2+E3");
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
    #[tokio::test]
    async fn maintenance_reconciliation_is_optional_retryable_and_event_driven() {
        let store = Arc::new(MemoryDispatchStore::new());
        let clock = Arc::new(ManualClock::new(42));
        let wake = Arc::new(LocalWakeSignal::new());

        let disabled_resolver = Arc::new(ReconciliationResolver {
            calls: AtomicUsize::new(0),
            fail_first: false,
        });
        let disabled_shutdown = CancellationToken::new();
        let disabled = tokio::spawn(maintenance_loop(MaintenanceContext {
            store: store.clone(),
            clock: clock.clone(),
            wake: wake.clone(),
            maintenance_wake: Arc::new(Notify::new()),
            shutdown: disabled_shutdown.clone(),
            config: DispatchServiceConfig {
                poll_interval: Duration::from_millis(2),
                terminal_reconciliation_interval: None,
                ..Default::default()
            },
            resolver: disabled_resolver.clone(),
            completion: None,
        }));
        tokio::time::sleep(Duration::from_millis(10)).await;
        disabled_shutdown.cancel();
        disabled.await.expect("disabled maintenance exits");
        assert_eq!(disabled_resolver.calls.load(Ordering::SeqCst), 0, "R1");

        let retrying_resolver = Arc::new(ReconciliationResolver {
            calls: AtomicUsize::new(0),
            fail_first: true,
        });
        let completion = Arc::new(RecordingCompletion::default());
        let enabled_shutdown = CancellationToken::new();
        let enabled = tokio::spawn(maintenance_loop(MaintenanceContext {
            store,
            clock,
            wake,
            maintenance_wake: Arc::new(Notify::new()),
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
