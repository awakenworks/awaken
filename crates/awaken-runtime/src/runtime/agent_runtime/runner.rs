//! AgentRuntime::run() implementation.

use std::sync::Arc;

use crate::backend::{
    BackendControl, BackendLocalRootContext, BackendRootRunRequest, ExecutionBackendError,
    LocalBackend, execute_remote_root_lifecycle, execution_capabilities,
    validate_root_execution_request,
};
use crate::loop_runner::{AgentLoopError, AgentRunResult};
use crate::registry::{ExecutionResolver, ResolvedExecution};
use awaken_contract::contract::active_agent::ActiveAgentIdKey;
use awaken_contract::contract::event_sink::{EventSink, NullEventSink};
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::message::{Message, Role, Visibility};
use awaken_contract::contract::storage::RunRecord;
use awaken_contract::contract::suspension::ToolCallStatus;
use awaken_contract::now_ms;
use awaken_contract::state::PersistedState;

use super::AgentRuntime;
use super::run_request::{RunRequest, ThreadContextSnapshot};

const DEFAULT_AGENT_ID: &str = "default";

/// RAII guard that unregisters the active run on drop, ensuring cleanup
/// even if the run future panics or is cancelled.
struct RunSlotGuard<'a> {
    runtime: &'a AgentRuntime,
    run_id: String,
}

impl Drop for RunSlotGuard<'_> {
    fn drop(&mut self) {
        self.runtime.unregister_run(&self.run_id);
    }
}

struct PreparedLocalRootExecution {
    messages: Vec<Message>,
    phase_runtime: crate::phase::PhaseRuntime,
    inbox: crate::inbox::InboxReceiver,
    inbox_sender: crate::inbox::InboxSender,
    /// Per-run wiring for context auto-compaction. Some when the preflight
    /// resolved agent declared `autocompact_threshold` and the runtime had
    /// not already attached a manager + summarizer.
    compaction: Option<CompactionRuntime>,
}

/// Per-run context auto-compaction wiring: shared manager + summarizer that
/// the loop's resolver-wrapper grafts onto every `ResolvedAgent` it produces.
#[derive(Clone)]
struct CompactionRuntime {
    manager: std::sync::Arc<crate::extensions::background::BackgroundTaskManager>,
    summarizer: std::sync::Arc<dyn crate::context::ContextSummarizer>,
}

/// Build the per-run compaction wiring when the preflight agent declared
/// `autocompact_threshold` and no upstream code (builder, custom resolver)
/// already attached a manager + summarizer.
///
/// The manager has its store and owner inbox bound here so background
/// compaction tasks can commit metadata and deliver completion events.
/// `BackgroundTaskPlugin`'s state keys are registered on the store; if a
/// matching plugin is already installed the dup error is treated as a
/// no-op since the keys are already live.
fn build_compaction_runtime(
    preflight_resolved: &crate::registry::ResolvedAgent,
    store: &crate::state::StateStore,
    owner_inbox: &crate::inbox::InboxSender,
) -> Result<Option<CompactionRuntime>, AgentLoopError> {
    let opts_in = preflight_resolved
        .context_policy()
        .and_then(|policy| policy.autocompact_threshold)
        .is_some();
    if !opts_in {
        return Ok(None);
    }
    if preflight_resolved.background_manager.is_some()
        && preflight_resolved.context_summarizer.is_some()
    {
        return Ok(None);
    }

    let manager = std::sync::Arc::new(crate::extensions::background::BackgroundTaskManager::new());
    manager.set_store(store.clone());
    manager.set_owner_inbox(owner_inbox.clone());

    match store.install_plugin(crate::extensions::background::BackgroundTaskPlugin::new(
        manager.clone(),
    )) {
        Ok(()) => {}
        Err(awaken_contract::StateError::PluginAlreadyInstalled { .. }) => {
            // Keys already registered by an upstream wiring; reuse store as-is.
        }
        Err(awaken_contract::StateError::KeyAlreadyRegistered { .. }) => {
            // A different plugin owns one of the background-task keys; reuse them.
        }
        Err(error) => return Err(AgentLoopError::PhaseError(error)),
    }

    let compaction_config = preflight_resolved
        .spec
        .config::<crate::context::CompactionConfigKey>()
        .unwrap_or_default();
    let summarizer: std::sync::Arc<dyn crate::context::ContextSummarizer> = std::sync::Arc::new(
        crate::context::DefaultSummarizer::with_config(compaction_config),
    );

    Ok(Some(CompactionRuntime {
        manager,
        summarizer,
    }))
}

