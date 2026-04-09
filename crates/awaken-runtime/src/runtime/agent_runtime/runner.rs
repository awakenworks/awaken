//! AgentRuntime::run() implementation.

use std::sync::Arc;

use crate::backend::{
    BackendControl, BackendRunRequest, ExecutionBackendError, execute_resolved_execution,
    execution_capabilities, validate_execution_request,
};
use crate::loop_runner::{AgentLoopError, AgentRunResult};
use crate::registry::{ExecutionResolver, ResolvedBackendAgent, ResolvedExecution};
use awaken_contract::contract::active_agent::ActiveAgentIdKey;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::message::gen_message_id;
use awaken_contract::contract::message::{Message, Role, Visibility};
use awaken_contract::contract::storage::{RunRecord, ThreadRunStore};
use awaken_contract::contract::suspension::ToolCallStatus;
use awaken_contract::now_ms;
use awaken_contract::state::PersistedState;
use serde_json::{Value, json};

use super::AgentRuntime;
use super::run_request::RunRequest;

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
}

impl AgentRuntime {
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
        let RunRequest {
            messages: request_messages,
            thread_id,
            agent_id,
            overrides,
            decisions,
            frontend_tools,
            origin: req_origin,
            parent_run_id: req_parent_run_id,
            parent_thread_id: req_parent_thread_id,
            continue_run_id,
            run_inbox,
        } = request;
        let agent_id = self.resolve_agent_id(agent_id, &thread_id).await?;
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
        let capabilities = execution_capabilities(&resolved_execution);
        let (run_id, is_continuation) = self
            .next_root_run_id(
                &thread_id,
                continue_run_id,
                matches!(&resolved_execution, ResolvedExecution::Local(_)),
            )
            .await?;
        let run_origin = match req_origin {
            awaken_contract::contract::mailbox::MailboxJobOrigin::User => {
                awaken_contract::contract::identity::RunOrigin::User
            }
            awaken_contract::contract::mailbox::MailboxJobOrigin::A2A => {
                awaken_contract::contract::identity::RunOrigin::Subagent
            }
            awaken_contract::contract::mailbox::MailboxJobOrigin::Internal => {
                awaken_contract::contract::identity::RunOrigin::Internal
            }
        };
        let run_identity = RunIdentity::new(
            thread_id.clone(),
            req_parent_thread_id,
            run_id.clone(),
            req_parent_run_id,
            agent_id.clone(),
            run_origin,
        );

        let mut run_inbox = run_inbox;
        let (messages, phase_runtime, inbox) = match &resolved_execution {
            ResolvedExecution::Local(preflight_resolved) => {
                let prepared = self
                    .prepare_local_root_execution(
                        preflight_resolved,
                        &thread_id,
                        request_messages,
                        &decisions,
                        run_inbox.take(),
                    )
                    .await?;
                (
                    prepared.messages,
                    Some(prepared.phase_runtime),
                    Some(prepared.inbox),
                )
            }
            ResolvedExecution::NonLocal(_) => (
                self.load_non_local_messages(&thread_id, request_messages)
                    .await?,
                None,
                run_inbox.take().map(|run_inbox| run_inbox.receiver),
            ),
        };
        let run_created_at = now_ms();

        let (handle, cancellation_token, raw_decision_rx) =
            self.create_run_channels(run_id.clone());
        let decision_rx = if capabilities.decisions {
            Some(raw_decision_rx)
        } else {
            drop(raw_decision_rx);
            None
        };

        let persisted_messages = match &resolved_execution {
            ResolvedExecution::Local(_) => None,
            ResolvedExecution::NonLocal(_) => Some(messages.clone()),
        };
        let backend_request = BackendRunRequest {
            agent_id: &agent_id,
            messages,
            sink: sink.clone(),
            resolver: run_resolver.as_ref(),
            run_identity: Some(run_identity.clone()),
            parent: None,
            phase_runtime: phase_runtime.as_ref(),
            checkpoint_store: phase_runtime.as_ref().and(self.storage.as_deref()),
            control: BackendControl {
                cancellation_token: Some(cancellation_token),
                decision_rx,
            },
            decisions,
            overrides,
            frontend_tools,
            inbox,
            is_continuation,
        };
        validate_execution_request(&resolved_execution, &backend_request).map_err(|error| {
            match error {
                ExecutionBackendError::Loop(loop_error) => loop_error,
                other => AgentLoopError::RuntimeError(crate::RuntimeError::ResolveFailed {
                    message: other.to_string(),
                }),
            }
        })?;

        // Register active run (guard ensures cleanup on drop/panic/cancellation)
        self.register_run(&thread_id, handle)
            .map_err(AgentLoopError::RuntimeError)?;
        let _guard = RunSlotGuard {
            runtime: self,
            run_id: run_id.clone(),
        };

