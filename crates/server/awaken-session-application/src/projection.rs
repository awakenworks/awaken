//! Neutral frozen Session and MCP realization projections.

use awaken_session_contract::{
    ApplicationSessionContributionFailure, FrozenSessionProjection, McpGenerationRef,
    PersistedSession, RunError, SessionMcpAttachment, StageMcpAttachment,
};

use super::SessionApplication;

impl SessionApplication {
    fn frozen_session_projection_for_resources(
        owner_scope: String,
        session: &PersistedSession,
        resource_revision: u64,
        resources: awaken_session_contract::ResolvedSessionResources,
    ) -> Result<FrozenSessionProjection, ApplicationSessionContributionFailure> {
        let baseline = session.frozen_baseline().cloned().ok_or_else(|| {
            ApplicationSessionContributionFailure::Unavailable(
                "Session creation intent was not consumed".into(),
            )
        })?;
        Ok(FrozenSessionProjection {
            workspace_id: owner_scope,
            revision: session.revision,
            baseline,
            environment: session.environment.clone(),
            resource_revision,
            resources,
            toolsets: session.tools.toolsets.clone(),
            mcp: session.mcp.attachments.clone(),
        })
    }

    /// Project durable Session truth into the exact immutable input installed by
    /// a local or remote Worker. Protocol adapters never rebuild this shape.
    pub fn frozen_session_projection(
        owner_scope: String,
        session: &PersistedSession,
    ) -> Result<FrozenSessionProjection, ApplicationSessionContributionFailure> {
        let resources = session
            .resources
            .pending
            .clone()
            .unwrap_or_else(|| session.resources.active.clone());
        Self::frozen_session_projection_for_resources(
            owner_scope,
            session,
            session.resources.revision,
            resources,
        )
    }

    /// Project only the Resource generation currently installed in a live
    /// Runtime. Lease renewal is MCP-only and must not consume a pending input
    /// mutation that still requires a dispatch claim.
    pub(crate) fn active_frozen_session_projection(
        owner_scope: String,
        session: &PersistedSession,
    ) -> Result<FrozenSessionProjection, ApplicationSessionContributionFailure> {
        Self::frozen_session_projection_for_resources(
            owner_scope,
            session,
            session.resources.active_revision(),
            session.resources.active.clone(),
        )
    }
}

pub(crate) fn mcp_generation_ref(
    session_id: &str,
    attachment: &SessionMcpAttachment,
) -> Result<McpGenerationRef, RunError> {
    let claim = attachment
        .realization
        .as_ref()
        .ok_or_else(|| RunError::internal("MCP generation has no durable realization claim"))?;
    Ok(McpGenerationRef {
        session_id: session_id.to_string(),
        attachment_id: attachment.attachment_id.clone(),
        generation: attachment.generation,
        runtime_incarnation: claim.runtime_incarnation.clone(),
        lease_epoch: claim.lease_epoch,
        lease_expires_at_unix_ms: claim.lease_expires_at_unix_ms,
    })
}

pub(crate) fn stage_mcp_request(
    workspace_id: &str,
    session_id: &str,
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
