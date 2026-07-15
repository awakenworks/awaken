//! Network topology as a value object (ADR-0045).
//!
//! The policy layer the connection *mechanism* deliberately excludes: a
//! [`ConnectionPlan`] names how two ends meet (a transport address, who dials, and
//! a credential *reference*), and a [`ChannelFactory`] turns a plan into a live
//! [`AgentChannel`]. The topologies — InProcess, Direct, and Reverse (a listener
//! the peer dials into) — are expressions of the two axes ([`DialAddr`] /
//! [`DialPolicy`]), not a per-topology enum. InProcess is the zero-cost degenerate
//! arm, so the same brain/hand code runs from a laptop to a fleet. (A brokered
//! topology is not modeled until a broker transport ships — a variant with no
//! producer would only mislead.)
//!
//! Credentials cross as a [`CredentialRef`] only; resolved material is produced
//! host-side just before dialing and never lives in a plan (ADR-0045 D4 / G34).

mod credential;
mod factory;
mod plan;

pub use credential::{AppliedAuth, CredentialError, CredentialResolver, NoAuth};
pub use factory::{
    ChannelFactory, ConnectError, TcpHandListener, TokioChannelFactory, UnixHandListener, bind_tcp,
    bind_unix, connect_with_retry, in_process_pair,
};
pub use plan::{ConnectionPlan, CredentialRef, DialAddr, DialPolicy};
