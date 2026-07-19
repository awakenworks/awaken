//! Runtime-neutral MCP server mechanics.
//!
//! This crate owns protocol lifecycle, SDK type mapping, request validation,
//! JSON-RPC error mapping, progress ordering, cancellation, and the axum-free
//! Streamable HTTP decision kernel. It deliberately knows nothing about an agent
//! runtime, authorization, approval, resources, stores, principals, or scopes.

#[cfg(any(test, kani))]
mod formal;
mod http;
mod server;

pub use http::{
    AllowAllOrigins, HttpContextError, McpHttpBody, McpHttpContextProvider, McpHttpMethod,
    McpHttpReply, OriginPolicy, handle_streamable_http, validate_protocol_version,
    validate_streamable_http_request,
};
pub use server::{
    McpCall, McpHostError, McpServer, McpToolHost, NotifySink, NullSink,
    SUPPORTED_PROTOCOL_VERSIONS, jsonrpc_reply, notify_tools_list_changed,
    tools_list_changed_notification,
};

pub use mcp::{CallToolResult, McpToolDefinition, ToolContent};
