//! MCP capability of [`SharedHost`]: the lazily started loopback relay that
//! holds authenticated MCP bearers host-side for sandboxed ACP sessions.

use std::sync::Arc;

/// Host-side MCP relay wiring. Consumer: sandboxed ACP realization in
/// [`crate::mcp_relay`].
#[derive(Default)]
pub(crate) struct McpRelayCapability {
    /// One process-wide reaper for the exact MCP process handles tracked in
    /// Session projections. Composition installs the existing Supervisor.
    pub(crate) process_reaper: Option<Arc<dyn awaken_session_environment_contract::ProcessReaper>>,
    /// The host's loopback MCP relay (α-reference resolver), started lazily on the first
    /// sandboxed ACP session that stages an authenticated MCP server. It holds the real
    /// bearers host-side and injects them when forwarding the sandbox's MCP calls, so the
    /// raw token never enters the sandbox. See [`crate::mcp_relay`].
    pub(crate) mcp_relay: tokio::sync::OnceCell<crate::mcp_relay::McpRelay>,
    /// Installed only by a deployment that exposes this relay to isolated
    /// containers. It issues existing Gateway CONNECT capabilities per call.
    pub(crate) outbound_proxy: Option<Arc<dyn crate::McpRelayOutboundProxy>>,
}

impl crate::host::SharedHost {
    #[must_use]
    pub fn with_mcp_relay_outbound_proxy(
        mut self,
        issuer: Arc<dyn crate::McpRelayOutboundProxy>,
    ) -> Self {
        self.mcp.outbound_proxy = Some(issuer);
        self
    }

    #[must_use]
    pub fn with_mcp_process_reaper(
        mut self,
        reaper: Arc<dyn awaken_session_environment_contract::ProcessReaper>,
    ) -> Self {
        self.mcp.process_reaper = Some(reaper);
        self
    }

    pub(crate) fn mcp_process_reaper(
        &self,
    ) -> Result<Arc<dyn awaken_session_environment_contract::ProcessReaper>, crate::HostError> {
        self.mcp.process_reaper.clone().ok_or_else(|| {
            crate::HostError::unavailable_classified(
                "mcp_generation_process_reaper_missing",
                "MCP generation process reaper is not installed",
            )
        })
    }
}
