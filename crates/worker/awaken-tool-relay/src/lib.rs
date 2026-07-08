//! Remote hand tool execution (ADR-0044).
//!
//! The brain–hand seam of 手腦分離. The runtime's already-defined `ToolExecutor`
//! port stays the boundary; this crate provides its two realizations' remote half
//! plus the hand server:
//!
//! - [`RemoteToolExecutor`] — the brain side: a `ToolExecutor` that frames a
//!   `ToolCall` to a hand over a byte channel and awaits the result.
//! - [`serve_hand`] / [`HandSession`] — the hand side: a value-returning tool
//!   server that links a tool registry only — no model client, no commit
//!   coordinator, no store (G33). It returns serializable data; the brain commits.
//!
//! The wire ([`HandRequest`] / [`HandReply`]) reuses the runtime's own `ToolCall`
//! / `ToolOutput` value objects rather than a parallel execution vocabulary
//! (ADR-0044 D2). Transport and topology are out of scope here — the crate is
//! written against an abstract `AsyncRead + AsyncWrite` channel, and
//! `awaken-connection-plan` (ADR-0045) chooses how the two ends meet.

mod executor;
mod serve;
pub mod wire;

pub use executor::RemoteToolExecutor;
pub use serve::{HandSession, ServeError, serve_hand};
pub use wire::{CorrelationId, HandError, HandErrorKind, HandReply, HandRequest, HandResult};
