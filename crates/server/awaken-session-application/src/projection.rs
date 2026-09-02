//! Neutral frozen Session and MCP realization projections.

use awaken_session_contract::{
    FrozenSessionProjection, McpGenerationRef, PersistedSession, RunError, SessionMcpAttachment,
    StageMcpAttachment,
};

use super::SessionApplication;

impl SessionApplication {
    async fn request_context(
        &self,
        _owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<Vec<awaken_agent_contract::agent::message::Message>, RunError> {
        let Some(spec) = session
            .frozen_baseline()
            .and_then(|baseline| baseline.transcript_prefix.as_ref())
        else {
            return Ok(Vec::new());
        };
        // Creation already admitted the source through the owning Workspace.
        // Recovery follows the persisted immutable reference directly to
        // committed Thread truth, so deleting/archiving the source Session
        // projection cannot orphan a branch or introduce a second owner lookup.
        let committed = self.committed_messages(&spec.snapshot.thread_id.0).await?;
        let end = usize::try_from(spec.snapshot.end_seq)
            .map_err(|_| RunError::unavailable("transcript prefix sequence overflow"))?;
        if committed.len() < end {
            return Err(RunError::unavailable(format!(
                "transcript prefix ending at {} is unavailable; committed end is {}",
                spec.snapshot.end_seq,
                committed.len()
            )));
        }
        let snapshot = awaken_agent_contract::thread::read::transcript::TranscriptSnapshot::new(
            spec.snapshot.thread_id.clone(),
            spec.snapshot.view,
            committed[..end].to_vec(),
        );
        snapshot
            .slice(spec)
            .map(|slice| slice.messages.to_vec())
            .map_err(|error| RunError::unavailable(error.to_string()))
    }

    async fn frozen_session_projection_for_resources(
        &self,
        owner_scope: String,
        session: &PersistedSession,
        resource_revision: u64,
        resources: awaken_session_contract::ResolvedSessionResources,
        materialize_request_context: bool,
    ) -> Result<FrozenSessionProjection, RunError> {
        let baseline = session
            .frozen_baseline()
            .cloned()
            .ok_or_else(|| RunError::unavailable("Session creation intent was not consumed"))?;
        let request_context = if materialize_request_context {
            self.request_context(&owner_scope, session).await?
        } else {
            Vec::new()
        };
        let (previous_resource_revision, previous_resources) =
            session.resources.active_generation();
        let previous_resource_manifest = Some(
            awaken_session_contract::SessionResourceManifest::at_revision(
                owner_scope.clone(),
                previous_resource_revision,
                previous_resources.clone(),
            ),
        );
        let agent_publication = match baseline.agent_revision {
            Some(source_revision) => {
                let snapshot = self.config_source.as_ref().and_then(|source| {
                    source.executable_snapshot_at_revision_in(
                        &owner_scope,
                        &baseline.agent_id,
                        source_revision,
                    )
                });
                match awaken_session_contract::frozen_agent_publication_decision(
                    &baseline,
                    snapshot.as_ref(),
                ) {
                    awaken_session_contract::FrozenAgentPublicationDecision::Unpinned
                    | awaken_session_contract::FrozenAgentPublicationDecision::OptionalMissing
                    | awaken_session_contract::FrozenAgentPublicationDecision::Exact => {}
                    awaken_session_contract::FrozenAgentPublicationDecision::MissingRequired => {
                        return Err(RunError::unavailable(format!(
                            "Agent '{}' publication revision {} is unavailable for Worker realization",
                            baseline.agent_id, source_revision
                        )));
                    }
                    awaken_session_contract::FrozenAgentPublicationDecision::Mismatch => {
                        return Err(RunError::internal(
                            "exact Agent publication does not match the frozen Session baseline",
                        ));
                    }
                }
                snapshot
                    .map(|snapshot| {
                        awaken_session_contract::project_effective_agent_publication(
                            baseline.model_override.as_ref(),
                            &baseline.system_prompt,
                            &owner_scope,
                            snapshot,
                        )
                    })
                    .transpose()?
            }
            None => None,
        };
        Ok(FrozenSessionProjection {
            workspace_id: owner_scope,
            revision: session.revision,
            baseline,
            agent_publication,
            environment: session.environment.clone(),
            resource_revision,
            resources,
            previous_resource_manifest,
            tools: session.tools.clone(),
            mcp: session.mcp.attachments.clone(),
            request_context,
        })
    }

    /// Project durable Session truth into the exact immutable input installed by
    /// a local or remote Worker. Protocol adapters never rebuild this shape.
    pub async fn frozen_session_projection(
        &self,
        owner_scope: String,
        session: &PersistedSession,
        materialize_request_context: bool,
    ) -> Result<FrozenSessionProjection, RunError> {
        let (resource_revision, resources) = session.resources.desired_generation();
        self.frozen_session_projection_for_resources(
            owner_scope,
            session,
            resource_revision,
            resources.clone(),
            materialize_request_context,
        )
        .await
    }

    /// Project only the Resource generation currently installed in a live
    /// Runtime. Lease renewal is MCP-only and must not consume a pending input
    /// mutation that still requires a dispatch claim.
    pub(crate) async fn active_frozen_session_projection(
        &self,
        owner_scope: String,
        session: &PersistedSession,
        materialize_request_context: bool,
    ) -> Result<FrozenSessionProjection, RunError> {
        let (resource_revision, resources) = session.resources.active_generation();
        self.frozen_session_projection_for_resources(
            owner_scope,
            session,
            resource_revision,
            resources.clone(),
            materialize_request_context,
        )
        .await
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
