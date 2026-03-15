use super::errors::{AgentOsResolveError, AgentOsRunError};
use super::prepare::{
    clear_tool_call_scope_state, request_has_user_input, run_lifecycle_running_patch,
    run_scope_cleanup_patches, set_or_validate_parent_thread_id, ActiveRunCleanupGuard,
};
use super::types::{AgentOs, AgentStateStoreStateCommitter, PreparedRun, RunStream};
use super::ResolvedRun;

use crate::composition::AgentOsWiringError;
use crate::contracts::runtime::run::TerminationReason;
use crate::contracts::runtime::RunIdentity;
use crate::contracts::storage::{ThreadHead, ThreadStore, VersionPrecondition};
use crate::contracts::thread::{CheckpointReason, Message, Thread};
use crate::contracts::{AgentEvent, RunContext, RunRequest, ToolCallDecision};
use crate::runtime::loop_runner::{
    run_loop_stream_with_context, AgentLoopError, RunCancellationToken, StateCommitter,
};
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;

impl AgentOs {
    pub fn agent_state_store(&self) -> Option<&Arc<dyn ThreadStore>> {
        self.agent_state_store.as_ref()
    }

    fn require_agent_state_store(&self) -> Result<&Arc<dyn ThreadStore>, AgentOsRunError> {
        self.agent_state_store
            .as_ref()
            .ok_or(AgentOsRunError::AgentStateStoreNotConfigured)
    }

    fn generate_id() -> String {
        uuid::Uuid::now_v7().simple().to_string()
    }

    /// Load a thread from storage. Returns the thread and its version.
    /// If the thread does not exist, returns `None`.
    pub async fn load_thread(&self, id: &str) -> Result<Option<ThreadHead>, AgentOsRunError> {
        let agent_state_store = self.require_agent_state_store()?;
        Ok(agent_state_store.load(id).await?)
    }

    pub async fn current_run_id_for_thread(
        &self,
        agent_id: &str,
        thread_id: &str,
    ) -> Result<Option<String>, AgentOsRunError> {
        if let Some(run_id) = self.active_run_id_for_thread(agent_id, thread_id).await {
            return Ok(Some(run_id));
        }
        let store = self.require_agent_state_store()?;
        let Some(record) = store.active_run_for_thread(thread_id).await? else {
            return Ok(None);
        };
        if !record.agent_id.is_empty() && record.agent_id != agent_id {
            return Ok(None);
        }
        Ok(Some(record.run_id))
    }

    async fn clear_suspended_calls_before_user_run_input(
        &self,
        run_request: &mut RunRequest,
    ) -> Result<(), AgentOsRunError> {
        let Some(thread_id) = run_request.thread_id.as_deref() else {
            return Ok(());
        };
        if !request_has_user_input(&run_request.messages) {
            return Ok(());
        }

        let store = self.require_agent_state_store()?;
        let Some(head) = store.load(thread_id).await? else {
            return Ok(());
        };
        if let Some(cleaned) = clear_tool_call_scope_state(&head.thread.state) {
            run_request.state = Some(cleaned);
        }
        Ok(())
    }

    pub(crate) async fn prepare_active_run_with_persistence(
        &self,
        owner_agent_id: &str,
        mut run_request: RunRequest,
        resolved: ResolvedRun,
        persist_run: bool,
        strip_lineage: bool,
    ) -> Result<(PreparedRun, String, String), AgentOsRunError> {
        if strip_lineage {
            run_request.run_id = None;
            run_request.parent_run_id = None;
            run_request.parent_thread_id = None;
        }

        let previous_run_id = if !run_request.messages.is_empty() {
            if let Some(thread_id) = run_request.thread_id.as_deref() {
                self.current_run_id_for_thread(owner_agent_id, thread_id)
                    .await?
            } else {
                None
            }
        } else {
            None
        };

        self.clear_suspended_calls_before_user_run_input(&mut run_request)
            .await?;

        let prepared = self
            .prepare_run_with_persistence(run_request, resolved, persist_run)
            .await?;
        let thread_id = prepared.thread_id().to_string();
        let run_id = prepared.run_id().to_string();

        if let Some(previous_run_id) = previous_run_id.filter(|candidate| candidate != &run_id) {
            self.cancel_active_run_by_id(&previous_run_id).await;
        }

        self.register_thread_run_handle(
            run_id.clone(),
            owner_agent_id,
            &thread_id,
            RunCancellationToken::new(),
        )
        .await;

        Ok((prepared, thread_id, run_id))
    }

