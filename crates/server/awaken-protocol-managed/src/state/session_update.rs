//! Managed Session metadata, tool, and generation-fenced MCP replacement.

use super::sessions::ManagedMcpCandidate;
use super::*;
use crate::types::McpServer;

impl ManagedState {
    const UPDATE_CAS_ATTEMPTS: usize = 3;

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
            if !record.session.mcp.needs_reconciliation() {
                continue;
            }
            let session_id = record.session.session_id.clone();
            match self
                .recover_mcp_projections(&record.workspace_id, record.session)
                .await
            {
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
    pub async fn update_session(
        &self,
        id: &str,
        title: Option<Option<String>>,
        metadata: Option<std::collections::BTreeMap<String, Option<String>>>,
        tools: Option<Vec<serde_json::Value>>,
        mcp_servers: Option<Vec<serde_json::Value>>,
        idempotency_key: Option<String>,
        if_match: Option<awaken_session_contract::SessionRevision>,
    ) -> Result<Session, StateError> {
        let request_hash =
            awaken_session_contract::stable_fingerprint(&(&title, &metadata, &tools, &mcp_servers));
        let command_record =
            idempotency_key
                .as_ref()
                .map(|key| awaken_session_contract::IdempotencyRecord {
                    key: format!(
                        "managed:update-command:{id}:{}",
                        Self::update_operation_id(id, key)
                    ),
                    payload_hash: request_hash.clone(),
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
            return self.get_session(id);
        }

        for attempt in 0..Self::UPDATE_CAS_ATTEMPTS {
            match self
                .update_session_once(
                    id,
                    title.clone(),
                    metadata.clone(),
                    tools.clone(),
                    mcp_servers.clone(),
                    command_record.clone(),
                    if_match,
                )
                .await
            {
                Err(StateError::Conflict)
                    if if_match.is_none() && attempt + 1 < Self::UPDATE_CAS_ATTEMPTS =>
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
        title: Option<Option<String>>,
        metadata: Option<std::collections::BTreeMap<String, Option<String>>>,
        tools: Option<Vec<serde_json::Value>>,
        mcp_servers: Option<Vec<serde_json::Value>>,
        command_record: Option<awaken_session_contract::IdempotencyRecord>,
        if_match: Option<awaken_session_contract::SessionRevision>,
    ) -> Result<Session, StateError> {
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
        if if_match.is_some_and(|expected| expected != persisted.revision) {
            return Err(StateError::Conflict);
        }
        let initial_title = persisted.title.clone();
        let initial_metadata = persisted.metadata.clone();
        let initial_tools = persisted.agent_tools.clone();
        let mut mcp_changed = false;
        let mcp_in_request = mcp_servers.is_some();
        if let Some(wire_servers) = mcp_servers {
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
                .map(|server| {
                    serde_json::from_value::<McpServer>(server).map_err(|error| {
                        StateError::Run(RunError::bad_request(format!(
                            "invalid agent.mcp_servers entry: {error}"
                        )))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
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
                    && lease.expires_at_unix_ms > now_unix_ms
            });
            let projection_requires_completion =
                persisted.mcp.attachments.iter().any(|attachment| {
                    matches!(
                        attachment.state,
                        awaken_session_contract::McpAttachmentState::Requested
                            | awaken_session_contract::McpAttachmentState::Realizing
                    ) || (attachment.state == awaken_session_contract::McpAttachmentState::Active
                        && !attachment.publication_acknowledged)
                });
            let active_requires_new_owner = !lease_is_current
                && persisted.mcp.attachments.iter().any(|attachment| {
                    attachment.state == awaken_session_contract::McpAttachmentState::Active
                });
            if projection_requires_completion || active_requires_new_owner {
                persisted = self
                    .recover_mcp_projections(&owner_scope, persisted)
                    .await?;
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
                persisted = self
                    .commit_session_snapshot(
                        &owner_scope,
                        persisted,
                        "mcp-replacement-intent",
                        Vec::new(),
                    )
                    .await?;
            }

            if !plan.requested.is_empty() {
                let now_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
                    .unwrap_or_default();
                let lease_epoch = persisted.realization.as_ref().map_or(1, |lease| {
                    if lease.runtime_incarnation == self.runtime_incarnation
                        && lease.expires_at_unix_ms > now_unix_ms
                    {
                        lease.epoch
                    } else {
                        lease.epoch.saturating_add(1)
                    }
                });
                let lease_expires_at_unix_ms = now_unix_ms.saturating_add(300_000);
                persisted.realization = Some(awaken_session_contract::SessionRealizationLease {
                    owner: "managed-runtime".into(),
                    runtime_incarnation: self.runtime_incarnation.clone(),
                    epoch: lease_epoch,
                    expires_at_unix_ms: lease_expires_at_unix_ms,
                });
                let mut realization_ids = Vec::with_capacity(plan.requested.len());
                for (attachment_id, generation) in &plan.requested {
                    let realization_id = awaken_session_contract::stable_fingerprint(&(
                        id,
                        attachment_id,
                        generation,
                        &self.runtime_incarnation,
                        lease_epoch,
                    ));
                    persisted
                        .mcp
                        .claim_realization(
                            attachment_id,
                            *generation,
                            awaken_session_contract::McpRealizationClaim {
                                realization_id: realization_id.clone(),
                                runtime_incarnation: self.runtime_incarnation.clone(),
                                lease_epoch,
                                lease_expires_at_unix_ms,
                                stage_idempotency_key: format!(
                                    "stage:{id}:{}:{}",
                                    attachment_id.0, generation.0
                                ),
                            },
                        )
                        .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
                    realization_ids.push((attachment_id.clone(), *generation, realization_id));
                }
                persisted = self
                    .commit_session_snapshot(
                        &owner_scope,
                        persisted,
                        "mcp-replacement-claim",
                        Vec::new(),
                    )
                    .await?;
                let staged = match self
                    .stage_claimed_mcp_generations(&owner_scope, &persisted, &plan.requested)
                    .await
                {
                    Ok(staged) => staged,
                    Err(error) => {
                        for (attachment_id, generation, realization_id) in realization_ids {
                            persisted
                                .mcp
                                .fail_realization(
                                    &attachment_id,
                                    generation,
                                    &realization_id,
                                    error.to_string(),
                                )
                                .map_err(|state_error| {
                                    StateError::Run(RunError::internal(state_error.to_string()))
                                })?;
                        }
                        self.commit_session_snapshot(
                            &owner_scope,
                            persisted,
                            "mcp-replacement-failed",
                            Vec::new(),
                        )
                        .await?;
                        return Err(StateError::Run(error));
                    }
                };
                for (attachment_id, generation, realization_id) in &realization_ids {
                    persisted
                        .mcp
                        .activate(attachment_id, *generation, realization_id)
                        .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
                }
                persisted
                    .mcp
                    .begin_obsolete_drains()
                    .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
                persisted = match self
                    .commit_session_snapshot(
                        &owner_scope,
                        persisted,
                        "mcp-replacement-activate",
                        Vec::new(),
                    )
                    .await
                {
                    Ok(persisted) => persisted,
                    Err(error) => {
                        for generation in staged {
                            let _ = self.runtime.drain_mcp_generation(generation).await;
                        }
                        return Err(error);
                    }
                };
                for generation in &staged {
                    self.runtime
                        .publish_mcp_generation(generation.clone())
                        .await
                        .map_err(StateError::Run)?;
                }
                for (attachment_id, generation, realization_id) in &realization_ids {
                    persisted
                        .mcp
                        .acknowledge_publication(attachment_id, *generation, realization_id)
                        .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
                }
                persisted = self
                    .commit_session_snapshot(
                        &owner_scope,
                        persisted,
                        "mcp-replacement-publication",
                        Vec::new(),
                    )
                    .await?;
            }

            mcp_changed |= persisted.mcp.attachments.iter().any(|attachment| {
                attachment.state == awaken_session_contract::McpAttachmentState::Draining
            });
            persisted = self
                .drain_persisted_mcp_generations(&owner_scope, persisted)
                .await?;
        }
        let title_in_request = title.is_some();
        if let Some(title) = title {
            persisted.title = title;
        }
        if let Some(patch) = metadata {
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
        if let Some(tools) = &tools {
            persisted.agent_tools = Some(tools.clone());
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
        if !semantic_changed || !command_applied {
            return self.get_session(id);
        }
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        let visible_mcp_servers = mcp_in_request.then(|| persisted.visible_mcp_servers());
        record.session.title = persisted.title;
        record.session.metadata = persisted.metadata;
        let agent_changed = tools_changed || mcp_changed;
        if let Some(tools) = tools {
            record.session.agent.tools = tools;
        }
        if let Some(visible_mcp_servers) = visible_mcp_servers {
            record.session.agent.mcp_servers = visible_mcp_servers;
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
        Ok(record.session_projection())
    }
}
