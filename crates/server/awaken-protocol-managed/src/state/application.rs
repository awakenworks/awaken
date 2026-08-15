//! Managed wire mapping for Session creation requests.
//!
//! Raw Session and published Agent MCP DTOs enter here once. This module only
//! preserves source identity and translates them into protocol-neutral MCP
//! candidates. Session finalization and root CAS belong exclusively to
//! [`awaken_session_application::SessionApplication`].

use crate::types::agent::AgentMcpServer;
use awaken_session_application::{McpAttachmentCandidate, McpAttachmentCandidateTarget};

pub(crate) fn agent_mcp_candidate(
    server: AgentMcpServer,
    origin: awaken_session_contract::McpAttachmentOrigin,
) -> McpAttachmentCandidate {
    McpAttachmentCandidate {
        name: server.name,
        target: McpAttachmentCandidateTarget::HttpUrl(server.url),
        prompts_as_skills: false,
        published_credential: None,
        origin,
    }
}

/// Preserve every create-time authoring candidate and its actual source. The
/// Session aggregate, not array order or credential presence, resolves logical
/// name precedence and target conflicts after canonical normalization.
pub(super) fn initial_mcp_candidates(
    agent: Option<&awaken_executable_agent_contract::ExecutableAgentSessionProfile>,
    agent_override: Option<&[AgentMcpServer]>,
) -> Vec<McpAttachmentCandidate> {
    let agent_len = agent_override.map_or_else(
        || agent.map_or(0, |view| view.mcp_servers.len()),
        <[AgentMcpServer]>::len,
    );
    let mut candidates = Vec::with_capacity(agent_len);
    if let Some(agent_override) = agent_override {
        candidates.extend(agent_override.iter().cloned().map(|server| {
            agent_mcp_candidate(server, awaken_session_contract::McpAttachmentOrigin::Agent)
        }));
    } else if let Some(agent) = agent {
        candidates.extend(agent.mcp_servers.iter().map(|server| {
            McpAttachmentCandidate {
                name: server.name.clone(),
                target: McpAttachmentCandidateTarget::Normalized(server.target.clone()),
                prompts_as_skills: server.prompts_as_skills,
                published_credential: server
                    .credential_source_id
                    .clone()
                    .zip(server.credential_revision),
                origin: awaken_session_contract::McpAttachmentOrigin::Agent,
            }
        }));
    }
    candidates
}
