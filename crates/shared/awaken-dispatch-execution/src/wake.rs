// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Static owner: the existing WakeSignal adapters carry non-authoritative latency hints;
// @formal reason: DispatchQueue remains the durable claim and settlement owner. Dynamic path: publication
// @formal reason: is bounded by cancellation and a 100 ms timeout, one PostgreSQL listener reconnects and
// @formal reason: stops with its owner, and durable polling recovers lost hints. This product-requirement
// @formal reason: classification covers notification concurrency and shutdown; it does not claim a proof
// @formal reason: of Tokio scheduling, transport delivery or database behavior.
//! The wake-signal seam for a distributed deployment.
//!
//! A `WakeSignal` is a *non-authoritative* push channel: it nudges an idle daemon
//! to drain when work arrives on another node, so a multi-node fleet need not
//! busy-poll. Wake records are hints (run-ingress design, "Distributed Placement
//! Rules"): losing or duplicating one only delays or repeats a drain — durable
//! pending input, committed facts, leases, and outboxes are the recovery truth,
//! so correctness never depends on a signal arriving.
//!
//! [`LocalWakeSignal`] is the single-process implementation (a `tokio` notify);
//! [`NatsWakeSignal`] (feature `nats`) fans the hint across nodes over a NATS
//! subject. The dispatch store stays the durable authority either way.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::dispatch::DispatchError;

/// A best-effort cross-node wake hint.
#[async_trait]
pub trait WakeSignal: Send + Sync {
    /// Publish a wake hint. Best-effort: a failure or a lost hint is tolerable.
    async fn publish(&self) -> Result<(), DispatchError>;

    /// Wait for the next wake hint.
    async fn wait(&self);
}

/// Notifications are latency hints, never durable admission or stop evidence.
/// One budget at the calling boundary prevents an unavailable transport from
/// blocking an already-committed command or the next reconciliation pass.
pub async fn publish_dispatch_hint(wake: &dyn WakeSignal, shutdown: &CancellationToken) {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => {}
        result = tokio::time::timeout(std::time::Duration::from_millis(100), wake.publish()) => {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "dispatch wake publication failed; polling will recover"),
                Err(_) => tracing::warn!("dispatch wake publication timed out; polling will recover"),
            }
        }
    }
}

/// The latency hint a command publisher gives the local dispatch claimer after
/// it changed durable dispatch state. It carries the same wake signal the
/// claimer waits on, selected once by the composition before the claimer
/// exists, so a publisher never needs the claimer itself. Like every wake, it
/// is never admission or settlement evidence.
#[derive(Clone)]
pub struct DispatchHint {
    wake: Arc<dyn WakeSignal>,
}

impl DispatchHint {
    #[must_use]
    pub fn new(wake: Arc<dyn WakeSignal>) -> Self {
        Self { wake }
    }

    /// The wake signal the local claimer is spawned with.
    #[must_use]
    pub fn wake(&self) -> Arc<dyn WakeSignal> {
        self.wake.clone()
    }

    /// Publish one hint within the shared publication budget.
    pub async fn publish(&self) {
        publish_dispatch_hint(self.wake.as_ref(), &CancellationToken::new()).await;
    }
}

/// Single-process wake signal over a shared `tokio` notify. A hint published when
/// no one is awaiting is held for the next `wait` (one permit), so an in-process
/// nudge is never lost.
#[derive(Clone, Default)]
pub struct LocalWakeSignal {
    notify: Arc<Notify>,
}

impl LocalWakeSignal {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl WakeSignal for LocalWakeSignal {
    async fn publish(&self) -> Result<(), DispatchError> {
        self.notify.notify_one();
        Ok(())
    }

    async fn wait(&self) {
        self.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn publication_budget_covers_failure_hang_and_pre_cancel_without_retry() {
        // C1 transport success/error/pending; C2 caller cancelled before send.
        // R1 !C2 => exactly one send, success/error return immediately, pending
        // returns at 100ms. R2 C2 => zero sends. All paths preserve caller-owned
        // durable state by accepting no storage/settlement capability here.
        struct Probe {
            mode: usize,
            calls: AtomicUsize,
        }
        #[async_trait]
        impl WakeSignal for Probe {
            async fn publish(&self) -> Result<(), DispatchError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                match self.mode {
                    0 => Ok(()),
                    1 => Err(DispatchError::Rejected("offline".into())),
                    _ => std::future::pending().await,
                }
            }
            async fn wait(&self) {
                std::future::pending().await
            }
        }
        for cancelled in [false, true] {
            for mode in 0..3 {
                let probe = Probe {
                    mode,
                    calls: AtomicUsize::new(0),
                };
                let shutdown = CancellationToken::new();
                if cancelled {
                    shutdown.cancel();
                }
                let start = tokio::time::Instant::now();
                publish_dispatch_hint(&probe, &shutdown).await;
                assert_eq!(probe.calls.load(Ordering::SeqCst), usize::from(!cancelled));
                let expected = if !cancelled && mode == 2 { 100 } else { 0 };
                assert_eq!(start.elapsed(), std::time::Duration::from_millis(expected));
            }
        }
    }
}
