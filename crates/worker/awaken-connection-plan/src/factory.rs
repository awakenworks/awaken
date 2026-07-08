//! Turning a [`ConnectionPlan`] into a live channel (ADR-0045 D3).
//!
//! One [`ChannelFactory::connect`] serves the dial side; [`in_process_pair`] is
//! the degenerate zero-serialization arm; [`bind_unix`] gives the listen side
//! (a hand, or a brain in the Reverse case) a one-shot acceptor. Every consumer
//! takes an [`AgentChannel`], so the same brain/hand code runs from a laptop
//! (InProcess/Unix) to a fleet (Tcp/Nats, later slices).

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use tokio::net::{UnixListener, UnixStream};

use crate::plan::{ConnectionPlan, DialAddr, DialPolicy};

/// Why a channel could not be established for a plan.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("transport is not supported in this slice: {0}")]
    Unsupported(String),
    #[error("connection plan dial-policy mismatch: {0}")]
    Policy(String),
    #[error("connection I/O failed: {0}")]
    Io(String),
}

/// Establishes the dialing end of a plan.
#[async_trait]
pub trait ChannelFactory: Send + Sync {
    /// Dial the plan's peer and return the established channel. The plan's `dial`
    /// must be [`DialPolicy::Dial`]; a `Listen` plan is served by [`bind_unix`].
    async fn connect(&self, plan: &ConnectionPlan) -> Result<Box<dyn AgentChannel>, ConnectError>;
}

/// The default factory over tokio transports (InProcess handled out-of-band by
/// [`in_process_pair`]; Unix here; Tcp/Nats in later slices).
pub struct TokioChannelFactory;

#[async_trait]
impl ChannelFactory for TokioChannelFactory {
    async fn connect(&self, plan: &ConnectionPlan) -> Result<Box<dyn AgentChannel>, ConnectError> {
        if plan.dial != DialPolicy::Dial {
            return Err(ConnectError::Policy(format!(
                "connect requires DialPolicy::Dial, got {:?}",
                plan.dial
            )));
        }
        match &plan.transport {
            DialAddr::Unix(path) => {
                let stream = UnixStream::connect(path)
                    .await
                    .map_err(|e| ConnectError::Io(e.to_string()))?;
                Ok(Box::new(stream))
            }
            DialAddr::InProcess => Err(ConnectError::Unsupported(
                "in_process: obtain both ends from in_process_pair()".to_string(),
            )),
            other => Err(ConnectError::Unsupported(format!("{other:?}"))),
        }
    }
}

/// The degenerate topology (ADR-0045 D3): both ends of an in-memory duplex, no
/// transport and no serialization. Returns `(brain_end, hand_end)`.
pub fn in_process_pair() -> (Box<dyn AgentChannel>, Box<dyn AgentChannel>) {
    let (brain, hand) = tokio::io::duplex(64 * 1024);
    (Box::new(brain), Box::new(hand))
}

/// A bound Unix listener that accepts one channel — the hand's side of a Direct
/// plan, or the brain's side of a Reverse plan.
pub struct UnixHandListener {
    listener: UnixListener,
}

impl UnixHandListener {
    /// Accept one inbound connection.
    pub async fn accept(&self) -> Result<Box<dyn AgentChannel>, ConnectError> {
        let (stream, _addr) = self
            .listener
            .accept()
            .await
            .map_err(|e| ConnectError::Io(e.to_string()))?;
        Ok(Box::new(stream))
    }
}

/// Bind a Unix listener for a plan whose transport is [`DialAddr::Unix`]. A stale
/// socket file at the path is removed best-effort first.
pub fn bind_unix(plan: &ConnectionPlan) -> Result<UnixHandListener, ConnectError> {
    match &plan.transport {
        DialAddr::Unix(path) => {
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path).map_err(|e| ConnectError::Io(e.to_string()))?;
            Ok(UnixHandListener { listener })
        }
        other => Err(ConnectError::Unsupported(format!(
            "bind_unix requires a Unix transport, got {other:?}"
        ))),
    }
}
