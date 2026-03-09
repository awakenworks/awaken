use super::*;
use crate::contracts::runtime::state::{
    reduce_state_actions, AnyStateAction, ScopeContext, StateScopeRegistry,
};
use crate::contracts::runtime::{RunLifecycleAction, RunLifecycleState, RunStatus};
use crate::contracts::storage::VersionPrecondition;
use crate::loop_runtime::loop_runner::{run_loop_stream_with_context, RunExecutionContext};
use futures::StreamExt;
use tirea_contract::runtime::suspended_calls_from_state;
use tirea_state::{Op, Patch, TrackedPatch};

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

struct ActiveRunCleanupGuard {
    run_id: String,
    registry: Arc<thread_run::ActiveThreadRunRegistry>,
    armed: bool,
}

impl ActiveRunCleanupGuard {
    fn new(run_id: String, registry: Arc<thread_run::ActiveThreadRunRegistry>) -> Self {
        Self {
            run_id,
            registry,
            armed: true,
        }
    }

    async fn cleanup_now(&mut self) {
        if !self.armed {
            return;
        }
        self.registry.remove_by_run_id(&self.run_id).await;
        self.armed = false;
    }
}

impl Drop for ActiveRunCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let run_id = self.run_id.clone();
        let registry = self.registry.clone();
        tokio::spawn(async move {
            registry.remove_by_run_id(&run_id).await;
        });
    }
}

/// Generate delete patches for all Run-scoped state paths that exist in the
/// current state. This ensures stale run-scoped state from a previous run is
/// cleaned up before the new run begins.
fn run_scope_cleanup_patches(
    base_state: &serde_json::Value,
    scope_registry: &StateScopeRegistry,
) -> Vec<TrackedPatch> {
    let paths = scope_registry.run_scoped_paths();
    let mut patches = Vec::new();
    for path in paths {
        let parsed = tirea_state::parse_path(path);
        if tirea_state::get_at_path(base_state, &parsed).is_some() {
            let patch = Patch::with_ops(vec![Op::delete(parsed)]);
            patches.push(TrackedPatch::new(patch).with_source("prepare_run:scope_cleanup"));
        }
    }
    patches
}

fn run_lifecycle_running_patch(
    base_state: &serde_json::Value,
    run_id: &str,
) -> Result<TrackedPatch, AgentOsRunError> {
    let updated_at = now_unix_millis();
    let actions = vec![AnyStateAction::new::<RunLifecycleState>(
        RunLifecycleAction::Set {
            id: run_id.to_string(),
            status: RunStatus::Running,
            done_reason: None,
            updated_at,
        },
    )];
    let mut patches = reduce_state_actions(
        actions,
        base_state,
        "agentos_prepare_run",
        &ScopeContext::run(),
    )
    .map_err(|e| AgentOsRunError::Loop(AgentLoopError::StateError(e.to_string())))?;
    let Some(patch) = patches.pop() else {
        return Err(AgentOsRunError::Loop(AgentLoopError::StateError(
            "failed to emit run lifecycle running patch: reducer produced no patch".to_string(),
        )));
    };
    Ok(patch)
}

fn set_or_validate_parent_thread_id(
    thread: &mut Thread,
    thread_id: &str,
    requested_parent_thread_id: Option<&str>,
) -> Result<bool, AgentOsRunError> {
    let Some(requested_parent_thread_id) = requested_parent_thread_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(false);
    };

    if let Some(existing) = thread.parent_thread_id.as_deref() {
        if existing != requested_parent_thread_id {
            return Err(AgentOsRunError::Loop(AgentLoopError::StateError(format!(
                "parent_thread_id mismatch for thread '{thread_id}': existing='{existing}', requested='{requested_parent_thread_id}'",
            ))));
        }
        return Ok(false);
    }

    thread.parent_thread_id = Some(requested_parent_thread_id.to_string());
    Ok(true)
}

fn request_has_user_input(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        message.role == crate::contracts::thread::Role::User && !message.content.trim().is_empty()
    })
}

fn clear_suspended_tool_call_state(state: &serde_json::Value) -> Option<serde_json::Value> {
    let suspended = suspended_calls_from_state(state);
    if suspended.is_empty() {
        return None;
    }

    let mut cleaned_state = state.clone();
    let root = cleaned_state.as_object_mut()?;
    let scope_obj = root
        .get_mut("__tool_call_scope")
        .and_then(serde_json::Value::as_object_mut)?;

    let mut removed_any = false;
    for call_id in suspended.keys() {
        if scope_obj.remove(call_id).is_some() {
            removed_any = true;
        }
    }

    if removed_any && scope_obj.is_empty() {
        root.remove("__tool_call_scope");
    }

    removed_any.then_some(cleaned_state)
}

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
        if let Some(cleaned) = clear_suspended_tool_call_state(&head.thread.state) {
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
        let thread_id = prepared.thread_id.clone();
        let run_id = prepared.run_id.clone();

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
        self.prepare_run_with_persistence(request, resolved, true)
            .await
    }

    /// Prepare a resolved run and control whether the run should be persisted.
    ///
    /// This powers dialog-style runs where short-lived execution state is needed
    /// but we intentionally do not keep durable run records.
    pub async fn prepare_run_with_persistence(
        &self,
        mut request: RunRequest,
        mut resolved: ResolvedRun,
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
            });
        }
        let committed = agent_state_store
            .append(&thread_id, &changeset, VersionPrecondition::Exact(version))
            .await?;
        version = committed.version;
        thread = thread.with_patches(delta_patches);
        thread.metadata.version = Some(version);

        // 5. Set run identity on the run_config
        resolved.run_config.set("run_id", run_id.clone())?;
        if let Some(parent) = parent_run_id.as_deref() {
            resolved
                .run_config
                .set("parent_run_id", parent.to_string())?;
        }
        resolved
            .run_config
            .set("agent_id", request.agent_id.clone())?;
        resolved.run_config.set(
            "origin",
            serde_json::to_value(request.origin)
                .map_err(|e| AgentOsRunError::Loop(AgentLoopError::StateError(e.to_string())))?,
        )?;
        let execution_ctx = RunExecutionContext::new(
            run_id.clone(),
            parent_run_id.clone(),
            request.agent_id.clone(),
            request.origin,
        );

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

        let run_ctx = RunContext::from_thread_with_registry(
            &thread,
            resolved.run_config,
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
            thread_id,
            run_id,
            agent: Arc::new(resolved.agent),
            tools: resolved.tools,
            run_ctx,
            execution_ctx,
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
        let events = run_loop_stream_with_context(
            prepared.agent,
            prepared.tools,
            prepared.run_ctx,
            prepared.execution_ctx,
            prepared.cancellation_token,
            prepared.state_committer,
            Some(prepared.decision_rx),
        );
        Ok(RunStream {
            thread_id: prepared.thread_id,
            run_id: prepared.run_id,
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
        let run_ctx = RunContext::from_thread_with_registry(
            &thread,
            resolved.run_config,
            resolved.agent.lattice_registry.clone(),
        )
        .map_err(|e| AgentOsRunError::Loop(AgentLoopError::StateError(e.to_string())))?;
        let execution_ctx = RunExecutionContext::new(
            thread.id.clone(),
            None,
            agent_id.to_string(),
            crate::contracts::storage::RunOrigin::Internal,
        );
        Ok(run_loop_stream_with_context(
            Arc::new(resolved.agent),
            resolved.tools,
            run_ctx,
            execution_ctx,
            cancellation_token,
            state_committer,
            None,
        ))
    }
}
