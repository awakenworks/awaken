//! Session lifecycle for [`ManagedState`]: create, rehydrate, get/list,
//! update, delete, and archive.

use super::*;
use crate::types::McpServer;
use serde_json::json;

struct EmptyResourceConfigSource;

impl awaken_resource_contract::ResourceConfigSource for EmptyResourceConfigSource {
    fn resolve_memory_store(
        &self,
        _workspace_id: &str,
        id: &str,
    ) -> Result<
        awaken_resource_contract::ResolvedMemoryStoreConfig,
        awaken_resource_contract::ResourceCatalogError,
    > {
        Err(awaken_resource_contract::ResourceCatalogError::NotFound(
            id.into(),
        ))
    }

    fn resolve_repository(
        &self,
        _workspace_id: &str,
        id: &str,
    ) -> Result<
        awaken_resource_contract::ResolvedRepositoryConfig,
        awaken_resource_contract::ResourceCatalogError,
    > {
        Err(awaken_resource_contract::ResourceCatalogError::NotFound(
            id.into(),
        ))
    }
}

fn lifecycle_fact(
    id: String,
    session_id: &str,
    workspace_id: Option<String>,
    event_type: &str,
) -> SessionLifecycleFact {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    SessionLifecycleFact {
        id,
        session_id: session_id.to_string(),
        workspace_id,
        event_type: event_type.to_string(),
        timestamp,
    }
}

