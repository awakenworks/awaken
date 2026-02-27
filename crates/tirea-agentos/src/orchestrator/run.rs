use super::*;
use crate::contracts::runtime::state_paths::RUN_LIFECYCLE_STATE_PATH;
use crate::contracts::runtime::RunStatus;
use crate::contracts::storage::VersionPrecondition;
use crate::runtime::loop_runner::run_loop_stream;
use tirea_state::{Op, Patch, Path, TrackedPatch};

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

fn run_lifecycle_running_patch(run_id: &str) -> TrackedPatch {
    TrackedPatch::new(Patch::with_ops(vec![Op::set(
        Path::root().key(RUN_LIFECYCLE_STATE_PATH),
        serde_json::json!({
            "id": run_id,
            "status": RunStatus::Running,
            "done_reason": serde_json::Value::Null,
            "updated_at": now_unix_millis(),
        }),
    )]))
    .with_source("agentos_prepare_run")
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
        mut request: RunRequest,
        mut resolved: ResolvedRun,
    ) -> Result<PreparedRun, AgentOsRunError> {
        let agent_state_store = self.require_agent_state_store()?;

        let thread_id = request.thread_id.unwrap_or_else(Self::generate_id);
        let run_id = request.run_id.unwrap_or_else(Self::generate_id);
        let parent_run_id = request.parent_run_id.clone();
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

        // 2. Set resource_id on thread if provided
        if let Some(ref resource_id) = request.resource_id {
            thread.resource_id = Some(resource_id.clone());
        }

        // 3. Deduplicate and append inbound messages
        let deduped_messages = Self::dedup_messages(&thread, request.messages);
        if !deduped_messages.is_empty() {
            thread = thread.with_messages(deduped_messages.clone());
        }

        // 4. Persist run-start changes (user messages + frontend state snapshot + run state)
        let delta_messages: Vec<Arc<Message>> =
            deduped_messages.into_iter().map(Arc::new).collect();
        let delta_patches = vec![run_lifecycle_running_patch(&run_id)];
        let changeset = crate::contracts::ThreadChangeSet::from_parts(
            run_id.clone(),
            parent_run_id.clone(),
            CheckpointReason::UserMessage,
            delta_messages,
            delta_patches.clone(),
            state_snapshot_for_delta,
        );
        let committed = agent_state_store
            .append(&thread_id, &changeset, VersionPrecondition::Exact(version))
            .await?;
        version = committed.version;
        thread = thread.with_patches(delta_patches);
        thread.metadata.version = Some(version);

        // 5. Set run identity on the run_config
        let _ = resolved.run_config.set("run_id", run_id.clone());
        if let Some(parent) = parent_run_id.as_deref() {
            let _ = resolved.run_config.set("parent_run_id", parent.to_string());
        }

        // 6. Validate plugin uniqueness (caller may have added plugins to resolved).
        Self::ensure_unique_plugin_ids(&resolved.agent.plugins)
            .map_err(AgentOsResolveError::from)
            .map_err(AgentOsRunError::from)?;

        let run_ctx = RunContext::from_thread(&thread, resolved.run_config)
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
            cancellation_token: None,
            state_committer: Some(Arc::new(AgentStateStoreStateCommitter::new(
                agent_state_store.clone(),
            ))),
            decision_tx,
            decision_rx,
        })
    }

    /// Execute a previously prepared run.
    pub fn execute_prepared(prepared: PreparedRun) -> Result<RunStream, AgentOsRunError> {
        let events = run_loop_stream(
            prepared.agent,
            prepared.tools,
            prepared.run_ctx,
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

    // --- Internal low-level helper (used by agent tools) ---

    pub(crate) fn run_stream_with_context(
        &self,
        agent_id: &str,
        thread: Thread,
        cancellation_token: Option<RunCancellationToken>,
        state_committer: Option<Arc<dyn StateCommitter>>,
    ) -> Result<impl futures::Stream<Item = AgentEvent> + Send, AgentOsRunError> {
        let resolved = self.resolve(agent_id)?;
        let run_ctx = RunContext::from_thread(&thread, resolved.run_config)
            .map_err(|e| AgentOsRunError::Loop(AgentLoopError::StateError(e.to_string())))?;
        Ok(run_loop_stream(
            Arc::new(resolved.agent),
            resolved.tools,
            run_ctx,
            cancellation_token,
            state_committer,
            None,
        ))
    }
}
