use super::*;
use crate::contracts::ToolCallContext;

fn to_tool_result(tool_name: &str, summary: SubAgentSummary) -> ToolResult {
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

fn tool_error(tool_name: &str, code: &str, message: impl Into<String>) -> ToolResult {
    ToolResult::error_with_code(tool_name, code, message)
}

#[derive(Debug)]
struct ToolArgError {
    code: &'static str,
    message: String,
}

impl ToolArgError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn into_tool_result(self, tool_name: &str) -> ToolResult {
        tool_error(tool_name, self.code, self.message)
    }
}

fn state_write_failed(tool_name: &str, err: impl std::fmt::Display) -> ToolResult {
    tool_error(
        tool_name,
        "state_error",
        format!("failed to persist sub-agent state: {err}"),
    )
}

fn scope_string(scope: Option<&tirea_contract::RunConfig>, key: &str) -> Option<String> {
    scope
        .and_then(|scope: &tirea_contract::RunConfig| scope.value(key))
        .and_then(|value: &serde_json::Value| value.as_str())
        .map(|value| value.to_string())
}

fn scope_run_id(scope: Option<&tirea_contract::RunConfig>) -> Option<String> {
    scope_string(scope, SCOPE_RUN_ID_KEY)
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

fn parse_caller_messages(scope: Option<&tirea_contract::RunConfig>) -> Option<Vec<Message>> {
    let value = scope.and_then(|scope| scope.value(SCOPE_CALLER_MESSAGES_KEY))?;
    serde_json::from_value::<Vec<Message>>(value.clone()).ok()
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

struct LaunchNewRunRequest {
    run_id: String,
    owner_thread_id: String,
    agent_id: String,
    parent_run_id: Option<String>,
    child_thread_id: String,
    messages: Vec<Message>,
    initial_state: Option<Value>,
    background: bool,
}

fn is_target_agent_visible(
    registry: &dyn AgentRegistry,
    target: &str,
    caller: Option<&str>,
    scope: Option<&tirea_contract::RunConfig>,
) -> bool {
    if caller.is_some_and(|c| c == target) {
        return false;
    }
    if !is_scope_allowed(
        scope,
        target,
        SCOPE_ALLOWED_AGENTS_KEY,
        SCOPE_EXCLUDED_AGENTS_KEY,
    ) {
        return false;
    }
    registry.get(target).is_some()
}

pub(super) fn sub_agent_thread_id(run_id: &str) -> String {
    format!("sub-agent-{run_id}")
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

    fn ensure_target_visible(
        &self,
        target_agent_id: &str,
        caller_agent_id: Option<&str>,
        scope: Option<&tirea_contract::RunConfig>,
    ) -> Result<(), ToolArgError> {
        if is_target_agent_visible(
            self.os.agents_registry().as_ref(),
            target_agent_id,
            caller_agent_id,
            scope,
        ) {
            return Ok(());
        }

        Err(ToolArgError::new(
            "unknown_agent",
            format!("Unknown or unavailable agent_id: {target_agent_id}"),
        ))
    }

    async fn persist_live_summary(
        &self,
        ctx: &ToolCallContext<'_>,
        run_id: &str,
        child_thread_id: &str,
        parent_run_id: Option<String>,
        summary: &SubAgentSummary,
        tool_name: &str,
    ) -> ToolResult {
        let sub = SubAgent {
            thread_id: child_thread_id.to_string(),
            parent_run_id,
            agent_id: summary.agent_id.clone(),
            status: summary.status,
            error: summary.error.clone(),
        };
        if let Err(err) = ctx
            .state_of::<SubAgentState>()
            .runs_insert(run_id.to_string(), sub)
        {
            return state_write_failed(tool_name, err);
        }
        to_tool_result(tool_name, summary.clone())
    }

    async fn launch_new_run(
        &self,
        ctx: &ToolCallContext<'_>,
        request: LaunchNewRunRequest,
        tool_name: &str,
    ) -> ToolResult {
        let LaunchNewRunRequest {
            run_id,
            owner_thread_id,
            agent_id,
            parent_run_id,
            child_thread_id,
            messages,
            initial_state,
            background,
        } = request;
        let sub = SubAgent {
            thread_id: child_thread_id.clone(),
            parent_run_id: parent_run_id.clone(),
            agent_id: agent_id.clone(),
            status: SubAgentStatus::Running,
            error: None,
        };
        let parent_tool_call_id = ctx.call_id().to_string();

        if background {
            let token = RunCancellationToken::new();
            let parent_run_id_bg = parent_run_id.clone();
            let parent_tool_call_id_bg = parent_tool_call_id.clone();
            let epoch = self
                .handles
                .put_running(
                    &run_id,
                    owner_thread_id.clone(),
                    child_thread_id.clone(),
                    agent_id.clone(),
                    parent_run_id.clone(),
                    Some(token.clone()),
                )
                .await;

            // Persist Running record immediately.
            if let Err(err) = ctx
                .state_of::<SubAgentState>()
                .runs_insert(run_id.to_string(), sub)
            {
                let _ = self.handles.remove_if_epoch(&run_id, epoch).await;
                return state_write_failed(tool_name, err);
            }

            let handles = self.handles.clone();
            let os = self.os.clone();
            let run_id_bg = run_id.clone();
            let agent_id_bg = agent_id.clone();
            let child_thread_id_bg = child_thread_id;
            let parent_thread_id_bg = owner_thread_id;
            tokio::spawn(async move {
                let completion = execute_sub_agent(
                    os,
                    SubAgentExecutionRequest {
                        agent_id: agent_id_bg,
                        child_thread_id: child_thread_id_bg,
                        run_id: run_id_bg.clone(),
                        parent_run_id: parent_run_id_bg,
                        parent_tool_call_id: Some(parent_tool_call_id_bg),
                        parent_thread_id: parent_thread_id_bg,
                        messages,
                        initial_state,
                        cancellation_token: Some(token),
                    },
                    None,
                )
                .await;
                let _ = handles
                    .update_after_completion(&run_id_bg, epoch, completion)
                    .await;
            });

            return to_tool_result(
                tool_name,
                SubAgentSummary {
                    run_id,
                    agent_id,
                    status: SubAgentStatus::Running,
                    error: None,
                },
            );
        }

        // Foreground run.
        let epoch = self
            .handles
            .put_running(
                &run_id,
                owner_thread_id.clone(),
                child_thread_id.clone(),
                agent_id.clone(),
                parent_run_id.clone(),
                None,
            )
            .await;

        let forward_progress =
            |update: crate::contracts::runtime::tool_call::ToolCallProgressUpdate| {
                ctx.report_tool_call_progress(update)
            };

        let completion = execute_sub_agent(
            self.os.clone(),
            SubAgentExecutionRequest {
                agent_id: agent_id.clone(),
                child_thread_id: child_thread_id.clone(),
                run_id: run_id.clone(),
                parent_run_id: parent_run_id.clone(),
                parent_tool_call_id: Some(parent_tool_call_id),
                parent_thread_id: owner_thread_id,
                messages,
                initial_state,
                cancellation_token: None,
            },
            Some(&forward_progress),
        )
        .await;

        let completed_sub = SubAgent {
            thread_id: child_thread_id,
            parent_run_id,
            agent_id: agent_id.clone(),
            status: completion.status,
            error: completion.error.clone(),
        };

        let summary = self
            .handles
            .update_after_completion(&run_id, epoch, completion)
            .await
            .unwrap_or_else(|| SubAgentSummary {
                run_id: run_id.clone(),
                agent_id,
                status: completed_sub.status,
                error: completed_sub.error.clone(),
            });

        if let Err(err) = ctx
            .state_of::<SubAgentState>()
            .runs_insert(run_id.to_string(), completed_sub)
        {
            return state_write_failed(tool_name, err);
        }
        to_tool_result(tool_name, summary)
    }
}

#[async_trait]
impl Tool for AgentRunTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            AGENT_RUN_TOOL_ID,
            "Agent Run",
            "Run or resume a registry agent; can run in background",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "agent_id": { "type": "string", "description": "Target agent id (required for new runs)" },
                "prompt": { "type": "string", "description": "Input for the target agent" },
                "run_id": { "type": "string", "description": "Existing run id to resume or inspect" },
                "fork_context": { "type": "boolean", "description": "Whether to fork caller state/messages into the new run" },
                "background": { "type": "boolean", "description": "true: run in background; false: wait for completion" }
            }
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, crate::contracts::runtime::tool_call::ToolError> {
        let tool_name = AGENT_RUN_TOOL_ID;
        let run_id = optional_string(&args, "run_id");
        let background = required_bool(&args, "background", false);
        let fork_context = required_bool(&args, "fork_context", false);

        let scope = ctx.run_config();
        let owner_thread_id = scope_string(Some(scope), SCOPE_CALLER_SESSION_ID_KEY);
        let Some(owner_thread_id) = owner_thread_id else {
            return Ok(tool_error(
                tool_name,
                "missing_scope",
                "missing caller thread context",
            ));
        };
        let caller_agent_id = scope_string(Some(scope), SCOPE_CALLER_AGENT_ID_KEY);
        let caller_run_id = scope_run_id(Some(scope));

        // ── Resume existing run by ID ──────────────────────────────
        if let Some(run_id) = run_id {
            // 1. Check live handle first.
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
                            Ok(v) => v,
                            Err(e) => return Ok(tool_error(tool_name, "unknown_run", e)),
                        };
                        let result = self
                            .persist_live_summary(
                                ctx,
                                &run_id,
                                &handle.child_thread_id,
                                handle.parent_run_id.clone(),
                                &existing,
                                tool_name,
                            )
                            .await;
                        return Ok(result);
                    }
                    SubAgentStatus::Stopped => {
                        // Resume from live handle.
                        let handle = match self
                            .handles
                            .handle_for_resume(&owner_thread_id, &run_id)
                            .await
                        {
                            Ok(v) => v,
                            Err(e) => return Ok(tool_error(tool_name, "unknown_run", e)),
                        };

                        if let Err(error) = self.ensure_target_visible(
                            &handle.agent_id,
                            caller_agent_id.as_deref(),
                            Some(scope),
                        ) {
                            return Ok(error.into_tool_result(tool_name));
                        }

                        let mut messages = Vec::new();
                        if let Some(prompt) = optional_string(&args, "prompt") {
                            messages.push(Message::user(prompt));
                        }

                        return Ok(self
                            .launch_new_run(
                                ctx,
                                LaunchNewRunRequest {
                                    run_id,
                                    owner_thread_id,
                                    agent_id: handle.agent_id,
                                    parent_run_id: caller_run_id,
                                    child_thread_id: handle.child_thread_id,
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

            // 2. Check persisted state.
            let persisted_opt = ctx
                .state_of::<SubAgentState>()
                .runs()
                .ok()
                .unwrap_or_default()
                .remove(&run_id);

            let Some(persisted) = persisted_opt else {
                return Ok(tool_error(
                    tool_name,
                    "unknown_run",
                    format!("Unknown run_id: {run_id}"),
                ));
            };

            // Orphaned running → mark stopped.
            if persisted.status == SubAgentStatus::Running {
                let stopped = SubAgent {
                    status: SubAgentStatus::Stopped,
                    error: Some(
                        "No live executor found in current process; marked stopped".to_string(),
                    ),
                    ..persisted.clone()
                };
                if let Err(err) = ctx
                    .state_of::<SubAgentState>()
                    .runs_insert(run_id.to_string(), stopped.clone())
                {
                    return Ok(state_write_failed(tool_name, err));
                }
                return Ok(to_tool_result(
                    tool_name,
                    SubAgentSummary {
                        run_id,
                        agent_id: stopped.agent_id,
                        status: SubAgentStatus::Stopped,
                        error: stopped.error,
                    },
                ));
            }

            match persisted.status {
                SubAgentStatus::Completed | SubAgentStatus::Failed => {
                    return Ok(to_tool_result(
                        tool_name,
                        SubAgentSummary {
                            run_id,
                            agent_id: persisted.agent_id,
                            status: persisted.status,
                            error: persisted.error,
                        },
                    ));
                }
                SubAgentStatus::Stopped => {
                    if let Err(error) = self.ensure_target_visible(
                        &persisted.agent_id,
                        caller_agent_id.as_deref(),
                        Some(scope),
                    ) {
                        return Ok(error.into_tool_result(tool_name));
                    }

                    let mut messages = Vec::new();
                    if let Some(prompt) = optional_string(&args, "prompt") {
                        messages.push(Message::user(prompt));
                    }

                    return Ok(self
                        .launch_new_run(
                            ctx,
                            LaunchNewRunRequest {
                                run_id,
                                owner_thread_id,
                                agent_id: persisted.agent_id,
                                parent_run_id: caller_run_id,
                                child_thread_id: persisted.thread_id,
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

        // ── New run ────────────────────────────────────────────────
        let target_agent_id = match required_string(&args, "agent_id") {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(tool_name)),
        };
        let prompt = match required_string(&args, "prompt") {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(tool_name)),
        };

        if let Err(error) =
            self.ensure_target_visible(&target_agent_id, caller_agent_id.as_deref(), Some(scope))
        {
            return Ok(error.into_tool_result(tool_name));
        }

        let run_id = uuid::Uuid::now_v7().to_string();
        let child_thread_id = sub_agent_thread_id(&run_id);

        let (messages, initial_state) = if fork_context {
            let fork_state = scope
                .value(SCOPE_CALLER_STATE_KEY)
                .cloned()
                .unwrap_or_else(|| json!({}));
            let mut msgs = if let Some(caller_msgs) = parse_caller_messages(Some(scope)) {
                filtered_fork_messages(caller_msgs)
            } else {
                Vec::new()
            };
            msgs.push(Message::user(prompt));
            (msgs, Some(fork_state))
        } else {
            (vec![Message::user(prompt)], None)
        };

        Ok(self
            .launch_new_run(
                ctx,
                LaunchNewRunRequest {
                    run_id,
                    owner_thread_id,
                    agent_id: target_agent_id,
                    parent_run_id: caller_run_id,
                    child_thread_id,
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
    handles: Arc<SubAgentHandleTable>,
}

impl AgentStopTool {
    pub fn new(handles: Arc<SubAgentHandleTable>) -> Self {
        Self { handles }
    }
}

#[async_trait]
impl Tool for AgentStopTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            AGENT_STOP_TOOL_ID,
            "Agent Stop",
            "Stop a background agent run by run_id",
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
        let tool_name = AGENT_STOP_TOOL_ID;
        let run_id = match required_string(&args, "run_id") {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(tool_name)),
        };
        let owner_thread_id = ctx
            .run_config()
            .value(SCOPE_CALLER_SESSION_ID_KEY)
            .and_then(|v: &serde_json::Value| v.as_str())
            .map(|v: &str| v.to_string());
        let Some(owner_thread_id) = owner_thread_id else {
            return Ok(tool_error(
                tool_name,
                "missing_scope",
                "missing caller thread context",
            ));
        };

        let mut persisted_runs = ctx
            .state_of::<SubAgentState>()
            .runs()
            .ok()
            .unwrap_or_default();
        let mut tree_ids = collect_descendant_run_ids_from_state(&persisted_runs, &run_id, true);
        if tree_ids.is_empty() {
            tree_ids.push(run_id.clone());
        }

        let mut summaries: HashMap<String, SubAgentSummary> = HashMap::new();
        let mut manager_error = None;

        match self
            .handles
            .stop_owned_tree(&owner_thread_id, &run_id)
            .await
        {
            Ok(stopped) => {
                for summary in stopped {
                    summaries.insert(summary.run_id.clone(), summary);
                }
            }
            Err(e) => {
                manager_error = Some(e);
            }
        }

        let mut stopped_any = !summaries.is_empty();
        for id in &tree_ids {
            let Some(run) = persisted_runs.get_mut(id) else {
                continue;
            };
            if run.status != SubAgentStatus::Running {
                continue;
            }

            if let Some(summary) = summaries.remove(id) {
                run.status = summary.status;
                run.error = summary.error;
            } else {
                run.status = SubAgentStatus::Stopped;
                run.error =
                    Some("No live executor found in current process; marked stopped".to_string());
            }
            stopped_any = true;
            if let Err(err) = ctx
                .state_of::<SubAgentState>()
                .runs_insert(id.to_string(), run.clone())
            {
                return Ok(state_write_failed(tool_name, err));
            }
        }

        if !stopped_any {
            if let Some(err) = manager_error {
                return Ok(tool_error(tool_name, "invalid_state", err));
            }
            return Ok(tool_error(
                tool_name,
                "invalid_state",
                format!("Run '{run_id}' cannot be stopped"),
            ));
        }

        if let Some(summary) = {
            if let Some(summary) = summaries.remove(&run_id) {
                Some(summary)
            } else {
                persisted_runs.get(&run_id).map(|run| SubAgentSummary {
                    run_id: run_id.clone(),
                    agent_id: run.agent_id.clone(),
                    status: run.status,
                    error: run.error.clone(),
                })
            }
        } {
            return Ok(to_tool_result(tool_name, summary));
        }

        let fallback_target = persisted_runs.remove(&run_id);
        if let Some(run) = fallback_target {
            return Ok(to_tool_result(
                tool_name,
                SubAgentSummary {
                    run_id,
                    agent_id: run.agent_id,
                    status: run.status,
                    error: run.error,
                },
            ));
        }

        Ok(tool_error(
            tool_name,
            "invalid_state",
            "No matching run state for stopped run",
        ))
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
}

#[async_trait]
impl Tool for AgentOutputTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            AGENT_OUTPUT_TOOL_ID,
            "Agent Output",
            "Retrieve the latest output from a sub-agent run",
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
        let tool_name = AGENT_OUTPUT_TOOL_ID;
        let run_id = match required_string(&args, "run_id") {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(tool_name)),
        };

        let persisted = ctx
            .state_of::<SubAgentState>()
            .runs()
            .ok()
            .unwrap_or_default();

        let Some(sub) = persisted.get(&run_id) else {
            return Ok(tool_error(
                tool_name,
                "unknown_run",
                format!("Unknown run_id: {run_id}"),
            ));
        };

        let output = match self.os.load_thread(&sub.thread_id).await {
            Ok(Some(head)) => head
                .thread
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.content.clone()),
            Ok(None) => None,
            Err(e) => {
                return Ok(tool_error(
                    tool_name,
                    "store_error",
                    format!("failed to load sub-agent thread: {e}"),
                ));
            }
        };

        Ok(ToolResult::success(
            tool_name,
            json!({
                "run_id": run_id,
                "agent_id": sub.agent_id,
                "status": sub.status.as_str(),
                "output": output,
            }),
        ))
    }
}
