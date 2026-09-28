//! MCP capability of [`SharedHost`]: the lazily started loopback relay that
//! holds authenticated MCP bearers host-side for sandboxed ACP sessions.

/// Host-side MCP relay wiring. Consumer: sandboxed ACP realization in
/// [`crate::mcp_relay`].
#[derive(Default)]
pub(crate) struct McpRelayCapability {
    /// The host's loopback MCP relay (α-reference resolver), started lazily on the first
    /// sandboxed ACP session that stages an authenticated MCP server. It holds the real
    /// bearers host-side and injects them when forwarding the sandbox's MCP calls, so the
    /// raw token never enters the sandbox. See [`crate::mcp_relay`].
    pub(crate) mcp_relay: tokio::sync::OnceCell<crate::mcp_relay::McpRelay>,
}
