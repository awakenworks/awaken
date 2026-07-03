//! `awaken-protocol-a2a` — the A2A (Agent2Agent) v1.0 protocol adapter.
//!
//! The anti-corruption boundary between the public A2A wire (`message:send` over
//! HTTP+JSON, returning a `Task`) and the neutral runtime. It owns the A2A DTOs,
//! the projection from committed `Message`s to an A2A `Task`, and the axum router;
//! it drives the shared neutral `ProtocolRuntime` port (from
//! `awaken-protocol-transport`) and constructs no runtime itself. It is the only
//! crate permitted to name A2A protocol vocabulary.
//!
//! A2A is request/response, not streaming: `message:send` returns a whole `Task`
//! (its `status` plus `history`), so — unlike the AI SDK / AG-UI stream adapters —
//! its errors are an HTTP status + JSON envelope (like the Managed adapter), not
//! an in-stream error event. It shares the same neutral host as every other
//! adapter, so an A2A caller and a Managed backend interact on the same thread.

pub mod client;
pub mod encoder;
pub mod request;
pub mod router;
pub mod types;

pub use awaken_protocol_transport::{DriverError, Pending, ProtocolRuntime, Resume, StepOutcome};
pub use client::{ClientError, HttpTransport, Response, Transport};
pub use router::{agent_card, router};
pub use types::{AgentCard, Artifact, SendMessageRequest, SendMessageResponse, Task, TaskState};
