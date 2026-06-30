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

use crate::dispatch::DispatchError;

/// A best-effort cross-node wake hint.
#[async_trait]
pub trait WakeSignal: Send + Sync {
    /// Publish a wake hint. Best-effort: a failure or a lost hint is tolerable.
    async fn publish(&self) -> Result<(), DispatchError>;

    /// Wait for the next wake hint.
    async fn wait(&self);
}

/// Single-process wake signal over a shared `tokio` notify. A hint published when
/// no one is waiting is held for the next `wait` (one permit), so an in-process
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

/// NATS-backed wake signal: publishes and awaits an empty message on a subject,
/// fanning the hint across a node fleet. Core NATS (not JetStream) — these are
/// fire-and-forget hints, exactly the at-most-once semantics a hint allows.
#[cfg(feature = "nats")]
pub struct NatsWakeSignal {
    client: async_nats::Client,
    subject: String,
}

#[cfg(feature = "nats")]
impl NatsWakeSignal {
    /// Connect to a NATS server and publish/await hints on `subject`.
    pub async fn connect(url: &str, subject: impl Into<String>) -> Result<Self, DispatchError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|err| DispatchError::Rejected(err.to_string()))?;
        Ok(Self {
            client,
            subject: subject.into(),
        })
    }
}

#[cfg(feature = "nats")]
#[async_trait]
impl WakeSignal for NatsWakeSignal {
    async fn publish(&self) -> Result<(), DispatchError> {
        self.client
            .publish(self.subject.clone(), Vec::new().into())
            .await
            .map_err(|err| DispatchError::Rejected(err.to_string()))
    }

    async fn wait(&self) {
        use futures_lite::StreamExt;
        // A lost subscription is tolerable (the poll fallback still drains).
        if let Ok(mut sub) = self.client.subscribe(self.subject.clone()).await {
            let _ = sub.next().await;
        }
    }
}