/// Resolver wrapper that grafts a per-run `BackgroundTaskManager` and
/// `ContextSummarizer` onto every `ResolvedAgent` whose context policy opts
/// in via `autocompact_threshold`. The same `Arc`s are reused across resolve
/// calls so the manager bound during `bind_local_execution_env` is the one
/// used by every subsequent loop step.
struct CompactionResolver<'a> {
    inner: &'a dyn crate::registry::ExecutionResolver,
    runtime: CompactionRuntime,
}

impl<'a> CompactionResolver<'a> {
    fn new(inner: &'a dyn crate::registry::ExecutionResolver, runtime: CompactionRuntime) -> Self {
        Self { inner, runtime }
    }

    fn graft(
        &self,
        mut resolved: crate::registry::ResolvedAgent,
    ) -> crate::registry::ResolvedAgent {
        let opts_in = resolved
            .context_policy()
            .and_then(|policy| policy.autocompact_threshold)
            .is_some();
        if !opts_in {
            return resolved;
        }
        if resolved.background_manager.is_none() {
            resolved.background_manager = Some(self.runtime.manager.clone());
        }
        if resolved.context_summarizer.is_none() {
            resolved.context_summarizer = Some(self.runtime.summarizer.clone());
        }
        resolved
    }
}

impl crate::registry::AgentResolver for CompactionResolver<'_> {
    fn resolve(
        &self,
        agent_id: &str,
    ) -> Result<crate::registry::ResolvedAgent, crate::RuntimeError> {
        self.inner
            .resolve(agent_id)
            .map(|resolved| self.graft(resolved))
    }

    fn agent_ids(&self) -> Vec<String> {
        self.inner.agent_ids()
    }
}

impl crate::registry::ExecutionResolver for CompactionResolver<'_> {
    fn resolve_execution(
        &self,
        agent_id: &str,
    ) -> Result<crate::registry::ResolvedExecution, crate::RuntimeError> {
        let execution = self.inner.resolve_execution(agent_id)?;
        Ok(match execution {
            crate::registry::ResolvedExecution::Local(resolved) => {
                crate::registry::ResolvedExecution::Local(Box::new(self.graft(*resolved)))
            }
            other => other,
        })
    }
}

impl AgentRuntime {
    /// Run an agent loop until it returns an [`AgentRunResult`].
    ///
    /// This is a convenience wrapper for one-shot CLI programs and examples
    /// that only need the final [`AgentRunResult`]. Use [`Self::run`] with an
    /// [`EventSink`] when streaming events to SSE, WebSocket, protocol adapters,
    /// or tests.
    pub async fn run_to_completion(
        &self,
        request: RunRequest,
    ) -> Result<AgentRunResult, AgentLoopError> {
        self.run(request, Arc::new(NullEventSink)).await
    }

    /// Run an agent loop.
    ///
    /// This is the single production entry point. It:
    /// 1. Resolves the agent from the registry
    /// 2. Loads thread messages from storage (if configured)
    /// 3. Applies resume decisions (if present in request)
    /// 4. Creates a PhaseRuntime and StateStore
    /// 5. Registers the active run
    /// 6. Calls `run_agent_loop` internally
    /// 7. Unregisters the run when complete
    ///
    /// Run an agent loop. Returns the result when the run completes.
    ///
    /// Use `cancel()` / `send_decisions()` on `AgentRuntime` for external
    /// control of in-flight runs.
    pub async fn run(
        &self,
        request: RunRequest,
        sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        self.run_inner(request, sink, None).await
    }

