//! Managed wire mapping for Session creation requests.
//!
//! Raw Agent MCP DTOs enter here once. This module only preserves source
//! identity and translates them into protocol-neutral MCP candidates; which
//! candidates a Session starts from, finalization and root CAS belong
//! exclusively to [`awaken_session_application::SessionApplication`].

use crate::types::agent::AgentMcpServer;
use awaken_session_application::{McpAttachmentCandidate, McpAttachmentCandidateTarget};

pub(crate) fn agent_mcp_candidate(
    server: AgentMcpServer,
    origin: crate::session_mcp::McpAttachmentOrigin,
) -> McpAttachmentCandidate {
    McpAttachmentCandidate {
        name: server.name,
        target: McpAttachmentCandidateTarget::HttpUrl(server.url),
        prompts_as_skills: false,
        published_credential: None,
        origin,
    }
}