    pub(crate) async fn start_prepared_active_run(
        &self,
        run_id: &str,
        prepared: PreparedRun,
    ) -> Result<RunStream, AgentOsRunError> {
        let token = self
            .active_thread_run_by_run_id(run_id)
            .await
            .ok_or_else(|| {
                AgentOsRunError::Loop(AgentLoopError::StateError(format!(
                    "active run handle missing for run '{run_id}'",
                )))
            })?
            .cancellation_token();
        let run = Self::execute_prepared(prepared.with_cancellation_token(token))?;
        if !self
            .bind_thread_run_decision_tx(run_id, run.decision_tx.clone())
            .await
        {
            self.remove_thread_run_handle(run_id).await;
            return Err(AgentOsRunError::Loop(AgentLoopError::StateError(format!(
                "active run handle missing for run '{run_id}'",
            ))));
        }
        Ok(self.wrap_run_stream_with_active_handle_cleanup(run))
    }

    pub async fn start_active_run_with_persistence(
        &self,
        owner_agent_id: &str,
        run_request: RunRequest,
        resolved: ResolvedRun,
        persist_run: bool,
        strip_lineage: bool,
    ) -> Result<RunStream, AgentOsRunError> {
        let (prepared, _thread_id, run_id) = self
            .prepare_active_run_with_persistence(
                owner_agent_id,
                run_request,
                resolved,
                persist_run,
                strip_lineage,
            )
            .await?;
        self.start_prepared_active_run(&run_id, prepared).await
    }

    fn wrap_run_stream_with_active_handle_cleanup(&self, run: RunStream) -> RunStream {
        let RunStream {
            thread_id,
            run_id,
            decision_tx,
            events,
        } = run;
        let run_id_for_cleanup = run_id.clone();
        let registry = self.active_runs.clone();
        let events = Box::pin(futures::stream::unfold(
            (
                events,
                Some(ActiveRunCleanupGuard::new(run_id_for_cleanup, registry)),
            ),
            |(mut inner, mut cleanup)| async move {
                match inner.next().await {
                    Some(event) => Some((event, (inner, cleanup))),
                    None => {
                        if let Some(mut cleanup) = cleanup.take() {
                            cleanup.cleanup_now().await;
                        }
                        None
                    }
                }
            },
        ));
        RunStream {
            thread_id,
            run_id,
            decision_tx,
            events,
        }
    }

    /// Prepare a resolved run for execution.
    ///
    /// This handles all deterministic pre-run logic:
    /// 1. Thread loading/creation from storage
    /// 2. Message deduplication and appending
    /// 3. Persisting pre-run state
    /// 4. Run-context creation
    ///
    /// Callers resolve first, optionally customize, then prepare:
    /// ```ignore
    /// let mut resolved = os.resolve("my-agent")?;
    /// resolved.tools.insert("extra".into(), tool);
    /// let prepared = os.prepare_run(request, resolved).await?;
    /// ```
    pub async fn prepare_run(
        &self,
        request: RunRequest,
        resolved: ResolvedRun,
    ) -> Result<PreparedRun, AgentOsRunError> {
        let owner_agent_id = request.agent_id.clone();
        self.prepare_active_run_with_persistence(&owner_agent_id, request, resolved, true, false)
            .await
            .map(|(prepared, _thread_id, _run_id)| prepared)
    }

