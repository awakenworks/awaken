//! Model Context Protocol (MCP) client extension.
//!
//! Connects to external MCP servers and exposes their tools as awaken runtime
//! tools ([`RawTool`](awaken_runtime_contract::tool::RawTool)), namespaced
//! `mcp__<server>__<tool>`. The wire protocol is spoken through the `mcp` SDK,
//! which stays the anti-corruption boundary: only neutral runtime values
//! (`ToolOutput`, `ToolDescriptor`) cross out of this crate.
//!
//! This is an extension (never the kernel): like `awaken-ext-builtin-tools` it
//! depends only on `awaken-runtime-contract` and the host wires it in.
//!
//! # Phasing
//!
//! Full parity with the reference MCP crates (goal / awaken-next) is built in
//! phases: P0 (this) covers tool discovery/invocation and the error three-state;
//! later phases add the stdio/HTTP transports, the connection manager,
//! `tools/list_changed` refresh, sampling, progress, resources, prompts, and
//! credentials.

pub mod client;
pub mod config;
pub mod credential;
pub mod error;
pub mod http;
pub mod id_mapping;
pub mod manager;
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
pub use awaken_mcp_wire::{jsonrpc, progress, sse};

pub use client::{McpConnection, connect_tools};
pub use config::{McpServerConnectionConfig, TransportTypeId};
pub use credential::{AuthChallenge, Credential, CredentialRefresher};
pub use error::McpError;
pub use http::{HttpTransport, HttpTransportBuilder};
pub use id_mapping::{to_tool_id, tool_namespace};
pub use manager::{McpManager, ServerStatus};
pub use plugin::{McpPlugin, McpServer, SensitiveFields};
pub use progress::{McpProgressUpdate, normalize_progress};
pub use sampling::{
    SamplingError, SamplingHandler, SamplingMessage, SamplingRequest, SamplingResponse,
};
pub use sensitive::{REDACTED, mark_sensitive, redact_arguments, sensitive_paths};
pub use stdio::{DEFAULT_TIMEOUT, StdioTransport};
pub use tool::{McpRawTool, mcp_tool_descriptor};
pub use transport::{ListChangedKind, McpToolTransport};
pub use types::{
    McpPromptArgument, McpPromptDefinition, McpPromptMessage, McpPromptResult,
    McpResourceDefinition,
};
