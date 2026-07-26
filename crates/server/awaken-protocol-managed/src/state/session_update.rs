//! Managed Session metadata, tool, and generation-fenced MCP replacement.

use super::application::ManagedMcpCandidate;
use super::*;
use crate::types::agent::{AgentTool, UrlMcpServer};

/// One application-layer Session update command compiled from the Managed wire.
/// Keeping its fields together prevents the public endpoint and CAS retry path
/// from growing parallel positional parameter lists.
pub(crate) struct SessionUpdateCommand {
    pub(crate) title: Option<Option<String>>,
    pub(crate) metadata: Option<Option<std::collections::BTreeMap<String, Option<String>>>>,
    pub(crate) tools: Option<Vec<AgentTool>>,
    pub(crate) mcp_servers: Option<Vec<UrlMcpServer>>,
    pub(crate) idempotency_key: Option<String>,
    pub(crate) if_match: Option<awaken_session_contract::SessionRevision>,
}

impl ManagedState {
    #[must_use]
    pub(crate) fn update_operation_id(id: &str, idempotency_key: &str) -> String {
        awaken_session_contract::stable_fingerprint(&(id, idempotency_key))
    }

    /// MCP projection recovery entry point. It consumes the same canonical
    /// repository recovery index as Resource activation, then drives only the
    /// MCP aggregate state machine through the sole stage/publish/drain paths.
    pub async fn reconcile_mcp_attachments(&self) -> usize {
        let pending = self.sessions_repo.reconcilable_sessions().await;
        let mut settled = 0;
        for record in pending {
            // Root lifecycle is the outer fence. A terminal Session may retain
            // nonterminal attachment facts solely as cleanup evidence; startup
            // must not resolve credentials or recreate routes for them.
            if record.session.is_terminal() || !record.session.mcp.needs_reconciliation() {
                continue;
            }
            let session_id = record.session.session_id.clone();
            match self.recover_mcp_projections(&session_id).await {
                Ok(_) => settled += 1,
                Err(error) => tracing::warn!(
                    session = %session_id,
                    error = ?error,
                    "Session MCP reconciliation remains pending"
                ),
            }
        }
        settled
    }

