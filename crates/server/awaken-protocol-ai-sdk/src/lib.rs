//! `awaken-protocol-ai-sdk` — the Vercel AI SDK v6 UI Message Stream adapter.
//!
//! This is the anti-corruption boundary between the public AI SDK wire (the
//! `useChat` / `DefaultChatTransport` protocol) and the neutral runtime. It owns
//! the public DTOs, the projection from committed `Message`s to UI Message Stream
//! parts, and the axum router; it drives the shared neutral `ProtocolRuntime` port
//! (from `awaken-protocol-transport`) and constructs no runtime itself. It is the
//! only crate permitted to name AI SDK protocol vocabulary.
//!
//! It shares nothing with the Managed Agents adapter above the neutral port: the
//! same host can back both, so an AI SDK frontend and a Managed backend interact
//! on the same thread, but their wire types stay one adapter each.

pub mod encoder;
pub mod request;
pub mod router;
pub mod types;

pub use awaken_protocol_transport::{DriverError, Pending, ProtocolRuntime, Resume, StepOutcome};
pub use router::router;
pub use types::{AiSdkChatRequest, UIStreamEvent};
