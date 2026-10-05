// @formal entrypoint ingress.epoch_reclaim
// @formal entrypoint remote_worker.maintenance_scheduler_respects_adapter_ownership
//! The autonomous dispatch daemon.
//!
//! [`DispatchService`] turns the [`DispatchWorker`] into a long-running service:
//! one background task drains the queue, woken by a nudge when new work arrives
//! and by a periodic timer otherwise (so a crashed lease is recovered without new
//! work). It owns the edge [`WallClock`](awaken_time::WallClock) and passes that same source into every
//! Worker drive, keeping claim, renewal, and ownership checks on one timeline.
//! It waits on a [`WakeSignal`] — a `LocalWakeSignal`
//! for one process, or a cross-node signal for a fleet (ADR-0011, ADR-0019).

use awaken_dispatch_contract::ControlDispatch;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use awaken_runtime_contract::activation::RunActivation;

use crate::Error;
use crate::dispatch::PendingInput;
use crate::wake::{LocalWakeSignal, WakeSignal, publish_dispatch_hint};
use crate::worker::DispatchWorker;
use awaken_dispatch_contract::RunDispatch;
use awaken_time::WallClock;
use awaken_time::WallClockExt;

/// One process-level observation cadence, independent of execution throughput.
/// An exact count can inspect a large queue, so keep it off every claim and at
/// roughly the default metrics-export cadence. Operations callers may still
/// request an exact `runnable_depth` observation explicitly.
pub(crate) const QUEUE_DEPTH_SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

pub(crate) fn spawn_dispatch_depth_sampler<S: ControlDispatch + 'static>(
    store: Arc<S>,
    report: Arc<dyn Fn(u64) + Send + Sync>,
    shutdown: CancellationToken,
    enabled: bool,
) -> Option<JoinHandle<()>> {
    // Database-less Workers do not poll upstream operations. The Coordinator
    // samples its own queue; no second remote maintenance loop is installed.
    if !enabled || !store.owns_local_maintenance() {
        return None;
    }
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(QUEUE_DEPTH_SAMPLE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    // Even a very large exact count cannot delay claim, outbox
                    // relay, or terminal reconciliation. Cancellation interrupts
                    // an in-flight metrics read without mutating dispatch truth.
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => break,
                        result = store.runnable_depth() => {
                            if let Ok(Some(depth)) = result {
                                report(depth);
                            }
                        }
                    }
                }
            }
        }
    }))
}

/// How the daemon paces itself.
#[derive(Debug, Clone, Copy)]
pub struct DispatchServiceConfig {
    /// Fallback drain cadence when no nudge arrives. Also the maximum delay
    /// before an expired lease is recovered.
    pub poll_interval: Duration,
    /// Crash-retry budget: after this many recovery claims, the next expired
    /// claim commits `Ended(Indeterminate)` instead of executing (ADR-0015).
    pub max_attempts: u64,
    /// If set, aged manual quarantines are GC'd on the poll cadence; `None`
    /// keeps them until an operator purges them (ADR-0023).
    pub dead_letter_ttl: Option<Duration>,
    /// Cadence for reconciling a quiescent Awaiting dispatch or expired Running
    /// lease against committed terminal Run truth. This repairs commit/settle
    /// and reconciliation-claim crash gaps without polling every idle queue tick;
    /// `None` disables it.
    pub terminal_reconciliation_interval: Option<Duration>,
    /// Disable this process's optional depth metric when a hosting adapter
    /// already samples the same authoritative queue for its scaler.
    pub sample_dispatch_queue_depth: bool,
}

impl Default for DispatchServiceConfig {
    fn default() -> Self {
        Self {
            // Cross-node wake drives prompt work; fallback reads need not hit
            // the durable queue twenty times per second while it is idle.
            poll_interval: Duration::from_millis(500),
            max_attempts: 5,
            dead_letter_ttl: None,
            terminal_reconciliation_interval: Some(Duration::from_secs(30)),
            sample_dispatch_queue_depth: true,
        }
    }
}

/// The one immutable schedule for adapter-owned maintenance. `None` means the
/// upstream Coordinator owns all three operations; ordinary claim execution and
/// retry exhaustion are intentionally outside this capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalMaintenanceSchedule {
    purge_dead_letters: bool,
    reconcile_terminals: bool,
}

impl LocalMaintenanceSchedule {
    pub(crate) const fn for_adapter(
        owns_local_maintenance: bool,
        dead_letter_gc_configured: bool,
        terminal_reconciliation_configured: bool,
    ) -> Option<Self> {
        if owns_local_maintenance {
            Some(Self {
                purge_dead_letters: dead_letter_gc_configured,
                reconcile_terminals: terminal_reconciliation_configured,
            })
        } else {
            None
        }
    }

    pub(crate) const fn purges_dead_letters(self) -> bool {
        self.purge_dead_letters
    }

    pub(crate) const fn reconciles_terminals(self) -> bool {
        self.reconcile_terminals
    }
}

