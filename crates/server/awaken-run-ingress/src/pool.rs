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
//! One renewal heartbeat and one maintenance loop (reap poison runs, GC aged
//! dead-letters, relay the cross-thread outbox) run once per process rather than
//! once per session. Correctness rests on the same durable-claim invariants as the
//! per-session daemon: `claim` is owner-scoped with `FOR UPDATE SKIP LOCKED`, so N
//! tasks take distinct runs; a dropped wake only defers work to the poll fallback;
//! a crash leaves the lease to expire and be reclaimed.

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
use crate::worker::DispatchWorker;
use awaken_run_ingress_contract::RunDispatch;
use std::sync::atomic::{AtomicU32, Ordering};

/// Resolves the worker that owns a thread's runtime. The pool claims from the one
/// shared queue, then asks the resolver for the session worker carrying the
/// claimed run's thread config and drives the run there.
///
/// The resolver MUST return workers that share the pool's store and claim owner,
/// so the pool's claim, the drive, and the heartbeat all agree on lease ownership
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
    clock: Arc<dyn Clock>,
    owner: String,
    lease_ms: u64,
    resolver: Arc<dyn WorkerResolver<S>>,
    wake: Arc<dyn WakeSignal>,
    shutdown: CancellationToken,
    drains: Vec<JoinHandle<()>>,
    wake_coordinator: JoinHandle<()>,
    maintenance: JoinHandle<()>,
    renewal: Option<JoinHandle<()>>,
    completion: Option<Arc<dyn CompletionSink>>,
    admission: Arc<PoolAdmission>,
}

#[derive(Debug, Clone, Copy)]
struct DrainAdmission {
    open: bool,
    generation: u64,
}

struct PoolAdmission {
    gate: tokio::sync::RwLock<DrainAdmission>,
    in_flight: Arc<AtomicU32>,
    wake: Notify,
    active_runs: Arc<ActiveRuns>,
}

/// Exact Runs whose claim is still being resolved or driven by this process.
///
/// The durable queue owns claim truth; this is only the process-local liveness
/// projection used by lease renewal. In particular, a resolver/drive failure
/// removes the Run immediately so an un-settled claim can expire and recover.
#[derive(Default)]
struct ActiveRuns(std::sync::Mutex<std::collections::BTreeMap<String, (RunId, usize)>>);

impl ActiveRuns {
    fn enter(self: &Arc<Self>, run_id: RunId) -> ActiveRunGuard {
        let mut active = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active
            .entry(run_id.0.clone())
            .and_modify(|(_, count)| *count += 1)
            .or_insert_with(|| (run_id.clone(), 1));
        ActiveRunGuard {
            active: self.clone(),
            run_id,
        }
    }

    fn snapshot(&self) -> Vec<RunId> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|(run_id, _)| run_id.clone())
            .collect()
    }
}

struct ActiveRunGuard {
    active: Arc<ActiveRuns>,
    run_id: RunId,
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        let mut active = self
            .active
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, count)) = active.get_mut(&self.run_id.0) {
            *count -= 1;
            if *count == 0 {
                active.remove(&self.run_id.0);
            }
        }
    }
}

impl Default for PoolAdmission {
    fn default() -> Self {
        Self {
            gate: tokio::sync::RwLock::new(DrainAdmission::default()),
            in_flight: Arc::new(AtomicU32::new(0)),
            wake: Notify::new(),
            active_runs: Arc::new(ActiveRuns::default()),
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
        let admission = Arc::new(PoolAdmission::default());
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
            shutdown.clone(),
            config.poll_interval,
        ));
        let maintenance = tokio::spawn(maintenance_loop(
            store.clone(),
            clock.clone(),
            wake.clone(),
            shutdown.clone(),
            config,
        ));
        let renewal = config.lease_renewal_interval.map(|interval| {
            tokio::spawn(renewal_loop(
                store.clone(),
                clock.clone(),
                shutdown.clone(),
                owner.clone(),
                lease_ms,
                interval,
                admission.clone(),
            ))
        });
        Self {
            store,
            clock,
            owner,
            lease_ms,
            resolver,
            wake,
            shutdown,
            drains,
            wake_coordinator,
            maintenance,
            renewal,
            completion,
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

    /// Persist cancellation and drive that exact row through the pool's one
    /// claim/resolver path. The claimed activation remains authoritative after a
    /// process restart; no Session config or runtime is reconstructed merely to
    /// discover its executor.
    pub async fn cancel(&self, run_id: &RunId) -> Result<bool, Error> {
        if self.store.cancel(run_id).await?.is_none() {
            return Ok(false);
        }
        // Wake peer/local drains as well as attempting the synchronous exact
        // claim below. A racing owner is valid; the durable intent remains the
        // authority and only one claimant can settle its new epoch.
        let _ = self.wake.publish().await;
        let now = self.clock.now_ms();
        let claimed = self
            .store
            .claim_run(
                run_id,
                &self.owner,
                self.lease_ms,
                now,
                &self.resolver.credential_realization_capabilities(),
            )
            .await?;
        let Some(claimed) = claimed else {
            // A drain worker may already own the newly fenced cancellation. The
            // durable intent is accepted and that owner must settle it.
            return Ok(true);
        };
        let _active_run = self
            .admission
            .active_runs
            .enter(claimed.lease.run_id.clone());
        let worker = self.resolver.worker_for_claimed(&claimed).await?;
        if let Some((settled_run, state)) = worker.drive_claimed(claimed, now).await?
            && let Some(sink) = &self.completion
        {
            sink.settled(&settled_run, &state);
        }
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
        if let Some(renewal) = self.renewal {
            let _ = renewal.await;
        }
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
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = admission.wake.notified() => {}
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
            Ok(true) => continue,
            Ok(false) => {}
            // A store/drive error is transient — the next tick retries — so log it
            // and back off rather than kill the task. A stale owner's rejected
            // commit (terminal-is-final fence) never reaches here: `drive_claimed`
            // absorbs it as an already-done settle, so this only fires on genuine
            // faults.
            Err(err) => {
                tracing::warn!(owner = %owner, error = %err, "drain tick failed; retrying");
            }
        }
    }
}

