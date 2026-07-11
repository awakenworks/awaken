//! `awaken-protocol-ag-ui` — the AG-UI (Agent User Interaction Protocol) adapter.
//!
//! The anti-corruption boundary between the public AG-UI wire (a `HttpAgent`
//! posting `RunAgentInput` and consuming an SSE event stream) and the neutral
//! runtime. It owns the public DTOs, an [`AgUiEncoder`] that transcodes the shared
//! neutral `AgentEvent` projection into AG-UI events, and the axum router; it
//! drives the shared neutral `ProtocolRuntime` port (from
//! `awaken-protocol-transport`) and constructs no runtime itself.
//!
//! Like the other protocol adapters, it shares nothing above the neutral port and
//! the neutral projection seam, so the same host backs it on the same thread.

pub mod encoder;
pub mod live;
pub mod request;
pub mod router;
pub mod types;

pub use awaken_protocol_transport::{DriverError, Pending, ProtocolRuntime, Resume, StepOutcome};
pub use encoder::AgUiEncoder;
pub use router::router;
pub use types::{AgUiEvent, RunAgentInput};