pub(crate) async fn wait_for_terminal_reconciliation(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(kani)]
/// @obligation remote_worker.maintenance_scheduler_respects_adapter_ownership
/// @kani default-features=false
#[kani::proof]
fn maintenance_schedule_exists_only_for_the_owning_adapter() {
    // Causes: C1 adapter owns local maintenance; C2 dead-letter GC configured;
    // C3 terminal reconciliation configured. Effects: E1 !C1 yields no local
    // schedule or timer; E2 C1 always relays; E3/E4 exactly mirror C2/C3.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // |---|---|---|---|---|
    // | R1 | no | any | any | E1 no schedule |
    // | R2 | yes | no | no | E2 relay only |
    // | R3 | yes | yes | no | E2 + E3 |
    // | R4 | yes | no | yes | E2 + E4 |
    // | R5 | yes | yes | yes | E2 + E3 + E4 |
    let owns: bool = kani::any();
    let gc: bool = kani::any();
    let scan: bool = kani::any();
    let schedule = LocalMaintenanceSchedule::for_adapter(owns, gc, scan);
    assert_eq!(schedule.is_some(), owns);
    if let Some(schedule) = schedule {
        assert_eq!(schedule.purges_dead_letters(), gc);
        assert_eq!(schedule.reconciles_terminals(), scan);
    }
}

/// A running daemon draining one dispatch queue against a runtime.
pub struct DispatchService<S> {
    worker: Arc<DispatchWorker<S>>,
    wake: Arc<dyn WakeSignal>,
    shutdown: CancellationToken,
    handle: JoinHandle<()>,
    depth_sampler: Option<JoinHandle<()>>,
}

impl<S: ControlDispatch + 'static> DispatchService<S> {
    /// Start the daemon with the single-process wake signal.
    pub fn spawn(
        worker: Arc<DispatchWorker<S>>,
        clock: Arc<dyn WallClock>,
        config: DispatchServiceConfig,
    ) -> Self {
        Self::spawn_with_wake(worker, clock, config, Arc::new(LocalWakeSignal::new()))
    }

    /// Start the daemon with a chosen [`WakeSignal`] — a `LocalWakeSignal` for one
    /// process, or a cross-node signal (e.g. NATS) so a fleet need not busy-poll.
    pub fn spawn_with_wake(
        worker: Arc<DispatchWorker<S>>,
        clock: Arc<dyn WallClock>,
        config: DispatchServiceConfig,
        wake: Arc<dyn WakeSignal>,
    ) -> Self {
        let shutdown = CancellationToken::new();
        // The standalone service drives Runs on its main loop. Counting a large
        // queue on that loop would reintroduce a claim-latency spike, so only
        // the non-authoritative sampler gets a separate owned task.
        let observer_worker = worker.clone();
        let report = Arc::new(move |depth| {
            observer_worker
                .runtime()
                .metrics()
                .record_dispatch_queue_depth(depth);
        });
        let depth_sampler = spawn_dispatch_depth_sampler(
            worker.store().clone(),
            report,
            shutdown.clone(),
            config.sample_dispatch_queue_depth,
        );
        let handle = tokio::spawn(run_loop(
            worker.clone(),
            clock.clone(),
            wake.clone(),
            shutdown.clone(),
            config,
        ));
        Self {
            worker,
            wake,
            shutdown,
            handle,
            depth_sampler,
        }
    }

    /// Durably enqueue a run and nudge the daemon to pick it up.
    pub async fn submit(&self, activation: RunActivation) -> Result<(), Error> {
        self.worker
            .store()
            .enqueue(
                RunDispatch::new(activation)
                    .with_traceparent(awaken_observability::current_traceparent()),
            )
            .await?;
        publish_dispatch_hint(self.wake.as_ref(), &self.shutdown).await;
        Ok(())
    }

    /// Durably deliver pending input and nudge the daemon to resume the run.
    pub async fn deliver(&self, input: PendingInput) -> Result<(), Error> {
        self.worker.store().append(input).await?;
        publish_dispatch_hint(self.wake.as_ref(), &self.shutdown).await;
        Ok(())
    }

    /// Stage a cross-thread delivery and nudge the daemon to relay it (M3b).
    pub async fn send(&self, input: PendingInput) -> Result<(), Error> {
        self.worker.store().stage(input).await?;
        publish_dispatch_hint(self.wake.as_ref(), &self.shutdown).await;
        Ok(())
    }

    /// Wake the daemon to drain immediately.
    pub async fn notify(&self) {
        publish_dispatch_hint(self.wake.as_ref(), &self.shutdown).await;
    }

    /// Stop the daemon and wait for the in-flight drain to finish.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = self.handle.await;
        if let Some(sampler) = self.depth_sampler {
            let _ = sampler.await;
        }
    }
}

