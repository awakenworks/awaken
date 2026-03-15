use super::manager::SubAgentCompletion;
use super::*;
use crate::composition::{A2aAgentBinding, AgentBinding, ResolvedAgent};
use crate::contracts::runtime::state::AnyStateAction;
use crate::contracts::runtime::tool_call::ToolExecutionEffect;
use crate::contracts::Thread;
use crate::contracts::ToolCallContext;

pub(super) fn to_tool_result(tool_name: &str, summary: SubAgentSummary) -> ToolResult {
    ToolResult::success(
        tool_name,
        json!({
            "run_id": summary.run_id,
            "agent_id": summary.agent_id,
            "status": summary.status.as_str(),
            "error": summary.error,
        }),
    )
}

pub(super) fn tool_error(tool_name: &str, code: &str, message: impl Into<String>) -> ToolResult {
    ToolResult::error_with_code(tool_name, code, message)
}

#[derive(Debug)]
pub(super) struct ToolArgError {
    code: &'static str,
    message: String,
}

impl ToolArgError {
    pub(super) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(super) fn into_tool_result(self, tool_name: &str) -> ToolResult {
        tool_error(tool_name, self.code, self.message)
    }
}

pub(super) fn sub_agent_upsert_action(run_id: impl Into<String>, sub: SubAgent) -> AnyStateAction {
    AnyStateAction::new::<SubAgentState>(SubAgentAction::Upsert {
        run_id: run_id.into(),
        sub,
    })
}

fn effect_with_sub_agent(
    effect: ToolExecutionEffect,
    run_id: impl Into<String>,
    sub: SubAgent,
) -> ToolExecutionEffect {
    effect.with_action(sub_agent_upsert_action(run_id, sub))
}

fn caller_thread_id(ctx: &ToolCallContext<'_>) -> Option<String> {
    ctx.caller_context()
        .thread_id()
        .or_else(|| ctx.run_identity().thread_id_opt())
        .map(str::to_string)
}

fn caller_run_id(ctx: &ToolCallContext<'_>) -> Option<String> {
    ctx.caller_context()
        .run_id()
        .or_else(|| ctx.run_identity().run_id_opt())
        .map(str::to_string)
}

fn required_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn optional_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn required_string(args: &Value, key: &str) -> Result<String, ToolArgError> {
    optional_string(args, key)
        .ok_or_else(|| ToolArgError::new("invalid_arguments", format!("missing '{key}'")))
}

fn caller_agent_id(ctx: &ToolCallContext<'_>) -> Option<String> {
    ctx.caller_context()
        .agent_id()
        .or_else(|| ctx.run_identity().agent_id_opt())
        .map(str::to_string)
}

fn caller_messages(ctx: &ToolCallContext<'_>) -> Vec<Message> {
    ctx.caller_context()
        .messages()
        .iter()
        .map(|message| message.as_ref().clone())
        .collect()
}

fn snapshot_runs(ctx: &ToolCallContext<'_>) -> HashMap<String, SubAgent> {
    ctx.snapshot_of::<SubAgentState>()
        .map(|state| state.runs)
        .unwrap_or_default()
}

fn latest_assistant_output(thread: &Thread) -> Option<String> {
    thread
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| message.content.clone())
}

fn filtered_fork_messages(messages: Vec<Message>) -> Vec<Message> {
    let visible: Vec<Message> = messages
        .into_iter()
        .filter(|m| m.visibility == crate::contracts::thread::Visibility::All)
        .collect();

    let assistant_call_ids: std::collections::HashSet<String> = visible
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|tc| tc.id.clone())
        .collect();

    let tool_result_ids: std::collections::HashSet<String> = visible
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    let paired_ids: std::collections::HashSet<String> = assistant_call_ids
        .intersection(&tool_result_ids)
        .cloned()
        .collect();

    visible
        .into_iter()
        .filter_map(|mut m| {
            if m.role == Role::System {
                return None;
            }

            match m.role {
                Role::Assistant => {
                    if let Some(tool_calls) = m.tool_calls.take() {
                        let filtered_calls: Vec<ToolCall> = tool_calls
                            .into_iter()
                            .filter(|tc| paired_ids.contains(&tc.id))
                            .collect();
                        m.tool_calls = if filtered_calls.is_empty() {
                            None
                        } else {
                            Some(filtered_calls)
                        };
                    }
                    Some(m)
                }
                Role::Tool => {
                    let call_id = m.tool_call_id.as_deref()?;
                    if paired_ids.contains(call_id) {
                        Some(m)
                    } else {
                        None
                    }
                }
                Role::User => Some(m),
                Role::System => None,
            }
        })
        .collect()
}

