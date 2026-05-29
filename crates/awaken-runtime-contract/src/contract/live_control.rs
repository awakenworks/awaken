//! Ephemeral live-control commands consumed by the runtime.
//!
//! This is intentionally smaller than the server mailbox contract: runtime
//! only needs to subscribe to commands for the active run and acknowledge
//! accepted deliveries. Durable queueing, leases, retries, and dispatch state
//! live in the server mailbox contract.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::message::Message;
use super::suspension::ToolCallResume;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LiveControlError {
    #[error("live control subscribe failed: {0}")]
    Subscribe(String),
}

// ── LiveRunCommand ─────────────────────────────────────────────────────────

/// Control command delivered to an active run's owning node (out-of-band
/// relative to durable dispatch). Consumed by the runtime forwarder attached
/// to each `RunHandle`; unsubscribed targets silently drop commands (best
/// effort — steering is ephemeral by design).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LiveRunCommand {
    /// Inject messages into the running agent's next step boundary inbox.
    Messages(Vec<Message>),
    /// Wake the owner run to consume already-staged pending messages.
    PendingBoundaryWake,
    /// Cooperatively cancel the run (`immediate` cancellation token).
    Cancel,
    /// Deliver tool-call resume decisions to the run.
    Decision(Vec<(String, ToolCallResume)>),
}

/// Exact live-run target for cross-node ephemeral control.
///
/// Thread-only routing is intentionally insufficient for distributed
/// backends: a stale subscriber for the same thread must not be able to ack a
/// command intended for a newer run/dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveRunTarget {
    pub thread_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_id: Option<String>,
}

impl LiveRunTarget {
    #[must_use]
    pub fn new(thread_id: impl Into<String>, run_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            dispatch_id: None,
        }
    }

    #[must_use]
    pub fn with_dispatch_id(mut self, dispatch_id: impl Into<String>) -> Self {
        self.dispatch_id = Some(dispatch_id.into());
        self
    }
}

/// Completion receipt for a delivered `LiveRunCommand`. Consumers call
/// [`LiveCommandReceipt::ack`] **only after the run has actually accepted
/// the command** (e.g. the inbox channel returned success). A dropped
/// receipt signals the producer that delivery did not complete, and the
/// producer's `deliver_live` resolves as
/// [`LiveDeliveryOutcome::NoSubscriber`] so the caller can fall back to
/// durable dispatch. Producers MUST NOT observe `Delivered` until the
/// receipt has been acknowledged.
pub trait LiveCommandReceipt: Send + Sync {
    /// Confirm the command was handed to the live consumer. Consumes the
    /// receipt; dropping the handle without calling `ack` is treated as
    /// non-delivery by the producer.
    fn ack(self: Box<Self>);
}

/// Entry yielded by a [`LiveRunCommandStream`]: the command plus the
/// receipt the consumer must `ack` once the run has received it.
pub struct LiveRunCommandEntry {
    pub command: LiveRunCommand,
    pub receipt: Box<dyn LiveCommandReceipt>,
}

impl std::fmt::Debug for LiveRunCommandEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveRunCommandEntry")
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
}

/// Stream of [`LiveRunCommandEntry`] consumed by the owning node's runtime
/// forwarder.
pub type LiveRunCommandStream = Pin<Box<dyn Stream<Item = LiveRunCommandEntry> + Send>>;

/// Outcome of a live-control delivery call — lets the caller decide
/// whether to fall back to the durable queue. `NoSubscriber` means *no node
/// acknowledged the command*; the command was either lost in transit or
/// the run failed to accept it. Callers must treat `NoSubscriber` as
/// "did not deliver" and fall back to durable dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveDeliveryOutcome {
    /// The owning run accepted the command (forwarder acked after handing
    /// the command to the in-process channel).
    Delivered,
    /// No subscriber, or the subscriber failed to accept within the
    /// producer's timeout. Caller should fall back to durable dispatch.
    NoSubscriber,
}

#[async_trait]
pub trait LiveRunCommandSource: Send + Sync {
    async fn open_live_channel_for(
        &self,
        target: &LiveRunTarget,
    ) -> Result<LiveRunCommandStream, LiveControlError>;
}