/// Convert the external/cross-node wake plus the fallback timer into one
/// process-local claim permit. The fixed-size drain pool therefore preserves
/// execution concurrency without multiplying idle queue polls by that capacity.
async fn wake_coordinator_loop(
    wake: Arc<dyn WakeSignal>,
    admission: Arc<PoolAdmission>,
    shutdown: CancellationToken,
    poll_interval: Duration,
) {
    // One initial authoritative poll recovers work that predated this process.
    admission.wake.notify_one();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = wake.wait() => admission.wake.notify_one(),
            _ = tokio::time::sleep(poll_interval) => admission.wake.notify_one(),
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
    let claimed = {
        let gate = admission.gate.read().await;
        if !gate.open {
            return Ok(false);
        }
        store
            .claim(
                owner,
                lease_ms,
                now,
                &resolver.credential_realization_capabilities(),
            )
            .await?
    };
    let Some(claimed) = claimed else {
        return Ok(false);
    };
    // A successful claim proves that more work may be queued. Hand one permit to
    // a peer before driving this run so capacity scales without idle pollers.
    admission.wake.notify_one();
    let _in_flight = InFlightGuard::new(admission.in_flight.clone());
    let _active_run = admission.active_runs.enter(claimed.lease.run_id.clone());
    // Route to the runtime that owns this run's thread, then drive+settle there.
    // The resolved worker shares this store and owner, so the settle it performs
    // acts on the same row this task just claimed.
    // Give the adapter the complete claim: the neutral pool does not interpret
    // sandbox/provider metadata, while a recovery-aware host can adopt it before
    // constructing the session runtime. Legacy resolvers use the default method,
    // which derives the same thread/agent arguments as before.
    let worker = resolver.worker_for_claimed(&claimed).await?;
    if let Some((run_id, state)) = worker.drive_claimed(claimed, now).await?
        && let Some(sink) = completion
    {
        // Signal the foreground waiter (if any) the instant the run settles.
        sink.settled(&run_id, &state);
    }
    Ok(true)
}

struct InFlightGuard(Arc<AtomicU32>);

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

/// Reap poison runs, GC aged dead-letters, and relay the cross-thread outbox — the
/// per-process maintenance the per-session daemon used to do per session.
async fn maintenance_loop<S: Dispatch + 'static>(
    store: Arc<S>,
    clock: Arc<dyn Clock>,
    wake: Arc<dyn WakeSignal>,
    shutdown: CancellationToken,
    config: DispatchServiceConfig,
) {
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let now = clock.now_ms();
        let _ = store.reap(config.max_attempts, now).await;
        if let Some(ttl) = config.dead_letter_ttl {
            let cutoff = now.saturating_sub(ttl.as_millis() as u64);
            let _ = store.purge_dead_letters_before(cutoff).await;
        }
        // A relay that moved staged deliveries into a thread's pending input made a
        // awaiting run wakeable — nudge the drain tasks so they pick it up now rather
        // than at the next poll.
        if store.relay().await.unwrap_or(0) > 0 {
            let _ = wake.publish().await;
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(config.poll_interval) => {}
        }
    }
}

/// Renew every near-expiry lease this process owns, so a long run is not reclaimed
/// while still executing (ADR-0024). One heartbeat for the whole pool.
async fn renewal_loop<S: Dispatch + 'static>(
    store: Arc<S>,
    clock: Arc<dyn Clock>,
    shutdown: CancellationToken,
    owner: String,
    lease_ms: u64,
    interval: Duration,
    admission: Arc<PoolAdmission>,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(interval) => {
                let now = clock.now_ms();
                for run_id in admission.active_runs.snapshot() {
                    let _ = store.renew_lease(&run_id, &owner, lease_ms, now).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod in_flight_tests {
    use super::*;

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
}
