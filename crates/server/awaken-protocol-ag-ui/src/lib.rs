//! `awaken-protocol-ag-ui` — the AG-UI (Agent User Interaction Protocol) adapter.
//!
//! The anti-corruption boundary between the public AG-UI wire (a `HttpAgent`
//! posting `RunAgentInput` and consuming an SSE event stream) and the neutral
//! runtime. It owns the public DTOs, an [`AgUiEncoder`] that transcodes the shared
//! neutral `AgentEvent` projection into AG-UI events, and the axum router; it
//! drives one [`AgUiRuntime`] port and constructs no runtime itself.
//!
//! Like the other protocol adapters, it shares nothing above the neutral port and
//! the neutral projection seam, so the same host backs it on the same thread.

pub mod encoder;
pub mod port;
pub mod request;
pub mod router;
pub mod types;

pub use encoder::AgUiEncoder;
pub use port::{AgUiRuntime, DriverError, Pending, Resume, StepOutcome};
pub use router::router;
pub use types::{AgUiEvent, RunAgentInput};
