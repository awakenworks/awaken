//! Session lifecycle for [`ManagedState`]: create, rehydrate, get/list,
//! update, delete, and archive.

use super::application::{ManagedMcpCandidate, initial_mcp_candidates};
use super::*;

pub(super) fn typed_mcp_servers(
    values: Vec<awaken_session_contract::VisibleMcpServer>,
) -> Vec<crate::types::agent::UrlMcpServer> {
    values
        .into_iter()
        .map(|server| crate::types::agent::UrlMcpServer {
            name: server.name,
            url: server.url,
            kind: crate::types::agent::UrlMcpServerKind::Url,
            prompts_as_skills: server.prompts_as_skills,
        })
        .collect()
}
use crate::types::AgentRef;

fn validate_session_skill_total(
    source: Option<&dyn awaken_session_contract::AgentConfigSource>,
    workspace_id: &str,
    root_agent_id: &str,
    root_view: Option<&awaken_session_contract::AgentConfigView>,
    root_skills: &[awaken_agent_contract::AgentSkillBinding],
) -> Result<(), RunError> {
    const MAX_SESSION_SKILLS: usize = 500;

    let mut total = root_skills.len();
    if total > MAX_SESSION_SKILLS {
        return Err(RunError::bad_request(
            "a session supports at most 500 skills across all agents",
        ));
    }

    // Agent identity, rather than roster-edge count, owns one mounted Skill set.
    // This also closes cycles in malformed/legacy published topologies.
    let mut seen = std::collections::BTreeSet::from([root_agent_id.to_string()]);
    let mut pending = root_view
        .into_iter()
        .flat_map(|view| view.delegate_ids.iter().cloned())
        .collect::<std::collections::VecDeque<_>>();
    while let Some(agent_id) = pending.pop_front() {
        if !seen.insert(agent_id.clone()) {
            continue;
        }
        let Some(view) = source.and_then(|source| source.agent_view_in(workspace_id, &agent_id))
        else {
            continue;
        };
        total = total.saturating_add(view.skills.len());
        if total > MAX_SESSION_SKILLS {
            return Err(RunError::bad_request(
                "a session supports at most 500 skills across all agents",
            ));
        }
        pending.extend(view.delegate_ids);
    }
    Ok(())
}

