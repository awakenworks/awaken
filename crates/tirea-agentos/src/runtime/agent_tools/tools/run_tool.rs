use super::*;
use crate::runtime::background_tasks::{
    BackgroundTaskManager, NewTaskSpec, SpawnParams, TaskResult as BgTaskResult, TaskStatus,
    TaskStore, TaskStoreError,
};
use schemars::JsonSchema;
use serde::Deserialize;

/// Arguments for the agent run tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentRunArgs {
    /// Target agent id (required for new runs).
    pub agent_id: Option<String>,
    /// Input for the target agent.
    pub prompt: Option<String>,
    /// Existing run id to resume or inspect.
    pub run_id: Option<String>,
    /// Whether to fork caller state/messages into the new run.
    #[serde(default)]
    pub fork_context: bool,
    /// true: run in background; false: wait for completion.
    #[serde(default)]
    pub background: bool,
}

/// Normalize optional string: trim whitespace and treat empty as None.
fn normalize_opt(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Task type used when registering sub-agent background runs with [`BackgroundTaskManager`].
pub(crate) const AGENT_RUN_TASK_TYPE: &str = "agent_run";

#[derive(Debug, Clone)]
struct PersistedAgentRunRecord {
    agent_id: String,
    thread_id: String,
    status: TaskStatus,
    error: Option<String>,
}

impl PersistedAgentRunRecord {
    fn from_task_state(task: crate::runtime::background_tasks::TaskState) -> Self {
        Self {
            agent_id: task
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            thread_id: task
                .metadata
                .get("thread_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            status: task.status,
            error: task.error,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentRunTool {
    os: AgentOs,
    bg_manager: Arc<BackgroundTaskManager>,
}

impl AgentRunTool {
    pub fn new(os: AgentOs, bg_manager: Arc<BackgroundTaskManager>) -> Self {
        Self { os, bg_manager }
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

    fn task_store(&self) -> Option<TaskStore> {
        self.os.agent_state_store().cloned().map(TaskStore::new)
    }

    async fn persist_task_start(
        &self,
        task_store: &TaskStore,
        run_id: &str,
        owner_thread_id: &str,
        description: &str,
        parent_task_id: Option<&str>,
        metadata: &Value,
    ) -> Result<(), TaskStoreError> {
        match task_store.load_task(run_id).await? {
            Some(task) => {
                if task.owner_thread_id != owner_thread_id {
                    return Err(TaskStoreError::OwnerMismatch {
                        task_id: run_id.to_string(),
                        expected_owner_thread_id: owner_thread_id.to_string(),
                        actual_owner_thread_id: task.owner_thread_id,
                    });
                }
                task_store.start_task_attempt(run_id).await?;
            }
            None => {
                task_store
                    .create_task(NewTaskSpec {
                        task_id: run_id.to_string(),
                        owner_thread_id: owner_thread_id.to_string(),
                        task_type: AGENT_RUN_TASK_TYPE.to_string(),
                        description: description.to_string(),
                        parent_task_id: parent_task_id.map(str::to_string),
                        supports_resume: true,
                        metadata: metadata.clone(),
                    })
                    .await?;
            }
        }
        Ok(())
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
        let parent_tool_call_id = ctx.call_id().to_string();
        let metadata = serde_json::json!({
            "thread_id": child_thread_id,
            "agent_id": agent_id,
        });
        let description = format!("agent:{agent_id}");
        let task_store = self.task_store();

        if background {
            let token = RunCancellationToken::new();
            let parent_run_id_bg = parent_run_id.clone();
            let parent_tool_call_id_bg = parent_tool_call_id.clone();

            if let Some(task_store) = &task_store {
                if let Err(err) = self
                    .persist_task_start(
                        task_store,
                        &run_id,
                        &owner_thread_id,
                        &description,
                        parent_run_id.as_deref(),
                        &metadata,
                    )
                    .await
                {
                    return tool_error(
                        tool_name,
                        "task_persist_failed",
                        format!("failed to persist task start: {err}"),
                    );
                }
            }

            // Spawn via BackgroundTaskManager for unified tracking.
            let os = self.os.clone();
            let run_id_bg = run_id.clone();
            let agent_id_bg = agent_id.clone();
            let child_thread_id_bg = child_thread_id.clone();
            let parent_thread_id_bg = owner_thread_id.clone();

            self.bg_manager
                .spawn_with_id(
                    SpawnParams {
                        task_id: run_id.clone(),
                        owner_thread_id,
                        task_type: AGENT_RUN_TASK_TYPE.to_string(),
                        description,
                        parent_task_id: parent_run_id.clone(),
                        metadata,
                    },
                    token.clone(),
                    move |_cancel| async move {
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

                        match completion.status {
                            SubAgentStatus::Completed => BgTaskResult::Success(serde_json::json!({
                                "run_id": run_id_bg,
                                "status": "completed"
                            })),
                            SubAgentStatus::Failed => {
                                BgTaskResult::Failed(completion.error.unwrap_or_default())
                            }
                            SubAgentStatus::Stopped => BgTaskResult::Stopped,
                            SubAgentStatus::Running => {
                                BgTaskResult::Success(serde_json::json!({ "run_id": run_id_bg }))
                            }
                        }
                    },
                )
                .await;

            return agent_tool_result(tool_name, &run_id, &agent_id, "running", None);
        }

        // Foreground run.
        let forward_progress =
            |update: crate::contracts::runtime::tool_call::ToolCallProgressUpdate| {
                ctx.report_tool_call_progress(update)
            };
        let owner_thread_id_for_exec = owner_thread_id.clone();

        let completion = execute_sub_agent(
            self.os.clone(),
            SubAgentExecutionRequest {
                agent_id: agent_id.clone(),
                child_thread_id: child_thread_id.clone(),
                run_id: run_id.clone(),
                parent_run_id: parent_run_id.clone(),
                parent_tool_call_id: Some(parent_tool_call_id),
                parent_thread_id: owner_thread_id_for_exec,
                messages,
                initial_state,
                cancellation_token: None,
            },
            Some(&forward_progress),
        )
        .await;

        let status = match completion.status {
            SubAgentStatus::Completed => TaskStatus::Completed,
            SubAgentStatus::Failed => TaskStatus::Failed,
            SubAgentStatus::Stopped => TaskStatus::Stopped,
            SubAgentStatus::Running => TaskStatus::Running,
        };

        if let Some(task_store) = &task_store {
            if let Err(err) = self
                .persist_task_start(
                    task_store,
                    &run_id,
                    &owner_thread_id,
                    &description,
                    parent_run_id.as_deref(),
                    &metadata,
                )
                .await
            {
                return tool_error(
                    tool_name,
                    "task_persist_failed",
                    format!("failed to persist task start: {err}"),
                );
            }

            if let Err(err) = task_store
                .persist_foreground_result(&run_id, status, completion.error.clone(), None)
                .await
            {
                return tool_error(
                    tool_name,
                    "task_persist_failed",
                    format!("failed to persist task outcome: {err}"),
                );
            }
        }

        agent_tool_result(
            tool_name,
            &run_id,
            &agent_id,
            status.as_str(),
            completion.error.as_deref(),
        )
    }
}

#[async_trait]
impl crate::contracts::runtime::tool_call::TypedTool for AgentRunTool {
    type Args = AgentRunArgs;

    fn tool_id(&self) -> &str {
        AGENT_RUN_TOOL_ID
    }
    fn name(&self) -> &str {
        "Agent Run"
    }
    fn description(&self) -> &str {
        "Run or resume a registry agent; can run in background"
    }

    async fn execute(
        &self,
        args: AgentRunArgs,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let tool_name = AGENT_RUN_TOOL_ID;
        let run_id = normalize_opt(args.run_id);
        let prompt = normalize_opt(args.prompt);
        let background = args.background;
        let fork_context = args.fork_context;

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
        let task_store = self.task_store();

        // ── Resume existing run by ID ──────────────────────────────
        if let Some(run_id) = run_id {
            // 1. Check live task in BackgroundTaskManager.
            if let Some(summary) = self.bg_manager.get(&owner_thread_id, &run_id).await {
                let agent_id = summary
                    .metadata
                    .get("agent_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let thread_id = summary
                    .metadata
                    .get("thread_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                match summary.status {
                    TaskStatus::Running
                    | TaskStatus::Completed
                    | TaskStatus::Failed
                    | TaskStatus::Cancelled => {
                        return Ok(agent_tool_result(
                            tool_name,
                            &run_id,
                            &agent_id,
                            summary.status.as_str(),
                            summary.error.as_deref(),
                        ));
                    }
                    TaskStatus::Stopped => {
                        // Resume stopped task.
                        if let Err(error) = self.ensure_target_visible(
                            &agent_id,
                            caller_agent_id.as_deref(),
                            Some(scope),
                        ) {
                            return Ok(error.into_tool_result(tool_name));
                        }

                        let mut messages = Vec::new();
                        if let Some(prompt) = prompt {
                            messages.push(Message::user(prompt));
                        }

                        return Ok(self
                            .launch_new_run(
                                ctx,
                                LaunchNewRunRequest {
                                    run_id,
                                    owner_thread_id,
                                    agent_id,
                                    parent_run_id: caller_run_id,
                                    child_thread_id: thread_id,
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

            let durable_persisted = if let Some(task_store) = task_store.clone() {
                match task_store
                    .load_task_for_owner(&owner_thread_id, &run_id)
                    .await
                {
                    Ok(Some(task)) => Some(task),
                    Ok(None) => None,
                    Err(err) => {
                        return Ok(tool_error(
                            tool_name,
                            "task_load_failed",
                            format!("Failed to load persisted run state: {err}"),
                        ));
                    }
                }
            } else {
                None
            };

            let durable_exists = durable_persisted.is_some();
            let persisted = durable_persisted.map(PersistedAgentRunRecord::from_task_state);

            let Some(persisted) = persisted else {
                return Ok(tool_error(
                    tool_name,
                    "unknown_run",
                    format!("Unknown run_id: {run_id}"),
                ));
            };

            if persisted.status == TaskStatus::Running {
                if durable_exists {
                    if let Some(task_store) = &task_store {
                        if let Err(err) = task_store
                            .persist_foreground_result(
                                &run_id,
                                TaskStatus::Stopped,
                                Some(
                                    "No live executor found in current process; marked stopped"
                                        .to_string(),
                                ),
                                None,
                            )
                            .await
                        {
                            return Ok(tool_error(
                                tool_name,
                                "task_persist_failed",
                                format!("Failed to mark orphaned task stopped: {err}"),
                            ));
                        }
                    }
                }
                return Ok(agent_tool_result(
                    tool_name,
                    &run_id,
                    &persisted.agent_id,
                    "stopped",
                    Some("No live executor found in current process; marked stopped"),
                ));
            }

            match persisted.status {
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled => {
                    return Ok(agent_tool_result(
                        tool_name,
                        &run_id,
                        &persisted.agent_id,
                        persisted.status.as_str(),
                        persisted.error.as_deref(),
                    ));
                }
                TaskStatus::Stopped => {
                    if let Err(error) = self.ensure_target_visible(
                        &persisted.agent_id,
                        caller_agent_id.as_deref(),
                        Some(scope),
                    ) {
                        return Ok(error.into_tool_result(tool_name));
                    }

                    let mut messages = Vec::new();
                    if let Some(prompt) = prompt {
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
                TaskStatus::Running => unreachable!("handled above"),
            }
        }

        // ── New run ────────────────────────────────────────────────
        let target_agent_id = normalize_opt(args.agent_id);
        let Some(target_agent_id) = target_agent_id else {
            return Ok(ToolArgError::new("invalid_arguments", "missing 'agent_id'")
                .into_tool_result(tool_name));
        };
        let Some(prompt) = prompt else {
            return Ok(
                ToolArgError::new("invalid_arguments", "missing 'prompt'")
                    .into_tool_result(tool_name),
            );
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