    /// Prepare a resolved run and control whether the run should be persisted.
    ///
    /// This powers dialog-style runs where short-lived execution state is needed
    /// but we intentionally do not keep durable run records.
    pub async fn prepare_run_with_persistence(
        &self,
        mut request: RunRequest,
        resolved: ResolvedRun,
        persist_run: bool,
    ) -> Result<PreparedRun, AgentOsRunError> {
        let agent_state_store = self.require_agent_state_store()?;

        let thread_id = request.thread_id.unwrap_or_else(Self::generate_id);
        let run_id = request.run_id.unwrap_or_else(Self::generate_id);
        let parent_run_id = request.parent_run_id.clone();
        let parent_thread_id = request.parent_thread_id.clone();
        let initial_decisions = std::mem::take(&mut request.initial_decisions);

        // 1. Load or create thread
        //    If frontend sent a state snapshot, apply it:
        //    - New thread: used as initial state
        //    - Existing thread: replaces current state (persisted in UserMessage delta)
        let frontend_state = request.state.take();
        let mut state_snapshot_for_delta: Option<serde_json::Value> = None;
        let (mut thread, mut version) = match agent_state_store.load(&thread_id).await? {
            Some(head) => {
                let mut t = head.thread;
                if let Some(state) = frontend_state {
                    t.state = state.clone();
                    t.patches.clear();
                    state_snapshot_for_delta = Some(state);
                }
                (t, head.version)
            }
            None => {
                let thread = if let Some(state) = frontend_state {
                    Thread::with_initial_state(thread_id.clone(), state)
                } else {
                    Thread::new(thread_id.clone())
                };
                let committed = agent_state_store.create(&thread).await?;
                (thread, committed.version)
            }
        };
        let parent_thread_id_updated =
            set_or_validate_parent_thread_id(&mut thread, &thread_id, parent_thread_id.as_deref())?;
        if parent_thread_id_updated {
            agent_state_store.save(&thread).await?;
            let refreshed = agent_state_store.load(&thread_id).await?.ok_or_else(|| {
                AgentOsRunError::Loop(AgentLoopError::StateError(format!(
                    "thread '{thread_id}' disappeared after parent_thread_id update",
                )))
            })?;
            thread = refreshed.thread;
            version = refreshed.version;
        }

        // 1a. Lazy context loading: trim pre-boundary messages.
        crate::runtime::context::trim_thread_to_latest_boundary(&mut thread);

        // 2. Set resource_id on thread if provided
        if let Some(ref resource_id) = request.resource_id {
            thread.resource_id = Some(resource_id.clone());
        }

        // 3. Deduplicate and append inbound messages
        let mut deduped_messages = Self::dedup_messages(&thread, request.messages);
        if !deduped_messages.is_empty() {
            deduped_messages = Self::attach_run_metadata_to_messages(deduped_messages, &run_id);
            thread = thread.with_messages(deduped_messages.clone());
        }

        // 4. Persist run-start changes (user messages + frontend state snapshot + run state)
        let delta_messages: Vec<Arc<Message>> =
            deduped_messages.into_iter().map(Arc::new).collect();
        // 4a. Clean up stale Run-scoped state from any previous run.
        let mut delta_patches =
            run_scope_cleanup_patches(&thread.state, &resolved.agent.state_scope_registry);
        // 4b. Apply cleanup patches to in-memory thread state so the lifecycle
        //     patch reducer sees a clean base.
        for cp in &delta_patches {
            thread.state =
                tirea_state::apply_patch(&thread.state, cp.patch()).map_err(|error| {
                    AgentOsRunError::Loop(AgentLoopError::StateError(format!(
                        "failed to apply run-scope cleanup patch for thread '{thread_id}': {error}"
                    )))
                })?;
        }
        delta_patches.push(run_lifecycle_running_patch(&thread.state, &run_id)?);
        let mut changeset = crate::contracts::ThreadChangeSet::from_parts(
            run_id.clone(),
            parent_run_id.clone(),
            CheckpointReason::UserMessage,
            delta_messages,
            delta_patches.clone(),
            Vec::new(),
            state_snapshot_for_delta,
        );
        if persist_run {
            changeset = changeset.with_run_meta(crate::contracts::RunMeta {
                agent_id: request.agent_id.clone(),
                origin: request.origin,
                status: crate::contracts::storage::RunStatus::Running,
                parent_thread_id: parent_thread_id.clone(),
                termination_code: None,
                termination_detail: None,
                source_mailbox_entry_id: request.source_mailbox_entry_id.clone(),
            });
        }
        let committed = agent_state_store
            .append(&thread_id, &changeset, VersionPrecondition::Exact(version))
            .await?;
        version = committed.version;
        thread = thread.with_patches(delta_patches);
        thread.metadata.version = Some(version);

        let mut run_identity = RunIdentity::new(
            thread_id.clone(),
            parent_thread_id.clone(),
            run_id.clone(),
            parent_run_id.clone(),
            request.agent_id.clone(),
            request.origin,
        );
        if let Some(parent_tool_call_id) = resolved.parent_tool_call_id.clone() {
            run_identity = run_identity.with_parent_tool_call_id(parent_tool_call_id);
        }

        // 6. Behavior uniqueness: wiring ensures base uniqueness, but callers
        //    may mutate `resolved.agent.behavior` after resolve.
        //    Validate the final composed behavior_ids for duplicates.
        {
            let ids = resolved.agent.behavior.behavior_ids();
            let mut seen = std::collections::HashSet::with_capacity(ids.len());
            for id in &ids {
                if !seen.insert(*id) {
                    return Err(AgentOsRunError::Resolve(AgentOsResolveError::Wiring(
                        AgentOsWiringError::BehaviorAlreadyInstalled(id.to_string()),
                    )));
                }
            }
        }

        let run_ctx = RunContext::from_thread_with_registry_and_identity(
            &thread,
            resolved.run_policy,
            run_identity.clone(),
            resolved.agent.lattice_registry.clone(),
        )
        .map_err(|e| AgentOsRunError::Loop(AgentLoopError::StateError(e.to_string())))?;
        let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
        for decision in initial_decisions {
            decision_tx
                .send(decision)
                .map_err(|e| AgentOsRunError::Loop(AgentLoopError::StateError(e.to_string())))?;
        }

        Ok(PreparedRun {
            agent: Arc::new(resolved.agent),
            tools: resolved.tools,
            run_ctx,
            cancellation_token: None,
            state_committer: Some(Arc::new(AgentStateStoreStateCommitter::new(
                agent_state_store.clone(),
                persist_run,
            ))),
            decision_tx,
            decision_rx,
        })
    }