fn is_target_visible(
    catalog: &dyn AgentCatalog,
    target_id: &str,
    caller_agent_id: Option<&str>,
    policy: Option<&tirea_contract::RunPolicy>,
) -> Option<ResolvedAgent> {
    if caller_agent_id.is_some_and(|caller| caller == target_id) {
        return None;
    }
    if !is_scope_allowed(policy, target_id, ScopeDomain::Agent) {
        return None;
    }
    catalog.get(target_id)
}

#[derive(Debug, Clone)]
pub(super) struct RemoteA2aRunRef {
    pub(super) remote_run_id: String,
    pub(super) mirror_thread_id: Option<String>,
}

pub(super) fn resolve_remote_a2a_target(
    catalog: &dyn AgentCatalog,
    target_id: &str,
) -> Result<A2aAgentBinding, ToolArgError> {
    let Some(target) = catalog.get(target_id) else {
        return Err(ToolArgError::new(
            "unknown_agent",
            format!("Unknown remote agent: {target_id}"),
        ));
    };
    match target.binding {
        AgentBinding::A2a(target) => Ok(target),
        AgentBinding::Local => Err(ToolArgError::new(
            "invalid_state",
            format!("Agent '{target_id}' is not backed by remote A2A"),
        )),
    }
}

pub(super) fn remote_a2a_run_ref(
    _run_id: &str,
    execution: &SubAgentExecutionRef,
    invalid_state_message: impl FnOnce() -> String,
) -> Result<RemoteA2aRunRef, ToolArgError> {
    match execution {
        SubAgentExecutionRef::Remote {
            protocol: SubAgentRemoteProtocol::A2a,
            remote_run_id,
            mirror_thread_id,
            ..
        } => Ok(RemoteA2aRunRef {
            remote_run_id: remote_run_id.clone(),
            mirror_thread_id: mirror_thread_id.clone(),
        }),
        SubAgentExecutionRef::Local { .. } => {
            Err(ToolArgError::new("invalid_state", invalid_state_message()))
        }
    }
}

fn attach_run_metadata(message: Message, run_id: &str, message_id: String) -> Message {
    let mut metadata = message.metadata.clone().unwrap_or_default();
    metadata.run_id = Some(run_id.to_string());
    message.with_id(message_id).with_metadata(metadata)
}

pub(super) fn remote_mirror_thread_id(run_id: &str) -> String {
    sub_agent_thread_id(run_id)
}

fn resolved_remote_mirror_thread_id(run_id: &str, execution: &SubAgentExecutionRef) -> String {
    execution
        .output_thread_id()
        .map(str::to_string)
        .unwrap_or_else(|| remote_mirror_thread_id(run_id))
}

pub(super) fn remote_mirror_input_messages(run_id: &str, messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            attach_run_metadata(
                message.clone(),
                run_id,
                format!("remote-a2a:{run_id}:input:{index}"),
            )
        })
        .collect()
}

fn remote_mirror_output_messages(run_id: &str, output_text: Option<&str>) -> Vec<Message> {
    output_text
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| {
            vec![attach_run_metadata(
                Message::assistant(text),
                run_id,
                format!("remote-a2a:{run_id}:output:0"),
            )]
        })
        .unwrap_or_default()
}

fn remote_mirror_raw_task_message(
    run_id: &str,
    snapshot: &super::remote_a2a::A2aTaskSnapshot,
) -> Result<Option<Message>, String> {
    if snapshot.raw_task.is_null() {
        return Ok(None);
    }
    let payload = serde_json::to_string(&json!({
        "kind": "remote_a2a_task",
        "protocol": "a2a",
        "status": snapshot.status.as_str(),
        "done": snapshot.done,
        "error": snapshot.error.clone(),
        "task": snapshot.raw_task.clone(),
    }))
    .map_err(|err| format!("failed to serialize remote A2A task payload: {err}"))?;
    Ok(Some(attach_run_metadata(
        Message::internal_system(payload),
        run_id,
        remote_task_payload_message_id(run_id),
    )))
}

fn remote_mirror_snapshot_messages(
    run_id: &str,
    snapshot: &super::remote_a2a::A2aTaskSnapshot,
) -> Result<Vec<Message>, String> {
    let mut messages = remote_mirror_output_messages(run_id, snapshot.output_text.as_deref());
    if let Some(raw_payload) = remote_mirror_raw_task_message(run_id, snapshot)? {
        messages.push(raw_payload);
    }
    Ok(messages)
}

