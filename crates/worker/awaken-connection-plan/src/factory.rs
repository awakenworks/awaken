//! Turning a [`ConnectionPlan`] into a live channel (ADR-0045 D3).
//!
//! One [`ChannelFactory::connect`] serves the dial side; [`in_process_pair`] is
//! the degenerate zero-serialization arm; [`bind_unix`] gives the listen side
//! (a hand, or a brain in the Reverse case) a one-shot acceptor. Every consumer
//! takes an [`AgentChannel`], so the same brain/hand code runs from a laptop
//! (InProcess/Unix) to a fleet (Tcp/Nats, later slices).

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};

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
            DialAddr::Tcp(addr) => {
                // Direct topology over the network: the brain dials the hand's
                // host:port (e.g. a Kubernetes Service). Disable Nagle so a small
                // framed request is not delayed behind the ACK timer.
                let stream = TcpStream::connect(addr)
                    .await
                    .map_err(|e| ConnectError::Io(e.to_string()))?;
                let _ = stream.set_nodelay(true);
                Ok(Box::new(stream))
            }
            DialAddr::InProcess => Err(ConnectError::Unsupported(
                "in_process: obtain both ends from in_process_pair()".to_string(),
            )),
            other => Err(ConnectError::Unsupported(format!("{other:?}"))),
        }
    }
}

/// Dial a plan, retrying while the peer is not yet listening (up to `attempts`
/// tries spaced `delay` apart). Useful when a hand pod may still be starting when
/// the brain dials it — the network Direct/Reverse cases in a cluster.
pub async fn connect_with_retry(
    factory: &TokioChannelFactory,
    plan: &ConnectionPlan,
    attempts: u32,
    delay: Duration,
) -> Result<Box<dyn AgentChannel>, ConnectError> {
    let mut last = ConnectError::Io("no attempts".to_string());
    for _ in 0..attempts.max(1) {
        // Bound each attempt so a momentarily-slow DNS resolution retries instead
        // of hanging the dial indefinitely.
        match tokio::time::timeout(Duration::from_secs(2), factory.connect(plan)).await {
            Ok(Ok(ch)) => return Ok(ch),
            Ok(Err(e)) => last = e,
            Err(_) => last = ConnectError::Io("connect attempt timed out".to_string()),
        }
        tokio::time::sleep(delay).await;
    }
    Err(last)
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

/// A bound TCP listener that accepts channels — the hand's side of a Direct plan
/// (the brain dials in), or the brain's rendezvous for a Reverse plan (the hand
/// dials out). Serves repeatedly, so a long-lived hand pod accepts reconnects.
pub struct TcpHandListener {
    listener: TcpListener,
}

impl TcpHandListener {
    /// Accept one inbound connection (Nagle disabled for framed traffic).
    pub async fn accept(&self) -> Result<Box<dyn AgentChannel>, ConnectError> {
        let (stream, _addr) = self
            .listener
            .accept()
            .await
            .map_err(|e| ConnectError::Io(e.to_string()))?;
        let _ = stream.set_nodelay(true);
        Ok(Box::new(stream))
    }

    /// The bound local address (useful when the plan asked for port 0).
    pub fn local_addr(&self) -> Result<std::net::SocketAddr, ConnectError> {
        self.listener
            .local_addr()
            .map_err(|e| ConnectError::Io(e.to_string()))
    }
}

/// Bind a TCP listener for a plan whose transport is [`DialAddr::Tcp`].
pub async fn bind_tcp(plan: &ConnectionPlan) -> Result<TcpHandListener, ConnectError> {
    match &plan.transport {
        DialAddr::Tcp(addr) => {
            let listener = TcpListener::bind(addr)
                .await
                .map_err(|e| ConnectError::Io(e.to_string()))?;
            Ok(TcpHandListener { listener })
        }
        other => Err(ConnectError::Unsupported(format!(
            "bind_tcp requires a Tcp transport, got {other:?}"
        ))),
    }
}
