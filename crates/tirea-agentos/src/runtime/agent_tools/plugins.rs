use super::state::current_unix_millis;
use super::*;
use crate::contracts::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use crate::contracts::runtime::phase::{ActionSet, BeforeInferenceAction, LifecycleAction};
use crate::runtime::background_tasks::{BackgroundTaskManager, TaskStatus, TaskStore};
#[cfg(feature = "permission")]
use tirea_extension_permission::resolve_permission_behavior;

#[cfg(not(feature = "permission"))]
fn resolve_permission_behavior(
    _state: &serde_json::Value,
    _action: &str,
) -> ToolPermissionBehavior {
    ToolPermissionBehavior::Allow
}

pub struct AgentRecoveryPlugin {
    bg_manager: Arc<BackgroundTaskManager>,
    task_store: Option<Arc<TaskStore>>,
}

impl AgentRecoveryPlugin {
    pub fn new(bg_manager: Arc<BackgroundTaskManager>) -> Self {
        Self {
            bg_manager,
            task_store: None,
        }
    }

    pub fn with_task_store(mut self, task_store: Option<Arc<TaskStore>>) -> Self {
        self.task_store = task_store;
        self
    }
}

#[async_trait]
impl AgentBehavior for AgentRecoveryPlugin {
    fn id(&self) -> &str {
        AGENT_RECOVERY_PLUGIN_ID
    }

    async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        use crate::contracts::runtime::{
            PendingToolCall, SuspendedCall, ToolCallResumeMode, ToolCallState,
        };
        let state = ctx.snapshot();
        let mut tasks = match &self.task_store {
            Some(task_store) => match task_store.list_tasks_for_owner(ctx.thread_id()).await {
                Ok(tasks) => tasks.into_iter().map(|task| task.summary()).collect(),
                Err(error) => {
                    tracing::warn!(
                        owner_thread_id = %ctx.thread_id(),
                        error = %error,
                        "failed to list persisted background tasks for recovery"
                    );
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        tasks.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.task_id.cmp(&b.task_id))
        });

        if tasks.is_empty() {
            return ActionSet::empty();
        }

        let has_suspended_recovery = has_suspended_recovery_interaction(&state);

        // Detect orphans: Running agent_run tasks in persisted state but no live handle.
        let mut orphaned_run_ids = Vec::new();
        let mut actions: ActionSet<LifecycleAction> = ActionSet::empty();

        for task in &tasks {
            if task.status != TaskStatus::Running {
                continue;
            }
            if task.task_type != AGENT_RUN_TOOL_ID {
                continue;
            }
            if !self.bg_manager.contains_any(&task.task_id).await {
                if let Some(task_store) = &self.task_store {
                    match task_store
                        .persist_foreground_result(
                            &task.task_id,
                            TaskStatus::Stopped,
                            Some(
                                "No live executor found in current process; marked stopped"
                                    .to_string(),
                            ),
                            None,
                        )
                        .await
                    {
                        Ok(()) => orphaned_run_ids.push(task.task_id.clone()),
                        Err(error) => {
                            tracing::warn!(
                                owner_thread_id = %ctx.thread_id(),
                                task_id = %task.task_id,
                                task_type = %task.task_type,
                                error = %error,
                                "failed to persist orphaned background task as stopped"
                            );
                        }
                    }
                }
            }
        }

        if has_suspended_recovery || orphaned_run_ids.is_empty() {
            return actions;
        }

        let run_id = orphaned_run_ids[0].clone();
        let Some(task) = tasks.iter().find(|task| task.task_id == run_id) else {
            return actions;
        };

        let agent_id = task
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let behavior = resolve_permission_behavior(&state, AGENT_RECOVERY_INTERACTION_ACTION);

        let make_suspended_call = |interaction: &Suspension| -> SuspendedCall {
            let call_id = interaction.id.clone();
            let call_arguments = interaction.parameters.clone();
            let pending = PendingToolCall::new(
                call_id.clone(),
                AGENT_RECOVERY_INTERACTION_ACTION,
                call_arguments.clone(),
            );
            SuspendedCall {
                call_id,
                tool_name: AGENT_RUN_TOOL_ID.to_string(),
                arguments: call_arguments,
                ticket: crate::contracts::runtime::phase::SuspendTicket::new(
                    interaction.clone(),
                    pending,
                    ToolCallResumeMode::ReplayToolCall,
                ),
            }
        };