impl ManagedState {
    /// `POST /v1/sessions`.
    ///
    /// MCP binding (ADR-0043 Phase 3): each requested server is bound to a vault
    /// credential by exact `mcp_server_url` match across the request's
    /// `vault_ids`, then the runtime provisions the thread via
    /// [`SessionRuntime::prepare_session`] BEFORE the record is inserted — a
    /// failed preparation fails the create (fail closed; the router maps the
    /// `RunError` to the error envelope). A `vault_ids` entry that names no
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
        self.check_bind(&req)?;
        // Mint an id no durable thread already owns: a fresh process restarts
        // the sequence at 0, but the store dir may hold committed truth from a
        // previous process (ADR-0039). Adopting such a thread would graft the
        // old transcript onto a NEW session, so skip forward instead — the
        // rehydration path (`ensure_session`) remains the only way to reattach
        // to an existing thread, and it is keyed by the caller's explicit id.
        let id = loop {
            let candidate = format!("sesn_{}", self.session_seq.fetch_add(1, Ordering::SeqCst));
            if !self.runtime.owns_thread(&candidate).await {
                break candidate;
            }
        };
        let agent_id = req.agent.id().to_string();
        let owner_scope = workspace_id
            .clone()
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
        let config_view = self
            .config_source
            .as_ref()
            .and_then(|source| source.agent_view_in(&owner_scope, &agent_id));
        // Resolve the session's effective model. Precedence: the official
        // `agent_with_overrides.model` (a per-session replace) wins; then the legacy
        // `metadata.awaken.model` selection; then the referenced agent's authoritative
        // model from the config plane (the config plane owns model/system/tools); else
        // `None` = the host default. Clearing the model is rejected — a session always
        // needs one (400 `agent_model_required`).
        let selected_model: Option<ModelConfig> = match req.agent.model_override() {
            ModelOverride::Set(cfg) => Some(cfg),
            ModelOverride::Cleared => {
                return Err(StateError::Run(RunError::bad_request(
                    "agent_model_required: a session override cannot clear `model`",
                )));
            }
            ModelOverride::Absent => req
                .awaken_model()
                .map(ModelConfig::new)
                .or_else(|| config_view.as_ref()?.model.clone().map(ModelConfig::new)),
        };
        // Echo the agent version the client pinned (or overrode over), defaulting to 1.
        let agent_version = req.agent.version().unwrap_or(1);
        // Session-inline bindings override the Agent defaults by name or URL. The
        // effective set is used for preparation, persistence, and wire projection,
        // so the UI shows what the runtime will actually connect.
        let mut effective_mcp_servers = req.mcp_servers.clone();
        if let Some(view) = &config_view {
            for server in &view.mcp_servers {
                if effective_mcp_servers
                    .iter()
                    .any(|current| current.name == server.name || current.url == server.url)
                {
                    continue;
                }
                effective_mcp_servers.push(McpServer {
                    name: server.name.clone(),
                    url: server.url.clone(),
                });
            }
        }
        let bindings = effective_mcp_servers
            .iter()
            .map(|server| {
                let credential_source_id = self
                    .vaults
                    .as_ref()
                    .and_then(|v| v.mcp_credential_source_for_url(&req.vault_ids, &server.url));
                // The matched credential's stored refresh configuration rides
                // along, so the host can keep the connection alive past the
                // access token's expiry.
                let refresh = match (&self.vaults, &credential_source_id) {
                    (Some(v), Some(source_id)) => v.mcp_refresh_for_source(source_id),
                    _ => None,
                };
                McpServerBinding {
                    name: server.name.clone(),
                    url: server.url.clone(),
                    // Store the neutral row id string on the binding; the typed lookup
                    // above stays local to this vault-aware assembly.
                    credential_source_id: credential_source_id.map(|id| id.0),
                    refresh,
                }
            })
            .collect();
        // Parse the wire `resources[]` (ADR-0038) into staged mounts, and project each
        // into a DTO entry so the created session echoes its create-time resources —
        // list/get/delete then address these and any later-attached ones uniformly.
        let resources = req
            .resources
            .iter()
            .map(|resource| {
                parse_session_input(resource).ok_or_else(|| {
                    StateError::Run(RunError::bad_request(
                        "invalid resource: unsupported type or malformed fields",
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
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
                authorization_token,
                initial_branch,
            } = &resource.target
            {
                let catalog = self.resource_catalog.as_ref().ok_or_else(|| {
                    StateError::Run(RunError::bad_request(
                        "repository resources require a configured Resource Catalog",
                    ))
                })?;
                let repository_id = format!("managed:{id}:repository:{index}");
                let credential_binding = match authorization_token {
                    Some(token) => {
                        let vaults = self.vaults.as_ref().ok_or_else(|| {
                            StateError::Run(RunError::bad_request(
                                "repository authorization requires a configured credential vault",
                            ))
                        })?;
                        Some(
                            vaults
                                .enter_session_bearer(&owner_scope, token.clone())
                                .await
                                .map_err(|error| {
                                    StateError::Run(RunError::bad_request(format!(
                                        "repository credential could not be stored: {error}"
                                    )))
                                })?
                                .0,
                        )
                    }
                    None => None,
                };
                catalog
                    .create_repository(
                        awaken_resource_contract::RepositoryDefinition {
                            id: repository_id.clone(),
                            workspace_id: owner_scope.clone(),
                            name: format!("Session repository {index}"),
                            description: "Managed compatibility Session input".into(),
                            metadata: Default::default(),
                            state: awaken_resource_contract::ResourceState::Active,
                            current_config_version:
                                awaken_resource_contract::ConfigVersion::INITIAL,
                        },
                        awaken_resource_contract::RepositoryConfigVersion {
                            repository_id: repository_id.clone(),
                            version: awaken_resource_contract::ConfigVersion::INITIAL,
                            remote_url: remote_url.clone(),
                            credential_binding,
                            initial_branch: initial_branch.clone(),
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
        // Sole composition/resolution point. Runtime receives this persisted,
        // secret-free manifest and never re-opens Agent or Resource config stores.
        let mut effective_inputs = match self.resource_catalog.as_deref() {
            Some(catalog) => awaken_session_contract::SessionInputResolver::resolve_inputs(
                &owner_scope,
                catalog,
                agent_defaults,
                &attachments,
            ),
            None => awaken_session_contract::SessionInputResolver::resolve_inputs(
                &owner_scope,
                &EmptyResourceConfigSource,
                agent_defaults,
                &attachments,
            ),
        }
        .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        if let Some(view) = &config_view {
            effective_inputs.skills = Some(
                self.runtime
                    .resolve_session_skills(&owner_scope, &view.skill_ids)
                    .await
                    .map_err(StateError::Run)?,
            );
        }
        let resource_dtos: Vec<serde_json::Value> = effective_inputs
            .inputs
            .iter()
            .map(|input| resolved_resource_dto(&id, input))
            .collect();
        // Resolve the session's environment (defaulting to the local one) and its
        // networking policy once, for both the SessionInit (staged before the first
        // turn) and the echoed Session object.
        let environment_id = req
            .environment_id
            .clone()
            .unwrap_or_else(|| "env_local".to_string());
        let (deny_egress, sandbox) = match self.environments.as_ref() {
            Some(e) => (
                e.deny_egress(&environment_id).await,
                e.sandbox_config(&environment_id).await,
            ),
            None => (false, None),
        };
        self.runtime
            .prepare_session(
                &id,
                SessionInit {
                    workspace_id: owner_scope.clone(),
                    agent_id: agent_id.clone(),
                    mcp_servers: bindings,
                    resources: effective_inputs.clone(),
                    model: selected_model.as_ref().map(|m| m.id.clone()),
                    runtime: req.awaken_runtime().map(str::to_string),
                    deny_egress,
                    sandbox,
                },
            )
            .await
            .map_err(StateError::Run)?;
        // A session assigned to a self-hosted environment is dispatched through that
        // environment's work queue — the control plane enqueues it as `session` work
        // for an external worker to claim and run (the session still exists here; the
        // work item is how a polling worker discovers and drives it).
        if let Some(envs) = self.environments.as_ref()
            && envs.is_self_hosted(&environment_id).await
        {
            envs.enqueue_session_work(&environment_id, &id).await;
        }
        // Enumerate the runtime's provisioned surface so the agent object reports what
        // the run can actually do (built-in toolset, custom tools, skills, delegates),
        // not an empty set. The wire shaping lives in `project`; the host supplies
        // neutral data.
        let caps = self.runtime.capabilities_for(&id);
        // Fail closed on a custom tool the real Managed API would reject at
        // definition time (charset / reserved `mcp__` prefix / `$ref`·`oneOf` /
        // length), so an invalid tool surfaces here as a 400 instead of silently
        // diverging from Anthropic at the first turn.
        for tool in &caps.custom_tools {
            project::validate_custom_tool(tool)
                .map_err(|msg| StateError::Run(RunError::bad_request(msg)))?;
        }
        let deployment_id = req.metadata.get("awaken.deployment_id").cloned();
        let session = Session {
            id: id.clone(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: agent_version,
                // R6: echo the session's actual model — the `agent_with_overrides`
                // override, else the legacy `metadata.awaken.model`, else the host
                // default — so the client sees which model the session runs.
                model: selected_model
                    .clone()
                    .unwrap_or_else(|| ModelConfig::new(self.runtime.model())),
                name: agent_id.clone(),
                description: None,
                system: None,
                tools: project::agent_tools(&caps),
                // Echo the accepted servers in the SDK's `{name, type:"url", url}` shape.
                mcp_servers: effective_mcp_servers
                    .iter()
                    .map(|s| serde_json::to_value(s).expect("mcp server wire serializes"))
                    .collect(),
                skills: config_view.as_ref().map_or_else(
                    || project::agent_skills(&caps),
                    |view| {
                        view.skill_ids
                            .iter()
                            .map(|id| json!({ "id": id }))
                            .collect()
                    },
                ),
                multiagent: project::agent_multiagent(&caps),
            },
            environment_id: environment_id.clone(),
            created_at: PROCESSED_AT.to_string(),
            updated_at: PROCESSED_AT.to_string(),
            archived_at: None,
            title: req.title,
            metadata: req.metadata,
            // The session's create-time mounts, echoed so the client can list/get them.
            resources: resource_dtos,
            outcome_evaluations: Vec::new(),
            status: "idle",
            stats: SessionStats::default(),
            usage: Usage::default(),
            vault_ids: req.vault_ids.clone(),
            deployment_id,
        };
        // Persist the session's config (secret-free) so a restart or a peer process
        // rehydrates its real agent/model/title/metadata/MCP, not a placeholder.
        // The core session record is tenancy-agnostic (authz is an edge aspect) —
        // it never stores a workspace/org.
        let created_fact = lifecycle_fact(
            format!("session:{id}:created"),
            &id,
            workspace_id.clone(),
            lifecycle_event::SESSION_IDLED,
        );
        self.sessions_repo
            .save_owned_with_lifecycle(
                &owner_scope,
                PersistedSession {
                    session_id: id.clone(),
                    agent_id: agent_id.clone(),
                    model: session.agent.model.id.clone(),
                    title: session.title.clone(),
                    metadata: session.metadata.clone(),
                    environment_id: session.environment_id.clone(),
                    mcp_servers: session.agent.mcp_servers.clone(),
                    effective_inputs: effective_inputs.clone(),
                    status: "idle".to_string(),
                    archived_at: None,
                },
                created_fact.clone(),
            )
            .await;
        self.owners.lock().unwrap().insert(id.clone(), owner_scope);
        self.sessions.lock().unwrap().insert(
            id.clone(),
            SessionRecord {
                agent_id,
                session: session.clone(),
                effective_inputs: effective_inputs.clone(),
                events: Vec::new(),
                child_threads: Vec::new(),
            },
        );
        // Project the committed create as a lifecycle fact: a fresh session is idle,
        // so fan out `session.status_idled` (the webhook catalog name — past-tense
        // fact, distinct from the SSE `session.status_idle` transition) to any
        // workspace-scoped subscribers. The owning workspace comes from the edge (the
        // aspect), passed in — never read back from the core record. Out-of-band.
        if let Some(sink) = &self.lifecycle_sink {
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

    /// A session object reconstructed for a rehydrated (post-restart) session.
    /// When the durable repo holds the session's config it is restored faithfully;
    /// otherwise (a session created before the repo existed, or a purely in-memory
    /// deployment) it falls back to the runtime's advertised surface with
    /// placeholder agent/title/metadata — the pre-repo behavior.
    pub(crate) fn rehydrated_session(
        &self,
        id: &str,
        persisted: Option<PersistedSession>,
    ) -> Session {
        let caps = self.runtime.capabilities_for(id);
        let (
            agent_id,
            model,
            environment_id,
            title,
            metadata,
            mcp_servers,
            effective_inputs,
            status,
            archived_at,
        ) = match persisted {
            Some(p) => (
                p.agent_id,
                p.model,
                p.environment_id,
                p.title,
                p.metadata,
                p.mcp_servers,
                p.effective_inputs,
                match p.status.as_str() {
                    "terminated" => "terminated",
                    _ => "idle",
                },
                p.archived_at,
            ),
            None => (
                "assistant".to_string(),
                self.runtime.model(),
                "env_local".to_string(),
                None,
                Default::default(),
                Vec::new(),
                Default::default(),
                "idle",
                None,
            ),
        };
        let deployment_id = metadata.get("awaken.deployment_id").cloned();
        Session {
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
                tools: project::agent_tools(&caps),
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
            resources: effective_inputs
                .inputs
                .iter()
                .map(|input| resolved_resource_dto(id, input))
                .collect(),
            outcome_evaluations: Vec::new(),
            status,
            stats: SessionStats::default(),
            usage: Usage::default(),
            vault_ids: Vec::new(),
            deployment_id,
        }
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
        if persisted
            .as_ref()
            .is_some_and(|session| session.status == "deleted")
        {
            return Err(StateError::NotFound);
        }
        let owner_scope = self
            .sessions_repo
            .owner(id)
            .await
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
        if let Some(session) = &persisted {
            self.runtime
                .apply_session_inputs(id, &owner_scope, &session.effective_inputs)
                .await?;
        }
        let messages = self.runtime.committed_messages(id).await;
        if messages.is_empty() {
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
            .map_or_else(|| "assistant".to_string(), |p| p.agent_id.clone());
        let effective_inputs = persisted
            .as_ref()
            .map(|session| session.effective_inputs.clone())
            .unwrap_or_default();
        let record = SessionRecord {
            agent_id,
            session: self.rehydrated_session(id, persisted),
            effective_inputs,
            events,
            child_threads: Vec::new(),
        };
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
            .map(|r| r.session.clone())
            .ok_or(StateError::NotFound)
    }

    /// `GET /v1/sessions` — every session, ascending id (deterministic).
    pub fn list_sessions(&self) -> Vec<Session> {
        let sessions = self.sessions.lock().unwrap();
        let mut out: Vec<Session> = sessions.values().map(|r| r.session.clone()).collect();
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
            .map(|r| r.session.clone())
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// `POST /v1/sessions/{id}` — update `title` and/or PATCH `metadata`
    /// (string upserts, null deletes, omitted preserves).
    /// `POST /v1/sessions/{id}` — update only `title` / `metadata`. `environment_id`
    /// is pinned at session creation and is not accepted here (Managed Agents
    /// contract: the container's environment is fixed for the session's lifetime —
    /// to change it, create a new session), so a caller sending it is ignored.
    pub fn update_session(
        &self,
        id: &str,
        title: Option<Option<String>>,
        metadata: Option<std::collections::BTreeMap<String, Option<String>>>,
    ) -> Result<Session, StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        let title_in_request = title.is_some();
        if let Some(title) = title {
            record.session.title = title;
        }
        if let Some(patch) = metadata {
            for (key, value) in patch {
                match value {
                    Some(v) => {
                        record.session.metadata.insert(key, v);
                    }
                    None => {
                        record.session.metadata.remove(&key);
                    }
                }
            }
        }
        // Announce the mutation on the event stream (`session.updated`): the new
        // title when the update set one, plus the full metadata bag.
        record.events.push(Event {
            id: self.next_event_id(),
            kind: OutboundKind::SessionUpdated {
                title: title_in_request
                    .then(|| record.session.title.clone())
                    .flatten(),
                metadata: record.session.metadata.clone(),
            },
            processed_at: Some(PROCESSED_AT.to_string()),
        });
        Ok(record.session.clone())
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
        // until the repository has atomically stored its tombstone and outbox fact.
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
        self.sessions_repo
            .delete_with_lifecycle(id, deleted_fact.clone())
            .await;

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
        self.end_session_sandboxes(id, &child_threads).await;
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
    async fn end_session_sandboxes(&self, id: &str, child_threads: &[serde_json::Value]) {
        let mut threads: Vec<String> = vec![id.to_string()];
        for child in child_threads {
            if let Some(child_run_id) = child["id"].as_str() {
                threads.push(child_run_id.to_string());
            }
        }
        for thread in threads {
            if let Err(err) = self.runtime.end_session(&thread).await {
                tracing::warn!(
                    session = id,
                    thread = %thread,
                    error = ?err,
                    "session teardown: sandbox dispose failed (best-effort)"
                );
            }
        }
    }

    /// `POST /v1/sessions/{id}/archive` — terminate the session: stamp
    /// `archived_at`, move `status` to `terminated`, and commit a
    /// `session.status_terminated` event so a streaming/listing client observes the
    /// terminal transition (not just the mutated status field). Idempotent: a
    /// re-archive returns the same terminal record without a second event.
    pub async fn archive_session(&self, id: &str) -> Result<Session, StateError> {
        let (newly_terminated, child_threads) = {
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
            self.sessions_repo
                .archive_with_lifecycle(id, PROCESSED_AT, terminated_fact.clone())
                .await;
        }
        let session = {
            let terminated_id = self.next_event_id();
            let mut sessions = self.sessions.lock().unwrap();
            let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
            if newly_terminated && record.session.archived_at.is_none() {
                record.session.archived_at = Some(PROCESSED_AT.to_string());
                record.session.status = "terminated";
                record.events.push(Event {
                    id: terminated_id,
                    kind: OutboundKind::SessionStatusTerminated {},
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
            record.session.clone()
        };
        // Archive is terminal (no further turns run on this session), so reap its
        // sandbox — but only on the transition, so a re-archive (idempotent) does
        // not re-dispose. The record survives as a tombstone; only the sandbox goes.
        if newly_terminated {
            self.end_session_sandboxes(id, &child_threads).await;
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
