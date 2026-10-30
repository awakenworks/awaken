//! Model Context Protocol (MCP) client extension.
//!
//! Connects to external MCP servers and exposes their tools as awaken runtime
//! tools ([`RawTool`](awaken_tool_contract::tool::RawTool)), namespaced
//! `mcp__<server>__<tool>`. This crate owns the client adapters; the shared
//! JSON-RPC peer, SSE parser and progress vocabulary belong to `awaken-mcp-wire`.
//! Tools cross into the runtime through its existing `ToolOutput` and
//! `ToolDescriptor` contracts.
//!
//! This is an extension (never the kernel): like `awaken-ext-builtin-tools` it
//! consumes the SDK contracts and the host wires its connections and plugins in.
//!
//! # Client capabilities and lifetime
//!
//! [`StdioTransport`] and [`HttpTransport`] implement the existing client
//! protocol, including discovery/invocation, negotiated Tasks, notifications,
//! sampling, progress, resources and prompts. HTTP also owns credential
//! challenge/refresh handling. Availability follows each connection's negotiated
//! capabilities; these exports do not grant unsupported server operations.
//!
//! [`McpServer`] owns its tool registry and refresh lifetime; [`McpManager`]
//! groups servers and reports their health. The manager reports dead-server
//! liveness and the host decides whether to replace that server; automatic
//! health-budget replacement is not implemented. This is distinct from the
//! HTTP transport's existing SSE reconnect handling.

pub mod client;
pub mod credential;
pub mod error;
pub mod http;
pub mod id_mapping;
pub mod manager;
pub mod notification;
pub mod plugin;
mod router;
pub mod sampling;
pub mod sensitive;
pub mod stdio;
pub mod tool;
pub mod transport;
pub mod types;

// The direction-neutral wire layer (JSON-RPC peer, SSE parser, progress
// vocabulary) is shared with the MCP server crate via `awaken-mcp-wire`;
// re-exported here so client-side paths (`awaken_ext_mcp::jsonrpc`, …) hold.
pub use awaken_mcp_wire::{
    CallToolResult, CreateTaskResult, McpTask, McpToolDefinition, McpTransportError, TaskStatus,
    jsonrpc, progress, sse,
};

pub use client::{McpConnection, connect_tools};
pub use credential::{
    AuthChallenge, AuthRetrySafety, Credential, CredentialRefresher, auth_retry_safety,
};
pub use error::McpError;
pub use http::{
    HTTP_FAILURE_BODY_LIMIT, HttpTransport, HttpTransportBuilder, McpHttpFailure, McpRequestTrace,
};
pub use id_mapping::{to_tool_id, tool_namespace};
pub use manager::{McpManager, ServerStatus};
pub use plugin::{McpPlugin, McpServer, SensitiveFields};
pub use progress::{McpProgressUpdate, normalize_progress};
pub use sampling::{
    SamplingError, SamplingHandler, SamplingMessage, SamplingRequest, SamplingResponse,
};
pub use sensitive::{REDACTED, mark_sensitive, redact_arguments, sensitive_paths};
pub use stdio::{DEFAULT_TIMEOUT, StdioTransport};
pub use tool::{MCP_TASK_OWNER, McpRawTool, mcp_tool_descriptor};
pub use transport::{ListChangedKind, McpToolTransport};
pub use types::{
    McpPromptArgument, McpPromptDefinition, McpPromptMessage, McpPromptResult,
    McpResourceDefinition,
};
