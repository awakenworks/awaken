//! The autonomous dispatch daemon.
//!
//! [`DispatchService`] turns the [`DispatchWorker`] into a long-running service:
//! one background task drains the queue, woken by a nudge when new work arrives
//! and by a periodic timer otherwise (so a crashed lease is recovered without new
//! work). It is the only part of the crate that reads a real [`Clock`], keeping
//! the worker deterministic. It waits on a [`WakeSignal`] — a `LocalWakeSignal`
//! for one process, or a cross-node signal for a fleet (ADR-0011, ADR-0019).

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use awaken_runtime_contract::activation::RunActivation;

use crate::Error;
use crate::clock::Clock;
use crate::dispatch::{Dispatch, PendingInput};
use crate::request::RunExecutionRequest;
use crate::wake::{LocalWakeSignal, WakeSignal};
use crate::worker::DispatchWorker;

/// How the daemon paces itself.
#[derive(Debug, Clone, Copy)]
pub struct DispatchServiceConfig {
    /// Fallback drain cadence when no nudge arrives. Also the maximum delay
    /// before an expired lease is recovered.
    pub poll_interval: Duration,
    /// Crash-retry budget: a dispatch reclaimed this many times without a settle
    /// is dead-lettered instead of run again (ADR-0015).
    pub max_attempts: u64,
    /// If set, dead-letters older than this are GC'd on the poll cadence; `None`
    /// keeps them until an operator purges them (ADR-0023).
    pub dead_letter_ttl: Option<Duration>,
    /// If set, a heartbeat renews this daemon's in-flight leases on this cadence so
    /// a long run is not reclaimed while still executing; `None` disables renewal
    /// (a single in-process daemon needs none). Use well under the lease (ADR-0024).
    pub lease_renewal_interval: Option<Duration>,
}

impl Default for DispatchServiceConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(50),
            max_attempts: 5,
            dead_letter_ttl: None,
            lease_renewal_interval: None,
        }
    }
}

/// A running daemon draining one dispatch queue against a runtime.
pub struct DispatchService<S> {
    worker: Arc<DispatchWorker<S>>,
    wake: Arc<dyn WakeSignal>,
    shutdown: CancellationToken,
    handle: JoinHandle<()>,
    renewal: Option<JoinHandle<()>>,
}

impl<S: Dispatch + 'static> DispatchService<S> {
    /// Start the daemon with the single-process wake signal.
    pub fn spawn(
        worker: Arc<DispatchWorker<S>>,
        clock: Arc<dyn Clock>,
        config: DispatchServiceConfig,
    ) -> Self {
        Self::spawn_with_wake(worker, clock, config, Arc::new(LocalWakeSignal::new()))
    }

    /// Start the daemon with a chosen [`WakeSignal`] — a `LocalWakeSignal` for one
    /// process, or a cross-node signal (e.g. NATS) so a fleet need not busy-poll.
    pub fn spawn_with_wake(
        worker: Arc<DispatchWorker<S>>,
        clock: Arc<dyn Clock>,
        config: DispatchServiceConfig,
        wake: Arc<dyn WakeSignal>,
    ) -> Self {
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(run_loop(
            worker.clone(),
            clock.clone(),
            wake.clone(),
            shutdown.clone(),
            config,
        ));
        // A separate heartbeat renews in-flight leases concurrently with the drain,
        // since the drain task is busy executing a long run (ADR-0024).
        let renewal = config.lease_renewal_interval.map(|interval| {
            tokio::spawn(renewal_loop(
                worker.clone(),
                clock,
                shutdown.clone(),
                interval,
            ))
        });
        Self {
            worker,
            wake,
            shutdown,
            handle,
            renewal,
        }
    }

    /// Durably enqueue a run and nudge the daemon to pick it up.
    pub async fn submit(&self, activation: RunActivation) -> Result<(), Error> {
        self.worker
            .store()
            .enqueue(
                RunExecutionRequest::new(activation)
                    .with_traceparent(awaken_observability::current_traceparent()),
            )
            .await?;
        let _ = self.wake.publish().await;
        Ok(())
    }

    /// Durably deliver pending input and nudge the daemon to resume the run.
    pub async fn deliver(&self, input: PendingInput) -> Result<(), Error> {
        self.worker.store().append(input).await?;
        let _ = self.wake.publish().await;
        Ok(())
    }

    /// Stage a cross-thread delivery and nudge the daemon to relay it (M3b).
    pub async fn send(&self, input: PendingInput) -> Result<(), Error> {
        self.worker.store().stage(input).await?;
        let _ = self.wake.publish().await;
        Ok(())
    }

    /// Wake the daemon to drain immediately.
    pub async fn notify(&self) {
        let _ = self.wake.publish().await;
    }

    /// Stop the daemon and wait for the in-flight drain to finish.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = self.wake.publish().await;
        let _ = self.handle.await;
        if let Some(renewal) = self.renewal {
            let _ = renewal.await;
        }
    }
}

/// Renew this daemon's in-flight leases on a cadence, so a long-running run is not
/// reclaimed by another node's recovery while it is still executing (ADR-0024).
async fn renewal_loop<S: Dispatch + 'static>(
    worker: Arc<DispatchWorker<S>>,
    clock: Arc<dyn Clock>,
    shutdown: CancellationToken,
    interval: Duration,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(interval) => {
                let now = clock.now_ms();
                let _ = worker
                    .store()
                    .renew_owned_leases(worker.owner(), worker.lease_ms(), now)
                    .await;
            }
        }
    }
}

async fn run_loop<S: Dispatch + 'static>(
    worker: Arc<DispatchWorker<S>>,
    clock: Arc<dyn Clock>,
    wake: Arc<dyn WakeSignal>,
    shutdown: CancellationToken,
    config: DispatchServiceConfig,
) {
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        // Dead-letter poison runs that have exhausted their crash-retry budget,
        // GC aged dead-letters if a ttl is set, relay staged cross-thread
        // deliveries, then drain everything runnable now. A store error is
        // transient: the next tick retries, so swallow it rather than kill the
        // daemon.
        let now = clock.now_ms();
        let _ = worker.store().reap(config.max_attempts, now).await;
        if let Some(ttl) = config.dead_letter_ttl {
            let cutoff = now.saturating_sub(ttl.as_millis() as u64);
            let _ = worker.store().purge_dead_letters_before(cutoff).await;
        }
        let _ = worker.store().relay().await;
        let _ = worker.run_until_idle(now).await;
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = wake.wait() => {}
            _ = tokio::time::sleep(config.poll_interval) => {}
        }
    }
}
