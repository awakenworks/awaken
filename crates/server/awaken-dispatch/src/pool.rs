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
use awaken_agent_contract::agent::run::{Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use awaken_runtime_contract::activation::RunActivation;

use crate::Error;
use crate::clock::Clock;
use crate::dispatch::{Dispatch, PendingInput};
use crate::request::RunExecutionRequest;
use crate::service::DispatchServiceConfig;
use crate::wake::{LocalWakeSignal, WakeSignal};
use crate::worker::DispatchWorker;

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
    /// The worker whose runtime owns `thread_id`, opening the session if needed.
    /// `model_ref` is the claimed run's own model binding (from its activation
    /// snapshot), so a cold worker — which has no in-process config service or
    /// per-session model binding — can resolve the run's configured model through
    /// its executor provider before the session's runtime is built. `None`/empty
    /// leaves the host default (the pre-existing behavior).
    async fn worker_for(
        &self,
        thread_id: &ThreadId,
        model_ref: Option<&str>,
    ) -> Result<Arc<DispatchWorker<S>>, Error>;
}

/// Notified the instant the pool settles a run, so a foreground submitter can await
/// its own run's completion by **event** rather than polling committed truth —
/// removing the poll-interval latency floor from the durable foreground path.
pub trait CompletionSink: Send + Sync {
    /// The pool drove `run_id` to a settled `phase` (`Ended` or `Waiting`).
    fn settled(&self, run_id: &RunId, phase: &Phase);
}

/// A running pool of drain tasks over one shared dispatch queue.
pub struct DispatchPool<S> {
    store: Arc<S>,
    wake: Arc<dyn WakeSignal>,
    shutdown: CancellationToken,
    drains: Vec<JoinHandle<()>>,
    maintenance: JoinHandle<()>,
    renewal: Option<JoinHandle<()>>,
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
        let drains = (0..concurrency.max(1))
            .map(|_| {
                tokio::spawn(drain_loop(
                    store.clone(),
                    clock.clone(),
                    wake.clone(),
                    shutdown.clone(),
                    owner.clone(),
                    lease_ms,
                    resolver.clone(),
                    config.poll_interval,
                    completion.clone(),
                ))
            })
            .collect();
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
                owner,
                lease_ms,
                interval,
            ))
        });
        Self {
            store,
            wake,
            shutdown,
            drains,
            maintenance,
            renewal,
        }
    }

    /// Durably enqueue a run and nudge the pool to pick it up.
    pub async fn submit(&self, activation: RunActivation) -> Result<(), Error> {
        self.store
            .enqueue(
                RunExecutionRequest::new(activation)
                    .with_traceparent(awaken_observability::current_traceparent()),
            )
            .await?;
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
        self.shutdown.cancel();
        let _ = self.wake.publish().await;
    }

    /// Whether a drain has been requested (the drain tasks are stopping/stopped).
    pub fn is_draining(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// Stop every task and wait for the in-flight drains to finish.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = self.wake.publish().await;
        for drain in self.drains {
            let _ = drain.await;
        }
        let _ = self.maintenance.await;
        if let Some(renewal) = self.renewal {
            let _ = renewal.await;
        }
    }
}

/// One drain task: claim a runnable dispatch, route it to its owning session's
/// worker, drive it, repeat until idle, then park on a wake or the poll timer.
#[allow(clippy::too_many_arguments)]
async fn drain_loop<S: Dispatch + 'static>(
    store: Arc<S>,
    clock: Arc<dyn Clock>,
    wake: Arc<dyn WakeSignal>,
    shutdown: CancellationToken,
    owner: String,
    lease_ms: u64,
    resolver: Arc<dyn WorkerResolver<S>>,
    poll_interval: Duration,
    completion: Option<Arc<dyn CompletionSink>>,
) {
    loop {
        if shutdown.is_cancelled() {
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
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = wake.wait() => {}
            _ = tokio::time::sleep(poll_interval) => {}
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
) -> Result<bool, Error> {
    let now = clock.now_ms();
    let Some(claimed) = store.claim(owner, lease_ms, now).await? else {
        return Ok(false);
    };
    // Route to the runtime that owns this run's thread, then drive+settle there.
    // The resolved worker shares this store and owner, so the settle it performs
    // acts on the same row this task just claimed.
    let thread_id = claimed.request.thread_id().clone();
    // The claimed run carries its own compiled model binding; hand it to the resolver
    // so a cold worker resolves THIS run's configured model (not the host default).
    let run_model_ref = claimed
        .request
        .activation
        .snapshot
        .resolved_spec
        .model_binding
        .model_ref
        .clone();
    let model_ref = Some(run_model_ref).filter(|m| !m.is_empty());
    let worker = resolver
        .worker_for(&thread_id, model_ref.as_deref())
        .await?;
    if let Some((run_id, phase)) = worker.drive_claimed(claimed, now).await?
        && let Some(sink) = completion
    {
        // Signal the foreground waiter (if any) the instant the run settles.
        sink.settled(&run_id, &phase);
    }
    Ok(true)
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
        // parked run wakeable — nudge the drain tasks so they pick it up now rather
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
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(interval) => {
                let now = clock.now_ms();
                let _ = store.renew_owned_leases(&owner, lease_ms, now).await;
            }
        }
    }
}