async fn run_loop<S: ControlDispatch + 'static>(
    worker: Arc<DispatchWorker<S>>,
    clock: Arc<dyn WallClock>,
    wake: Arc<dyn WakeSignal>,
    shutdown: CancellationToken,
    config: DispatchServiceConfig,
) {
    let maintenance = LocalMaintenanceSchedule::for_adapter(
        worker.store().owns_local_maintenance(),
        config.dead_letter_ttl.is_some(),
        config.terminal_reconciliation_interval.is_some(),
    );
    let mut next_terminal_reconciliation = maintenance
        .filter(|schedule| schedule.reconciles_terminals())
        .map(|_| awaken_time::mono::now());
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        // Resolve poison runs through committed terminal truth, GC aged manual
        // quarantines if a ttl is set, relay staged cross-thread deliveries,
        // then drain a bounded batch. A store/commit error is transient:
        // the next tick retries, so keep the daemon alive.
        let now = clock.now_ms();
        // Bound scheduling work, not an in-flight authoritative attempt. Never
        // collect every result while arrivals can keep the queue nonempty.
        const BATCH_LIMIT: usize = 64;
        let mut exhausted = Ok(());
        let mut exhaustion_batch_full = true;
        for _ in 0..BATCH_LIMIT {
            if shutdown.is_cancelled() {
                return;
            }
            match worker
                .resolve_one_retry_exhausted(config.max_attempts)
                .await
            {
                Ok(true) => continue,
                Ok(false) => {
                    exhaustion_batch_full = false;
                    break;
                }
                Err(error) => {
                    exhaustion_batch_full = false;
                    exhausted = Err(error);
                    break;
                }
            }
        }
        if let Err(error) = &exhausted {
            tracing::warn!(%error, "retry-exhaustion terminal resolution failed; retrying");
        }
        if shutdown.is_cancelled() {
            break;
        }
        if let Some(maintenance) = maintenance {
            if maintenance.purges_dead_letters() {
                let ttl = config
                    .dead_letter_ttl
                    .expect("the maintenance schedule retained its GC interval");
                let cutoff = now.saturating_sub(ttl.as_millis() as u64);
                if let Err(error) = worker.store().purge_dead_letters_before(cutoff).await {
                    tracing::warn!(%error, "dispatch dead-letter purge failed; retrying");
                }
            }
            if let Err(error) = worker.store().relay().await {
                tracing::warn!(%error, "dispatch relay failed; retrying");
            }
            if let Some(deadline) = next_terminal_reconciliation
                && awaken_time::mono::now() >= deadline
            {
                if let Err(error) = worker.reconcile_committed_terminals(256).await {
                    tracing::warn!(%error, "terminal dispatch reconciliation failed; retrying");
                }
                let interval = config
                    .terminal_reconciliation_interval
                    .expect("the maintenance schedule retained its scan interval");
                next_terminal_reconciliation = Some(awaken_time::mono::now() + interval);
            }
        }
        // Do not enter ordinary execution after a failed terminal commit. The
        // exhausted claim remains leased and is retried through the same command
        // after expiry; running an ordinary drain here could reopen another
        // exhausted row before terminal resolution converges.
        let mut drain_batch_full = false;
        if exhausted.is_ok() && !exhaustion_batch_full {
            drain_batch_full = true;
            for _ in 0..BATCH_LIMIT {
                if shutdown.is_cancelled() {
                    return;
                }
                match worker.tick(clock.clone()).await {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        drain_batch_full = false;
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "dispatch drain failed; retrying");
                        drain_batch_full = false;
                        break;
                    }
                }
            }
        }
        if exhaustion_batch_full || drain_batch_full {
            tokio::task::yield_now().await;
            continue;
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = wake.wait() => {}
            _ = wait_for_terminal_reconciliation(next_terminal_reconciliation) => {}
            _ = tokio::time::sleep(config.poll_interval) => {}
        }
    }
}

#[cfg(test)]
mod maintenance_schedule_tests {
    use super::*;

    #[test]
    fn adapter_ownership_selects_the_complete_maintenance_schedule() {
        // Causes: C1 local/upstream ownership; C2 optional GC; C3 optional scan.
        // Effects: E1 upstream has no local work/timer; E2 local always relays;
        // E3/E4 GC and scan exactly follow configuration. R1 and the four local
        // C2/C3 combinations exhaust the schedule used by Service and Pool.
        //
        // | Rule | local | GC | scan | expected |
        // |---|---|---|---|---|
        // | R1 | no | any | any | no schedule |
        // | R2 | yes | no | no | relay |
        // | R3 | yes | yes | no | relay + GC |
        // | R4 | yes | no | yes | relay + scan |
        // | R5 | yes | yes | yes | relay + GC + scan |
        for gc in [false, true] {
            for scan in [false, true] {
                assert!(
                    LocalMaintenanceSchedule::for_adapter(false, gc, scan).is_none(),
                    "R1/E1"
                );
                let schedule = LocalMaintenanceSchedule::for_adapter(true, gc, scan)
                    .expect("R2-R5 local schedule");
                assert_eq!(schedule.purges_dead_letters(), gc, "R3-R5/E3");
                assert_eq!(schedule.reconciles_terminals(), scan, "R4-R5/E4");
            }
        }
    }
}
