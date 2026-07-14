//! Session lifecycle for [`ManagedState`]: create, rehydrate, get/list,
//! update, delete, and archive.

use super::*;

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
        let bindings = req
            .mcp_servers
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
                    credential_source_id,
                    refresh,
                }
            })
            .collect();
        // Parse the wire `resources[]` (ADR-0038) into staged mounts, and project each
        // into a DTO entry so the created session echoes its create-time resources —
        // list/get/delete then address these and any later-attached ones uniformly.
        let resources: Vec<SessionResource> = req
            .resources
            .iter()
            .filter_map(parse_session_resource)
            .collect();
        let resource_dtos: Vec<serde_json::Value> = resources
            .iter()
            .enumerate()
            .map(|(n, r)| resource_dto(&id, n, r))
            .collect();
        // Resolve the session's environment (defaulting to the local one) and its
        // networking policy once, for both the SessionInit (staged before the first
        // turn) and the echoed Session object.
        let environment_id = req
            .environment_id
            .clone()
            .unwrap_or_else(|| "env_local".to_string());
        let deny_egress = match self.environments.as_ref() {
            Some(e) => e.deny_egress(&environment_id).await,
            None => false,
        };
        self.runtime
            .prepare_session(
                &id,
                SessionInit {
                    agent_id: agent_id.clone(),
                    mcp_servers: bindings,
                    resources,
                    model: req.awaken_model().map(str::to_string),
                    runtime: req.awaken_runtime().map(str::to_string),
                    deny_egress,
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
        let caps = self.runtime.capabilities();
        let session = Session {
            id: id.clone(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: 1,
                // R6: echo the session's actual model — the requested override, else
                // the host default — so the client sees which model the session runs.
                model: ModelConfig::new(
                    req.awaken_model()
                        .map(str::to_string)
                        .unwrap_or_else(|| self.runtime.model()),
                ),
                name: agent_id.clone(),
                description: None,
                system: None,
                tools: project::agent_tools(&caps),
                // Echo the accepted servers in the SDK's `{name, type:"url", url}` shape.
                mcp_servers: req
                    .mcp_servers
                    .iter()
                    .map(|s| serde_json::to_value(s).expect("mcp server wire serializes"))
                    .collect(),
                skills: project::agent_skills(&caps),
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
            deployment_id: None,
        };
        // Persist the session's config (secret-free) so a restart or a peer process
        // rehydrates its real agent/model/title/metadata/MCP, not a placeholder.
        // The core session record is tenancy-agnostic (authz is an edge aspect) —
        // it never stores a workspace/org.
        self.sessions_repo
            .save(PersistedSession {
                session_id: id.clone(),
                agent_id: agent_id.clone(),
                model: session.agent.model.id.clone(),
                title: session.title.clone(),
                metadata: session.metadata.clone(),
                environment_id: session.environment_id.clone(),
                mcp_servers: session.agent.mcp_servers.clone(),
            })
            .await;
        // Project the committed create as a lifecycle fact: a fresh session is idle,
        // so fan out `session.status_idled` (the webhook catalog name — past-tense
        // fact, distinct from the SSE `session.status_idle` transition) to any
        // workspace-scoped subscribers. The owning workspace comes from the edge (the
        // aspect), passed in — never read back from the core record. Out-of-band.
        if let Some(sink) = &self.lifecycle_sink {
            sink.emit(&id, workspace_id.as_deref(), lifecycle_event::SESSION_IDLED)
                .await;
        }
        // Record the session's owner (ADR-0051): in the aspect-layer in-memory
        // index (same-process) and — for a durable backend — beside the persisted
        // config, so the edge ownership guard fences a cross-tenant request even
        // across a restart that lost the index. A bare/self-hosted create (no
        // resolved workspace) owns under the seeded default scope.
        let owner_scope = workspace_id
            .clone()
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
        self.owners
            .lock()
            .unwrap()
            .insert(id.clone(), owner_scope.clone());
        self.sessions_repo.set_owner(&id, &owner_scope).await;
        self.sessions.lock().unwrap().insert(
            id,
            SessionRecord {
                agent_id,
                session: session.clone(),
                events: Vec::new(),
                child_threads: Vec::new(),
            },
        );
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
        let caps = self.runtime.capabilities();
        let (agent_id, model, environment_id, title, metadata, mcp_servers) = match persisted {
            Some(p) => (
                p.agent_id,
                p.model,
                p.environment_id,
                p.title,
                p.metadata,
                p.mcp_servers,
            ),
            None => (
                "assistant".to_string(),
                self.runtime.model(),
                "env_local".to_string(),
                None,
                Default::default(),
                Vec::new(),
            ),
        };
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
            archived_at: None,
            title,
            metadata,
            // Non-durable: `PersistedSession` does not carry the create-time mounts,
            // and rehydration does not re-stage them into the host, so a rehydrated
            // session reports no resources. (Durable resources + restart re-staging
            // is a separate slice; storing the DTO alone would falsely show mounts
            // the sandbox no longer has.)
            resources: Vec::new(),
            outcome_evaluations: Vec::new(),
            status: "idle",
            stats: SessionStats::default(),
            usage: Usage::default(),
            vault_ids: Vec::new(),
            deployment_id: None,
        }
    }

    /// Recover a session whose in-memory record was lost from durable truth (a
    /// process restart, ADR-0039). If the store holds a committed transcript for
    /// `id`, rebuild the record — the projected history plus a reconstructed
    /// session object — so a resume can continue the parked run. A thread with no
    /// committed truth stays `NotFound` (fail closed): the store is authoritative.
    pub(crate) async fn ensure_session(&self, id: &str) -> Result<(), StateError> {
        if self.sessions.lock().unwrap().contains_key(id) {
            return Ok(());
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
        let persisted = self.sessions_repo.get(id).await;
        let agent_id = persisted
            .as_ref()
            .map_or_else(|| "assistant".to_string(), |p| p.agent_id.clone());
        let record = SessionRecord {
            agent_id,
            session: self.rehydrated_session(id, persisted),
            events,
            child_threads: Vec::new(),
        };
        self.sessions
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert(record);
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

    /// `DELETE /v1/sessions/{id}` — drop the in-memory record.
    pub fn delete_session(&self, id: &str) -> Result<(), StateError> {
        self.sessions
            .lock()
            .unwrap()
            .remove(id)
            .map(|_| ())
            .ok_or(StateError::NotFound)
    }

    /// `POST /v1/sessions/{id}/archive` — terminate the session: stamp
    /// `archived_at`, move `status` to `terminated`, and commit a
    /// `session.status_terminated` event so a streaming/listing client observes the
    /// terminal transition (not just the mutated status field). Idempotent: a
    /// re-archive returns the same terminal record without a second event.
    pub async fn archive_session(&self, id: &str) -> Result<Session, StateError> {
        // Mutate under the lock, then release it before any await (the sink is async,
        // and a std `MutexGuard` must not be held across `.await`). `newly_terminated`
        // gates the projection so a re-archive (idempotent) fans out no second event.
        let (session, newly_terminated) = {
            let terminated_id = self.next_event_id();
            let mut sessions = self.sessions.lock().unwrap();
            let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
            let newly = record.session.archived_at.is_none();
            if newly {
                record.session.archived_at = Some(PROCESSED_AT.to_string());
                record.session.status = "terminated";
                record.events.push(Event {
                    id: terminated_id,
                    kind: OutboundKind::SessionStatusTerminated {},
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
            (record.session.clone(), newly)
        };
        // Project the terminal transition as a lifecycle fact, mirroring create's
        // `session.status_idled`. The owning workspace is resolved from the session's
        // persisted owner (the archive edge carries only the id) so a subscription in
        // that workspace is matched even after a restart lost the in-memory index.
        if newly_terminated && let Some(sink) = &self.lifecycle_sink {
            let owner = self.resolve_owner(id).await;
            sink.emit(id, owner.as_deref(), lifecycle_event::SESSION_TERMINATED)
                .await;
        }
        Ok(session)
    }
}