fn validate_sandbox_provisioning_runtime(
    provisioning: awaken_provisioning_contract::SandboxProvisioning,
    runtime: Option<&str>,
) -> Result<(), RunError> {
    if provisioning == awaken_provisioning_contract::SandboxProvisioning::OnToolUse
        && !matches!(runtime, None | Some("awaken"))
    {
        return Err(RunError::bad_request(format!(
            "sandbox_provisioning_unsupported: `on_tool_use` requires the native awaken runtime, got `{}`",
            runtime.unwrap_or_default()
        )));
    }
    Ok(())
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

impl ManagedState {
    pub fn validate_dream_agent(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<(), StateError> {
        if agent_id == crate::dream::BUILT_IN_DREAM_AGENT_ID {
            return Ok(());
        }
        let Some(source) = &self.config_source else {
            return Err(StateError::Run(RunError::bad_request(format!(
                "dream agent Agent `{agent_id}` is unavailable"
            ))));
        };
        if source.agent_view_in(workspace_id, agent_id).is_none()
            || source.agent_unavailable_in(workspace_id, agent_id)
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "dream agent Agent `{agent_id}` is unavailable"
            ))));
        }
        Ok(())
    }

    /// Frozen committed Session input used by the dream
    /// application. Workspace ownership is checked before the Runtime transcript
    /// is read; the returned messages preserve tool calls and tool results exactly
    /// as committed by the ordinary Session authority.
    pub async fn dream_transcript(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Vec<awaken_agent_contract::agent::message::Message>, StateError> {
        self.ensure_session(session_id).await?;
        let owner = self
            .resolve_owner(session_id)
            .await
            .ok_or(StateError::NotFound)?;
        if owner != workspace_id {
            return Err(StateError::NotFound);
        }
        Ok(self.runtime.committed_messages(session_id).await)
    }

    pub async fn prepare_protocol_session(
        &self,
        workspace_id: &str,
        thread_id: &str,
        agent_id: &str,
    ) -> Result<(), StateError> {
        if self.sessions_repo.get(thread_id).await.is_some() {
            // A durable row does not imply that this process has reconstructed
            // the runtime projection.  Reuse the authoritative restart path so
            // resources and the exact Environment snapshot are restored before
            // a non-Managed adapter is allowed to execute the next turn.
            return self.ensure_session(thread_id).await;
        }
        let request = serde_json::from_value(serde_json::json!({"agent": agent_id}))
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        match self
            .create_session_with_identity(
                request,
                Some(workspace_id.to_string()),
                Some(thread_id.to_string()),
            )
            .await
        {
            Ok(_) => Ok(()),
            // Concurrent first turns share the explicit protocol thread id. The
            // durable create fence chooses one winner; every loser adopts that
            // exact committed Session through the normal recovery seam.
            Err(StateError::Conflict) if self.sessions_repo.get(thread_id).await.is_some() => {
                self.ensure_session(thread_id).await
            }
            Err(error) => Err(error),
        }
    }

    /// Bounded retry for a root Session CAS. A retry always reloads the
    /// aggregate and reruns the command's domain checks; stale snapshots are
    /// never merged wholesale.
    pub(super) const ROOT_CAS_ATTEMPTS: usize = 3;

    fn wire_session_status(status: &str) -> &'static str {
        match status {
            "preparing" => "preparing",
            "activating" => "activating",
            "activation_failed" => "failed",
            "terminated" => "terminated",
            _ => "idle",
        }
    }

    /// Refresh the disposable HTTP projection after the one durable root CAS.
    /// Every mutation crosses this seam, so realization, update, archive, and
    /// recovery cannot each invent a second cache-synchronization path.
    fn refresh_cached_projection(&self, persisted: &PersistedSession) -> Result<(), StateError> {
        let mcp_servers = typed_mcp_servers(persisted.visible_mcp_servers());
        let mut sessions = self.sessions.lock().unwrap();
        let Some(record) = sessions.get_mut(&persisted.session_id) else {
            return Ok(());
        };
        record.session.status = Self::wire_session_status(&persisted.status);
        record.session.title = persisted.title.clone();
        record.session.metadata = persisted.metadata.clone();
        record.session.deployment_id = persisted.metadata.get("awaken.deployment_id").cloned();
        record.session.archived_at = persisted.archived_at.clone();
        record.session.agent.mcp_servers = mcp_servers;
        record.resource_state = persisted.resources.clone();
        Ok(())
    }

    /// Sole Managed anti-corruption compiler for create-time and hot MCP input.
    /// URL identity, Vault ordering and exact credential pinning cannot be
    /// repeated by either caller after this function returns.
    pub(super) async fn normalize_mcp_drafts(
        &self,
        candidates: Vec<ManagedMcpCandidate>,
        ordered_vault_ids: &[String],
    ) -> Result<Vec<awaken_session_contract::McpAttachmentDraft>, StateError> {
        let mut drafts = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let server = candidate.server;
            let credential = match candidate.published_credential {
                Some((id, revision)) => {
                    let source_id = awaken_credential_vault::CredentialSourceId(id.clone());
                    let access = if let Some(vaults) = &self.vaults {
                        let access =
                            vaults
                                .mcp_access_for_source(&source_id)
                                .await
                                .map_err(|error| {
                                    StateError::Run(RunError::bad_request(format!(
                                        "MCP credential could not be pinned exactly: {error}"
                                    )))
                                })?;
                        if access.credential.revision != revision {
                            return Err(StateError::Run(RunError::bad_request(
                                "published MCP credential revision no longer matches",
                            )));
                        }
                        access
                    } else {
                        awaken_credential_contract::CredentialAccess::new(
                            awaken_credential_contract::CredentialRef { id, revision },
                            awaken_credential_contract::CredentialMaterialSource::ControlPlaneReference,
                            awaken_credential_contract::CredentialUsage::HttpHeader {
                                name: "authorization".into(),
                                scheme: Some("Bearer".into()),
                            },
                            awaken_credential_contract::CredentialExecutionPolicy::self_hosted_provider(),
                        )
                    };
                    Some(access)
                }
                None => match &self.vaults {
                    Some(vaults) => {
                        match vaults.mcp_credential_source_for_url(ordered_vault_ids, &server.url) {
                            Some(source_id) => {
                                Some(vaults.mcp_access_for_source(&source_id).await.map_err(
                                    |error| {
                                        StateError::Run(RunError::bad_request(format!(
                                            "MCP credential could not be pinned exactly: {error}"
                                        )))
                                    },
                                )?)
                            }
                            None => None,
                        }
                    }
                    None => None,
                },
            };
            let target =
                awaken_session_contract::McpTarget::parse_http(&server.url).map_err(|_| {
                    StateError::Run(RunError::bad_request(format!(
                        "invalid MCP server URL for `{}`",
                        server.name
                    )))
                })?;
            drafts.push(awaken_session_contract::McpAttachmentDraft {
                name: server.name,
                target,
                prompts_as_skills: server.prompts_as_skills,
                credential,
                origin: candidate.origin,
            });
        }
        Ok(drafts)
    }

    /// Rebuild the process-local projection through the same phase driver used
    /// by creation and hot replacement. Recovery is a trigger, not a second
    /// realization algorithm.
    pub(super) async fn recover_mcp_projections(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, StateError> {
        let session = self
            .sessions_repo
            .get(session_id)
            .await
            .ok_or(StateError::NotFound)?;
        if !session.mcp.needs_reconciliation() {
            return Ok(session);
        }
        self.realize_session_locally(session_id).await
    }

    /// The sole application-layer compiler for an existing Session write. Every
    /// specialized command builds a complete replacement, then crosses this root
    /// CAS seam; no caller retries by writing a stale aggregate snapshot.
    pub(crate) async fn commit_session_snapshot(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        operation: &str,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<PersistedSession, StateError> {
        let expected_revision = session.revision;
        let payload = awaken_session_contract::SessionMutationPayload::Replace(session.clone());
        let payload_hash = payload.stable_hash();
        let idempotency = awaken_session_contract::IdempotencyRecord {
            key: format!(
                "managed:{operation}:{}:{}:{payload_hash}",
                session.session_id, expected_revision.0
            ),
            payload_hash,
        };
        self.commit_session_snapshot_with_record(owner_scope, session, idempotency, lifecycle_facts)
            .await
            .map(|(session, _)| session)
    }

    pub(super) async fn commit_session_snapshot_with_record(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        idempotency: awaken_session_contract::IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<(PersistedSession, bool), StateError> {
        let expected_revision = session.revision;
        let payload = awaken_session_contract::SessionMutationPayload::Replace(session.clone());
        let mutation = awaken_session_contract::SessionMutation {
            expected_revision,
            idempotency,
            payload,
            lifecycle_facts,
        };
        match self
            .sessions_repo
            .commit_mutation(owner_scope, mutation)
            .await
            .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?
        {
            awaken_session_contract::SessionMutationResult::Applied { new_revision } => {
                session.revision = new_revision;
                self.refresh_cached_projection(&session)?;
                Ok((session, true))
            }
            awaken_session_contract::SessionMutationResult::Replayed { .. } => {
                let session = self
                    .sessions_repo
                    .get(&session.session_id)
                    .await
                    .ok_or(StateError::NotFound)?;
                self.refresh_cached_projection(&session)?;
                Ok((session, false))
            }
            awaken_session_contract::SessionMutationResult::Conflict { .. } => {
                Err(StateError::Conflict)
            }
            awaken_session_contract::SessionMutationResult::IdempotencyMismatch => {
                Err(StateError::IdempotencyMismatch)
            }
        }
    }

    async fn create_session_snapshot(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
    ) -> Result<PersistedSession, StateError> {
        let payload = awaken_session_contract::SessionMutationPayload::Replace(session.clone());
        let payload_hash = payload.stable_hash();
        let revision = self
            .sessions_repo
            .create(
                owner_scope,
                session.clone(),
                awaken_session_contract::IdempotencyRecord {
                    key: format!("managed:create:{}:{payload_hash}", session.session_id),
                    payload_hash,
                },
                Vec::new(),
            )
            .await
            .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
        session.revision = revision;
        Ok(session)
    }

    async fn tombstone_session_snapshot(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
        fact: ManagedLifecycleFact,
    ) -> Result<(), StateError> {
        let deleted_revision =
            awaken_session_contract::SessionRevision(
                session.revision.0.checked_add(1).ok_or_else(|| {
                    StateError::Run(RunError::internal("Session revision exhausted"))
                })?,
            );
        let payload = awaken_session_contract::SessionMutationPayload::Delete(
            awaken_session_contract::SessionTombstone {
                session_id: session.session_id.clone(),
                deleted_revision,
                deleted_at: fact.timestamp.to_string(),
            },
        );
        let payload_hash = payload.stable_hash();
        let mutation = awaken_session_contract::SessionMutation {
            expected_revision: session.revision,
            idempotency: awaken_session_contract::IdempotencyRecord {
                key: format!(
                    "managed:delete:{}:{}:{payload_hash}",
                    session.session_id, session.revision.0
                ),
                payload_hash,
            },
            payload,
            lifecycle_facts: vec![fact],
        };
        match self
            .sessions_repo
            .commit_mutation(owner_scope, mutation)
            .await
            .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?
        {
            awaken_session_contract::SessionMutationResult::Applied { .. }
            | awaken_session_contract::SessionMutationResult::Replayed { .. } => Ok(()),
            awaken_session_contract::SessionMutationResult::Conflict { .. } => {
                Err(StateError::Conflict)
            }
            awaken_session_contract::SessionMutationResult::IdempotencyMismatch => Err(
                StateError::Run(RunError::internal("Session idempotency mismatch")),
            ),
        }
    }

    /// `POST /v1/sessions`.
    ///
    /// MCP binding (ADR-0043 Phase 3): each requested server is bound to a vault
    /// credential by exact `mcp_server_url` match across the request's
    /// `vault_ids`. The preparation intent, frozen generation-1 state, and exact
    /// realization claim all commit before [`SessionRuntime::prepare_session`]
    /// performs external I/O. A failed realization leaves recoverable failed
    /// state and fails the create (the router maps the `RunError` to the error
    /// envelope). A `vault_ids` entry that names no
    /// existing vault fails the create closed too ([`VaultState::has_vault`]):
    /// a 404 naming the vault id, BEFORE anything is provisioned — never a
    /// silent no-binding whose 401 only surfaces at the first turn. (Without a
    /// wired vault surface there is nothing to validate against and every
    /// binding resolves to no credential, as before.)
    /// Fail-closed bind-time legality check, shared by session creation and any
    /// pre-flight bind check: every vault a session references must exist. This is
    /// the one validation that must hold *before* an id is minted or a thread is
    /// prepared, so it lives in a single method rather than inline — a dry-run
    /// bind check calls exactly this, and gets exactly the error create would.
    pub fn check_bind(&self, req: &SessionCreateParams) -> Result<(), StateError> {
        if let Some(vaults) = &self.vaults
            && let Some(unknown) = req.vault_ids.iter().find(|v| !vaults.has_vault(v))
        {
            return Err(StateError::VaultNotFound(unknown.clone()));
        }
        Ok(())
    }

    pub async fn create_session(
        &self,
        req: SessionCreateParams,
        // The edge-resolved owning workspace (aspect): handed to the lifecycle
        // sink for webhook/usage stamping, but NEVER stored on the core session.
        workspace_id: Option<String>,
    ) -> Result<Session, StateError> {
        self.create_session_with_identity(req, workspace_id, None)
            .await
    }

    /// The canonical public create command: create the Session and enqueue its
    /// admitted initial Events through the ordinary Event command. Protocol
    /// handlers and Deployment launchers share this method so neither can create
    /// an inert Session or invent a follow-up Event path.
    pub async fn create_session_with_initial_events(
        self: &Arc<Self>,
        req: SessionCreateParams,
        workspace_id: Option<String>,
    ) -> Result<Session, StateError> {
        let initial_events = req.initial_events.clone();
        let mut session = self.create_session(req, workspace_id).await?;
        if !initial_events.is_empty() {
            self.start_initial_events(&session.id, initial_events)?;
            session.status = "running";
        }
        Ok(session)
    }

    /// Create the Control-owned half of an externally dispatched application
    /// Session under the dispatcher's exact durable thread identity.
    ///
    /// Only application-contribution Sessions may cross this embedding seam:
    /// their Runtime projection is realized by the claim-owning Worker, while
    /// this aggregate remains the sole author of the frozen baseline and
    /// realization generations. Ordinary public Session creation continues to
    /// mint its own identity through [`Self::create_session`].
    pub async fn create_application_session(
        &self,
        session_id: impl Into<String>,
        req: SessionCreateParams,
        workspace_id: Option<String>,
    ) -> Result<Session, StateError> {
        let session_id = session_id.into();
        if session_id.trim().is_empty() {
            return Err(StateError::Run(RunError::bad_request(
                "application Session id is empty",
            )));
        }
        if !req.application_contribution_required {
            return Err(StateError::Run(RunError::bad_request(
                "externally identified Session requires an application contribution",
            )));
        }
        self.create_session_with_identity(req, workspace_id, Some(session_id))
            .await
    }

    async fn create_session_with_identity(
        &self,
        req: SessionCreateParams,
        workspace_id: Option<String>,
        explicit_id: Option<String>,
    ) -> Result<Session, StateError> {
        // Initial-event admission is atomic with Session creation: validate the
        // complete batch before bind checks, identity allocation, persistence, or
        // Runtime preparation. The shared validator is also used by Deployments.
        req.validate_initial_events()
            .map_err(|message| StateError::Run(RunError::bad_request(message)))?;
        self.check_bind(&req)?;
        // Mint an id no durable thread already owns: a fresh process restarts
        // the sequence at 0, but the store dir may hold committed truth from a
        // previous process (ADR-0039). Adopting such a thread would graft the
        // old transcript onto a NEW session, so skip forward instead — the
        // rehydration path (`ensure_session`) remains the only way to reattach
        // to an existing thread, and it is keyed by the caller's explicit id.
        let id = match explicit_id {
            Some(id) => id,
            None => loop {
                let candidate = format!("sesn_{}", self.session_seq.fetch_add(1, Ordering::SeqCst));
                if !self.runtime.owns_thread(&candidate).await
                    && self.sessions_repo.get(&candidate).await.is_none()
                {
                    break candidate;
                }
            },
        };
        let agent_id = req.agent.id().to_string();
        let owner_scope = workspace_id
            .clone()
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
        let config_view = self
            .config_source
            .as_ref()
            .and_then(|source| source.agent_view_in(&owner_scope, &agent_id));
        let is_built_in_dream_agent = agent_id == crate::dream::BUILT_IN_DREAM_AGENT_ID
            && req
                .metadata
                .get("awaken.session.origin")
                .is_some_and(|origin| origin == "dream");
        if config_view.is_none()
            && self
                .config_source
                .as_ref()
                .is_some_and(|source| source.agent_unavailable_in(&owner_scope, &agent_id))
            && !is_built_in_dream_agent
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "agent_unavailable: agent `{agent_id}` cannot start a new session"
            ))));
        }
        // Cause/effect decision table for Session model authority:
        // R1 no override + published Agent -> inherit its complete publication;
        // R2 equal official override -> accept without changing the route;
        // R3 different official override + published Agent -> reject before state;
        // R4 cleared override -> reject. Metadata never selects execution.
        // This prevents a model string from being stitched to the Agent's old
        // backend/credential pins. Selecting another route requires publishing an
        // Agent for that Managed model id first.
        let selected_model: Option<ModelConfig> = match req.agent.model_override() {
            ModelOverride::Set(cfg) => {
                if config_view
                    .as_ref()
                    .and_then(|view| view.model.as_deref())
                    .is_some_and(|published| published != cfg.id)
                {
                    return Err(StateError::Run(RunError::bad_request(
                        "agent_model_override_unpublished: publish or update an Agent with this model id before creating the Session",
                    )));
                }
                Some(cfg)
            }
            ModelOverride::Cleared => {
                return Err(StateError::Run(RunError::bad_request(
                    "agent_model_required: a session override cannot clear `model`",
                )));
            }
            ModelOverride::Absent => config_view
                .as_ref()
                .and_then(|view| view.model.clone())
                .map(ModelConfig::new),
        };
        // Echo the agent version the client pinned (or overrode over), defaulting to 1.
        let agent_version = req.agent.version().unwrap_or(1);
        let delegate_ids = config_view
            .as_ref()
            .map(|view| view.delegate_ids.clone())
            .unwrap_or_default();
        // Anthropic requires MCP declarations and toolsets to be a bijective
        // reference: every declared server has a toolset and every toolset names
        // a declared server. Validate create-time overrides before provisioning.
        if let AgentRef::Object(override_ref) = &req.agent
            && let (Some(Some(mcp_servers)), Some(Some(tools))) =
                (&override_ref.mcp_servers, &override_ref.tools)
        {
            let declared = mcp_servers
                .iter()
                .map(|server| server.name.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let toolset_names = tools
                .iter()
                .filter_map(|tool| match tool {
                    crate::types::agent::AgentTool::McpToolset {
                        mcp_server_name, ..
                    } => Some(mcp_server_name.as_str()),
                    _ => None,
                })
                .collect::<std::collections::BTreeSet<_>>();
            if declared != toolset_names {
                return Err(StateError::Run(RunError::bad_request(
                    "each mcp_server must be referenced by exactly one mcp_toolset",
                )));
            }
        }
        let agent_mcp_override = match &req.agent {
            AgentRef::Object(override_ref) => override_ref.mcp_servers.as_ref().map(|servers| {
                servers
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|server| crate::types::McpServer {
                        name: server.name.clone(),
                        url: server.url.clone(),
                        prompts_as_skills: server.prompts_as_skills,
                    })
                    .collect::<Vec<_>>()
            }),
            AgentRef::Id(_) => None,
        };
        let mcp_drafts = self
            .normalize_mcp_drafts(
                initial_mcp_candidates(
                    &req.mcp_servers,
                    config_view.as_ref(),
                    agent_mcp_override.as_deref(),
                ),
                &req.vault_ids,
            )
            .await?;
        // Parse the wire `resources[]` (ADR-0038) into staged mounts, and project each
        // into a DTO entry so the created session echoes its create-time resources —
        // list/get/delete then address these and any later-attached ones uniformly.
        const MAX_SESSION_MEMORY_STORES: usize = 8;
        const MAX_MEMORY_INSTRUCTIONS_CHARS: usize = 4_096;
        let memory_resources = req.resources.iter().filter_map(|resource| match resource {
            crate::types::resource::ResourceInput::MemoryStore { instructions, .. } => {
                Some(instructions)
            }
            _ => None,
        });
        let mut memory_count = 0usize;
        for instructions in memory_resources {
            memory_count += 1;
            if instructions.as_ref().is_some_and(|instructions| {
                instructions.chars().count() > MAX_MEMORY_INSTRUCTIONS_CHARS
            }) {
                return Err(StateError::Run(RunError::bad_request(format!(
                    "memory store instructions support at most {MAX_MEMORY_INSTRUCTIONS_CHARS} characters"
                ))));
            }
        }
        if memory_count > MAX_SESSION_MEMORY_STORES {
            return Err(StateError::Run(RunError::bad_request(format!(
                "a Session supports at most {MAX_SESSION_MEMORY_STORES} memory stores"
            ))));
        }
        let resources = req
            .resources
            .iter()
            .map(crate::types::resource::ResourceInput::to_parsed_input)
            .collect::<Vec<_>>();
        if resources
            .iter()
            .filter(|resource| matches!(resource.target, ParsedInputTarget::File(_)))
            .count()
            > super::resource::MAX_SESSION_FILE_RESOURCES
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "a Session supports at most {} files",
                super::resource::MAX_SESSION_FILE_RESOURCES
            ))));
        }
        // Lower compatibility Repository URLs/tokens before the neutral resolver:
        // the catalog receives a Session-scoped definition and a Vault reference,
        // never the token. File/Memory already carry platform identities on wire.
        let agent_defaults = config_view
            .as_ref()
            .map(|view| view.resources.as_slice())
            .unwrap_or_default();
        let mut attachments = Vec::with_capacity(resources.len());
        for (index, resource) in resources.iter().enumerate() {
            let repository_id = if let ParsedInputTarget::Repository {
                remote_url,
                credential_binding,
                initial_branch,
                initial_commit,
            } = &resource.target
            {
                let catalog = self.resource_catalog.as_ref().ok_or_else(|| {
                    StateError::Run(RunError::bad_request(
                        "repository resources require a configured Resource Catalog",
                    ))
                })?;
                let repository_id = format!("managed:{id}:repository:{index}");
                catalog
                    .create_repository(
                        awaken_resource_contract::RepositoryDefinition {
                            id: repository_id.clone().into(),
                            workspace_id: owner_scope.clone(),
                            name: format!("Session repository {index}"),
                            description: "Managed compatibility Session input".into(),
                            metadata: Default::default(),
                            state: awaken_resource_contract::ResourceState::Active,
                            current_config_version:
                                awaken_resource_contract::ConfigVersion::INITIAL,
                            timestamps: Default::default(),
                        },
                        awaken_resource_contract::RepositoryConfigVersion {
                            repository_id: repository_id.clone().into(),
                            version: awaken_resource_contract::ConfigVersion::INITIAL,
                            remote_url: remote_url.clone(),
                            credential_binding: credential_binding.clone(),
                            initial_branch: initial_branch.clone(),
                            initial_commit: initial_commit.clone(),
                            clone_policy: awaken_resource_contract::ClonePolicy::default(),
                        },
                    )
                    .map_err(|error| {
                        StateError::Run(RunError::bad_request(format!(
                            "repository resource could not be configured: {error}"
                        )))
                    })?;
                Some(awaken_resource_contract::RepositoryId::from(repository_id))
            } else {
                None
            };
            let binding = input_binding(
                format!("session:{id}:input:{index}"),
                resource,
                repository_id,
            );
            let normalized = binding.mount_path.trim_start_matches('/');
            let replaces = agent_defaults
                .iter()
                .find(|default| default.mount_path.trim_start_matches('/') == normalized)
                .map(|default| default.binding_id.clone());
            attachments.push(awaken_session_contract::SessionInputAttachment { binding, replaces });
        }
        // Resolve the session's environment (defaulting to the local one) and its
        // networking policy once, for both the SessionInit (staged before the first
        // turn) and the echoed Session object.
        let agent_environment = config_view
            .as_ref()
            .and_then(|view| view.environment.as_ref());
        // The immutable Agent publication is the only backend authority.  The
        // baseline persists this projection so recovery and Worker placement do
        // not need to reopen the publication registry.
        let published_backend_ref = config_view
            .as_ref()
            .map(|view| view.backend_ref.clone())
            .filter(|backend_ref| !backend_ref.trim().is_empty());
        let environment_id = req
            .environment_id
            .clone()
            .or_else(|| agent_environment.map(|binding| binding.environment_id.clone()))
            .unwrap_or_else(|| "env_local".to_string());
        let mcp_targets = mcp_drafts
            .iter()
            .map(|draft| draft.target.clone())
            .collect::<Vec<_>>();
        let environment = match self.environments.as_ref() {
            Some(environments) => {
                let snapshot = match (req.environment_id.as_ref(), agent_environment) {
                    (None, Some(binding)) => {
                        environments
                            .snapshot_exact_for_session(
                                &binding.environment_id,
                                binding.revision,
                                published_backend_ref.as_deref(),
                                &mcp_targets,
                            )
                            .await
                    }
                    _ => {
                        environments
                            .snapshot_for_session(
                                &environment_id,
                                published_backend_ref.as_deref(),
                                &mcp_targets,
                            )
                            .await
                    }
                };
                snapshot.ok_or_else(|| {
                    StateError::Run(RunError::bad_request(format!(
                        "environment `{environment_id}` is unavailable"
                    )))
                })?
            }
            None => crate::routes::environments::default_environment_snapshot(
                environment_id.clone(),
                published_backend_ref.as_deref(),
            ),
        };
        validate_sandbox_provisioning_runtime(
            environment.sandbox_provisioning,
            req.awaken_runtime(),
        )
        .map_err(StateError::Run)?;
        // Sole protocol-neutral composition/resolution point. Runtime receives this
        // persisted, secret-free result and never re-opens Agent or Resource stores.
        let compiled_defaults = awaken_session_contract::SessionDefaultsCompiler::compile(
            &owner_scope,
            self.resource_catalog
                .as_deref()
                .map(|catalog| catalog as &dyn awaken_resource_contract::ResourceConfigSource),
            environment,
            agent_defaults,
            &attachments,
        )
        .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        let environment = compiled_defaults.environment;
        let mut resolved_resources = compiled_defaults.resources;
        let effective_skills = match &req.agent {
            AgentRef::Object(override_ref) => override_ref.skills.as_ref().map(|skills| {
                skills
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(crate::types::agent::AgentSkill::into_binding)
                    .collect::<Vec<_>>()
            }),
            AgentRef::Id(_) => None,
        }
        .or_else(|| config_view.as_ref().map(|view| view.skills.clone()));
        validate_session_skill_total(
            self.config_source.as_deref(),
            &owner_scope,
            &agent_id,
            config_view.as_ref(),
            effective_skills.as_deref().unwrap_or_default(),
        )
        .map_err(StateError::Run)?;
        if let Some(skills) = &effective_skills {
            resolved_resources.skills = Some(
                self.runtime
                    .resolve_session_skills(&owner_scope, skills)
                    .await
                    .map_err(StateError::Run)?,
            );
        }
        self.pin_repository_credentials(
            &owner_scope,
            &environment.credential_realization.resource_holder,
            &mut resolved_resources,
        )
        .await?;
        // Validate the advertised tool surface before persisting an activation or
        // touching a Host. A definition error cannot strand Prepared resources.
        let caps = self.runtime.capabilities_for(&id);
        for tool in &caps.custom_tools {
            project::validate_custom_tool(tool)
                .map_err(|msg| StateError::Run(RunError::bad_request(msg)))?;
        }
        let inherited_toolsets = config_view
            .as_ref()
            .filter(|view| !view.toolsets.is_empty())
            .map(|view| view.toolsets.clone())
            .unwrap_or_else(|| project::toolset_policies(&project::agent_tools(&caps)));
        let effective_toolsets = match &req.agent {
            AgentRef::Object(reference) => reference
                .tools
                .as_ref()
                .map(|tools| project::toolset_policies(tools.as_deref().unwrap_or_default()))
                .unwrap_or(inherited_toolsets),
            AgentRef::Id(_) => inherited_toolsets,
        };
        let initial_agent_toolsets = project::resolved_toolsets(&effective_toolsets);
        let resolved_model = selected_model
            .clone()
            .unwrap_or_else(|| ModelConfig::new(self.runtime.model()));
        let execution_model_ref = config_view
            .as_ref()
            .and_then(|view| view.execution_model_ref.clone())
            .unwrap_or_else(|| resolved_model.id.clone());
        let application_required = req.application_contribution_required;
        let creation_intent = awaken_session_contract::SessionCreationIntent {
            control: awaken_session_contract::ControlSessionCreationInputs {
                environment,
                mcp_authoring: awaken_session_contract::SessionMcpAuthoringContext {
                    ordered_vault_ids: req.vault_ids.clone(),
                },
                agent_id: agent_id.clone(),
                model: resolved_model.id.clone(),
                execution_model_ref,
                runtime: published_backend_ref,
                delegate_ids: delegate_ids.clone(),
                toolsets: effective_toolsets,
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                resources: resolved_resources,
                initial_mcp: mcp_drafts,
            },
            application: if application_required {
                awaken_session_contract::ApplicationContributionState::Required
            } else {
                awaken_session_contract::ApplicationContributionState::Absent
            },
        };
        // Compile before insert so malformed no-application input cannot strand a
        // preparation row. The transient result is committed exactly once after
        // the insert; required applications compile only after their contribution.
        let compiled = if application_required {
            None
        } else {
            Some(
                creation_intent
                    .clone()
                    .finalize(Vec::new())
                    .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?,
            )
        };
        let mut persisted = PersistedSession {
            session_id: id.clone(),
            revision: Default::default(),
            baseline: awaken_session_contract::SessionBaselineState::Preparing(creation_intent),
            title: req.title.clone(),
            metadata: req.metadata.clone(),
            agent_tools: initial_agent_toolsets,
            environment_binding: None,
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            status: "preparing".to_string(),
            archived_at: None,
        };
        // The activation intent and owner fence commit before Host/worker IO.
        persisted = self
            .create_session_snapshot(&owner_scope, persisted)
            .await?;
        if let Some(compiled) = compiled {
            self.commit_compiled_session_creation(&owner_scope, persisted, compiled)
                .await?;
            // One phase driver now owns create, update, and recovery realization.
            // The frozen baseline and Requested generations are durable before the
            // driver performs any Runtime effect.
            persisted = match self.realize_session_locally(&id).await {
                Ok(persisted) => persisted,
                Err(error) => {
                    let _ = self
                        .release_terminal_resources(&id, Some(&owner_scope), &[])
                        .await;
                    return Err(error);
                }
            };
        }
        let deployment_id = req.metadata.get("awaken.deployment_id").cloned();
        let session_tools = if let Some(view) = &config_view {
            let mut tools = if view.toolsets.is_empty() {
                project::agent_tools(&caps)
            } else {
                project::resolved_toolsets(&view.toolsets)
            };
            let client_tools = project::agent_client_tools(&view.client_tools)
                .map_err(|error| StateError::Run(RunError::bad_request(error)))?;
            tools.retain(|candidate| match candidate {
                crate::types::agent::AgentTool::Custom { name, .. } => !client_tools.iter().any(
                    |tool| matches!(tool, crate::types::agent::AgentTool::Custom { name: client_name, .. } if client_name == name),
                ),
                _ => true,
            });
            tools.extend(client_tools);
            tools
        } else {
            project::agent_tools(&caps)
        };
        let mut session = Session {
            id: id.clone(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: agent_version,
                // Echo the accepted official override or the published Agent model.
                model: resolved_model,
                name: agent_id.clone(),
                description: None,
                system: config_view.as_ref().and_then(|view| view.system.clone()),
                tools: session_tools,
                // Echo the accepted servers in the SDK's `{name, type:"url", url}` shape.
                mcp_servers: typed_mcp_servers(persisted.visible_mcp_servers()),
                skills: effective_skills.as_ref().map_or_else(
                    || project::agent_skills(&caps),
                    |skills| {
                        skills
                            .iter()
                            .cloned()
                            .map(crate::types::agent::AgentSkill::from_binding)
                            .collect()
                    },
                ),
                multiagent: config_view.as_ref().map_or_else(
                    || project::agent_multiagent(&caps),
                    |view| project::agent_multiagent_ids(&view.delegate_ids),
                ),
            },
            environment_id: environment_id.clone(),
            created_at: PROCESSED_AT.to_string(),
            updated_at: PROCESSED_AT.to_string(),
            archived_at: None,
            title: req.title,
            metadata: req.metadata,
            // Resource wire values are projected from `resource_state` on response.
            resources: Vec::new(),
            outcome_evaluations: Vec::new(),
            status: if application_required {
                "preparing"
            } else {
                "idle"
            },
            stats: SessionStats::default(),
            usage: Usage::default(),
            vault_ids: req.vault_ids.clone(),
            deployment_id,
        };
        // Anthropic's create-time overrides are session-local replacements. Null
        // clears nullable/list fields; an empty list also clears a list field.
        if let AgentRef::Object(override_ref) = &req.agent {
            if let Some(system) = &override_ref.system {
                session.agent.system = system.clone();
            }
            if let Some(tools) = &override_ref.tools {
                if tools.as_ref().is_none_or(|tools| tools.is_empty())
                    && !session.agent.skills.is_empty()
                {
                    return Err(StateError::Run(RunError::bad_request(
                        "cannot clear tools while skills are configured",
                    )));
                }
                session.agent.tools = project::resolved_tools(tools.as_deref().unwrap_or_default());
            }
        }
        persisted.agent_tools = session.agent.tools.clone();
        // Persist the session's config (secret-free) so a restart or a peer process
        // rehydrates its real agent/model/title/metadata/MCP, not a placeholder.
        // The core session record is tenancy-agnostic (authz is an edge aspect) —
        // it never stores a workspace/org.
        let created_fact = if application_required {
            persisted = self
                .commit_session_snapshot(
                    &owner_scope,
                    persisted,
                    "record-preparing-config",
                    Vec::new(),
                )
                .await?;
            None
        } else {
            let fact = lifecycle_fact(
                format!("session:{id}:created"),
                &id,
                workspace_id.clone(),
                lifecycle_event::SESSION_IDLED,
            );
            persisted = self
                .commit_session_snapshot(&owner_scope, persisted, "activate", vec![fact.clone()])
                .await?;
            Some(fact)
        };
        self.owners.lock().unwrap().insert(id.clone(), owner_scope);
        let record = SessionRecord {
            agent_id,
            session,
            resource_state: persisted.resources,
            events: Vec::new(),
            child_threads: Vec::new(),
        };
        let session = record.session_projection();
        self.sessions.lock().unwrap().insert(id.clone(), record);
        // Dispatch only after the active activation and Session lifecycle fact are
        // durable. A worker can never claim a work item whose resource intent is
        // still merely Prepared.
        if !application_required
            && let Some(envs) = self.environments.as_ref()
            && envs.is_self_hosted(&environment_id).await
        {
            envs.enqueue_session_work(&environment_id, &id).await;
        }
        // Project the committed create as a lifecycle fact: a fresh session is idle,
        // so fan out `session.status_idled` (the webhook catalog name — past-tense
        // fact, distinct from the SSE `session.status_idle` transition) to any
        // workspace-scoped subscribers. The owning workspace comes from the edge (the
        // aspect), passed in — never read back from the core record. Out-of-band.
        if let (Some(sink), Some(created_fact)) = (&self.lifecycle_sink, &created_fact) {
            sink.emit_fact(
                &created_fact.id,
                &id,
                workspace_id.as_deref(),
                lifecycle_event::SESSION_IDLED,
            )
            .await;
        }
        Ok(session)
    }

    /// The owner scope of `session_id`, if this process created (or has cached) it —
    /// the aspect-layer session→owner lookup the edge ownership guard consults
    /// (ADR-0051). `None` when the id is unknown to this process (e.g. a cross-
    /// process session before rehydration), where the guard falls through and the
    /// persistence layer remains the fence.
    #[must_use]
    pub fn owner_scope(&self, session_id: &str) -> Option<String> {
        self.owners.lock().unwrap().get(session_id).cloned()
    }

    /// Resolve the owner scope of `session_id` for the edge ownership guard,
    /// consulting the in-memory index first (same-process, no I/O) and then the
    /// durable store (cross-process, after a restart lost the index). `None` when
    /// no backend knows the session — a genuinely unknown id, where the guard
    /// falls through and the handler's own `NotFound` answers.
    pub async fn resolve_owner(&self, session_id: &str) -> Option<String> {
        if let Some(scope) = self.owner_scope(session_id) {
            return Some(scope);
        }
        self.sessions_repo.owner(session_id).await
    }

    async fn reconcile_persisted_resources(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
    ) -> Result<PersistedSession, StateError> {
        session = self
            .ensure_repository_credentials_pinned(owner_scope, session)
            .await?;
        let session_id = session.session_id.clone();
        if session.status == "idle" {
            if let Some(desired) = session.resources.pending.clone() {
                session
                    .resources
                    .start_attempt()
                    .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
                session = self
                    .commit_session_snapshot(
                        owner_scope,
                        session,
                        "resource-reconcile-attempt",
                        Vec::new(),
                    )
                    .await?;
                if let Err(error) = self
                    .runtime
                    .apply_session_inputs(&session_id, owner_scope, &desired)
                    .await
                {
                    session
                        .resources
                        .note_retryable_failure(error.to_string())
                        .map_err(|state_error| {
                            StateError::Run(RunError::internal(state_error.to_string()))
                        })?;
                    self.commit_session_snapshot(
                        owner_scope,
                        session,
                        "resource-reconcile-failed",
                        Vec::new(),
                    )
                    .await?;
                    return Err(StateError::Run(error));
                }
                session
                    .resources
                    .commit()
                    .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
                session = self
                    .commit_session_snapshot(
                        owner_scope,
                        session,
                        "resource-reconcile-active",
                        Vec::new(),
                    )
                    .await?;
                return Ok(session);
            }

            if session.resources.activations.iter().any(|activation| {
                activation.state == awaken_session_contract::ActivationState::Releasing
            }) {
                return Err(StateError::Run(RunError::internal(
                    "resource activation has Releasing records without a pending manifest",
                )));
            }
            // Cause graph:
            //   pending -> realize pending generation
            //   no pending + active inputs -> replay/adopt the retained generation
            //   no pending + no active inputs -> no external resource effect
            //
            // Decision table:
            // | Rule | pending | active inputs | activations | Runtime apply |
            // | R1   | yes     | any           | any         | desired       |
            // | R2   | no      | nonempty      | empty       | active+adopt  |
            // | R3   | no      | nonempty      | present     | active        |
            // | R4   | no      | empty         | empty       | none          |
            //
            // R4 is important for retained pre-ADR-66 rows: manufacturing an
            // empty "activation" before prepare_session is both redundant and
            // invalid for a fresh Runtime incarnation.
            if session.resources.active.inputs.is_empty()
                && session.resources.activations.is_empty()
            {
                return Ok(session);
            }
            self.runtime
                .apply_session_inputs(&session_id, owner_scope, &session.resources.active)
                .await?;
            if session.resources.activations.is_empty() {
                session.resources.adopt_legacy_active(&session_id);
                session = self
                    .commit_session_snapshot(
                        owner_scope,
                        session,
                        "resource-adopt-legacy",
                        Vec::new(),
                    )
                    .await?;
            }
            return Ok(session);
        }

        // A non-live Session never resumes a Prepared generation. Persist the
        // release intent, tear down idempotently, then terminalize every record.
        if session.resources.pending.is_none() {
            session
                .resources
                .begin_release()
                .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
        }
        session = self
            .commit_session_snapshot(owner_scope, session, "resource-release-intent", Vec::new())
            .await?;
        self.runtime
            .end_session(&session_id)
            .await
            .map_err(StateError::Run)?;
        if !self
            .retire_session_repositories(owner_scope, &session_id, &session.resources)
            .await
        {
            return Err(StateError::Run(RunError::internal(
                "Session-scoped Repository cleanup remains pending",
            )));
        }
        session
            .resources
            .complete_terminal_release("Session terminated before activation completed");
        session = self
            .commit_session_snapshot(
                owner_scope,
                session,
                "resource-release-complete",
                Vec::new(),
            )
            .await?;
        if session.status == "deleted" {
            self.tombstone_session_snapshot(
                owner_scope,
                &session,
                lifecycle_fact(
                    format!("session:{session_id}:deleted"),
                    &session_id,
                    Some(owner_scope.to_string()),
                    lifecycle_event::SESSION_DELETED,
                ),
            )
            .await?;
        }
        Ok(session)
    }

    async fn retire_session_repositories(
        &self,
        owner_scope: &str,
        session_id: &str,
        resources: &awaken_session_contract::SessionResourceState,
    ) -> bool {
        if self.resource_catalog.is_none() {
            return true;
        }
        let prefix = format!("managed:{session_id}:repository:");
        let mut ids = std::collections::BTreeSet::new();
        for manifest in std::iter::once(&resources.active).chain(resources.pending.iter()) {
            for input in &manifest.inputs {
                if let awaken_session_contract::ResolvedInputSource::Repository {
                    repository_id,
                    ..
                } = &input.source
                    && repository_id.as_str().starts_with(&prefix)
                {
                    ids.insert(repository_id.to_string());
                }
            }
        }
        let mut retired = true;
        for repository_id in ids {
            if !self.retire_repository(owner_scope, &repository_id).await {
                retired = false;
                tracing::warn!(
                    session = session_id,
                    repository = %repository_id,
                    "Session-scoped Repository cleanup remains pending"
                );
            }
        }
        retired
    }

    pub(crate) async fn retire_repository(&self, owner_scope: &str, repository_id: &str) -> bool {
        let Some(catalog) = &self.resource_catalog else {
            return true;
        };
        let definition = match catalog.repository(owner_scope, repository_id) {
            Ok(Some(definition)) => definition,
            Ok(None) => return true,
            Err(error) => {
                tracing::warn!(repository = repository_id, error = ?error, "Repository catalog read failed");
                return false;
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default();
        if let Some(scheduler) = &self.resource_purge_scheduler
            && let Err(error) = scheduler
                .schedule_purge(
                    awaken_resource_contract::ResourceTarget::new(
                        owner_scope,
                        awaken_resource_contract::ResourceKind::Repository,
                        repository_id,
                    ),
                    Some(definition.current_config_version.0),
                    now,
                    now,
                )
                .await
        {
            tracing::warn!(repository = repository_id, error = ?error, "Repository purge scheduling failed");
            return false;
        }
        catalog
            .set_repository_state(
                owner_scope,
                repository_id,
                awaken_resource_contract::ResourceState::Deleted,
            )
            .is_ok()
    }

    /// ResourceReclaimer entry point. Composition roots call this after durable
    /// stores and the Runtime Host are wired. It scans only Session application
    /// state; authorization principals and policy objects never cross this seam.
    pub async fn reconcile_resource_activations(&self) -> usize {
        let pending = self.sessions_repo.reconcilable_sessions().await;
        let mut settled = 0;
        for record in pending {
            let owner_scope = record.workspace_id;
            let session = record.session;
            if session.status != "deleted"
                && !session.resources.needs_reconciliation()
                && (session.status == "idle" || !session.resources.has_active())
            {
                continue;
            }
            match self
                .reconcile_persisted_resources(&owner_scope, session.clone())
                .await
            {
                Ok(_) => settled += 1,
                Err(error) => tracing::warn!(
                    session = %session.session_id,
                    error = ?error,
                    "Session resource reconciliation remains pending"
                ),
            }
        }
        settled
    }

    /// A session object reconstructed for a rehydrated (post-restart) session.
    /// When the durable repo holds the session's config it is restored faithfully;
    /// otherwise (a session created before the repo existed, or a purely in-memory
    /// deployment) it falls back to the runtime's advertised surface with
    /// placeholder agent/title/metadata — the pre-repo behavior.
    pub(crate) fn rehydrated_session(
        &self,
        id: &str,
        persisted: Option<PersistedSession>,
    ) -> Result<Session, StateError> {
        let caps = self.runtime.capabilities_for(id);
        let default_tools = project::agent_tools(&caps);
        let (
            agent_id,
            model,
            environment_id,
            title,
            metadata,
            agent_tools,
            mcp_servers,
            status,
            archived_at,
        ) = match persisted {
            Some(p) => {
                let (agent_id, model, environment_id) = p
                    .frozen_baseline()
                    .map(|baseline| {
                        (
                            baseline.agent_id.clone(),
                            baseline.model.clone(),
                            baseline.environment.environment_id.clone(),
                        )
                    })
                    .unwrap_or_else(|| {
                        (
                            "assistant".into(),
                            self.runtime.model(),
                            p.environment_id().to_string(),
                        )
                    });
                let mcp_servers = typed_mcp_servers(p.visible_mcp_servers());
                (
                    agent_id,
                    model,
                    environment_id,
                    p.title,
                    p.metadata,
                    p.agent_tools,
                    mcp_servers,
                    Self::wire_session_status(&p.status),
                    p.archived_at,
                )
            }
            None => (
                "assistant".to_string(),
                self.runtime.model(),
                "env_local".to_string(),
                None,
                Default::default(),
                default_tools,
                Vec::new(),
                "idle",
                None,
            ),
        };
        let deployment_id = metadata.get("awaken.deployment_id").cloned();
        Ok(Session {
            id: id.to_string(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: 1,
                model: ModelConfig::new(model),
                name: agent_id,
                description: None,
                system: None,
                tools: agent_tools,
                mcp_servers,
                skills: project::agent_skills(&caps),
                multiagent: project::agent_multiagent(&caps),
            },
            environment_id,
            created_at: PROCESSED_AT.to_string(),
            updated_at: PROCESSED_AT.to_string(),
            archived_at,
            title,
            metadata,
            resources: Vec::new(),
            outcome_evaluations: Vec::new(),
            status,
            stats: SessionStats::default(),
            usage: Usage::default(),
            vault_ids: Vec::new(),
            deployment_id,
        })
    }

    /// Recover a session whose in-memory record was lost from durable truth (a
    /// process restart, ADR-0039). If the store holds a committed transcript for
    /// `id`, rebuild the record — the projected history plus a reconstructed
    /// session object — so a resume can continue the awaiting run. A thread with no
    /// committed truth stays `NotFound` (fail closed): the store is authoritative.
    pub(crate) async fn ensure_session(&self, id: &str) -> Result<(), StateError> {
        if self.sessions.lock().unwrap().contains_key(id) {
            return Ok(());
        }
        // Install the persisted, already-resolved resource snapshot BEFORE opening
        // runtime history. Opening a thread constructs its context; doing that first
        // would transiently resolve today's Agent/Skill configuration and could both
        // drift from the Session pin and mutate its sandbox before the pin is known.
        let persisted = self.sessions_repo.get(id).await;
        let owner_scope = self
            .sessions_repo
            .owner(id)
            .await
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
        let mut persisted = match persisted {
            Some(session) => Some(
                self.reconcile_persisted_resources(&owner_scope, session)
                    .await?,
            ),
            None => None,
        };
        // Terminated Sessions remain readable tombstones; deleted and failed
        // activation rows are hidden from the public read model.
        if persisted.as_ref().is_some_and(|session| {
            matches!(session.status.as_str(), "deleted" | "activation_failed")
        }) {
            return Err(StateError::NotFound);
        }
        if let Some(session) = persisted.clone() {
            let baseline = session.frozen_baseline().cloned().ok_or_else(|| {
                StateError::Run(RunError::internal(
                    "cannot realize a Session whose baseline is still preparing",
                ))
            })?;
            // Reconciliation already crosses the canonical projection synchronizer,
            // which calls `prepare_session`. Calling it here as well used to stage
            // resources/runtime twice after cache loss. A settled Session has no
            // reconciliation phase, so it prepares directly through the same port.
            let recovered = if session.mcp.needs_reconciliation() {
                self.recover_mcp_projections(id).await?
            } else {
                self.runtime
                    .prepare_session(
                        id,
                        SessionInit {
                            workspace_id: owner_scope.clone(),
                            agent_id: baseline.agent_id.clone(),
                            delegate_ids: baseline.delegate_ids.clone(),
                            toolsets: Some(project::toolset_policies(&session.agent_tools)),
                            resources: session.resources.active.clone(),
                            model: Some(baseline.execution_model_ref.clone()),
                            runtime: baseline.runtime.clone(),
                            environment: baseline.environment.clone(),
                        },
                    )
                    .await
                    .map_err(StateError::Run)?;
                session
            };
            persisted = Some(recovered.clone());
            if let Some(binding) = recovered.environment_binding.as_deref() {
                self.runtime
                    .restore_session_environment(&baseline.agent_id, id, binding)
                    .await
                    .map_err(StateError::Run)?;
            }
        }
        let messages = self.runtime.committed_messages(id).await;
        if messages.is_empty() && persisted.is_none() {
            return Err(StateError::NotFound);
        }
        let events: Vec<Event> = project_messages(&messages, None)
            .into_iter()
            .map(|event| Event {
                id: event.id.unwrap_or_else(|| self.next_event_id()),
                kind: event.kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            })
            .collect();
        let agent_id = persisted
            .as_ref()
            .and_then(PersistedSession::agent_id)
            .map_or_else(|| "assistant".to_string(), str::to_string);
        let resource_state = persisted
            .as_ref()
            .map(|session| session.resources.clone())
            .unwrap_or_default();
        let session = self.rehydrated_session(id, persisted)?;
        let delegated_runs = self
            .runtime
            .delegated_runs(id)
            .await
            .map_err(StateError::Run)?;
        let mut record = SessionRecord {
            agent_id,
            session,
            resource_state,
            events,
            child_threads: Vec::new(),
        };
        self.append_delegation_projections(&mut record, &delegated_runs);
        self.sessions
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert(record);
        self.owners
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert(owner_scope);
        Ok(())
    }

    /// `GET /v1/sessions/{id}`.
    pub fn get_session(&self, id: &str) -> Result<Session, StateError> {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(id)
            .map(SessionRecord::session_projection)
            .ok_or(StateError::NotFound)
    }

    pub async fn session_revision(
        &self,
        id: &str,
    ) -> Result<awaken_session_contract::SessionRevision, StateError> {
        self.sessions_repo
            .get(id)
            .await
            .map(|session| session.revision)
            .ok_or(StateError::NotFound)
    }

    /// `GET /v1/sessions` — every session, ascending id (deterministic).
    pub fn list_sessions(&self) -> Vec<Session> {
        let sessions = self.sessions.lock().unwrap();
        let mut out: Vec<Session> = sessions
            .values()
            .map(SessionRecord::session_projection)
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Sessions owned by `scope` — the tenancy-fenced list (ADR-0051), so a
    /// workspace's `GET /v1/sessions` never sees another's. A session with no
    /// recorded owner belongs to the seeded default scope. Mirrors the per-id
    /// ownership guard, which the collection route does not pass through.
    pub fn list_sessions_scoped(&self, scope: &str) -> Vec<Session> {
        // Snapshot owners first (lock, clone, drop) so we never hold two locks at
        // once — create_session takes `owners` on its own path.
        let owners = self.owners.lock().unwrap().clone();
        let sessions = self.sessions.lock().unwrap();
        let mut out: Vec<Session> = sessions
            .values()
            .filter(|r| {
                owners
                    .get(&r.session.id)
                    .map_or(scope == DEFAULT_SCOPE, |owner| owner == scope)
            })
            .map(SessionRecord::session_projection)
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// `DELETE /v1/sessions/{id}` — commit a terminal `session.deleted` event,
    /// push it to any open SSE stream, then drop the in-memory record. The
    /// broadcast happens *before* removal because after the record is gone there
    /// is nothing to backfill from: a live frame is the only way a streaming
    /// client observes the deletion, and a subsequent `events.list`/`retrieve`
    /// is a 404 (delete removes the session; it does not tombstone it as archive
    /// does).
    pub async fn delete_session(&self, id: &str) -> Result<(), StateError> {
        // Snapshot before the durable commit, but do not remove the visible record
        // until the repository has atomically stored the terminal state, cleanup
        // intent, and outbox fact. The row becomes a tombstone only after every
        // external cleanup succeeds; until then it is hidden application state for
        // the ResourceReclaimer.
        let child_threads = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(id)
                .ok_or(StateError::NotFound)?
                .child_threads
                .clone()
        };
        let owner = self.resolve_owner(id).await;
        let deleted_fact = lifecycle_fact(
            format!("session:{id}:deleted"),
            id,
            owner.clone(),
            lifecycle_event::SESSION_DELETED,
        );
        let mut persisted = self
            .sessions_repo
            .get(id)
            .await
            .ok_or(StateError::NotFound)?;
        persisted.status = "deleted".into();
        if persisted.resources.pending.is_none() {
            persisted
                .resources
                .begin_release()
                .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
        }
        let owner_scope = owner.as_deref().unwrap_or(DEFAULT_SCOPE);
        self.commit_session_snapshot(
            owner_scope,
            persisted,
            "delete-intent",
            vec![deleted_fact.clone()],
        )
        .await?;

        {
            let deleted_id = self.next_event_id();
            let mut sessions = self.sessions.lock().unwrap();
            let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
            let from = record.events.len();
            record.events.push(Event {
                id: deleted_id,
                kind: OutboundKind::SessionDeleted {},
                processed_at: Some(PROCESSED_AT.to_string()),
            });
            self.broadcast_committed_from(id, record, from);
            sessions.remove(id);
        }
        // Terminal edge: dispose the session's sandbox(es) at the host — the main
        // thread is the session id, and each spawned child agent thread gets its own.
        // Best-effort teardown: the session IS deleted from the client's view
        // regardless, so a dispose failure must not resurrect a deleted session.
        if self
            .release_terminal_resources(id, owner.as_deref(), &child_threads)
            .await
        {
            let released = self
                .sessions_repo
                .get(id)
                .await
                .ok_or(StateError::NotFound)?;
            self.tombstone_session_snapshot(owner_scope, &released, deleted_fact.clone())
                .await?;
        }
        // Project the deletion as a lifecycle fact so a webhook subscriber is
        // notified, mirroring create's `session.status_idled` and archive's
        // `session.status_terminated`. The owner is resolved from the persisted
        // owner (the delete edge carries only the id).
        if let Some(sink) = &self.lifecycle_sink {
            sink.emit_fact(
                &deleted_fact.id,
                id,
                owner.as_deref(),
                lifecycle_event::SESSION_DELETED,
            )
            .await;
        }
        Ok(())
    }

    /// Dispose the host sandbox(es) for a session being torn down at a terminal
    /// edge: the main thread (the session id) plus each spawned child agent thread
    /// (the Runtime relationship's stable child Run id). Best-effort — the terminal transition has already committed, so
    /// a dispose failure is logged, never propagated (it must not resurrect the
    /// session). `SessionRuntime::end_session` is a no-op for a thread that never
    /// materialized a sandbox, so deriving child ids is safe.
    async fn end_session_sandboxes(&self, id: &str, child_threads: &[SessionThread]) -> bool {
        let mut threads: Vec<String> = vec![id.to_string()];
        for child in child_threads {
            threads.push(child.id.clone());
        }
        let mut released = true;
        for thread in threads {
            if let Err(err) = self.runtime.end_session(&thread).await {
                released = false;
                tracing::warn!(
                    session = id,
                    thread = %thread,
                    error = ?err,
                    "session teardown: sandbox dispose failed (best-effort)"
                );
            }
        }
        released
    }

    async fn release_terminal_resources(
        &self,
        id: &str,
        owner_scope: Option<&str>,
        child_threads: &[SessionThread],
    ) -> bool {
        let Some(mut persisted) = self.sessions_repo.get(id).await else {
            return self.end_session_sandboxes(id, child_threads).await;
        };
        let owner_scope = owner_scope.unwrap_or(DEFAULT_SCOPE);
        let needs_release_intent = persisted.resources.pending.is_none()
            && persisted.resources.activations.iter().any(|activation| {
                activation.state == awaken_session_contract::ActivationState::Active
            });
        if needs_release_intent {
            if let Err(error) = persisted.resources.begin_release() {
                tracing::warn!(session = id, error = ?error, "could not persist resource release intent");
                return false;
            }
            persisted = match self
                .commit_session_snapshot(
                    owner_scope,
                    persisted,
                    "terminal-release-intent",
                    Vec::new(),
                )
                .await
            {
                Ok(session) => session,
                Err(error) => {
                    tracing::warn!(session = id, error = ?error, "could not persist resource release intent");
                    return false;
                }
            };
        }
        if self.end_session_sandboxes(id, child_threads).await
            && self
                .retire_session_repositories(owner_scope, id, &persisted.resources)
                .await
        {
            persisted
                .resources
                .complete_terminal_release("Session terminated");
            match self
                .commit_session_snapshot(
                    owner_scope,
                    persisted,
                    "terminal-release-complete",
                    Vec::new(),
                )
                .await
            {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(session = id, error = ?error, "could not persist resource release completion");
                    false
                }
            }
        } else {
            false
        }
    }

    /// `POST /v1/sessions/{id}/archive` — terminate the session: stamp
    /// `archived_at`, move `status` to `terminated`, and commit a
    /// `session.status_terminated` event so a streaming/listing client observes the
    /// terminal transition (not just the mutated status field). Idempotent: a
    /// re-archive returns the same terminal record without a second event.
    pub async fn archive_session(&self, id: &str) -> Result<Session, StateError> {
        let (mut newly_terminated, child_threads) = {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(id).ok_or(StateError::NotFound)?;
            (
                record.session.archived_at.is_none(),
                record.child_threads.clone(),
            )
        };
        let owner = self.resolve_owner(id).await;
        let terminated_fact = lifecycle_fact(
            format!("session:{id}:terminated"),
            id,
            owner.clone(),
            lifecycle_event::SESSION_TERMINATED,
        );
        if newly_terminated {
            let mut persisted = self
                .sessions_repo
                .get(id)
                .await
                .ok_or(StateError::NotFound)?;
            if persisted.status == "terminated" {
                newly_terminated = false;
            } else {
                persisted.status = "terminated".into();
                persisted.archived_at = Some(PROCESSED_AT.into());
                self.commit_session_snapshot(
                    owner.as_deref().unwrap_or(DEFAULT_SCOPE),
                    persisted,
                    "archive",
                    vec![terminated_fact.clone()],
                )
                .await?;
            }
        }
        let session = {
            let terminated_id = self.next_event_id();
            let mut sessions = self.sessions.lock().unwrap();
            let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
            if newly_terminated {
                record.session.archived_at = Some(PROCESSED_AT.to_string());
                record.session.status = "terminated";
                record.events.push(Event {
                    id: terminated_id,
                    kind: OutboundKind::SessionStatusTerminated {},
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
            record.session_projection()
        };
        // Archive is terminal (no further turns run on this session), so reap its
        // sandbox — but only on the transition, so a re-archive (idempotent) does
        // not re-dispose. The record survives as a tombstone; only the sandbox goes.
        let release_required = newly_terminated
            || self.sessions_repo.get(id).await.is_some_and(|persisted| {
                persisted.resources.pending.is_some()
                    || persisted.resources.activations.iter().any(|activation| {
                        activation.state == awaken_session_contract::ActivationState::Active
                    })
            });
        if release_required
            && !self
                .release_terminal_resources(id, owner.as_deref(), &child_threads)
                .await
        {
            return Err(StateError::Run(RunError::internal(format!(
                "Session `{id}` terminal resources could not be released"
            ))));
        }
        // Project the terminal transition as a lifecycle fact, mirroring create's
        // `session.status_idled`. The owning workspace is resolved from the session's
        // persisted owner (the archive edge carries only the id) so a subscription in
        // that workspace is matched even after a restart lost the in-memory index.
        if newly_terminated && let Some(sink) = &self.lifecycle_sink {
            sink.emit_fact(
                &terminated_fact.id,
                id,
                owner.as_deref(),
                lifecycle_event::SESSION_TERMINATED,
            )
            .await;
        }
        Ok(session)
    }
}

#[cfg(test)]
mod sandbox_provisioning_runtime_tests {
    use super::*;
    use awaken_provisioning_contract::SandboxProvisioning::{Eager, OnToolUse};

    #[test]
    fn native_only_lazy_provisioning_decision_table() {
        for (case, provisioning, runtime, accepted) in [
            ("C1 eager native", Eager, None, true),
            ("C2 eager ACP", Eager, Some("acp:claude"), true),
            ("C3 lazy implicit native", OnToolUse, None, true),
            ("C4 lazy explicit native", OnToolUse, Some("awaken"), true),
            ("C5 lazy ACP", OnToolUse, Some("acp:claude"), false),
            ("C6 lazy unknown runtime", OnToolUse, Some("remote"), false),
        ] {
            assert_eq!(
                validate_sandbox_provisioning_runtime(provisioning, runtime).is_ok(),
                accepted,
                "{case}"
            );
        }
    }
}