    /// Execute a previously prepared run.
    pub fn execute_prepared(prepared: PreparedRun) -> Result<RunStream, AgentOsRunError> {
        let thread_id = prepared.thread_id().to_string();
        let run_id = prepared.run_id().to_string();
        let run_identity = prepared.run_ctx.run_identity().clone();
        let events = run_loop_stream_with_context(
            prepared.agent,
            prepared.tools,
            prepared.run_ctx,
            run_identity,
            prepared.cancellation_token,
            prepared.state_committer,
            Some(prepared.decision_rx),
        );
        Ok(RunStream {
            thread_id,
            run_id,
            decision_tx: prepared.decision_tx,
            events,
        })
    }

    /// Resolve, prepare, and execute an agent run.
    ///
    /// This is the primary entry point. Callers that need to customize
    /// the resolved wiring should use [`resolve`] + mutation + [`prepare_run`]
    /// + [`execute_prepared`] instead.
    pub async fn run_stream(&self, request: RunRequest) -> Result<RunStream, AgentOsRunError> {
        let resolved = self.resolve(&request.agent_id)?;
        let prepared = self.prepare_run(request, resolved).await?;
        Self::execute_prepared(prepared)
    }

    /// Resolve, prepare, and execute an agent run with automatic mode-switch restart.
    ///
    /// When the agent has named modes and the run terminates with a mode-switch
    /// stop code, the loop automatically re-resolves with the new mode overlay
    /// and starts a new run on the same thread. The mode switch is invisible to
    /// the stream consumer.
    ///
    /// If the agent has no modes, this delegates directly to [`run_stream`].
    #[cfg(feature = "mode")]
    pub async fn run(&self, request: RunRequest) -> Result<RunStream, AgentOsRunError> {
        use crate::extensions::mode::MODE_SWITCH_STOP_CODE;

        let definition = self
            .agent(&request.agent_id)
            .ok_or_else(|| AgentOsResolveError::AgentNotFound(request.agent_id.clone()))?;

        // Fast path: no modes configured — delegate to run_stream.
        if definition.modes.is_empty() {
            return self.run_stream(request).await;
        }

        let agent_id = request.agent_id.clone();
        let os = self.clone();

        // Read initial mode from thread state (if thread exists).
        let initial_mode = if let Some(thread_id) = request.thread_id.as_deref() {
            os.read_mode_from_thread_state(thread_id).await
        } else {
            None
        };

        // First run: resolve with initial mode.
        let resolved = os.resolve_with_mode(&agent_id, initial_mode.as_deref())?;
        let first_prepared = os.prepare_run(request, resolved).await?;
        let outer_thread_id = first_prepared.thread_id().to_string();
        let outer_run_id = first_prepared.run_id().to_string();
        let thread_id = outer_thread_id.clone();
        let first_stream = Self::execute_prepared(first_prepared)?;
        let (outer_decision_tx, _outer_decision_rx) =
            tokio::sync::mpsc::unbounded_channel::<ToolCallDecision>();

        let events: Pin<Box<dyn Stream<Item = AgentEvent> + Send>> =
            Box::pin(async_stream::stream! {
                let mut current_stream = first_stream;

                loop {
                    let mut is_mode_switch = false;

                    while let Some(event) = current_stream.events.next().await {
                        match &event {
                            AgentEvent::RunFinish { termination, .. }
                                if matches!(
                                    termination,
                                    TerminationReason::Stopped(ref s)
                                        if s.code == MODE_SWITCH_STOP_CODE
                                ) =>
                            {
                                // Swallow the mode-switch RunFinish — don't forward.
                                is_mode_switch = true;
                                break;
                            }
                            _ => yield event,
                        }
                    }

                    if !is_mode_switch {
                        break;
                    }

                    // Read the requested mode from thread state.
                    let new_mode = os.read_mode_from_thread_state(&thread_id).await;

                    // Re-resolve with the new mode.
                    let resolved = match os.resolve_with_mode(&agent_id, new_mode.as_deref()) {
                        Ok(r) => r,
                        Err(e) => {
                            yield AgentEvent::Error {
                                message: format!("mode switch resolve failed: {e}"),
                                code: None,
                            };
                            break;
                        }
                    };

                    // Build a resume request on the same thread (no new messages).
                    let resume_request = RunRequest {
                        agent_id: agent_id.clone(),
                        thread_id: Some(thread_id.clone()),
                        run_id: None,
                        parent_run_id: None,
                        parent_thread_id: None,
                        resource_id: None,
                        origin: Default::default(),
                        state: None,
                        messages: Vec::new(),
                        initial_decisions: Vec::new(),
                        source_mailbox_entry_id: None,
                    };

                    let prepared = match os.prepare_run(resume_request, resolved).await {
                        Ok(p) => p,
                        Err(e) => {
                            yield AgentEvent::Error {
                                message: format!("mode switch prepare failed: {e}"),
                                code: None,
                            };
                            break;
                        }
                    };

                    current_stream = match Self::execute_prepared(prepared) {
                        Ok(s) => s,
                        Err(e) => {
                            yield AgentEvent::Error {
                                message: format!("mode switch execute failed: {e}"),
                                code: None,
                            };
                            break;
                        }
                    };
                }
            });

        Ok(RunStream {
            thread_id: outer_thread_id,
            run_id: outer_run_id,
            decision_tx: outer_decision_tx,
            events,
        })
    }

