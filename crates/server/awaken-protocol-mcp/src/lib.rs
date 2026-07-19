//! Model Context Protocol (MCP) protocol adapter — the *server* side.
//!
//! Exposes runtime tools ([`RawTool`](awaken_runtime_contract::tool::RawTool) +
//! [`ToolDescriptor`](awaken_runtime_contract::resolved::ToolDescriptor)) to
//! external MCP clients, over stdio ([`McpStdioServer`]) and Streamable HTTP
//! ([`router`]). It is the egress mirror of `awaken-ext-mcp` (the client,
//! ingress): the same wire layer (`awaken-mcp-wire`), the same three-state
//! result mapping run in reverse, and the `mcp` SDK as the anti-corruption
//! boundary for wire types.
//! Protocol lifecycle and Streamable HTTP decisions live in the runtime-neutral
//! `awaken-mcp-server-core`; this crate is only the awaken RawTool/Store/gate
//! anti-corruption adapter plus compatibility facade.
//!
//! Design rules:
//!
//! - **Explicit export set.** A server serves only the tools the host handed it
//!   ([`ToolExportSource`]) — never a whole runtime registry by default. That
//!   keeps `bash`-class tools and re-imported `mcp__*__*` tools from leaking to
//!   external clients unless the host opts them in.
//! - **Same gate as internal execution.** An optional
//!   [`ToolGateHook`](awaken_runtime_contract::permission::ToolGateHook) is
//!   consulted before every `tools/call`, so the external path cannot widen
//!   what permission allows (G21).
//! - **Progress is first-class.** A tool that implements [`ProgressRawTool`]
//!   streams `notifications/progress` to clients that sent a `progressToken`;
//!   plain [`RawTool`] exports run unchanged.
//!
//! Result mapping (the mirror of `McpRawTool` in `awaken-ext-mcp`):
//!
//! - `Ok(ToolOutput { is_error: false })` → `CallToolResult` (success);
//! - `Ok(ToolOutput { is_error: true })` → `CallToolResult { isError: true }`
//!   (model-visible tool failure, the client's run continues);
//! - `Err(ToolError)` → a JSON-RPC error (protocol-level failure).
//!
//! `ToolOutput.state` (staged state commands) is meaningful only inside a run
//! and is dropped at this boundary.

pub mod export;
pub mod http;
pub mod service;
pub mod stdio;

pub use export::{
    McpExportedTool, ProgressRawTool, SharedExports, StaticExports, ToolExec, ToolExportSource,
};
pub use http::{McpHttpConfig, router};
pub use service::{AwakenMcpContext, McpToolService, NotifySink, NullSink};
pub use stdio::McpStdioServer;
