//! Per-Session MCP for the Runtime Host: exact-generation staging, publication
//! and drain of Session MCP attachments, native MCP client wiring, MCP
//! application input, and the Worker relay that keeps authenticated MCP
//! credentials out of a sandbox. The Runtime Host holds one [`SessionMcpState`]
//! in each Session slot and drives it through [`McpSessions`].

mod mcp;
mod mcp_material;
mod mcp_relay;
mod mcp_relay_exposure;
#[cfg(any(test, feature = "test-support"))]
pub mod test_mcp;

pub use mcp::{
    ExtMcpProbe, McpCredentialMaterialization, McpError, McpErrorKind, McpGenerationAuthority,
    McpInputHost, McpInputRecipients, McpQuiescenceProof, McpSessionSlots, McpSessions,
    McpStagingSession, project_mcp_session_transport,
};
pub use mcp_material::{
    McpGenerationProjection, McpProjectionState, McpQuiescenceAdmissionFence, McpRefreshMaterial,
    McpStagingActivity, McpTransportMaterial, McpTransportMaterialKind, McpWiring, SessionMcpState,
};
pub use mcp_relay::{McpRelay, McpRelayOutboundProxy};
pub use mcp_relay_exposure::ContainerMcpRelayExposure;