    /// Read the requested mode from thread state via JSON path.
    ///
    /// Returns `requested_mode` if set, otherwise `active_mode`.
    /// No dependency on extension crate types — pure JSON path lookup.
    #[cfg(feature = "mode")]
    async fn read_mode_from_thread_state(&self, thread_id: &str) -> Option<String> {
        let head = self.load_thread(thread_id).await.ok()??;
        let state = &head.thread.state;
        let mode_state = state.get("agent_mode")?;
        // Prefer requested_mode (pending switch), fall back to active_mode.
        mode_state
            .get("requested_mode")
            .and_then(|v| v.as_str())
            .or_else(|| mode_state.get("active_mode").and_then(|v| v.as_str()))
            .map(String::from)
    }

    /// Deduplicate incoming messages against existing thread messages.
    ///
    /// Skips messages whose ID or tool_call_id already exists in the thread.
    fn dedup_messages(thread: &Thread, incoming: Vec<Message>) -> Vec<Message> {
        use std::collections::HashSet;

        let existing_ids: HashSet<&str> = thread
            .messages
            .iter()
            .filter_map(|m| m.id.as_deref())
            .collect();
        let existing_tool_call_ids: HashSet<&str> = thread
            .messages
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();

        incoming
            .into_iter()
            .filter(|m| {
                // Dedup tool messages by tool_call_id
                if let Some(ref tc_id) = m.tool_call_id {
                    if existing_tool_call_ids.contains(tc_id.as_str()) {
                        return false;
                    }
                }
                // Dedup by message id
                if let Some(ref id) = m.id {
                    if existing_ids.contains(id.as_str()) {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    fn attach_run_metadata_to_messages(mut messages: Vec<Message>, run_id: &str) -> Vec<Message> {
        messages.iter_mut().for_each(|message| {
            let mut metadata = message.metadata.clone().unwrap_or_default();
            metadata.run_id = Some(run_id.to_string());
            message.metadata = Some(metadata);
        });
        messages
    }

    // --- Internal low-level helper (legacy) ---

    #[deprecated(note = "Use prepare_run + execute_prepared instead")]
    #[allow(dead_code)]
    pub(crate) fn run_stream_with_context(
        &self,
        agent_id: &str,
        thread: Thread,
        cancellation_token: Option<RunCancellationToken>,
        state_committer: Option<Arc<dyn StateCommitter>>,
    ) -> Result<impl futures::Stream<Item = AgentEvent> + Send, AgentOsRunError> {
        let resolved = self.resolve(agent_id)?;
        let run_identity = RunIdentity::new(
            thread.id.clone(),
            thread.parent_thread_id.clone(),
            thread.id.clone(),
            None,
            agent_id.to_string(),
            crate::contracts::storage::RunOrigin::Internal,
        );
        let run_ctx = RunContext::from_thread_with_registry_and_identity(
            &thread,
            resolved.run_policy,
            run_identity.clone(),
            resolved.agent.lattice_registry.clone(),
        )
        .map_err(|e| AgentOsRunError::Loop(AgentLoopError::StateError(e.to_string())))?;
        Ok(run_loop_stream_with_context(
            Arc::new(resolved.agent),
            resolved.tools,
            run_ctx,
            run_identity,
            cancellation_token,
            state_committer,
            None,
        ))
    }
}
