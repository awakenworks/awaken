//! The connection plan value object (ADR-0045 D2).
//!
//! `ConnectionPlan` names *how two ends meet*: a transport address, whether they
//! meet directly or via a broker, who initiates, and *which* credential to
//! present — a reference, never resolved material (ADR-0045 D4 / G34). It is a
//! serializable value object with no product-hosting vocabulary and no secret,
//! safe to log, persist, and carry across the config-to-host edge.

use serde::{Deserialize, Serialize};

/// Where the peer is, expressed as a transport address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialAddr {
    /// Same process: an in-memory duplex, no transport, no serialization.
    InProcess,
    /// A Unix-domain socket path (same host).
    Unix(String),
    /// A TCP `host:port` (remote). Reserved for a later slice.
    Tcp(String),
    /// An HTTP(S) URL (remote, gateway-frontable). Reserved for a later slice.
    Http(String),
    /// A NATS broker plus the subject pair the two legs meet on. Reserved for a
    /// later slice.
    Nats {
        url: String,
        inbox: String,
        outbox: String,
    },
}

/// Whether the two ends meet directly or through a broker (ADR-0045 D2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wiring {
    /// The two ends connect to each other with nothing in between.
    Direct,
    /// Both legs meet at a broker; `Relay` opaque bytes vs a terminating trust
    /// boundary is a broker-trust detail carried in `broker`.
    Relay { broker: String },
}

/// Who initiates the connection — orthogonal to which side is the requester.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialPolicy {
    /// This end dials the peer's listening address.
    Dial,
    /// This end listens; the peer dials in (NAT / no inbound — the reverse case).
    Listen,
    /// Neither end dials the other; both reach a broker.
    ViaBroker,
}

/// An opaque reference to a credential resolved *by the host* immediately before
/// dialing (ADR-0043 boundary). The plan carries the reference only; resolved
/// material never lives in, serializes with, or logs from a `ConnectionPlan`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRef(pub String);

/// One realized connection topology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionPlan {
    /// Where the peer is.
    pub transport: DialAddr,
    /// Direct or brokered.
    pub wiring: Wiring,
    /// Who initiates.
    pub dial: DialPolicy,
    /// Which credential to present, if any. A loopback (`InProcess`/`Unix`) plan
    /// usually needs none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<CredentialRef>,
}

impl ConnectionPlan {
    /// The degenerate topology: same process, zero-cost (ADR-0045 D3).
    pub fn in_process() -> Self {
        Self {
            transport: DialAddr::InProcess,
            wiring: Wiring::Direct,
            dial: DialPolicy::Dial,
            credential: None,
        }
    }

    /// Brain dials a hand listening on a Unix socket (same-host Direct).
    pub fn unix_dial(path: impl Into<String>) -> Self {
        Self {
            transport: DialAddr::Unix(path.into()),
            wiring: Wiring::Direct,
            dial: DialPolicy::Dial,
            credential: None,
        }
    }

    /// This end listens on a Unix socket; the peer dials in (the Reverse case).
    pub fn unix_listen(path: impl Into<String>) -> Self {
        Self {
            transport: DialAddr::Unix(path.into()),
            wiring: Wiring::Direct,
            dial: DialPolicy::Listen,
            credential: None,
        }
    }

    /// Brain dials a hand at a TCP `host:port` (remote Direct — e.g. a Kubernetes
    /// Service across pods).
    pub fn tcp_dial(addr: impl Into<String>) -> Self {
        Self {
            transport: DialAddr::Tcp(addr.into()),
            wiring: Wiring::Direct,
            dial: DialPolicy::Dial,
            credential: None,
        }
    }

    /// This end listens on a TCP `host:port`; the peer dials in — the hand's side
    /// of a Direct plan, or a brain rendezvous for a Reverse (NAT) plan.
    pub fn tcp_listen(addr: impl Into<String>) -> Self {
        Self {
            transport: DialAddr::Tcp(addr.into()),
            wiring: Wiring::Direct,
            dial: DialPolicy::Listen,
            credential: None,
        }
    }

    /// Attach the credential reference the host will resolve before dialing.
    #[must_use]
    pub fn with_credential(mut self, credential: CredentialRef) -> Self {
        self.credential = Some(credential);
        self
    }
}