        match &resolved_execution {
            ResolvedExecution::Local(_) => {
                let result = execute_resolved_execution(&resolved_execution, backend_request)
                    .await
                    .map_err(local_root_execution_error)?;
                Ok(AgentRunResult {
                    response: result.response.unwrap_or_default(),
                    termination: result.termination,
                    steps: result.steps,
                })
            }
            ResolvedExecution::NonLocal(non_local) => {
                self.run_non_local_root_execution(
                    non_local,
                    backend_request,
                    persisted_messages.expect("non-local runs keep message history"),
                    run_created_at,
                    &run_identity,
                    &sink,
                )
                .await
            }
        }
    }

    async fn prepare_local_root_execution(
        &self,
        preflight_resolved: &crate::registry::ResolvedAgent,
        thread_id: &str,
        request_messages: Vec<Message>,
        decisions: &[(
            String,
            awaken_contract::contract::suspension::ToolCallResume,
        )],
        run_inbox: Option<super::run_request::RunInbox>,
    ) -> Result<PreparedLocalRootExecution, AgentLoopError> {
        let store = crate::state::StateStore::new();
        let phase_runtime =
            crate::phase::PhaseRuntime::new(store.clone()).map_err(AgentLoopError::PhaseError)?;
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .map_err(AgentLoopError::PhaseError)?;

        let preflight_key_registrations = preflight_resolved.env.key_registrations.clone();
        if !preflight_key_registrations.is_empty() {
            store
                .register_keys(&preflight_key_registrations)
                .map_err(AgentLoopError::PhaseError)?;
        }

        let mut messages = if let Some(ref ts) = self.storage {
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
        messages.extend(request_messages);

        let run_inbox = run_inbox.unwrap_or_else(|| {
            let (sender, receiver) = crate::inbox::inbox_channel();
            super::run_request::RunInbox { sender, receiver }
        });
        let owner_inbox = run_inbox.sender.clone();
        for plugin in &preflight_resolved.env.plugins {
            plugin.bind_runtime_context(&store, Some(&owner_inbox));
        }

        Ok(PreparedLocalRootExecution {
            messages,
            phase_runtime,
            inbox: run_inbox.receiver,
        })
    }

    async fn run_non_local_root_execution(
        &self,
        non_local: &ResolvedBackendAgent,
        request: BackendRunRequest<'_>,
        mut messages: Vec<Message>,
        run_created_at: u64,
        run_identity: &RunIdentity,
        sink: &Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        sink.emit(AgentEvent::RunStart {
            thread_id: run_identity.thread_id.clone(),
            run_id: run_identity.run_id.clone(),
            parent_run_id: run_identity.parent_run_id.clone(),
        })
        .await;
        sink.emit(AgentEvent::StepStart {
            message_id: gen_message_id(),
        })
        .await;

        let execution_started_at = now_ms();
        let delegate_result = match non_local.backend.execute(request).await {
            Ok(result) => result,
            Err(error) => {
                let termination = TerminationReason::Error(non_local_backend_error_message(error));
                return self
                    .finish_non_local_run(
                        &run_identity.thread_id,
                        &run_identity.run_id,
                        &run_identity.agent_id,
                        run_identity.parent_run_id.clone(),
                        run_created_at,
                        messages,
                        termination,
                        0,
                        String::new(),
                        sink,
                    )
                    .await;
            }
        };

        let termination = delegate_result.termination.clone();
        let response = delegate_result.response.unwrap_or_default();
        let steps = delegate_result.steps;
        if !response.is_empty() {
            sink.emit(AgentEvent::TextDelta {
                delta: response.clone(),
            })
            .await;
            messages.push(Message::assistant(response.clone()));
        }

        if matches!(
            termination,
            TerminationReason::NaturalEnd | TerminationReason::BehaviorRequested
        ) {
            sink.emit(AgentEvent::InferenceComplete {
                model: non_local.spec.model.clone(),
                usage: None,
                duration_ms: now_ms().saturating_sub(execution_started_at),
            })
            .await;
        }

        self.finish_non_local_run(
            &run_identity.thread_id,
            &run_identity.run_id,
            &run_identity.agent_id,
            run_identity.parent_run_id.clone(),
            run_created_at,
            messages,
            termination,
            steps,
            response,
            sink,
        )
        .await
    }

    async fn load_non_local_messages(
        &self,
        thread_id: &str,
        request_messages: Vec<Message>,
    ) -> Result<Vec<Message>, AgentLoopError> {
        let mut messages = if let Some(ref storage) = self.storage {
            storage
                .load_messages(thread_id)
                .await
                .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        strip_unpaired_tool_calls(&mut messages);
        messages.extend(request_messages);
        Ok(messages)
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_non_local_run(
        &self,
        thread_id: &str,
        run_id: &str,
        agent_id: &str,
        parent_run_id: Option<String>,
        run_created_at: u64,
        messages: Vec<Message>,
        termination: TerminationReason,
        steps: usize,
        response: String,
        sink: &Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        let (status, status_reason) = termination.to_run_status();
        let mut result_json = json!({
            "response": response,
            "status": run_status_label(status),
        });
        if let Some(reason) = &status_reason {
            result_json["status_reason"] = Value::String(reason.clone());
        }

        persist_non_local_checkpoint(
            self.storage.as_deref(),
            thread_id,
            run_id,
            agent_id,
            parent_run_id,
            run_created_at,
            &messages,
            status,
            status_reason,
            steps,
        )
        .await?;

        sink.emit(AgentEvent::StepEnd).await;
        sink.emit(AgentEvent::RunFinish {
            thread_id: thread_id.to_string(),
            run_id: run_id.to_string(),
            result: Some(result_json),
            termination: termination.clone(),
        })
        .await;

        Ok(AgentRunResult {
            response,
            termination,
            steps,
        })
    }

    async fn next_root_run_id(
        &self,
        thread_id: &str,
        continue_run_id: Option<String>,
        allow_waiting_reuse: bool,
    ) -> Result<(String, bool), AgentLoopError> {
        if let Some(run_id) = continue_run_id {
            return Ok((run_id, true));
        }
        if allow_waiting_reuse
            && let Some(ref ts) = self.storage
            && let Some(prev) = ts
                .latest_run(thread_id)
                .await
                .map_err(|e| AgentLoopError::StorageError(e.to_string()))?
            && prev.status == awaken_contract::contract::lifecycle::RunStatus::Waiting
            && prev.termination_code.as_deref() == Some("awaiting_tasks")
        {
            return Ok((prev.run_id.clone(), true));
        }
        Ok((uuid::Uuid::now_v7().to_string(), false))
    }

    async fn resolve_agent_id(
        &self,
        requested_agent_id: Option<String>,
        thread_id: &str,
    ) -> Result<String, AgentLoopError> {
        if let Some(agent_id) = requested_agent_id {
            return Ok(agent_id);
        }

        if let Some(inferred) = self.infer_agent_id_from_thread(thread_id).await? {
            return Ok(inferred);
        }

        Ok(DEFAULT_AGENT_ID.to_string())
    }

    async fn infer_agent_id_from_thread(
        &self,
        thread_id: &str,
    ) -> Result<Option<String>, AgentLoopError> {
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
}

fn non_local_backend_error_message(error: ExecutionBackendError) -> String {
    match error {
        ExecutionBackendError::AgentNotFound(message)
        | ExecutionBackendError::ExecutionFailed(message)
        | ExecutionBackendError::RemoteError(message) => message,
        ExecutionBackendError::Loop(error) => error.to_string(),
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

fn run_status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "running",
        RunStatus::Waiting => "waiting",
        RunStatus::Done => "done",
    }
}

async fn persist_non_local_checkpoint(
    storage: Option<&dyn ThreadRunStore>,
    thread_id: &str,
    run_id: &str,
    agent_id: &str,
    parent_run_id: Option<String>,
    run_created_at: u64,
    messages: &[Message],
    status: RunStatus,
    status_reason: Option<String>,
    steps: usize,
) -> Result<(), AgentLoopError> {
    let Some(storage) = storage else {
        return Ok(());
    };

    let record = RunRecord {
        run_id: run_id.to_string(),
        thread_id: thread_id.to_string(),
        agent_id: agent_id.to_string(),
        parent_run_id,
        status,
        termination_code: status_reason,
        created_at: run_created_at / 1000,
        updated_at: now_ms() / 1000,
        steps,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    };
    storage
        .checkpoint(thread_id, messages, &record)
        .await
        .map_err(|error| AgentLoopError::StorageError(error.to_string()))
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
mod tests {
    use super::super::*;
    #[cfg(feature = "a2a")]
    use crate::extensions::a2a::{
        AgentBackend, AgentBackendError, AgentBackendFactory, AgentBackendFactoryError,
        DelegateRunResult, DelegateRunStatus,
    };
    use crate::loop_runner::build_agent_env;
    use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
    #[cfg(feature = "a2a")]
    use crate::registry::memory::{
        MapAgentSpecRegistry, MapBackendRegistry, MapModelRegistry, MapPluginSource,
        MapProviderRegistry, MapToolRegistry,
    };
    #[cfg(feature = "a2a")]
    use crate::registry::snapshot::RegistryHandle;
    #[cfg(feature = "a2a")]
    use crate::registry::traits::{BackendRegistry, ModelEntry, RegistrySet};
    use crate::registry::{AgentResolver, ResolvedAgent};
    use crate::state::{KeyScope, StateCommand, StateKey, StateKeyOptions};
    use crate::{PhaseContext, PhaseHook};
    use async_trait::async_trait;
    use awaken_contract::PersistedState;
    use awaken_contract::contract::active_agent::ActiveAgentIdKey;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::event::AgentEvent;
    use awaken_contract::contract::event_sink::{EventSink, NullEventSink, VecEventSink};
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_contract::contract::inference::{InferenceOverride, StopReason, StreamResult};
    use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
    use awaken_contract::contract::message::Message;
    use awaken_contract::contract::storage::{
        RunQuery, RunRecord, RunStore, ThreadRunStore, ThreadStore,
    };
    use awaken_contract::contract::suspension::ResumeDecisionAction;
    use awaken_contract::contract::suspension::ToolCallResume;
    use awaken_contract::contract::tool::{
        Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
    };
    #[cfg(feature = "a2a")]
    use awaken_contract::registry_spec::{AgentSpec, RemoteEndpoint};
    use awaken_stores::InMemoryStore;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct ScriptedLlm {
        responses: Mutex<Vec<StreamResult>>,
        seen_overrides: Mutex<Vec<Option<InferenceOverride>>>,
    }

    impl ScriptedLlm {
        fn new(responses: Vec<StreamResult>) -> Self {
            Self {
                responses: Mutex::new(responses),
                seen_overrides: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmExecutor for ScriptedLlm {
        async fn execute(
            &self,
            request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.seen_overrides
                .lock()
                .expect("lock poisoned")
                .push(request.overrides.clone());
            let mut responses = self.responses.lock().expect("lock poisoned");
            if responses.is_empty() {
                Ok(StreamResult {
                    content: vec![ContentBlock::text("done")],
                    tool_calls: vec![],
                    usage: None,
                    stop_reason: Some(StopReason::EndTurn),
                    has_incomplete_tool_calls: false,
                })
            } else {
                Ok(responses.remove(0))
            }
        }

        fn name(&self) -> &str {
            "scripted"
        }
    }

    #[cfg(feature = "a2a")]
    struct StaticRemoteBackend {
        response: String,
        delay_ms: u64,
    }

    #[cfg(feature = "a2a")]
    #[async_trait]
    impl AgentBackend for StaticRemoteBackend {
        fn capabilities(&self) -> crate::backend::BackendCapabilities {
            crate::backend::BackendCapabilities {
                cancellation: true,
                decisions: false,
                overrides: false,
                frontend_tools: false,
                continuation: false,
            }
        }

        async fn execute(
            &self,
            request: crate::backend::BackendRunRequest<'_>,
        ) -> Result<DelegateRunResult, AgentBackendError> {
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            Ok(DelegateRunResult {
                agent_id: request.agent_id.to_string(),
                status: DelegateRunStatus::Completed,
                termination: TerminationReason::NaturalEnd,
                response: Some(self.response.clone()),
                steps: 1,
                run_id: Some("child-remote-run".into()),
                inbox: None,
            })
        }
    }

    #[cfg(feature = "a2a")]
    struct StaticRemoteBackendFactory;

    #[cfg(feature = "a2a")]
    impl AgentBackendFactory for StaticRemoteBackendFactory {
        fn backend(&self) -> &str {
            "test-remote"
        }

        fn build(
            &self,
            endpoint: &RemoteEndpoint,
        ) -> Result<Arc<dyn AgentBackend>, AgentBackendFactoryError> {
            if endpoint.backend != "test-remote" {
                return Err(AgentBackendFactoryError::InvalidConfig(format!(
                    "unexpected backend '{}'",
                    endpoint.backend
                )));
            }
            let delay_ms = endpoint
                .options
                .get("delay_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            Ok(Arc::new(StaticRemoteBackend {
                response: "remote root response".into(),
                delay_ms,
            }))
        }
    }

    #[cfg(feature = "a2a")]
    fn build_remote_runtime(endpoint: RemoteEndpoint) -> AgentRuntime {
        let mut models = MapModelRegistry::new();
        models
            .register_model(
                "test-model",
                ModelEntry {
                    provider: "mock".into(),
                    model_name: "mock-model".into(),
                },
            )
            .unwrap();

        let mut providers = MapProviderRegistry::new();
        providers
            .register_provider("mock", Arc::new(ScriptedLlm::new(Vec::new())))
            .unwrap();

        let mut agents = MapAgentSpecRegistry::new();
        agents
            .register_spec(
                AgentSpec::new("remote-root")
                    .with_model("test-model")
                    .with_system_prompt("remote root")
                    .with_endpoint(endpoint),
            )
            .unwrap();

        let mut backends = MapBackendRegistry::new();
        backends
            .register_backend_factory(Arc::new(StaticRemoteBackendFactory))
            .unwrap();

        let registries = RegistrySet {
            agents: Arc::new(agents),
            tools: Arc::new(MapToolRegistry::new()),
            models: Arc::new(models),
            providers: Arc::new(providers),
            plugins: Arc::new(MapPluginSource::new()),
            backends: Arc::new(backends) as Arc<dyn BackendRegistry>,
        };
        let handle = RegistryHandle::new(registries.clone());
        AgentRuntime::new(Arc::new(
            crate::registry::resolve::DynamicRegistryResolver::new(handle.clone()),
        ))
        .with_registry_handle(handle)
        .with_thread_run_store(Arc::new(InMemoryStore::new()))
    }

    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn run_supports_endpoint_root_agents() {
        let runtime = build_remote_runtime(RemoteEndpoint {
            backend: "test-remote".into(),
            base_url: "https://remote.example.com".into(),
            ..Default::default()
        });

        let sink = Arc::new(VecEventSink::new());
        let result = runtime
            .run(
                RunRequest::new("remote-thread", vec![Message::user("hello")])
                    .with_agent_id("remote-root"),
                sink.clone(),
            )
            .await
            .expect("endpoint root run should succeed");

        assert_eq!(result.response, "remote root response");
        assert!(matches!(result.termination, TerminationReason::NaturalEnd));

        let events = sink.events();
        assert!(matches!(events.first(), Some(AgentEvent::RunStart { .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TextDelta { delta } if delta == "remote root response"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::RunFinish {
                termination: TerminationReason::NaturalEnd,
                ..
            }
        )));

        let latest_run = runtime
            .thread_run_store()
            .expect("store")
            .latest_run("remote-thread")
            .await
            .expect("run lookup should succeed")
            .expect("run record should be persisted");
        assert_eq!(latest_run.agent_id, "remote-root");
        assert_eq!(latest_run.status, RunStatus::Done);

        let messages = runtime
            .thread_run_store()
            .expect("store")
            .load_messages("remote-thread")
            .await
            .expect("message lookup should succeed")
            .expect("messages should be persisted");
        assert!(messages.iter().any(|message| {
            message.role == awaken_contract::contract::message::Role::Assistant
                && message.text() == "remote root response"
        }));
    }

    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn run_rejects_remote_overrides_without_backend_capability() {
        let runtime = build_remote_runtime(RemoteEndpoint {
            backend: "test-remote".into(),
            base_url: "https://remote.example.com".into(),
            ..Default::default()
        });

        let error = runtime
            .run(
                RunRequest::new("remote-thread-overrides", vec![Message::user("hello")])
                    .with_agent_id("remote-root")
                    .with_overrides(InferenceOverride {
                        temperature: Some(0.2),
                        ..Default::default()
                    }),
                Arc::new(VecEventSink::new()),
            )
            .await
            .expect_err("remote backend should reject overrides");

        assert!(error.to_string().contains("does not support: overrides"));
    }

    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn run_rejects_remote_resume_decisions_without_backend_capability() {
        let runtime = build_remote_runtime(RemoteEndpoint {
            backend: "test-remote".into(),
            base_url: "https://remote.example.com".into(),
            ..Default::default()
        });

        let error = runtime
            .run(
                RunRequest::new("remote-thread-decisions", vec![Message::user("hello")])
                    .with_agent_id("remote-root")
                    .with_decisions(vec![(
                        "call-1".into(),
                        ToolCallResume {
                            decision_id: "d1".into(),
                            action: ResumeDecisionAction::Resume,
                            result: Value::Null,
                            reason: None,
                            updated_at: 1,
                        },
                    )]),
                Arc::new(VecEventSink::new()),
            )
            .await
            .expect_err("remote backend should reject resume decisions");

        assert!(error.to_string().contains("does not support: decisions"));
    }

    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn run_rejects_remote_frontend_tools_without_backend_capability() {
        let runtime = build_remote_runtime(RemoteEndpoint {
            backend: "test-remote".into(),
            base_url: "https://remote.example.com".into(),
            ..Default::default()
        });

        let error = runtime
            .run(
                RunRequest::new("remote-thread-frontend", vec![Message::user("hello")])
                    .with_agent_id("remote-root")
                    .with_frontend_tools(vec![ToolDescriptor::new(
                        "browser",
                        "browser",
                        "frontend tool",
                    )]),
                Arc::new(VecEventSink::new()),
            )
            .await
            .expect_err("remote backend should reject frontend tools");

        assert!(
            error
                .to_string()
                .contains("does not support: frontend_tools")
        );
    }

    #[tokio::test]
    async fn run_rejects_remote_continuation_without_backend_capability() {
        let runtime = build_remote_runtime(RemoteEndpoint {
            backend: "test-remote".into(),
            base_url: "https://remote.example.com".into(),
            ..Default::default()
        });

        let error = runtime
            .run(
                RunRequest::new("remote-thread-cont", vec![Message::user("hello")])
                    .with_agent_id("remote-root")
                    .with_continue_run_id("existing-run"),
                Arc::new(VecEventSink::new()),
            )
            .await
            .expect_err("remote backend should reject continuation");

        assert!(error.to_string().contains("does not support: continuation"));
    }

    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn send_decisions_returns_false_for_remote_backend_without_decision_support() {
        let mut endpoint = RemoteEndpoint {
            backend: "test-remote".into(),
            base_url: "https://remote.example.com".into(),
            ..Default::default()
        };
        endpoint
            .options
            .insert("delay_ms".into(), serde_json::json!(100));
        let runtime = Arc::new(build_remote_runtime(endpoint));
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        let run_task = {
            let runtime = runtime.clone();
            let sink = sink.clone();
            tokio::spawn(async move {
                runtime
                    .run(
                        RunRequest::new("remote-thread-live", vec![Message::user("hello")])
                            .with_agent_id("remote-root"),
                        sink,
                    )
                    .await
            })
        };

        tokio::task::yield_now().await;
        let sent = runtime.send_decisions(
            "remote-thread-live",
            vec![(
                "call-1".into(),
                ToolCallResume {
                    decision_id: "d1".into(),
                    action: ResumeDecisionAction::Resume,
                    result: Value::Null,
                    reason: None,
                    updated_at: 1,
                },
            )],
        );
        assert!(
            !sent,
            "remote backends without decision support must not expose a live decision channel"
        );

        let result = run_task
            .await
            .expect("join should succeed")
            .expect("run should succeed");
        assert_eq!(result.response, "remote root response");
    }

    struct ToggleSuspendTool {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Tool for ToggleSuspendTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("dangerous", "dangerous", "suspend then succeed")
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(ToolResult::suspended("dangerous", "needs approval").into())
            } else {
                Ok(ToolResult::success_with_message("dangerous", args, "approved").into())
            }
        }
    }

    struct EchoTool {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("echo", "echo", "echo success")
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::success("echo", args).into())
        }
    }

    struct SpawnShortBgTaskTool {
        manager: Arc<crate::extensions::background::BackgroundTaskManager>,
        delay_ms: u64,
    }

    #[async_trait]
    impl Tool for SpawnShortBgTaskTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("spawn_bg", "spawn_bg", "spawn short background task")
        }

        async fn execute(
            &self,
            _args: Value,
            ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            let delay = self.delay_ms;
            self.manager
                .spawn(
                    &ctx.run_identity.thread_id,
                    "bg",
                    None,
                    "short task",
                    crate::extensions::background::TaskParentContext::default(),
                    move |_task_ctx| async move {
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                        crate::extensions::background::TaskResult::Success(json!({
                            "done": true,
                            "source": "background"
                        }))
                    },
                )
                .await
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
            Ok(ToolResult::success("spawn_bg", json!({"spawned": true})).into())
        }
    }

    struct RecordingLlm {
        responses: Mutex<Vec<StreamResult>>,
        requests: Arc<Mutex<Vec<InferenceRequest>>>,
    }

    impl RecordingLlm {
        fn new(responses: Vec<StreamResult>, requests: Arc<Mutex<Vec<InferenceRequest>>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests,
            }
        }
    }

    #[async_trait]
    impl LlmExecutor for RecordingLlm {
        async fn execute(
            &self,
            request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.requests.lock().expect("lock poisoned").push(request);
            let mut responses = self.responses.lock().expect("lock poisoned");
            Ok(responses.remove(0))
        }

        fn name(&self) -> &str {
            "recording"
        }
    }

    struct FixedResolver {
        agent: ResolvedAgent,
        plugins: Vec<Arc<dyn Plugin>>,
    }

    impl AgentResolver for FixedResolver {
        fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, crate::error::RuntimeError> {
            let mut agent = self.agent.clone();
            agent.env = build_agent_env(&self.plugins, &agent)?;
            Ok(agent)
        }
    }

    struct ThreadCounterKey;

    impl StateKey for ThreadCounterKey {
        const KEY: &'static str = "test.thread_counter";
        type Value = u32;
        type Update = u32;

        fn apply(value: &mut Self::Value, update: Self::Update) {
            *value = update;
        }
    }

    struct ThreadCounterPlugin;

    impl Plugin for ThreadCounterPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "test.thread-counter",
            }
        }

        fn register(
            &self,
            registrar: &mut PluginRegistrar,
        ) -> Result<(), awaken_contract::StateError> {
            registrar.register_key::<ThreadCounterKey>(StateKeyOptions {
                persistent: true,
                scope: KeyScope::Thread,
                ..StateKeyOptions::default()
            })?;
            registrar.register_phase_hook(
                "test.thread-counter",
                awaken_contract::model::Phase::RunStart,
                ThreadCounterHook,
            )
        }
    }

    struct ThreadCounterHook;

    #[async_trait]
    impl PhaseHook for ThreadCounterHook {
        async fn run(
            &self,
            ctx: &PhaseContext,
        ) -> Result<StateCommand, awaken_contract::StateError> {
            let next = ctx.state::<ThreadCounterKey>().copied().unwrap_or(0) + 1;
            let mut cmd = StateCommand::new();
            cmd.update::<ThreadCounterKey>(next);
            Ok(cmd)
        }
    }

    struct SequentialVisibilityKey;

    impl StateKey for SequentialVisibilityKey {
        const KEY: &'static str = "test.sequential_visibility";
        type Value = bool;
        type Update = bool;

        fn apply(value: &mut Self::Value, update: Self::Update) {
            *value = update;
        }
    }

    struct SequentialVisibilityPlugin;

    impl Plugin for SequentialVisibilityPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "test.sequential-visibility",
            }
        }

        fn register(
            &self,
            registrar: &mut PluginRegistrar,
        ) -> Result<(), awaken_contract::StateError> {
            registrar.register_key::<SequentialVisibilityKey>(StateKeyOptions::default())?;
            registrar.register_phase_hook(
                "test.sequential-visibility",
                awaken_contract::model::Phase::AfterToolExecute,
                SequentialVisibilityHook,
            )
        }
    }

    struct SequentialVisibilityHook;

    #[async_trait]
    impl PhaseHook for SequentialVisibilityHook {
        async fn run(
            &self,
            ctx: &PhaseContext,
        ) -> Result<StateCommand, awaken_contract::StateError> {
            let mut cmd = StateCommand::new();
            if ctx.tool_name.as_deref() == Some("writer") {
                cmd.update::<SequentialVisibilityKey>(true);
            }
            Ok(cmd)
        }
    }

    struct WriterTool;

    #[async_trait]
    impl Tool for WriterTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("writer", "writer", "writes marker in hook")
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::success("writer", Value::Null).into())
        }
    }

    struct ReaderTool {
        saw_marker: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl Tool for ReaderTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("reader", "reader", "reads marker from snapshot")
        }

        async fn execute(
            &self,
            _args: Value,
            ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            let saw = ctx
                .snapshot
                .get::<SequentialVisibilityKey>()
                .copied()
                .unwrap_or(false);
            self.saw_marker.store(saw, Ordering::SeqCst);
            Ok(ToolResult::success("reader", Value::Null).into())
        }
    }

    fn seeded_run_record(
        run_id: &str,
        thread_id: &str,
        agent_id: &str,
        state: Option<PersistedState>,
    ) -> RunRecord {
        RunRecord {
            run_id: run_id.to_string(),
            thread_id: thread_id.to_string(),
            agent_id: agent_id.to_string(),
            parent_run_id: None,
            status: RunStatus::Done,
            termination_code: Some("natural".into()),
            created_at: 1,
            updated_at: 1,
            steps: 1,
            input_tokens: 0,
            output_tokens: 0,
            state,
        }
    }

    #[tokio::test]
    async fn run_request_overrides_are_forwarded_to_inference() {
        let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("ok")],
            tool_calls: vec![],
            usage: Some(awaken_contract::contract::inference::TokenUsage {
                prompt_tokens: Some(11),
                completion_tokens: Some(7),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }]));
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm.clone()),
            plugins: vec![],
        });
        let runtime = AgentRuntime::new(resolver);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);
        let override_req = InferenceOverride {
            temperature: Some(0.3),
            max_tokens: Some(77),
            ..Default::default()
        };

        let result = runtime
            .run(
                RunRequest::new("thread-ovr", vec![Message::user("hi")])
                    .with_agent_id("agent")
                    .with_overrides(override_req.clone()),
                sink.clone(),
            )
            .await
            .expect("run should succeed");

        assert_eq!(
            result.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );
        let seen = llm.seen_overrides.lock().expect("lock poisoned");
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].as_ref().and_then(|o| o.temperature),
            override_req.temperature
        );
        assert_eq!(
            seen[0].as_ref().and_then(|o| o.max_tokens),
            override_req.max_tokens
        );
    }

    #[tokio::test]
    async fn send_decisions_resumes_waiting_run() {
        let llm = Arc::new(ScriptedLlm::new(vec![
            StreamResult {
                content: vec![ContentBlock::text("calling tool")],
                tool_calls: vec![awaken_contract::contract::message::ToolCall::new(
                    "c1",
                    "dangerous",
                    json!({"x": 1}),
                )],
                usage: None,
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            },
            StreamResult {
                content: vec![ContentBlock::text("finished")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            },
        ]));
        let tool = Arc::new(ToggleSuspendTool {
            calls: AtomicUsize::new(0),
        });
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm).with_tool(tool),
            plugins: vec![],
        });
        let runtime = Arc::new(AgentRuntime::new(resolver));
        let sink = Arc::new(VecEventSink::new());

        let run_task = {
            let runtime = Arc::clone(&runtime);
            let sink = sink.clone();
            tokio::spawn(async move {
                runtime
                    .run(
                        RunRequest::new("thread-live", vec![Message::user("go")])
                            .with_agent_id("agent"),
                        sink as Arc<dyn EventSink>,
                    )
                    .await
            })
        };

        let mut sent = false;
        for _ in 0..40 {
            if runtime.send_decisions(
                "thread-live",
                vec![(
                    "c1".into(),
                    ToolCallResume {
                        decision_id: "d1".into(),
                        action: ResumeDecisionAction::Resume,
                        result: Value::Null,
                        reason: None,
                        updated_at: 1,
                    },
                )],
            ) {
                sent = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(sent, "should send decision while run is active");

        let result = run_task
            .await
            .expect("join should succeed")
            .expect("run should succeed");
        assert_eq!(
            result.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );

        let events = sink.take();
        assert!(
            events.iter().any(|event| {
                matches!(
                    event,
                    AgentEvent::ToolCallResumed { target_id, result }
                        if target_id == "c1" && result == &json!({"x": 1})
                )
            }),
            "resumed replay should emit ToolCallResumed with the final tool result: {events:?}"
        );
    }

    #[tokio::test]
    async fn background_events_buffer_while_suspended_until_decision_arrives() {
        use awaken_contract::contract::message::{Role, Visibility};

        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = Arc::new(RecordingLlm::new(
            vec![
                StreamResult {
                    content: vec![ContentBlock::text("start tools")],
                    tool_calls: vec![
                        awaken_contract::contract::message::ToolCall::new(
                            "bg1",
                            "spawn_bg",
                            json!({}),
                        ),
                        awaken_contract::contract::message::ToolCall::new(
                            "c1",
                            "dangerous",
                            json!({"x": 1}),
                        ),
                    ],
                    usage: None,
                    stop_reason: Some(StopReason::ToolUse),
                    has_incomplete_tool_calls: false,
                },
                StreamResult {
                    content: vec![ContentBlock::text("done after approval")],
                    tool_calls: vec![],
                    usage: None,
                    stop_reason: Some(StopReason::EndTurn),
                    has_incomplete_tool_calls: false,
                },
            ],
            requests.clone(),
        ));
        let manager = Arc::new(crate::extensions::background::BackgroundTaskManager::new());
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm)
                .with_tool(Arc::new(SpawnShortBgTaskTool {
                    manager: manager.clone(),
                    delay_ms: 25,
                }))
                .with_tool(Arc::new(ToggleSuspendTool {
                    calls: AtomicUsize::new(0),
                })),
            plugins: vec![Arc::new(
                crate::extensions::background::BackgroundTaskPlugin::new(manager),
            )],
        });
        let runtime = Arc::new(AgentRuntime::new(resolver));
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        let run_task = {
            let runtime = runtime.clone();
            let sink = sink.clone();
            tokio::spawn(async move {
                runtime
                    .run(
                        RunRequest::new("thread-bg-suspend", vec![Message::user("go")])
                            .with_agent_id("agent"),
                        sink,
                    )
                    .await
            })
        };

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert_eq!(
            requests.lock().expect("lock poisoned").len(),
            1,
            "background completion must not resume the LLM before the suspended tool is decided"
        );

        let sent = runtime.send_decisions(
            "thread-bg-suspend",
            vec![(
                "c1".into(),
                ToolCallResume {
                    decision_id: "d1".into(),
                    action: ResumeDecisionAction::Resume,
                    result: Value::Null,
                    reason: None,
                    updated_at: 1,
                },
            )],
        );
        assert!(sent, "decision should reach the waiting run");

        let result = run_task
            .await
            .expect("join should succeed")
            .expect("run should succeed");
        assert_eq!(
            result.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );

        let recorded = requests.lock().expect("lock poisoned");
        assert_eq!(
            recorded.len(),
            2,
            "run should resume exactly once after approval"
        );
        assert!(
            recorded[1].messages.iter().any(|message| {
                message.role == Role::User
                    && message.visibility == Visibility::Internal
                    && message.text().contains("background-task-event")
                    && message.text().contains("\"done\":true")
            }),
            "buffered background event should be injected into the resumed request"
        );
    }

    #[tokio::test]
    async fn new_user_message_supersedes_suspended_calls_but_keeps_completed_results() {
        use awaken_contract::contract::lifecycle::RunStatus;
        use awaken_contract::contract::message::Role;
        use awaken_contract::contract::storage::ThreadStore;
        use awaken_stores::InMemoryStore;

        let llm = Arc::new(ScriptedLlm::new(vec![
            StreamResult {
                content: vec![ContentBlock::text("call tools")],
                tool_calls: vec![
                    awaken_contract::contract::message::ToolCall::new(
                        "c_echo",
                        "echo",
                        json!({"ok": true}),
                    ),
                    awaken_contract::contract::message::ToolCall::new(
                        "c_suspend",
                        "dangerous",
                        json!({"danger": true}),
                    ),
                ],
                usage: None,
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            },
            StreamResult {
                content: vec![ContentBlock::text("fresh answer")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            },
        ]));
        let echo = Arc::new(EchoTool {
            calls: AtomicUsize::new(0),
        });
        let dangerous = Arc::new(ToggleSuspendTool {
            calls: AtomicUsize::new(0),
        });
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm)
                .with_tool(echo.clone())
                .with_tool(dangerous.clone()),
            plugins: vec![],
        });
        let store = Arc::new(InMemoryStore::new());
        let runtime = Arc::new(
            AgentRuntime::new(resolver)
                .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>),
        );
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        let first_run = {
            let runtime = runtime.clone();
            let sink = sink.clone();
            tokio::spawn(async move {
                runtime
                    .run(
                        RunRequest::new("thread-supersede", vec![Message::user("first")])
                            .with_agent_id("agent"),
                        sink,
                    )
                    .await
            })
        };

        let wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(run) = store
                .latest_run("thread-supersede")
                .await
                .expect("latest run lookup should succeed")
                && run.status == RunStatus::Waiting
                && run.termination_code.as_deref() == Some("suspended")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < wait_deadline,
                "timed out waiting for suspended checkpoint"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert!(
            runtime.cancel_and_wait_by_thread("thread-supersede").await,
            "new message path should be able to supersede the suspended run"
        );

        let first = first_run
            .await
            .expect("join should succeed")
            .expect("first run should terminate cleanly");
        assert_eq!(
            first.termination,
            awaken_contract::contract::lifecycle::TerminationReason::Cancelled
        );

        let second = runtime
            .run(
                RunRequest::new("thread-supersede", vec![Message::user("second")])
                    .with_agent_id("agent"),
                sink,
            )
            .await
            .expect("second run should succeed");
        assert_eq!(
            second.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );
        assert_eq!(
            echo.calls.load(Ordering::SeqCst),
            1,
            "successful tool calls from the superseded run must not replay"
        );
        assert_eq!(
            dangerous.calls.load(Ordering::SeqCst),
            1,
            "suspended tool calls must be superseded instead of replayed on new user input"
        );

        let messages = ThreadStore::load_messages(&*store, "thread-supersede")
            .await
            .expect("load messages should succeed")
            .expect("thread messages should exist");
        assert!(
            messages.iter().any(|message| message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("c_echo")),
            "completed tool result should remain in durable history"
        );
        assert!(
            !messages
                .iter()
                .filter(|message| message.role == Role::Assistant)
                .filter_map(|message| message.tool_calls.as_ref())
                .flatten()
                .any(|call| call.id == "c_suspend"),
            "superseded suspended tool calls should be stripped from later history"
        );
    }

    #[tokio::test]
    async fn sequential_tool_execution_sees_latest_state_between_calls() {
        let llm = Arc::new(ScriptedLlm::new(vec![
            StreamResult {
                content: vec![ContentBlock::text("tools")],
                tool_calls: vec![
                    awaken_contract::contract::message::ToolCall::new("c1", "writer", json!({})),
                    awaken_contract::contract::message::ToolCall::new("c2", "reader", json!({})),
                ],
                usage: None,
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            },
            StreamResult {
                content: vec![ContentBlock::text("done")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            },
        ]));
        let saw_marker = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm)
                .with_tool(Arc::new(WriterTool))
                .with_tool(Arc::new(ReaderTool {
                    saw_marker: saw_marker.clone(),
                })),
            plugins: vec![Arc::new(SequentialVisibilityPlugin)],
        });
        let runtime = AgentRuntime::new(resolver);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        let result = runtime
            .run(
                RunRequest::new("thread-seq-visibility", vec![Message::user("go")])
                    .with_agent_id("agent"),
                sink.clone(),
            )
            .await
            .expect("run should succeed");

        assert_eq!(
            result.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );
        assert!(
            saw_marker.load(Ordering::SeqCst),
            "second tool should observe state written after first tool"
        );
    }

    #[tokio::test]
    async fn checkpoint_persists_state_and_thread_together() {
        let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("ok")],
            tool_calls: vec![],
            usage: Some(awaken_contract::contract::inference::TokenUsage {
                prompt_tokens: Some(11),
                completion_tokens: Some(7),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }]));
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm),
            plugins: vec![],
        });
        let store = Arc::new(InMemoryStore::new());
        let runtime = AgentRuntime::new(resolver)
            .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        let result = runtime
            .run(
                RunRequest::new("thread-tx", vec![Message::user("hi")]).with_agent_id("agent"),
                sink.clone(),
            )
            .await
            .expect("run should succeed");
        assert_eq!(
            result.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );

        let latest = store
            .latest_run("thread-tx")
            .await
            .expect("latest run lookup")
            .expect("run persisted");
        assert_eq!(latest.thread_id, "thread-tx");
        assert!(latest.state.is_some(), "state snapshot should be persisted");
        assert_eq!(latest.input_tokens, 11);
        assert_eq!(latest.output_tokens, 7);

        let msgs = store
            .load_messages("thread-tx")
            .await
            .expect("load messages")
            .expect("thread should exist");
        assert!(!msgs.is_empty());
    }

    #[tokio::test]
    async fn run_request_without_agent_id_prefers_latest_thread_state_agent() {
        let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("ok")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }]));
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm),
            plugins: vec![],
        });
        let store = Arc::new(InMemoryStore::new());

        let mut extensions = HashMap::new();
        extensions.insert(
            <ActiveAgentIdKey as StateKey>::KEY.to_string(),
            Value::String("agent-from-state".into()),
        );
        store
            .create_run(&seeded_run_record(
                "seed-1",
                "thread-infer-state",
                "agent-from-record",
                Some(PersistedState {
                    revision: 1,
                    extensions,
                }),
            ))
            .await
            .expect("seed run record");

        let runtime = AgentRuntime::new(resolver)
            .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        runtime
            .run(
                RunRequest::new("thread-infer-state", vec![Message::user("hi")]),
                sink.clone(),
            )
            .await
            .expect("run should succeed");

        let latest = store
            .latest_run("thread-infer-state")
            .await
            .expect("latest run lookup")
            .expect("run persisted");
        assert_eq!(latest.agent_id, "agent-from-state");
    }

    #[tokio::test]
    async fn run_request_without_agent_id_falls_back_to_latest_run_record_agent_id() {
        let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("ok")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }]));
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm),
            plugins: vec![],
        });
        let store = Arc::new(InMemoryStore::new());

        store
            .create_run(&seeded_run_record(
                "seed-2",
                "thread-infer-record",
                "agent-from-record",
                None,
            ))
            .await
            .expect("seed run record");

        let runtime = AgentRuntime::new(resolver)
            .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        runtime
            .run(
                RunRequest::new("thread-infer-record", vec![Message::user("hi")]),
                sink.clone(),
            )
            .await
            .expect("run should succeed");

        let latest = store
            .latest_run("thread-infer-record")
            .await
            .expect("latest run lookup")
            .expect("run persisted");
        assert_eq!(latest.agent_id, "agent-from-record");
    }

    #[tokio::test]
    async fn thread_scoped_state_restores_before_run_start_hooks() {
        let llm = Arc::new(ScriptedLlm::new(vec![
            StreamResult {
                content: vec![ContentBlock::text("ok-1")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            },
            StreamResult {
                content: vec![ContentBlock::text("ok-2")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            },
        ]));
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm),
            plugins: vec![Arc::new(ThreadCounterPlugin)],
        });
        let store = Arc::new(InMemoryStore::new());
        let runtime = AgentRuntime::new(resolver)
            .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        runtime
            .run(
                RunRequest::new("thread-counter", vec![Message::user("first")])
                    .with_agent_id("agent"),
                sink.clone(),
            )
            .await
            .expect("first run should succeed");

        runtime
            .run(
                RunRequest::new("thread-counter", vec![Message::user("second")])
                    .with_agent_id("agent"),
                sink.clone(),
            )
            .await
            .expect("second run should succeed");

        let runs = store
            .list_runs(&RunQuery {
                thread_id: Some("thread-counter".into()),
                ..RunQuery::default()
            })
            .await
            .expect("run list lookup");

        let max_counter = runs
            .items
            .iter()
            .filter_map(|record| record.state.as_ref())
            .filter_map(|persisted| persisted.extensions.get(ThreadCounterKey::KEY))
            .filter_map(serde_json::Value::as_u64)
            .max()
            .expect("thread counter should be persisted");
        assert_eq!(max_counter, 2, "counter should continue across runs");
    }

    // -----------------------------------------------------------------------
    // Truncation recovery tests
    // -----------------------------------------------------------------------

    /// LLM executor that emits truncated tool call JSON on the first call,
    /// then a normal response on subsequent calls.
    struct TruncatingLlm {
        call_count: AtomicUsize,
        /// Responses to return after the first (truncated) call.
        followup_responses: Mutex<Vec<StreamResult>>,
    }

    impl TruncatingLlm {
        fn new(followup_responses: Vec<StreamResult>) -> Self {
            Self {
                call_count: AtomicUsize::new(0),
                followup_responses: Mutex::new(followup_responses),
            }
        }
    }

    #[async_trait]
    impl LlmExecutor for TruncatingLlm {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            unreachable!("execute_stream is overridden");
        }

        fn execute_stream(
            &self,
            _request: InferenceRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            awaken_contract::contract::executor::InferenceStream,
                            InferenceExecutionError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            use awaken_contract::contract::executor::{InferenceStream, LlmStreamEvent};
            use awaken_contract::contract::inference::TokenUsage;

            Box::pin(async move {
                let n = self.call_count.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // First call: emit a tool call with truncated JSON, then MaxTokens
                    let events: Vec<Result<LlmStreamEvent, InferenceExecutionError>> = vec![
                        Ok(LlmStreamEvent::TextDelta("partial ".into())),
                        Ok(LlmStreamEvent::ToolCallStart {
                            id: "tc1".into(),
                            name: "calculator".into(),
                        }),
                        // Truncated JSON: missing closing brace
                        Ok(LlmStreamEvent::ToolCallDelta {
                            id: "tc1".into(),
                            args_delta: r#"{"expr": "1+1"#.into(),
                        }),
                        Ok(LlmStreamEvent::Usage(TokenUsage {
                            prompt_tokens: Some(50),
                            completion_tokens: Some(100),
                            ..Default::default()
                        })),
                        Ok(LlmStreamEvent::Stop(StopReason::MaxTokens)),
                    ];
                    Ok(Box::pin(futures::stream::iter(events)) as InferenceStream)
                } else {
                    // Subsequent calls: return from followup queue
                    let mut followups = self.followup_responses.lock().expect("lock poisoned");
                    let result = if followups.is_empty() {
                        StreamResult {
                            content: vec![ContentBlock::text("final response")],
                            tool_calls: vec![],
                            usage: None,
                            stop_reason: Some(StopReason::EndTurn),
                            has_incomplete_tool_calls: false,
                        }
                    } else {
                        followups.remove(0)
                    };
                    let events =
                        awaken_contract::contract::executor::collected_to_stream_events(result);
                    Ok(Box::pin(futures::stream::iter(events)) as InferenceStream)
                }
            })
        }

        fn name(&self) -> &str {
            "truncating"
        }
    }

    #[tokio::test]
    async fn truncation_recovery_continues_on_max_tokens() {
        // First call returns MaxTokens with truncated tool call
        // Second call returns EndTurn with final text
        let llm = Arc::new(TruncatingLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("completed response")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }]));
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm.clone())
                .with_max_continuation_retries(2),
            plugins: vec![],
        });
        let runtime = AgentRuntime::new(resolver);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        let result = runtime
            .run(
                RunRequest::new("thread-trunc", vec![Message::user("hi")]).with_agent_id("agent"),
                sink.clone(),
            )
            .await
            .expect("run should succeed");

        assert_eq!(
            result.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );
        // The final response should be from the second (continuation) call
        assert_eq!(result.response, "completed response");
        // Two calls total: truncated + continuation
        assert_eq!(llm.call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn truncation_recovery_gives_up_after_max_retries() {
        // All calls return MaxTokens with truncated tool calls
        // (the TruncatingLlm always returns truncated on first call,
        //  and we provide followups that are also truncated)
        struct AlwaysTruncatingLlm {
            call_count: AtomicUsize,
        }

        #[async_trait]
        impl LlmExecutor for AlwaysTruncatingLlm {
            async fn execute(
                &self,
                _request: InferenceRequest,
            ) -> Result<StreamResult, InferenceExecutionError> {
                unreachable!("execute_stream is overridden");
            }

            fn execute_stream(
                &self,
                _request: InferenceRequest,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                awaken_contract::contract::executor::InferenceStream,
                                InferenceExecutionError,
                            >,
                        > + Send
                        + '_,
                >,
            > {
                use awaken_contract::contract::executor::{InferenceStream, LlmStreamEvent};
                use awaken_contract::contract::inference::TokenUsage;

                Box::pin(async move {
                    self.call_count.fetch_add(1, Ordering::SeqCst);
                    // Always return truncated tool call
                    let events: Vec<Result<LlmStreamEvent, InferenceExecutionError>> = vec![
                        Ok(LlmStreamEvent::TextDelta("truncated ".into())),
                        Ok(LlmStreamEvent::ToolCallStart {
                            id: format!("tc{}", self.call_count.load(Ordering::SeqCst)),
                            name: "calculator".into(),
                        }),
                        Ok(LlmStreamEvent::ToolCallDelta {
                            id: format!("tc{}", self.call_count.load(Ordering::SeqCst)),
                            args_delta: r#"{"incomplete"#.into(),
                        }),
                        Ok(LlmStreamEvent::Usage(TokenUsage {
                            prompt_tokens: Some(50),
                            completion_tokens: Some(100),
                            ..Default::default()
                        })),
                        Ok(LlmStreamEvent::Stop(StopReason::MaxTokens)),
                    ];
                    Ok(Box::pin(futures::stream::iter(events)) as InferenceStream)
                })
            }

            fn name(&self) -> &str {
                "always_truncating"
            }
        }

        let llm = Arc::new(AlwaysTruncatingLlm {
            call_count: AtomicUsize::new(0),
        });
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm.clone())
                .with_max_continuation_retries(2),
            plugins: vec![],
        });
        let runtime = AgentRuntime::new(resolver);
        let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

        let result = runtime
            .run(
                RunRequest::new("thread-trunc-max", vec![Message::user("hi")])
                    .with_agent_id("agent"),
                sink.clone(),
            )
            .await
            .expect("run should succeed");

        // Should give up after 1 initial + 2 retries = 3 calls total
        assert_eq!(llm.call_count.load(Ordering::SeqCst), 3);
        // After giving up, the result has no tools, so it ends naturally
        // with the text from the last truncated response
        assert_eq!(
            result.termination,
            awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd
        );
        assert_eq!(result.response, "truncated ");
    }

    // ── strip_unpaired_tool_calls tests ──────────────────────────────

    mod strip_unpaired {
        use super::super::strip_unpaired_tool_calls;
        use awaken_contract::contract::message::{Message, Role, ToolCall};

        fn assistant_with_calls(text: &str, call_ids: &[&str]) -> Message {
            let mut msg = Message::assistant(text);
            msg.tool_calls = Some(
                call_ids
                    .iter()
                    .map(|id| ToolCall {
                        id: id.to_string(),
                        name: "test_tool".into(),
                        arguments: serde_json::json!({}),
                    })
                    .collect(),
            );
            msg
        }

        fn tool_response(call_id: &str) -> Message {
            Message::tool(call_id, "result")
        }

        #[test]
        fn paired_calls_unchanged() {
            let mut msgs = vec![
                Message::user("hi"),
                assistant_with_calls("calling", &["tc1"]),
                tool_response("tc1"),
                Message::assistant("done"),
            ];
            let original_len = msgs.len();
            strip_unpaired_tool_calls(&mut msgs);
            assert_eq!(msgs.len(), original_len);
            // tc1 should still be present
            assert!(msgs[1].tool_calls.as_ref().unwrap().len() == 1);
        }

        #[test]
        fn trailing_unpaired_calls_stripped() {
            let mut msgs = vec![
                Message::user("hi"),
                assistant_with_calls("calling", &["tc1", "tc2"]),
                tool_response("tc1"),
                // tc2 has no tool_response — should be stripped
            ];
            strip_unpaired_tool_calls(&mut msgs);
            let calls = msgs[1].tool_calls.as_ref().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "tc1");
        }

        #[test]
        fn all_unpaired_removes_tool_calls_field() {
            let mut msgs = vec![
                Message::user("hi"),
                assistant_with_calls("", &["tc1"]),
                // no tool response at all
            ];
            strip_unpaired_tool_calls(&mut msgs);
            // Assistant message with no text and no tool calls should be removed
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].role, Role::User);
        }

        #[test]
        fn middle_paired_not_affected() {
            let mut msgs = vec![
                Message::user("first"),
                assistant_with_calls("first call", &["tc1"]),
                tool_response("tc1"),
                Message::user("second"),
                assistant_with_calls("", &["tc2"]),
                // tc2 has no response — stripped, then empty msg removed
            ];
            strip_unpaired_tool_calls(&mut msgs);
            // tc1 should still be intact
            assert_eq!(msgs[1].tool_calls.as_ref().unwrap().len(), 1);
            // tc2 stripped → empty assistant removed → 4 messages left
            assert_eq!(msgs.len(), 4); // user, assistant+tc1, tool, user
        }

        #[test]
        fn no_tool_calls_is_noop() {
            let mut msgs = vec![Message::user("hi"), Message::assistant("hello")];
            strip_unpaired_tool_calls(&mut msgs);
            assert_eq!(msgs.len(), 2);
        }

        #[test]
        fn empty_messages_is_noop() {
            let mut msgs: Vec<Message> = vec![];
            strip_unpaired_tool_calls(&mut msgs);
            assert!(msgs.is_empty());
        }
    }
}