pub(super) fn remote_task_payload_message_id(run_id: &str) -> String {
    format!("remote-a2a:{run_id}:task-payload:0")
}

pub(super) async fn append_remote_mirror_messages(
    os: &AgentOs,
    thread_id: &str,
    parent_thread_id: Option<&str>,
    run_id: &str,
    parent_run_id: Option<&str>,
    reason: crate::contracts::CheckpointReason,
    messages: Vec<Message>,
) -> Result<(), String> {
    let Some(store) = os.agent_state_store().cloned() else {
        return Err(
            "remote A2A delegation requires agent_state_store to mirror remote thread output"
                .to_string(),
        );
    };

    let mut loaded = store
        .load(thread_id)
        .await
        .map_err(|err| format!("failed to load remote mirror thread '{thread_id}': {err}"))?;
    let mut version = match &loaded {
        Some(head) => head.version,
        None => {
            let mut thread = crate::contracts::thread::Thread::new(thread_id.to_string());
            if let Some(parent_thread_id) = parent_thread_id
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                thread = thread.with_parent_thread_id(parent_thread_id.to_string());
            }
            store
                .create(&thread)
                .await
                .map_err(|err| {
                    format!("failed to create remote mirror thread '{thread_id}': {err}")
                })?
                .version
        }
    };

    if let Some(parent_thread_id) = parent_thread_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let needs_parent_update = match loaded.as_mut() {
            Some(head) => match head.thread.parent_thread_id.as_deref() {
                Some(existing) if existing != parent_thread_id => {
                    return Err(format!(
                        "parent_thread_id mismatch for remote mirror thread '{thread_id}': existing='{existing}', requested='{parent_thread_id}'",
                    ));
                }
                Some(_) => false,
                None => {
                    head.thread.parent_thread_id = Some(parent_thread_id.to_string());
                    true
                }
            },
            None => false,
        };
        if needs_parent_update {
            let thread = loaded
                .as_ref()
                .map(|head| head.thread.clone())
                .expect("loaded thread should exist when parent update is needed");
            store.save(&thread).await.map_err(|err| {
                format!("failed to save remote mirror thread '{thread_id}': {err}")
            })?;
            version = store
                .load(thread_id)
                .await
                .map_err(|err| {
                    format!("failed to refresh remote mirror thread '{thread_id}': {err}")
                })?
                .ok_or_else(|| {
                    format!("remote mirror thread '{thread_id}' disappeared after save")
                })?
                .version;
        }
    }

    if messages.is_empty() {
        return Ok(());
    }

    let changeset = crate::contracts::ThreadChangeSet::from_parts(
        run_id.to_string(),
        parent_run_id.map(str::to_string),
        reason,
        messages.into_iter().map(std::sync::Arc::new).collect(),
        Vec::new(),
        Vec::new(),
        None,
    );
    store
        .append(
            thread_id,
            &changeset,
            crate::contracts::storage::VersionPrecondition::Exact(version),
        )
        .await
        .map_err(|err| format!("failed to append remote mirror thread '{thread_id}': {err}"))?;
    Ok(())
}

pub(super) async fn sync_remote_snapshot_to_mirror_thread(
    os: &AgentOs,
    thread_id: &str,
    parent_thread_id: Option<&str>,
    run_id: &str,
    parent_run_id: Option<&str>,
    snapshot: &super::remote_a2a::A2aTaskSnapshot,
) -> Result<(), String> {
    let messages = remote_mirror_snapshot_messages(run_id, snapshot)?;
    append_remote_mirror_messages(
        os,
        thread_id,
        parent_thread_id,
        run_id,
        parent_run_id,
        crate::contracts::CheckpointReason::AssistantTurnCommitted,
        messages,
    )
    .await
}

pub(super) fn sub_agent_thread_id(run_id: &str) -> String {
    format!("sub-agent-{run_id}")
}

pub(super) fn persisted_summary(run_id: &str, sub: &SubAgent) -> SubAgentSummary {
    SubAgentSummary {
        run_id: run_id.to_string(),
        agent_id: sub.agent_id.clone(),
        status: sub.status,
        error: sub.error.clone(),
    }
}

fn stopped_orphan_error() -> String {
    "No live executor found in current process; marked stopped".to_string()
}