    /// Apply mutable Managed Session fields. MCP arrays are canonical full
    /// replacements; wire projection is derived only after durable activation
    /// and exact Runtime publication/drain complete.
    pub(crate) async fn update_session(
        &self,
        id: &str,
        command: SessionUpdateCommand,
    ) -> Result<(Session, awaken_session_contract::SessionRevision), StateError> {
        let request_hash = awaken_session_contract::stable_fingerprint(&(
            &command.title,
            &command.metadata,
            &command.tools,
            &command.mcp_servers,
        ));
        let command_record = command.idempotency_key.as_ref().map(|key| {
            awaken_session_contract::IdempotencyRecord {
                key: format!(
                    "managed:update-command:{id}:{}",
                    Self::update_operation_id(id, key)
                ),
                payload_hash: request_hash.clone(),
            }
        });
        if let Some(record) = &command_record
            && let Some(receipt) = self
                .sessions_repo
                .idempotency_receipt(id, &record.key)
                .await
        {
            if receipt.payload_hash != record.payload_hash {
                return Err(StateError::IdempotencyMismatch);
            }
            return Ok((self.get_session(id)?, receipt.committed_revision));
        }

        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            match self
                .update_session_once(id, &command, command_record.clone())
                .await
            {
                Err(StateError::Conflict)
                    if command.if_match.is_none() && attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
                {
                    continue;
                }
                result => return result,
            }
        }
        Err(StateError::Conflict)
    }

    async fn update_session_once(
        &self,
        id: &str,
        command: &SessionUpdateCommand,
        command_record: Option<awaken_session_contract::IdempotencyRecord>,
    ) -> Result<(Session, awaken_session_contract::SessionRevision), StateError> {
        {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(id).ok_or(StateError::NotFound)?;
            if record.session.status != "idle" {
                return Err(StateError::Run(RunError::bad_request(
                    "session agent updates require an idle session; interrupt the active run first",
                )));
            }
        }
        let owner_scope = self
            .sessions_repo
            .owner(id)
            .await
            .ok_or(StateError::NotFound)?;
        let mut persisted = self
            .sessions_repo
            .get(id)
            .await
            .ok_or(StateError::NotFound)?;
        if command
            .if_match
            .is_some_and(|expected| expected != persisted.revision)
        {
            return Err(StateError::Conflict);
        }
        let initial_title = persisted.title.clone();
        let initial_metadata = persisted.metadata.clone();
        let initial_tools = persisted.agent_tools.clone();
        let command_receipt_key = command_record.as_ref().map(|record| record.key.clone());
        let mut mcp_changed = false;
        let mcp_in_request = command.mcp_servers.is_some();
        if let Some(wire_servers) = command.mcp_servers.clone() {
            let baseline = persisted
                .frozen_baseline()
                .ok_or_else(|| {
                    StateError::Run(RunError::classified(
                        "session_not_frozen",
                        "MCP attachments cannot change while the Session baseline is preparing",
                    ))
                })?
                .clone();
            let servers = wire_servers
                .into_iter()
                .map(|server| crate::types::McpServer {
                    name: server.name,
                    url: server.url,
                })
                .collect::<Vec<_>>();
            let drafts = self
                .normalize_mcp_drafts(
                    servers
                        .into_iter()
                        .map(|server| ManagedMcpCandidate {
                            server,
                            published_credential: None,
                            origin: awaken_session_contract::McpAttachmentOrigin::Session,
                        })
                        .collect(),
                    &baseline.mcp_authoring.ordered_vault_ids,
                )
                .await?;

            // Cause graph: current unexpired owner -> reuse lease; otherwise
            // recover old nonterminal projections first. The desired-set CAS is
            // always durable before a new generation claim or Runtime effect.
            let now_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or_default();
            let lease_is_current = persisted.realization.as_ref().is_some_and(|lease| {
                lease.runtime_incarnation == self.runtime_incarnation
                    && awaken_session_contract::realization_lease_is_live_at(
                        lease.expires_at_unix_ms,
                        now_unix_ms,
                    )
            });
            let projection_requires_completion =
                persisted.mcp.attachments.iter().any(|attachment| {
                    matches!(
                        attachment.state,
                        awaken_session_contract::McpAttachmentState::Requested
                            | awaken_session_contract::McpAttachmentState::Realizing
                            | awaken_session_contract::McpAttachmentState::Draining
                    ) || (attachment.state == awaken_session_contract::McpAttachmentState::Active
                        && !attachment.publication_acknowledged)
                });
            let active_requires_new_owner = !lease_is_current
                && persisted.mcp.attachments.iter().any(|attachment| {
                    attachment.state == awaken_session_contract::McpAttachmentState::Active
                });
            if projection_requires_completion || active_requires_new_owner {
                persisted = self.recover_mcp_projections(id).await?;
                mcp_changed = true;
            }

            let plan = persisted
                .mcp
                .request_full_replacement(
                    drafts,
                    Some(baseline.environment.credential_realization.mcp_holder),
                )
                .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
            if plan.changed {
                mcp_changed = true;
                self.commit_session_snapshot(
                    &owner_scope,
                    persisted,
                    "mcp-replacement-intent",
                    Vec::new(),
                )
                .await?;
                persisted = self.realize_session_locally(id).await?;
            }
        }
        let title_in_request = command.title.is_some();
        if let Some(title) = command.title.clone() {
            persisted.title = title;
        }
        if let Some(patch) = command.metadata.clone() {
            match patch {
                None => persisted.metadata.clear(),
                Some(patch) => {
                    for (key, value) in patch {
                        match value {
                            Some(v) => {
                                persisted.metadata.insert(key, v);
                            }
                            None => {
                                persisted.metadata.remove(&key);
                            }
                        }
                    }
                }
            }
        }
        if let Some(tools) = &command.tools {
            persisted.agent_tools = Some(
                tools
                    .iter()
                    .map(|tool| serde_json::to_value(tool).expect("typed AgentTool serializes"))
                    .collect(),
            );
        }
        let title_changed = persisted.title != initial_title;
        let metadata_changed = persisted.metadata != initial_metadata;
        let tools_changed = persisted.agent_tools != initial_tools;
        let semantic_changed = title_changed || metadata_changed || tools_changed || mcp_changed;
        let mut command_applied = true;
        if title_changed || metadata_changed || tools_changed || command_record.is_some() {
            persisted = match command_record {
                Some(record) => {
                    let (committed, applied) = self
                        .commit_session_snapshot_with_record(
                            &owner_scope,
                            persisted,
                            record,
                            Vec::new(),
                        )
                        .await?;
                    command_applied = applied;
                    committed
                }
                None => {
                    self.commit_session_snapshot(&owner_scope, persisted, "update", Vec::new())
                        .await?
                }
            };
        }
        let response_revision = if command_applied {
            persisted.revision
        } else {
            let key = command_receipt_key.as_deref().ok_or_else(|| {
                StateError::Run(RunError::internal(
                    "replayed Session update has no idempotency key",
                ))
            })?;
            self.sessions_repo
                .idempotency_receipt(id, key)
                .await
                .ok_or_else(|| {
                    StateError::Run(RunError::internal(
                        "replayed Session update has no idempotency receipt",
                    ))
                })?
                .committed_revision
        };
        if !semantic_changed || !command_applied {
            return Ok((self.get_session(id)?, response_revision));
        }
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        let visible_mcp_servers = mcp_in_request.then(|| persisted.visible_mcp_servers());
        record.session.title = persisted.title;
        record.session.metadata = persisted.metadata;
        let agent_changed = tools_changed || mcp_changed;
        if let Some(tools) = command.tools.clone() {
            record.session.agent.tools = tools;
        }
        if let Some(visible_mcp_servers) = visible_mcp_servers {
            record.session.agent.mcp_servers =
                super::sessions::typed_mcp_servers(visible_mcp_servers)?;
        }
        record.events.push(Event {
            id: self.next_event_id(),
            kind: OutboundKind::SessionUpdated {
                title: title_in_request
                    .then(|| record.session.title.clone())
                    .flatten(),
                metadata: record.session.metadata.clone(),
                agent: agent_changed.then(|| record.session.agent.clone()),
            },
            processed_at: Some(PROCESSED_AT.to_string()),
        });
        Ok((record.session_projection(), response_revision))
    }
}
