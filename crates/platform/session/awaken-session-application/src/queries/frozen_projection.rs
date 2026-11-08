// @formal entrypoint agent_profile.worker_projection_requires_exact_frozen_publication
// @formal entrypoint session.transcript_prefix_is_exact_request_only_context_not_durable_truth
// @formal entrypoint session_resources.terminal_assignment_retains_desired_generation
//! Frozen Session and scoped transcript reads over the selected Query ports.

use awaken_agent_contract::agent::message::Message;
use awaken_runtime_contract::runs::RunError;
use awaken_session_contract::PersistedSession;
use awaken_session_execution_contract::FrozenSessionProjection;

use crate::{SessionProjectionReads, SessionQueries};

impl SessionQueries {
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
        let committed = self
            .thread_reads
            .committed_messages(&spec.snapshot.thread_id.0)
            .await?;
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
        resources: crate::session_resource::ResolvedSessionResources,
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
            crate::session_resource::SessionResourceManifest::at_revision(
                owner_scope.clone(),
                previous_resource_revision,
                previous_resources.clone(),
            ),
        );
        let agent_publication = match baseline.agent_revision {
            Some(source_revision) => {
                let snapshot = self.executable_snapshot_at_revision(
                    &owner_scope,
                    &baseline.agent_id,
                    source_revision,
                );
                match awaken_session_execution_contract::frozen_agent_publication_decision(
                    &baseline,
                    snapshot.as_ref(),
                ) {
                    awaken_session_execution_contract::FrozenAgentPublicationDecision::Unpinned
                    | awaken_session_execution_contract::FrozenAgentPublicationDecision::OptionalMissing
                    | awaken_session_execution_contract::FrozenAgentPublicationDecision::Exact => {}
                    awaken_session_execution_contract::FrozenAgentPublicationDecision::MissingRequired => {
                        return Err(RunError::unavailable(format!(
                            "Agent '{}' publication revision {} is unavailable for Worker realization",
                            baseline.agent_id, source_revision
                        )));
                    }
                    awaken_session_execution_contract::FrozenAgentPublicationDecision::Mismatch => {
                        return Err(RunError::internal(
                            "exact Agent publication does not match the frozen Session baseline",
                        ));
                    }
                }
                snapshot
                    .map(|snapshot| {
                        awaken_session_execution_contract::project_effective_agent_publication(
                            baseline.model_override.as_ref(),
                            &baseline.system_prompt,
                            Some(resources.skills()),
                            &owner_scope,
                            snapshot,
                        )
                    })
                    .transpose()?
            }
            // An inline Agent's Session-scoped publication is the Session's
            // exact pin; the same projection law lowers its overrides.
            None => baseline
                .inline_agent_publication
                .as_deref()
                .cloned()
                .map(|snapshot| {
                    awaken_session_execution_contract::project_effective_agent_publication(
                        baseline.model_override.as_ref(),
                        &baseline.system_prompt,
                        Some(resources.skills()),
                        &owner_scope,
                        snapshot,
                    )
                })
                .transpose()?,
        };
        Ok(FrozenSessionProjection {
            bound_source_end: None,
            workspace_id: owner_scope,
            revision: session.revision,
            baseline,
            agent_publication,
            environment: session.environment.clone(),
            environment_materialization: session
                .environment_materialization()
                .cloned()
                .map(Box::new),
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
        self.frozen_runtime_projection(owner_scope, session, materialize_request_context, true)
            .await
    }

    /// Project desired resources when the caller includes pending inputs;
    /// otherwise retain installed resources for MCP-only phases. Context
    /// materialization is independent and uses the same projection kernel.
    pub(crate) async fn frozen_runtime_projection(
        &self,
        owner_scope: String,
        session: &PersistedSession,
        materialize_request_context: bool,
        include_pending_resources: bool,
    ) -> Result<FrozenSessionProjection, RunError> {
        let (resource_revision, resources) = if include_pending_resources {
            session.resources.desired_generation()
        } else {
            session.resources.active_generation()
        };
        self.frozen_session_projection_for_resources(
            owner_scope,
            session,
            resource_revision,
            resources.clone(),
            materialize_request_context,
        )
        .await
    }
    /// Read the committed transcript after enforcing durable Session ownership.
    /// Protocol projections are deliberately bypassed: the Session aggregate and
    /// Runtime commit log are the only authorities involved.
    pub async fn session_transcript(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Vec<Message>, RunError> {
        self.read_session_projection(session_id, Some(workspace_id))
            .await
            .map_err(|error| match error {
                crate::SessionProjectionRecoveryError::NotFound => {
                    RunError::bad_request("Session was not found")
                }
                crate::SessionProjectionRecoveryError::Rejected(error) => error,
                crate::SessionProjectionRecoveryError::Unavailable(message) => {
                    RunError::unavailable(message)
                }
            })?
            .ok_or_else(|| RunError::bad_request("Session was not found"))?;
        self.thread_reads.committed_messages(session_id).await
    }
}
