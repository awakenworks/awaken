//! `awaken-protocol-transport` — the neutral seam shared by the protocol adapters,
//! below the wire-DTO layer.
//!
//! AG-UI, AI SDK, and A2A each translate their own wire vocabulary onto one neutral
//! host, so they all drive the same seam: a step outcome, a pending tool, a resume
//! command, a driver fault. This crate owns that seam ([`ProtocolRuntime`] and its
//! value objects) plus the wire-agnostic [`blocks_text`] helper, so no adapter
//! redeclares an identical copy. It names no wire type and constructs no runtime.

mod convert;
mod port;
mod stream;

pub use convert::blocks_text;
pub use port::{DriverError, Pending, ProtocolRuntime, Resume, StepOutcome};
pub use stream::ChannelStreamSink;