// ---------------------------------------------------------------------------
// AgentRunTool
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AgentRunTool {
    os: AgentOs,
    handles: Arc<SubAgentHandleTable>,
}

impl AgentRunTool {
    pub fn new(os: AgentOs, handles: Arc<SubAgentHandleTable>) -> Self {
        Self { os, handles }
    }

    fn visible_target(
        &self,
        target_id: &str,
        caller_agent_id: Option<&str>,
        policy: Option<&tirea_contract::RunPolicy>,
    ) -> Result<ResolvedAgent, ToolArgError> {
        is_target_visible(
            self.os.agent_catalog().as_ref(),
            target_id,
            caller_agent_id,
            policy,
        )
        .ok_or_else(|| {
            ToolArgError::new(
                "unknown_agent",
                format!("Unknown or unavailable agent_id: {target_id}"),
            )
        })
    }

    fn persist_summary_effect(
        &self,
        run_id: &str,
        execution: SubAgentExecutionRef,
        parent_run_id: Option<String>,
        summary: &SubAgentSummary,
        tool_name: &str,
    ) -> ToolExecutionEffect {
        let sub = SubAgent {
            execution,
            parent_run_id,
            agent_id: summary.agent_id.clone(),
            status: summary.status,
            error: summary.error.clone(),
        };
        effect_with_sub_agent(
            ToolExecutionEffect::from(to_tool_result(tool_name, summary.clone())),
            run_id.to_string(),
            sub,
        )
    }

    fn backend_for_target(&self, target: &ResolvedAgent) -> Box<dyn AgentBackend> {
        resolve_backend_for_target(target)
    }

    fn backend_for_execution(
        &self,
        execution: &SubAgentExecutionRef,
    ) -> Result<Box<dyn AgentBackend>, ToolArgError> {
        resolve_backend_for_execution(Some(self.os.agent_catalog().as_ref()), execution)
    }
}