        match behavior {
            ToolPermissionBehavior::Allow => {
                let interaction = build_recovery_interaction(&run_id, agent_id);
                let suspended_call = make_suspended_call(&interaction);
                let call_id = suspended_call.call_id.clone();
                let resume_token = suspended_call.ticket.pending.id.clone();
                let arguments = suspended_call.arguments.clone();

                actions = actions.and(LifecycleAction::State(
                    suspended_call.clone().into_state_action(),
                ));
                actions = actions.and(LifecycleAction::State(
                    ToolCallState {
                        call_id: call_id.clone(),
                        tool_name: AGENT_RUN_TOOL_ID.to_string(),
                        arguments,
                        status: crate::contracts::runtime::ToolCallStatus::Resuming,
                        resume_token: Some(resume_token),
                        resume: Some(crate::contracts::runtime::ToolCallResume {
                            decision_id: recovery_target_id(&run_id),
                            action: crate::contracts::io::ResumeDecisionAction::Resume,
                            result: serde_json::Value::Bool(true),
                            reason: None,
                            updated_at: current_unix_millis(),
                        }),
                        scratch: serde_json::Value::Null,
                        updated_at: current_unix_millis(),
                    }
                    .into_state_action(),
                ));
            }
            ToolPermissionBehavior::Deny => {}
            ToolPermissionBehavior::Ask => {
                let interaction = build_recovery_interaction(&run_id, agent_id);
                let suspended_call = make_suspended_call(&interaction);
                actions = actions.and(LifecycleAction::State(suspended_call.into_state_action()));
            }
        }
        actions
    }
}

#[derive(Clone)]
pub struct AgentToolsPlugin {
    agents: Arc<dyn AgentRegistry>,
    max_entries: usize,
    max_chars: usize,
}

impl AgentToolsPlugin {
    pub fn new(agents: Arc<dyn AgentRegistry>) -> Self {
        Self {
            agents,
            max_entries: 64,
            max_chars: 16 * 1024,
        }
    }

    pub fn with_limits(mut self, max_entries: usize, max_chars: usize) -> Self {
        self.max_entries = max_entries.max(1);
        self.max_chars = max_chars.max(256);
        self
    }

    pub(super) fn render_available_agents(
        &self,
        caller_agent: Option<&str>,
        policy: Option<&tirea_contract::runtime::RunPolicy>,
    ) -> String {
        let mut ids = self.agents.ids();
        ids.sort();
        if let Some(caller) = caller_agent {
            ids.retain(|id| id != caller);
        }
        ids.retain(|id| is_scope_allowed(policy, id, crate::contracts::scope::ScopeDomain::Agent));
        if ids.is_empty() {
            return String::new();
        }

        let total = ids.len();
        let mut out = String::new();
        out.push_str("<available_agents>\n");

        let mut shown = 0usize;
        for id in ids.into_iter().take(self.max_entries) {
            out.push_str("<agent>\n");
            out.push_str(&format!("<id>{}</id>\n", id));
            out.push_str("</agent>\n");
            shown += 1;
            if out.len() >= self.max_chars {
                break;
            }
        }

        out.push_str("</available_agents>\n");
        if shown < total {
            out.push_str(&format!(
                "Note: available_agents truncated (total={}, shown={}).\n",
                total, shown
            ));
        }

        out.push_str("<agent_tools_usage>\n");
        out.push_str("Run or resume: tool \"agent_run\" with {\"agent_id\":\"<id>\",\"prompt\":\"...\",\"fork_context\":false,\"run_in_background\":false}.\n");
        out.push_str("Resume existing run: tool \"agent_run\" with {\"run_id\":\"...\",\"prompt\":\"optional\",\"run_in_background\":false}.\n");
        out.push_str(
            "Check background run: tool \"task_status\" with {\"task_id\":\"<run_id>\"}.\n",
        );
        out.push_str(
            "Cancel background run: tool \"task_cancel\" with {\"task_id\":\"<run_id>\"}.\n",
        );
        out.push_str("Retrieve output: tool \"task_output\" with {\"task_id\":\"<run_id>\"}.\n");
        out.push_str(
            "Statuses: running, completed, failed, cancelled, stopped (stopped can be resumed).\n",
        );
        out.push_str("</agent_tools_usage>");

        if out.len() > self.max_chars {
            out.truncate(self.max_chars);
        }

        out.trim_end().to_string()
    }
}

#[async_trait]
impl AgentBehavior for AgentToolsPlugin {
    fn id(&self) -> &str {
        AGENT_TOOLS_PLUGIN_ID
    }

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let caller_agent = ctx.run_identity().agent_id_opt();
        let rendered = self.render_available_agents(caller_agent, Some(ctx.run_policy()));
        if rendered.is_empty() {
            ActionSet::empty()
        } else {
            ActionSet::single(BeforeInferenceAction::AddSystemContext(rendered))
        }
    }
}
