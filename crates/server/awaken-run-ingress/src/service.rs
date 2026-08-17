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
use crate::wake::{LocalWakeSignal, WakeSignal};
use crate::worker::DispatchWorker;
use awaken_run_ingress_contract::RunDispatch;

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
}

impl Default for DispatchServiceConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(50),
            max_attempts: 5,
            dead_letter_ttl: None,
            terminal_reconciliation_interval: Some(Duration::from_secs(30)),
        }
    }
}

/// A running daemon draining one dispatch queue against a runtime.
pub struct DispatchService<S> {
    worker: Arc<DispatchWorker<S>>,
    wake: Arc<dyn WakeSignal>,
    shutdown: CancellationToken,
    handle: JoinHandle<()>,
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
        Self {
            worker,
            wake,
            shutdown,
            handle,
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
    }
}

async fn run_loop<S: Dispatch + 'static>(
    worker: Arc<DispatchWorker<S>>,
    clock: Arc<dyn Clock>,
    wake: Arc<dyn WakeSignal>,
    shutdown: CancellationToken,
    config: DispatchServiceConfig,
) {
    let mut next_terminal_reconciliation = tokio::time::Instant::now();
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        // Resolve poison runs through committed terminal truth, GC aged manual
        // quarantines if a ttl is set, relay staged cross-thread deliveries,
        // then drain everything runnable now. A store/commit error is transient:
        // the next tick retries, so keep the daemon alive.
        let now = clock.now_ms();
        let exhausted = loop {
            match worker
                .resolve_one_retry_exhausted(config.max_attempts, now)
                .await
            {
                Ok(true) => continue,
                Ok(false) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        if let Err(error) = &exhausted {
            tracing::warn!(%error, "retry-exhaustion terminal resolution failed; retrying");
        }
        if let Some(ttl) = config.dead_letter_ttl {
            let cutoff = now.saturating_sub(ttl.as_millis() as u64);
            let _ = worker.store().purge_dead_letters_before(cutoff).await;
        }
        let _ = worker.store().relay().await;
        if let Some(interval) = config.terminal_reconciliation_interval
            && tokio::time::Instant::now() >= next_terminal_reconciliation
        {
            if let Err(error) = worker.reconcile_committed_terminals(now, 256).await {
                tracing::warn!(%error, "terminal dispatch reconciliation failed; retrying");
            }
            next_terminal_reconciliation = tokio::time::Instant::now() + interval;
        }
        // Do not enter ordinary execution after a failed terminal commit. The
        // exhausted claim remains leased and is retried through the same command
        // after expiry; running an ordinary drain here could reopen another
        // exhausted row before terminal resolution converges.
        if exhausted.is_ok() {
            let _ = worker.run_until_idle(now).await;
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = wake.wait() => {}
            _ = tokio::time::sleep(config.poll_interval) => {}
        }
    }
}
