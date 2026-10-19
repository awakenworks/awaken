// @formal surface exclude=application_adapter
// @formal owner crates/contract/awaken-session-configuration-contract/src/mcp/generation.rs
// @formal reason: MCP request encoding reads the existing generation claim and delegates its
// @formal reason: identity validation to McpGenerationRef; no state transition or effect is selected.
//! Existing MCP claim-to-request encoding; frozen Session reads live with Queries.

use awaken_session_configuration_contract::mcp::{
    McpGenerationRef, SessionMcpAttachment, StageMcpAttachment,
};
use awaken_session_types::RunError;

pub(crate) fn mcp_generation_ref(
    session_id: &awaken_session_types::SessionId,
    attachment: &SessionMcpAttachment,
) -> Result<McpGenerationRef, RunError> {
    let claim = attachment
        .realization
        .as_ref()
        .ok_or_else(|| RunError::internal("MCP generation has no durable realization claim"))?;
    McpGenerationRef::from_claim(session_id, &attachment.generation, claim)
        .ok_or_else(|| RunError::internal("MCP realization claim carries a zero lease epoch"))
}

pub(crate) fn stage_mcp_request(
    workspace_id: &str,
    session_id: &awaken_session_types::SessionId,
    attachment: &SessionMcpAttachment,
) -> Result<StageMcpAttachment, RunError> {
    let claim = attachment
        .realization
        .as_ref()
        .ok_or_else(|| RunError::internal("MCP generation has no durable realization claim"))?;
    Ok(StageMcpAttachment {
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