    #[doc(hidden)]
    pub async fn run_with_thread_context(
        &self,
        request: RunRequest,
        sink: Arc<dyn EventSink>,
        thread_ctx: Option<ThreadContextSnapshot>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        self.run_inner(request, sink, thread_ctx).await
    }

    async fn run_inner(
        &self,
        request: RunRequest,
        sink: Arc<dyn EventSink>,
        thread_ctx: Option<ThreadContextSnapshot>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        let RunRequest {
            messages: request_messages,
            messages_already_persisted,
            thread_id,
            agent_id,
            overrides,
            decisions,
            frontend_tools,
            origin: req_origin,
            run_mode,
            adapter,
            parent_run_id: req_parent_run_id,
            parent_thread_id: req_parent_thread_id,
            continue_run_id,
            run_id_hint,
            dispatch_id_hint,
            dispatch_id,
            session_id,
            transport_request_id,
            run_inbox,
        } = request;
        let new_messages = request_messages.clone();
        let requested_continue_run_id = continue_run_id.clone();
        let agent_id = self
            .resolve_agent_id(agent_id, &thread_id, &thread_ctx)
            .await?;
        let run_resolver: Arc<dyn ExecutionResolver> =
            if let Some(snapshot) = self.registry_snapshot() {
                Arc::new(crate::registry::resolve::RegistrySetResolver::new(
                    snapshot.into_registries(),
                ))
            } else {
                self.execution_resolver_arc()
            };
        let resolved_execution = run_resolver
            .resolve_execution(&agent_id)
            .map_err(AgentLoopError::RuntimeError)?;
        let capabilities =
            execution_capabilities(&resolved_execution).map_err(local_root_execution_error)?;
        let (run_id, is_continuation) = self
            .next_root_run_id(
                &thread_id,
                continue_run_id,
                run_id_hint,
                dispatch_id_hint,
                matches!(&resolved_execution, ResolvedExecution::Local(_)),
                &thread_ctx,
            )
            .await?;
        let run_origin = match req_origin {
            awaken_contract::contract::storage::RunRequestOrigin::User => {
                awaken_contract::contract::identity::RunOrigin::User
            }
            awaken_contract::contract::storage::RunRequestOrigin::A2A => {
                awaken_contract::contract::identity::RunOrigin::Subagent
            }
            awaken_contract::contract::storage::RunRequestOrigin::Internal => {
                awaken_contract::contract::identity::RunOrigin::Internal
            }
        };
        let mut run_identity = RunIdentity::new(
            thread_id.clone(),
            req_parent_thread_id,
            run_id.clone(),
            req_parent_run_id,
            agent_id.clone(),
            run_origin,
        )
        .with_run_mode(run_mode)
        .with_adapter(adapter);
        if let Some(dispatch_id) = dispatch_id {
            run_identity = run_identity.with_dispatch_id(dispatch_id);
        }
        if let Some(session_id) = session_id {
            run_identity = run_identity.with_session_id(session_id);
        }
        if let Some(transport_request_id) = transport_request_id {
            run_identity = run_identity.with_transport_request_id(transport_request_id);
        }

        let mut run_inbox = run_inbox;
        let mut compaction_runtime: Option<CompactionRuntime> = None;
        let (messages, phase_runtime, inbox, live_inbox_sender, previous_non_local_state) =
            match &resolved_execution {
                ResolvedExecution::Local(preflight_resolved) => {
                    let prepared = self
                        .prepare_local_root_execution(
                            preflight_resolved,
                            &thread_id,
                            request_messages,
                            messages_already_persisted,
                            &decisions,
                            run_inbox.take(),
                            &thread_ctx,
                        )
                        .await?;
                    compaction_runtime = prepared.compaction;
                    (
                        prepared.messages,
                        Some(prepared.phase_runtime),
                        Some(prepared.inbox),
                        Some(prepared.inbox_sender),
                        None,
                    )
                }
                ResolvedExecution::NonLocal(_) => {
                    let live_inbox_sender =
                        run_inbox.as_ref().map(|run_inbox| run_inbox.sender.clone());
                    (
                        self.load_non_local_messages(
                            &thread_id,
                            request_messages,
                            messages_already_persisted,
                            &thread_ctx,
                        )
                        .await?,
                        None,
                        run_inbox.take().map(|run_inbox| run_inbox.receiver),
                        live_inbox_sender,
                        self.load_non_local_state(
                            &thread_id,
                            requested_continue_run_id.as_deref(),
                            &thread_ctx,
                        )
                        .await?,
                    )
                }
            };
        let run_created_at = now_ms();

        let (handle, cancellation_token, raw_decision_rx) = self.create_run_channels_with_inbox(
            run_id.clone(),
            run_identity.trace.dispatch_id.clone(),
            live_inbox_sender,
        );
        let runtime_cancellation_token = cancellation_token.clone();
        let decision_rx = if capabilities.decisions {
            Some(raw_decision_rx)
        } else {
            drop(raw_decision_rx);
            None
        };

        // Wrap the resolver so every `ResolvedAgent` it produces during this
        // run carries the per-run compaction manager + summarizer when the
        // agent opted in via `autocompact_threshold`. Lifetime is tied to
        // `backend_request`, which is consumed before this scope ends.
        let compaction_resolver = compaction_runtime
            .clone()
            .map(|runtime| CompactionResolver::new(run_resolver.as_ref(), runtime));
        let resolver_for_backend: &dyn ExecutionResolver = match compaction_resolver.as_ref() {
            Some(wrapper) => wrapper,
            None => run_resolver.as_ref(),
        };

        let backend_request = BackendRootRunRequest {
            agent_id: &agent_id,
            messages,
            new_messages,
            sink: sink.clone(),
            resolver: resolver_for_backend,
            run_identity: run_identity.clone(),
            checkpoint_store: match &resolved_execution {
                ResolvedExecution::Local(_) => phase_runtime.as_ref().and(self.storage.as_deref()),
                ResolvedExecution::NonLocal(_) => self.storage.as_deref(),
            },
            control: BackendControl {
                cancellation_token: capabilities
                    .cancellation
                    .supports_cooperative_token()
                    .then_some(cancellation_token),
                decision_rx,
            },
            decisions,
            overrides,
            frontend_tools,
            local: phase_runtime
                .as_ref()
                .map(|phase_runtime| BackendLocalRootContext { phase_runtime }),
            inbox,
            is_continuation,
        };
        validate_root_execution_request(&resolved_execution, &backend_request).map_err(
            |error| match error {
                ExecutionBackendError::Loop(loop_error) => loop_error,
                other => AgentLoopError::RuntimeError(crate::RuntimeError::ResolveFailed {
                    message: other.to_string(),
                }),
            },
        )?;

        // Register active run (guard ensures cleanup on drop/panic/cancellation)
        self.register_run(&thread_id, handle)
            .map_err(AgentLoopError::RuntimeError)?;
        let _guard = RunSlotGuard {
            runtime: self,
            run_id: run_id.clone(),
        };

        match &resolved_execution {
            ResolvedExecution::Local(_) => {
                let result = LocalBackend::new()
                    .execute_root_with_thread_context(backend_request, thread_ctx)
                    .await
                    .map_err(local_root_execution_error)?;
                Ok(AgentRunResult {
                    run_id: run_id.clone(),
                    response: result.response.unwrap_or_default(),
                    termination: result.termination,
                    steps: result.steps,
                })
            }
            ResolvedExecution::NonLocal(non_local) => {
                execute_remote_root_lifecycle(
                    non_local,
                    backend_request,
                    run_created_at,
                    runtime_cancellation_token,
                    previous_non_local_state,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_local_root_execution(
        &self,
        preflight_resolved: &crate::registry::ResolvedAgent,
        thread_id: &str,
        request_messages: Vec<Message>,
        messages_already_persisted: bool,
        decisions: &[(
            String,
            awaken_contract::contract::suspension::ToolCallResume,
        )],
        run_inbox: Option<super::run_request::RunInbox>,
        thread_ctx: &Option<ThreadContextSnapshot>,
    ) -> Result<PreparedLocalRootExecution, AgentLoopError> {
        let store = crate::state::StateStore::new();
        let phase_runtime =
            crate::phase::PhaseRuntime::new(store.clone()).map_err(AgentLoopError::PhaseError)?;
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .map_err(AgentLoopError::PhaseError)?;
        let run_inbox = run_inbox.unwrap_or_else(|| {
            let (sender, receiver) = crate::inbox::inbox_channel();
            super::run_request::RunInbox { sender, receiver }
        });
        let owner_inbox = run_inbox.sender.clone();
        crate::backend::LocalBackend::bind_local_execution_env(
            &store,
            preflight_resolved,
            Some(&owner_inbox),
        )
        .map_err(AgentLoopError::PhaseError)?;

        let compaction = build_compaction_runtime(preflight_resolved, &store, &owner_inbox)?;

        let mut messages = if let Some(ctx) = thread_ctx {
            if let Some(ref prev_run) = ctx.latest_run
                && let Some(ref persisted) = prev_run.state
            {
                store
                    .restore_thread_scoped(
                        persisted.clone(),
                        awaken_contract::UnknownKeyPolicy::Skip,
                    )
                    .map_err(AgentLoopError::PhaseError)?;
            }
            ctx.messages.clone()
        } else if let Some(ref ts) = self.storage {
            if let Some(prev_run) = ts
                .latest_run(thread_id)
                .await
                .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
                && let Some(persisted) = prev_run.state
            {
                store
                    .restore_thread_scoped(persisted, awaken_contract::UnknownKeyPolicy::Skip)
                    .map_err(AgentLoopError::PhaseError)?;
            }
            ts.load_messages(thread_id)
                .await
                .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
                .unwrap_or_default()
        } else {
            vec![]
        };
        if should_supersede_suspended_calls(&request_messages, decisions) {
            strip_superseded_suspended_tool_calls(&mut messages, &store);
        }
        strip_unpaired_tool_calls(&mut messages);
        if !messages_already_persisted {
            messages.extend(request_messages);
        }

        Ok(PreparedLocalRootExecution {
            messages,
            phase_runtime,
            inbox: run_inbox.receiver,
            inbox_sender: owner_inbox,
            compaction,
        })
    }

    async fn load_non_local_messages(
        &self,
        thread_id: &str,
        request_messages: Vec<Message>,
        messages_already_persisted: bool,
        thread_ctx: &Option<ThreadContextSnapshot>,
    ) -> Result<Vec<Message>, AgentLoopError> {
        let mut messages = if let Some(ctx) = thread_ctx {
            ctx.messages.clone()
        } else if let Some(ref storage) = self.storage {
            storage
                .load_messages(thread_id)
                .await
                .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        strip_unpaired_tool_calls(&mut messages);
        if !messages_already_persisted {
            messages.extend(request_messages);
        }
        Ok(messages)
    }

    async fn next_root_run_id(
        &self,
        thread_id: &str,
        continue_run_id: Option<String>,
        run_id_hint: Option<String>,
        dispatch_id_hint: Option<String>,
        allow_waiting_reuse: bool,
        thread_ctx: &Option<ThreadContextSnapshot>,
    ) -> Result<(String, bool), AgentLoopError> {
        if let Some(run_id) = continue_run_id {
            // Check cache first for continue_run_id.
            if let Some(ctx) = thread_ctx
                && ctx.run_cache.contains_key(&run_id)
            {
                return Ok((run_id, true));
            }
            let Some(ref ts) = self.storage else {
                return Err(AgentLoopError::InvalidResume(format!(
                    "continue_run_id '{run_id}' requires run storage"
                )));
            };
            if ts
                .load_run(&run_id)
                .await
                .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
                .is_some()
            {
                return Ok((run_id, true));
            }
            return Err(AgentLoopError::InvalidResume(format!(
                "continue_run_id '{run_id}' does not reference an existing run"
            )));
        }
        if let Some(run_id) = run_id_hint.and_then(|id| {
            let trimmed = id.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }) {
            // Check cache first, then store.
            let existing = if let Some(ctx) = thread_ctx {
                ctx.run_cache.get(&run_id).cloned()
            } else {
                None
            };
            let existing = if existing.is_some() {
                existing
            } else if let Some(ref ts) = self.storage {
                ts.load_run(&run_id)
                    .await
                    .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
            } else {
                None
            };
            if let Some(existing) = existing {
                if existing.status == awaken_contract::contract::lifecycle::RunStatus::Created {
                    return Ok((run_id, false));
                }
                return Err(AgentLoopError::InvalidResume(format!(
                    "run_id_hint '{run_id}' already exists as a run"
                )));
            }
            return Ok((run_id, false));
        }
        if let Some(run_id) = dispatch_id_hint.and_then(|id| {
            let trimmed = id.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }) {
            if let Some(ref ts) = self.storage
                && ts
                    .load_run(&run_id)
                    .await
                    .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
                    .is_some()
            {
                return Err(AgentLoopError::InvalidResume(format!(
                    "dispatch_id_hint '{run_id}' already exists as a run"
                )));
            }
            return Ok((run_id, false));
        }
        if allow_waiting_reuse {
            if let Some(ctx) = thread_ctx {
                if let Some(run) = ctx.latest_run.as_ref().filter(|r| r.is_resumable_waiting()) {
                    return Ok((run.run_id.clone(), true));
                }
            } else if let Some(prev) = self.reusable_waiting_run(thread_id).await? {
                return Ok((prev.run_id.clone(), true));
            }
        }
        Ok((uuid::Uuid::now_v7().to_string(), false))
    }

    async fn reusable_waiting_run(
        &self,
        thread_id: &str,
    ) -> Result<Option<RunRecord>, AgentLoopError> {
        let Some(ref ts) = self.storage else {
            return Ok(None);
        };

        if let Some(thread) = ts
            .load_thread(thread_id)
            .await
            .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
            && let Some(open_run_id) = thread.open_run_id.as_deref()
            && let Some(run) = ts
                .load_run(open_run_id)
                .await
                .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
            && run.thread_id == thread_id
            && run.is_resumable_waiting()
        {
            return Ok(Some(run));
        }

        Ok(ts
            .latest_run(thread_id)
            .await
            .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
            .filter(RunRecord::is_resumable_waiting))
    }

    async fn resolve_agent_id(
        &self,
        requested_agent_id: Option<String>,
        thread_id: &str,
        thread_ctx: &Option<ThreadContextSnapshot>,
    ) -> Result<String, AgentLoopError> {
        if let Some(agent_id) = requested_agent_id {
            return Ok(agent_id);
        }

        if let Some(inferred) = self
            .infer_agent_id_from_thread(thread_id, thread_ctx)
            .await?
        {
            return Ok(inferred);
        }

        Ok(DEFAULT_AGENT_ID.to_string())
    }

    async fn infer_agent_id_from_thread(
        &self,
        thread_id: &str,
        thread_ctx: &Option<ThreadContextSnapshot>,
    ) -> Result<Option<String>, AgentLoopError> {
        if let Some(ctx) = thread_ctx {
            if let Some(ref prev_run) = ctx.latest_run {
                if let Some(agent_id) = prev_run.state.as_ref().and_then(active_agent_from_state) {
                    return Ok(Some(agent_id));
                }
                let agent_id = prev_run.agent_id.trim();
                if !agent_id.is_empty() {
                    return Ok(Some(agent_id.to_string()));
                }
            }
            return Ok(None);
        }

        let Some(storage) = &self.storage else {
            return Ok(None);
        };

        let Some(prev_run) = storage
            .latest_run(thread_id)
            .await
            .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
        else {
            return Ok(None);
        };

        if let Some(agent_id) = prev_run.state.as_ref().and_then(active_agent_from_state) {
            return Ok(Some(agent_id));
        }

        let agent_id = prev_run.agent_id.trim();
        if agent_id.is_empty() {
            Ok(None)
        } else {
            Ok(Some(agent_id.to_string()))
        }
    }

    async fn load_non_local_state(
        &self,
        thread_id: &str,
        continue_run_id: Option<&str>,
        thread_ctx: &Option<ThreadContextSnapshot>,
    ) -> Result<Option<PersistedState>, AgentLoopError> {
        if let Some(ctx) = thread_ctx {
            if let Some(run_id) = continue_run_id {
                return Ok(ctx.run_cache.get(run_id).and_then(|r| r.state.clone()));
            }
            return Ok(ctx.latest_run.as_ref().and_then(|r| r.state.clone()));
        }

        let Some(storage) = &self.storage else {
            return Ok(None);
        };

        if let Some(run_id) = continue_run_id {
            return Ok(storage
                .load_run(run_id)
                .await
                .map_err(|error| AgentLoopError::StorageError(error.to_string()))?
                .and_then(|run| run.state));
        }

        Ok(storage
            .latest_run(thread_id)
            .await
            .map_err(|error| AgentLoopError::StorageError(error.to_string()))?
            .and_then(|run| run.state))
    }
}

fn local_root_execution_error(error: ExecutionBackendError) -> AgentLoopError {
    match error {
        ExecutionBackendError::Loop(loop_error) => loop_error,
        other => AgentLoopError::RuntimeError(crate::RuntimeError::ResolveFailed {
            message: other.to_string(),
        }),
    }
}

fn active_agent_from_state(state: &PersistedState) -> Option<String> {
    state
        .extensions
        .get(<ActiveAgentIdKey as awaken_contract::StateKey>::KEY)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
}

/// Remove unpaired tool calls from message history.
///
/// When a run is cancelled while tool calls are pending, the history may
/// contain assistant messages with `tool_calls` that have no matching
/// `Tool` role response. These "orphaned" calls confuse LLMs on the next
/// turn. This function strips unanswered calls from all assistant messages.
fn strip_unpaired_tool_calls(messages: &mut Vec<Message>) {
    use std::collections::HashSet;

    // Collect all tool call IDs that have a Tool-role response.
    let answered: HashSet<String> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    // Strip unanswered tool calls from all assistant messages.
    for msg in messages.iter_mut() {
        if msg.role != Role::Assistant {
            continue;
        }
        if let Some(ref mut calls) = msg.tool_calls {
            calls.retain(|c| answered.contains(&c.id));
            if calls.is_empty() {
                msg.tool_calls = None;
            }
        }
    }

    // Remove trailing empty assistant messages (no text, no tool calls).
    while let Some(last) = messages.last() {
        if last.role == Role::Assistant
            && last.tool_calls.is_none()
            && last.text().trim().is_empty()
        {
            messages.pop();
        } else {
            break;
        }
    }
}

fn should_supersede_suspended_calls(
    request_messages: &[Message],
    decisions: &[(
        String,
        awaken_contract::contract::suspension::ToolCallResume,
    )],
) -> bool {
    decisions.is_empty()
        && request_messages
            .iter()
            .any(|message| message.role == Role::User && message.visibility == Visibility::All)
}

fn strip_superseded_suspended_tool_calls(
    messages: &mut Vec<Message>,
    store: &crate::state::StateStore,
) {
    use std::collections::HashSet;

    let suspended_ids: HashSet<String> = store
        .read::<crate::agent::state::ToolCallStates>()
        .unwrap_or_default()
        .calls
        .into_iter()
        .filter_map(|(call_id, state)| {
            (state.status == ToolCallStatus::Suspended).then_some(call_id)
        })
        .collect();
    if suspended_ids.is_empty() {
        return;
    }

    for message in messages.iter_mut() {
        if message.role != Role::Assistant {
            continue;
        }
        if let Some(ref mut calls) = message.tool_calls {
            calls.retain(|call| !suspended_ids.contains(&call.id));
            if calls.is_empty() {
                message.tool_calls = None;
            }
        }
    }

    messages.retain(|message| {
        !(message.role == Role::Tool
            && message
                .tool_call_id
                .as_ref()
                .is_some_and(|call_id| suspended_ids.contains(call_id)))
    });

    while let Some(last) = messages.last() {
        if last.role == Role::Assistant
            && last.tool_calls.is_none()
            && last.text().trim().is_empty()
        {
            messages.pop();
        } else {
            break;
        }
    }
}

#[cfg(test)]
#[path = "runner_test.rs"]
mod tests;
