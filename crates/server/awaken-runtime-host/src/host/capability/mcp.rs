//! MCP capability of [`SharedHost`]: the composition-installed dependencies of
//! the Session MCP lifecycle ([`awaken_session_mcp::McpSessions`]), which also owns the
//! lazily started loopback relay that holds authenticated MCP bearers host-side
//! for sandboxed ACP sessions.

use std::sync::Arc;

impl crate::host::SharedHost {
    #[must_use]
    pub fn with_mcp_relay_outbound_proxy(
        mut self,
        issuer: Arc<dyn awaken_session_mcp::McpRelayOutboundProxy>,
    ) -> Self {
        self.mcp.set_outbound_proxy(issuer);
        self
    }

    #[must_use]
    pub fn with_mcp_process_reaper(
        mut self,
        reaper: Arc<dyn awaken_session_environment_contract::ProcessReaper>,
    ) -> Self {
        self.mcp.set_process_reaper(reaper);
        self
    }
}