#[async_trait]
impl Tool for AgentRunTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            AGENT_RUN_TOOL_ID,
            "Agent Run",
            "Run or inspect a delegated agent target; can run in background",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "agent_id": { "type": "string", "description": "Delegation target id (required for new runs)" },
                "prompt": { "type": "string", "description": "Input for the target agent" },
                "run_id": { "type": "string", "description": "Existing run id to inspect or resume" },
                "fork_context": { "type": "boolean", "description": "Whether to fork caller state/messages into a new local run" },
                "background": { "type": "boolean", "description": "true: run in background; false: wait for completion" }
            }
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, crate::contracts::runtime::tool_call::ToolError> {
        self.execute_effect(args, ctx).await.map(|e| e.result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, crate::contracts::runtime::tool_call::ToolError> {
        let tool_name = AGENT_RUN_TOOL_ID;
        let run_id = optional_string(&args, "run_id");
        let background = required_bool(&args, "background", false);
        let fork_context = required_bool(&args, "fork_context", false);

        let policy = ctx.run_policy();
        let owner_thread_id = caller_thread_id(ctx);
        let Some(owner_thread_id) = owner_thread_id else {
            return Ok(ToolExecutionEffect::from(tool_error(
                tool_name,
                "missing_scope",
                "missing caller thread context",
            )));
        };
        let caller_agent_id = caller_agent_id(ctx);
        let caller_run_id = caller_run_id(ctx);
        let persisted_runs = snapshot_runs(ctx);

        if let Some(run_id) = run_id {
            if let Some(existing) = self
                .handles
                .get_owned_summary(&owner_thread_id, &run_id)
                .await
            {
                match existing.status {
                    SubAgentStatus::Running
                    | SubAgentStatus::Completed
                    | SubAgentStatus::Failed => {
                        let handle = match self
                            .handles
                            .handle_for_resume(&owner_thread_id, &run_id)
                            .await
                        {
                            Ok(handle) => handle,
                            Err(err) => {
                                return Ok(ToolExecutionEffect::from(tool_error(
                                    tool_name,
                                    "unknown_run",
                                    err,
                                )));
                            }
                        };
                        return Ok(self.persist_summary_effect(
                            &run_id,
                            handle.execution.clone(),
                            handle.parent_run_id.clone(),
                            &existing,
                            tool_name,
                        ));
                    }
                    SubAgentStatus::Stopped => {
                        let handle = match self
                            .handles
                            .handle_for_resume(&owner_thread_id, &run_id)
                            .await
                        {
                            Ok(handle) => handle,
                            Err(err) => {
                                return Ok(ToolExecutionEffect::from(tool_error(
                                    tool_name,
                                    "unknown_run",
                                    err,
                                )));
                            }
                        };
                        let execution = handle.execution.clone();
                        let backend = match self.backend_for_execution(&execution) {
                            Ok(backend) => backend,
                            Err(err) => {
                                return Ok(ToolExecutionEffect::from(
                                    err.into_tool_result(tool_name),
                                ));
                            }
                        };
                        if !backend.supports_resume() {
                            return Ok(ToolExecutionEffect::from(tool_error(
                                tool_name,
                                "invalid_state",
                                format!(
                                    "Run '{run_id}' is remote and cannot be resumed; start a new run with agent_id"
                                ),
                            )));
                        }
                        let agent_id = handle.agent_id.clone();
                        let parent_run_id = handle.parent_run_id.clone();
                        let prompt = optional_string(&args, "prompt");
                        if let Err(err) =
                            self.visible_target(&agent_id, caller_agent_id.as_deref(), Some(policy))
                        {
                            return Ok(ToolExecutionEffect::from(err.into_tool_result(tool_name)));
                        }
                        let mut messages = Vec::new();
                        if let Some(prompt) = prompt {
                            messages.push(Message::user(prompt));
                        }
                        let output_thread_id = execution
                            .output_thread_id()
                            .map(str::to_string)
                            .unwrap_or_else(|| backend.default_output_thread_id(&run_id));
                        return Ok(backend
                            .start_effect(
                                &self.os,
                                &self.handles,
                                ctx,
                                AgentBackendStartRequest {
                                    run_id,
                                    owner_thread_id,
                                    agent_id,
                                    parent_run_id: caller_run_id.clone().or(parent_run_id),
                                    output_thread_id,
                                    messages,
                                    initial_state: None,
                                    background,
                                },
                                tool_name,
                            )
                            .await);
                    }
                }
            }

            let Some(persisted) = persisted_runs.get(&run_id).cloned() else {
                return Ok(ToolExecutionEffect::from(tool_error(
                    tool_name,
                    "unknown_run",
                    format!("Unknown run_id: {run_id}"),
                )));
            };

            if persisted.status == SubAgentStatus::Running {
                let backend = match self.backend_for_execution(&persisted.execution) {
                    Ok(backend) => backend,
                    Err(err) => {
                        return Ok(ToolExecutionEffect::from(err.into_tool_result(tool_name)));
                    }
                };
                if backend.supports_orphan_refresh() {
                    return Ok(backend
                        .refresh_effect(
                            &self.os,
                            ctx,
                            AgentBackendRefreshRequest {
                                run_id,
                                owner_thread_id,
                                persisted,
                            },
                            tool_name,
                        )
                        .await);
                }
                let stopped = SubAgent {
                    status: SubAgentStatus::Stopped,
                    error: Some(stopped_orphan_error()),
                    ..persisted.clone()
                };
                return Ok(effect_with_sub_agent(
                    ToolExecutionEffect::from(to_tool_result(
                        tool_name,
                        persisted_summary(&run_id, &stopped),
                    )),
                    run_id,
                    stopped,
                ));
            }

            match persisted.status {
                SubAgentStatus::Completed | SubAgentStatus::Failed => {
                    return Ok(ToolExecutionEffect::from(to_tool_result(
                        tool_name,
                        persisted_summary(&run_id, &persisted),
                    )));
                }
                SubAgentStatus::Stopped => {
                    let backend = match self.backend_for_execution(&persisted.execution) {
                        Ok(backend) => backend,
                        Err(err) => {
                            return Ok(ToolExecutionEffect::from(err.into_tool_result(tool_name)));
                        }
                    };
                    if !backend.supports_resume() {
                        return Ok(ToolExecutionEffect::from(tool_error(
                            tool_name,
                            "invalid_state",
                            format!(
                                "Run '{run_id}' is remote and cannot be resumed; start a new run with agent_id"
                            ),
                        )));
                    }
                    if let Err(err) = self.visible_target(
                        &persisted.agent_id,
                        caller_agent_id.as_deref(),
                        Some(policy),
                    ) {
                        return Ok(ToolExecutionEffect::from(err.into_tool_result(tool_name)));
                    }
                    let mut messages = Vec::new();
                    if let Some(prompt) = optional_string(&args, "prompt") {
                        messages.push(Message::user(prompt));
                    }
                    let output_thread_id = persisted
                        .execution
                        .output_thread_id()
                        .map(str::to_string)
                        .unwrap_or_else(|| backend.default_output_thread_id(&run_id));
                    return Ok(backend
                        .start_effect(
                            &self.os,
                            &self.handles,
                            ctx,
                            AgentBackendStartRequest {
                                run_id,
                                owner_thread_id,
                                agent_id: persisted.agent_id,
                                parent_run_id: caller_run_id.clone().or(persisted.parent_run_id),
                                output_thread_id,
                                messages,
                                initial_state: None,
                                background,
                            },
                            tool_name,
                        )
                        .await);
                }
                SubAgentStatus::Running => unreachable!("handled above"),
            }
        }

        let target_id = match required_string(&args, "agent_id") {
            Ok(target_id) => target_id,
            Err(err) => return Ok(ToolExecutionEffect::from(err.into_tool_result(tool_name))),
        };
        let prompt = match required_string(&args, "prompt") {
            Ok(prompt) => prompt,
            Err(err) => return Ok(ToolExecutionEffect::from(err.into_tool_result(tool_name))),
        };
        let target = match self.visible_target(&target_id, caller_agent_id.as_deref(), Some(policy))
        {
            Ok(target) => target,
            Err(err) => return Ok(ToolExecutionEffect::from(err.into_tool_result(tool_name))),
        };
        let backend = self.backend_for_target(&target);
        if fork_context && !backend.supports_fork_context() {
            return Ok(ToolExecutionEffect::from(tool_error(
                tool_name,
                "invalid_arguments",
                "fork_context is only supported for hosted local agents",
            )));
        }

        let run_id = uuid::Uuid::now_v7().to_string();
        let output_thread_id = backend.default_output_thread_id(&run_id);
        let (messages, initial_state) = if fork_context {
            let fork_state = ctx.snapshot();
            let mut messages = filtered_fork_messages(caller_messages(ctx));
            messages.push(Message::user(prompt));
            (messages, Some(fork_state))
        } else {
            (vec![Message::user(prompt)], None)
        };
        Ok(backend
            .start_effect(
                &self.os,
                &self.handles,
                ctx,
                AgentBackendStartRequest {
                    run_id,
                    owner_thread_id,
                    agent_id: target_id,
                    parent_run_id: caller_run_id,
                    output_thread_id,
                    messages,
                    initial_state,
                    background,
                },
                tool_name,
            )
            .await)
    }
}

// ---------------------------------------------------------------------------
// AgentStopTool
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AgentStopTool {
    os: Option<AgentOs>,
    handles: Arc<SubAgentHandleTable>,
}

impl AgentStopTool {
    #[cfg(test)]
    pub fn new(handles: Arc<SubAgentHandleTable>) -> Self {
        Self { os: None, handles }
    }

    pub fn with_os(os: AgentOs, handles: Arc<SubAgentHandleTable>) -> Self {
        Self {
            os: Some(os),
            handles,
        }
    }

    fn backend_for_execution(
        &self,
        execution: &SubAgentExecutionRef,
    ) -> Result<Box<dyn AgentBackend>, ToolArgError> {
        let catalog = self.os.as_ref().map(AgentOs::agent_catalog);
        resolve_backend_for_execution(catalog.as_deref(), execution)
    }
}

#[async_trait]
impl Tool for AgentStopTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            AGENT_STOP_TOOL_ID,
            "Agent Stop",
            "Stop a delegated run by run_id",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string", "description": "Run id returned by agent_run" }
            },
            "required": ["run_id"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, crate::contracts::runtime::tool_call::ToolError> {
        self.execute_effect(args, ctx).await.map(|e| e.result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, crate::contracts::runtime::tool_call::ToolError> {
        let tool_name = AGENT_STOP_TOOL_ID;
        let run_id = match required_string(&args, "run_id") {
            Ok(run_id) => run_id,
            Err(err) => return Ok(ToolExecutionEffect::from(err.into_tool_result(tool_name))),
        };
        let owner_thread_id = caller_thread_id(ctx);
        let Some(owner_thread_id) = owner_thread_id else {
            return Ok(ToolExecutionEffect::from(tool_error(
                tool_name,
                "missing_scope",
                "missing caller thread context",
            )));
        };

        let persisted_runs = snapshot_runs(ctx);
        let live_requests = self
            .handles
            .prepare_stop_owned_tree(&owner_thread_id, &run_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|request| (request.run_id.clone(), request))
            .collect::<HashMap<_, _>>();

        let mut work_ids = collect_descendant_run_ids_from_state(&persisted_runs, &run_id, true);
        if work_ids.is_empty() {
            work_ids.push(run_id.clone());
        }
        for id in live_requests.keys() {
            if !work_ids.contains(id) {
                work_ids.push(id.clone());
            }
        }

        let mut stopped_any = false;
        let mut root_summary = None;
        let mut effect: Option<ToolExecutionEffect> = None;

        for id in work_ids {
            let persisted = persisted_runs.get(&id).cloned();
            if persisted
                .as_ref()
                .is_some_and(|run| run.status != SubAgentStatus::Running)
            {
                if id == run_id {
                    root_summary = persisted.as_ref().map(|run| persisted_summary(&id, run));
                }
                continue;
            }

            let summary = if let Some(request) = live_requests.get(&id) {
                match &request.execution {
                    SubAgentExecutionRef::Local { .. } => {
                        if let Some(token) = &request.cancellation_token {
                            token.cancel();
                        }
                        self.handles
                            .mark_stop_requested(
                                &id,
                                request.epoch,
                                Some("stopped by owner request".to_string()),
                            )
                            .await
                            .unwrap_or(SubAgentSummary {
                                run_id: id.clone(),
                                agent_id: request.agent_id.clone(),
                                status: SubAgentStatus::Stopped,
                                error: Some("stopped by owner request".to_string()),
                            })
                    }
                    SubAgentExecutionRef::Remote {
                        protocol: SubAgentRemoteProtocol::A2a,
                        ..
                    } => {
                        let backend = match self.backend_for_execution(&request.execution) {
                            Ok(backend) => backend,
                            Err(err) => {
                                return Ok(ToolExecutionEffect::from(
                                    err.into_tool_result(tool_name),
                                ));
                            }
                        };
                        let remote_run = persisted.clone().unwrap_or(SubAgent {
                            execution: request.execution.clone(),
                            parent_run_id: None,
                            agent_id: request.agent_id.clone(),
                            status: SubAgentStatus::Running,
                            error: None,
                        });
                        let remote_summary = match backend.stop(&id, &remote_run).await {
                            Ok(summary) => summary,
                            Err(err) => {
                                return Ok(ToolExecutionEffect::from(
                                    err.into_tool_result(tool_name),
                                ));
                            }
                        };
                        if remote_summary.status == SubAgentStatus::Stopped {
                            self.handles
                                .mark_stop_requested(
                                    &id,
                                    request.epoch,
                                    remote_summary.error.clone(),
                                )
                                .await
                                .unwrap_or(remote_summary)
                        } else {
                            if remote_summary.status != SubAgentStatus::Running {
                                let _ = self
                                    .handles
                                    .update_after_completion(
                                        &id,
                                        request.epoch,
                                        SubAgentCompletion {
                                            status: remote_summary.status,
                                            error: remote_summary.error.clone(),
                                        },
                                    )
                                    .await;
                            }
                            remote_summary
                        }
                    }
                }
            } else {
                let Some(run) = persisted.clone() else {
                    continue;
                };
                match &run.execution {
                    SubAgentExecutionRef::Local { .. } => SubAgentSummary {
                        run_id: id.clone(),
                        agent_id: run.agent_id.clone(),
                        status: SubAgentStatus::Stopped,
                        error: Some(stopped_orphan_error()),
                    },
                    SubAgentExecutionRef::Remote {
                        protocol: SubAgentRemoteProtocol::A2a,
                        ..
                    } => {
                        let backend = match self.backend_for_execution(&run.execution) {
                            Ok(backend) => backend,
                            Err(err) => {
                                return Ok(ToolExecutionEffect::from(
                                    err.into_tool_result(tool_name),
                                ));
                            }
                        };
                        match backend.stop(&id, &run).await {
                            Ok(summary) => summary,
                            Err(err) => {
                                return Ok(ToolExecutionEffect::from(
                                    err.into_tool_result(tool_name),
                                ));
                            }
                        }
                    }
                }
            };

            if let Some(run) = persisted {
                let updated = SubAgent {
                    status: summary.status,
                    error: summary.error.clone(),
                    ..run
                };
                let next_effect = effect.take().unwrap_or_else(|| {
                    ToolExecutionEffect::from(to_tool_result(tool_name, summary.clone()))
                });
                effect = Some(effect_with_sub_agent(next_effect, id.clone(), updated));
            }

            if id == run_id {
                root_summary = Some(summary.clone());
            }
            stopped_any = true;
        }

        if !stopped_any {
            return Ok(ToolExecutionEffect::from(tool_error(
                tool_name,
                "invalid_state",
                format!("Run '{run_id}' cannot be stopped"),
            )));
        }

        if let Some(summary) = root_summary {
            return Ok(effect
                .unwrap_or_else(|| ToolExecutionEffect::from(to_tool_result(tool_name, summary))));
        }

        Ok(ToolExecutionEffect::from(tool_error(
            tool_name,
            "invalid_state",
            "No matching run state for stopped run",
        )))
    }
}

// ---------------------------------------------------------------------------
// AgentOutputTool
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AgentOutputTool {
    os: AgentOs,
}

impl AgentOutputTool {
    pub fn new(os: AgentOs) -> Self {
        Self { os }
    }

    fn backend_for_execution(
        &self,
        execution: &SubAgentExecutionRef,
    ) -> Result<Box<dyn AgentBackend>, ToolArgError> {
        resolve_backend_for_execution(Some(self.os.agent_catalog().as_ref()), execution)
    }
}

#[async_trait]
impl Tool for AgentOutputTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            AGENT_OUTPUT_TOOL_ID,
            "Agent Output",
            "Retrieve the latest output from a delegated run",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string", "description": "Run id returned by agent_run" }
            },
            "required": ["run_id"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, crate::contracts::runtime::tool_call::ToolError> {
        self.execute_effect(args, ctx).await.map(|e| e.result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, crate::contracts::runtime::tool_call::ToolError> {
        let tool_name = AGENT_OUTPUT_TOOL_ID;
        let run_id = match required_string(&args, "run_id") {
            Ok(run_id) => run_id,
            Err(err) => return Ok(ToolExecutionEffect::from(err.into_tool_result(tool_name))),
        };

        let persisted = snapshot_runs(ctx);
        let Some(sub) = persisted.get(&run_id).cloned() else {
            return Ok(ToolExecutionEffect::from(tool_error(
                tool_name,
                "unknown_run",
                format!("Unknown run_id: {run_id}"),
            )));
        };

        let thread_id = resolved_remote_mirror_thread_id(&run_id, &sub.execution);
        let mut sub = sub;
        let mut effect = ToolExecutionEffect::from(ToolResult::success(tool_name, json!({})));
        let mut thread = match self.os.load_thread(&thread_id).await {
            Ok(Some(head)) => Some(head),
            Ok(None) => None,
            Err(err) => {
                return Ok(ToolExecutionEffect::from(tool_error(
                    tool_name,
                    "store_error",
                    format!("failed to load sub-agent thread: {err}"),
                )));
            }
        };

        if matches!(sub.execution, SubAgentExecutionRef::Remote { .. })
            && sub.execution.output_thread_id().is_none()
        {
            sub.execution = sub
                .execution
                .clone()
                .with_mirror_thread_id(thread_id.clone());
            effect = effect_with_sub_agent(effect, run_id.clone(), sub.clone());
        }

        if thread.is_none() {
            let backend = match self.backend_for_execution(&sub.execution) {
                Ok(backend) => backend,
                Err(err) => {
                    return Ok(ToolExecutionEffect::from(err.into_tool_result(tool_name)));
                }
            };
            let actions = match backend
                .sync_output_effect(
                    &self.os,
                    ctx,
                    AgentBackendOutputSyncRequest {
                        run_id: run_id.clone(),
                        owner_thread_id: caller_thread_id(ctx),
                        thread_id: thread_id.clone(),
                        sub: sub.clone(),
                    },
                )
                .await
            {
                Ok(actions) => actions,
                Err(err) => return Ok(ToolExecutionEffect::from(err.into_tool_result(tool_name))),
            };
            for action in actions {
                effect = effect.with_action(action);
            }
            thread = match self.os.load_thread(&thread_id).await {
                Ok(Some(head)) => Some(head),
                Ok(None) => None,
                Err(err) => {
                    return Ok(ToolExecutionEffect::from(tool_error(
                        tool_name,
                        "store_error",
                        format!("failed to load sub-agent thread: {err}"),
                    )));
                }
            };
        }

        let output = thread
            .as_ref()
            .and_then(|head| latest_assistant_output(&head.thread));
        effect.result = ToolResult::success(
            tool_name,
            json!({
                "run_id": run_id,
                "agent_id": sub.agent_id,
                "status": sub.status.as_str(),
                "error": sub.error,
                "output": output,
            }),
        );
        Ok(effect)
    }
}
