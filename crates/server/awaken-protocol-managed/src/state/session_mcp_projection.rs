//! Managed wire projection helpers for durable Session MCP generations.

use super::RunError;

pub(super) fn typed_mcp_servers(
    values: Vec<awaken_session_contract::VisibleMcpServer>,
) -> Vec<crate::types::agent::AgentMcpServer> {
    values
        .into_iter()
        .map(|server| match server.target {
            awaken_session_contract::McpTarget::Http(target) => {
                crate::types::agent::AgentMcpServer::Url {
                    name: server.name,
                    url: target.url,
                    prompts_as_skills: server.prompts_as_skills,
                }
            }
            awaken_session_contract::McpTarget::SandboxStdio(target) => {
                crate::types::agent::AgentMcpServer::SandboxStdio {
                    name: server.name,
                    command: target.command,
                    args: target.args,
                    prompts_as_skills: server.prompts_as_skills,
                }
            }
        })
        .collect()
}

pub(super) fn mcp_generation_ref(
    session_id: &str,
    attachment: &awaken_session_contract::SessionMcpAttachment,
) -> Result<awaken_session_contract::McpGenerationRef, RunError> {
    let claim = attachment
        .realization
        .as_ref()
        .ok_or_else(|| RunError::internal("MCP generation has no durable realization claim"))?;
    Ok(awaken_session_contract::McpGenerationRef {
        session_id: session_id.to_string(),
        attachment_id: attachment.attachment_id.clone(),
        generation: attachment.generation,
        runtime_incarnation: claim.runtime_incarnation.clone(),
        lease_epoch: claim.lease_epoch,
        lease_expires_at_unix_ms: claim.lease_expires_at_unix_ms,
    })
}

pub(super) fn stage_mcp_request(
    workspace_id: &str,
    session_id: &str,
    attachment: &awaken_session_contract::SessionMcpAttachment,
) -> Result<awaken_session_contract::StageMcpAttachment, RunError> {
    let claim = attachment
        .realization
        .as_ref()
        .ok_or_else(|| RunError::internal("MCP generation has no durable realization claim"))?;
    Ok(awaken_session_contract::StageMcpAttachment {
        workspace_id: workspace_id.to_string(),
        generation: mcp_generation_ref(session_id, attachment)?,
        realization_id: claim.realization_id.clone(),
        stage_idempotency_key: claim.stage_idempotency_key.clone(),
        name: attachment.name.clone(),
        target: attachment.target.clone(),
        prompts_as_skills: attachment.prompts_as_skills,
        credential: attachment.credential.clone(),
        selected_plaintext_holder: attachment.selected_plaintext_holder.clone(),
    })
}
