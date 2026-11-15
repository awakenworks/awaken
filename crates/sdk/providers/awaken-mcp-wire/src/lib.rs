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

pub mod http;
pub mod jsonrpc;
pub mod progress;
pub mod sse;
mod types;

/// Maximum raw bytes in one inbound MCP message, before UTF-8 or JSON decoding.
/// The catalog collector separately limits the aggregate across discovery pages.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub use jsonrpc::{
    JsonRpcNotifier, JsonRpcPeer, ServerNotification, ServerRequestError, ServerRequestHandler,
};
pub use progress::{McpProgressUpdate, normalize_progress};
pub use sse::SseParser;
pub use types::{
    Annotations, CallToolParams, CallToolResult, ClientInfo, CreateTaskResult, Icon, IconTheme,
    InitializeCapabilities, InitializeParams, InitializeResult, ListToolsParams, ListToolsResult,
    MCP_PROTOCOL_VERSION, McpTask, McpToolDefinition, McpTransportError, Role, ServerCapabilities,
    ServerInfo, ServerTaskCapabilities, ServerTaskRequests, ServerTaskToolRequests,
    ServerToolCapabilities, TaskIdParams, TaskMetadata, TaskStatus, TaskSupport, ToolAnnotations,
    ToolContent, ToolExecution,
};
