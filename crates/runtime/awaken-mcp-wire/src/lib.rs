//! Shared MCP wire layer.
//!
//! The direction-neutral pieces of the MCP wire protocol, consumed by both
//! `awaken-ext-mcp` (the client: imports external servers' tools) and
//! `awaken-protocol-mcp` (the server: exposes runtime tools to external
//! clients):
//!
//! - [`jsonrpc`] — a symmetric JSON-RPC 2.0 peer over an async byte stream;
//! - [`sse`] — an incremental Server-Sent Events parser (Streamable HTTP);
//! - [`progress`] — the `notifications/progress` vocabulary and throttling.
//!
//! Hoisted out of `awaken-ext-mcp` when the server crate arrived, so the two
//! directions speak one wire shape that cannot drift — the same precedent as
//! `awaken-credential`.

pub mod jsonrpc;
pub mod progress;
pub mod sse;

pub use jsonrpc::{
    JsonRpcNotifier, JsonRpcPeer, ServerNotification, ServerRequestError, ServerRequestHandler,
};
pub use progress::{McpProgressUpdate, normalize_progress};
pub use sse::SseParser;
