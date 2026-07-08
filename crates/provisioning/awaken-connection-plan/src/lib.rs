//! Network topology as a value object (ADR-0045).
//!
//! The policy layer the connection *mechanism* deliberately excludes: a
//! [`ConnectionPlan`] names how two ends meet (address, wiring, dial direction,
//! and a credential *reference*), and a [`ChannelFactory`] turns a plan into a
//! live [`AgentChannel`]. The four topologies — InProcess, Direct, Reverse,
//! Relay — are expressions of the three axes ([`DialAddr`] / [`Wiring`] /
//! [`DialPolicy`]), not a fifth enum. InProcess is the zero-cost degenerate arm,
//! so the same brain/hand code runs from a laptop to a fleet.
//!
//! Credentials cross as a [`CredentialRef`] only; resolved material is produced
//! host-side just before dialing and never lives in a plan (ADR-0045 D4 / G34).

mod credential;
mod factory;
mod plan;

pub use credential::{AppliedAuth, CredentialError, CredentialResolver, NoAuth};
pub use factory::{
    bind_unix, in_process_pair, ChannelFactory, ConnectError, TokioChannelFactory, UnixHandListener,
};
pub use plan::{ConnectionPlan, CredentialRef, DialAddr, DialPolicy, Wiring};
