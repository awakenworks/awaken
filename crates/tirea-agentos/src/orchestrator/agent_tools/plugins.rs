use super::state::current_unix_millis;
use super::*;
use crate::contracts::runtime::plugin::agent::{AgentBehavior, ReadOnlyContext};
use crate::contracts::runtime::plugin::phase::effect::PhaseOutput;
use crate::contracts::runtime::plugin::phase::{
    AfterToolExecuteContext, BeforeInferenceContext, PluginPhaseContext, RunStartContext,
};
use tirea_extension_permission::PermissionState;
pub struct AgentRecoveryPlugin {
    manager: Arc<AgentRunManager>,
}

impl AgentRecoveryPlugin {
    pub fn new(manager: Arc<AgentRunManager>) -> Self {
        Self { manager }
    }

    async fn on_run_start(&self, step: &mut RunStartContext<'_, '_>) {
        let state = step.snapshot();
        let mut runs = parse_persisted_runs_from_doc(&state);
        if runs.is_empty() {
            return;
        }

        let has_suspended_recovery = has_suspended_recovery_interaction(&state);

        let outcome =
            reconcile_persisted_runs(self.manager.as_ref(), step.thread_id(), &mut runs).await;
        if outcome.changed {
            let delegation = step.state_of::<DelegationState>();
            if delegation.set_runs(runs.clone()).is_err() {
                return;
            }
        }

        if has_suspended_recovery || outcome.orphaned_run_ids.is_empty() {
            return;
        }

        let run_id = outcome.orphaned_run_ids[0].clone();
        let Some(run) = runs.get(&run_id) else {
            return;
        };

        let behavior = {
            let state = step.state_of::<PermissionState>();
            if let Ok(tools) = state.tools() {
                if let Some(permission) = tools.get(AGENT_RECOVERY_INTERACTION_ACTION) {
                    *permission
                } else {
                    state.default_behavior().ok().unwrap_or_default()
                }
            } else {
                state.default_behavior().ok().unwrap_or_default()
            }
        };
        match behavior {
            ToolPermissionBehavior::Allow => {
                schedule_recovery_replay(step, &run_id, run);
            }
            ToolPermissionBehavior::Deny => {}
            ToolPermissionBehavior::Ask => {
                let interaction = build_recovery_interaction(&run_id, run);
                set_suspended_recovery_interaction(step, interaction);
            }
        }
    }
}

#[async_trait]
impl AgentPlugin for AgentRecoveryPlugin {
    fn id(&self) -> &str {
        AGENT_RECOVERY_PLUGIN_ID
    }

    async fn run_start(&self, ctx: &mut RunStartContext<'_, '_>) {
        self.on_run_start(ctx).await;
    }
}

#[async_trait]
impl AgentBehavior for AgentRecoveryPlugin {
    fn id(&self) -> &str {
        AGENT_RECOVERY_PLUGIN_ID
    }

    async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        use crate::contracts::runtime::plugin::phase::state_spec::AnyStateAction;
        use crate::contracts::runtime::{
            PendingToolCall, SuspendedCall, SuspendedToolCallsAction, SuspendedToolCallsState,
            ToolCallResumeMode, ToolCallState, ToolCallStatesAction, ToolCallStatesMap,
        };

        let state = ctx.snapshot();
        let mut runs = parse_persisted_runs_from_doc(&state);
        if runs.is_empty() {
            return PhaseOutput::default();
        }

        let has_suspended_recovery = has_suspended_recovery_interaction(&state);

        let outcome =
            reconcile_persisted_runs(self.manager.as_ref(), ctx.thread_id(), &mut runs).await;

        let mut output = PhaseOutput::default();

        if outcome.changed {
            output = output.with_state_action(AnyStateAction::new::<DelegationState>(
                DelegationAction::SetRuns(runs.clone()),
            ));
        }

        if has_suspended_recovery || outcome.orphaned_run_ids.is_empty() {
            return output;
        }

        let run_id = outcome.orphaned_run_ids[0].clone();
        let Some(run) = runs.get(&run_id) else {
            return output;
        };

        let behavior = ctx
            .snapshot_of::<PermissionState>()
            .ok()
            .map(|ps| {
                ps.tools
                    .get(AGENT_RECOVERY_INTERACTION_ACTION)
                    .copied()
                    .unwrap_or(ps.default_behavior)
            })
            .unwrap_or_default();

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
                ticket: crate::contracts::runtime::plugin::phase::SuspendTicket::new(
                    interaction.clone(),
                    pending,
                    ToolCallResumeMode::ReplayToolCall,
                ),
            }
        };

        match behavior {
            ToolPermissionBehavior::Allow => {
                let interaction = build_recovery_interaction(&run_id, run);
                let suspended_call = make_suspended_call(&interaction);
                let call_id = suspended_call.call_id.clone();
                let resume_token = suspended_call.ticket.pending.id.clone();
                let arguments = suspended_call.arguments.clone();

                output = output
                    .with_state_action(AnyStateAction::new::<SuspendedToolCallsState>(
                        SuspendedToolCallsAction::InsertCall {
                            call: suspended_call,
                        },
                    ))
                    .with_state_action(AnyStateAction::new::<ToolCallStatesMap>(
                        ToolCallStatesAction::InsertState {
                            state: ToolCallState {
                                call_id,
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
                            },
                        },
                    ));
            }
            ToolPermissionBehavior::Deny => {}
            ToolPermissionBehavior::Ask => {
                let interaction = build_recovery_interaction(&run_id, run);
                let suspended_call = make_suspended_call(&interaction);
                output = output.with_state_action(AnyStateAction::new::<
                    SuspendedToolCallsState,
                >(
                    SuspendedToolCallsAction::InsertCall {
                        call: suspended_call,
                    },
                ));
            }
        }
        output
    }
}

#[derive(Clone)]
pub struct AgentToolsPlugin {
    agents: Arc<dyn AgentRegistry>,
    manager: Arc<AgentRunManager>,
    max_entries: usize,
    max_chars: usize,
}

impl AgentToolsPlugin {
    pub fn new(agents: Arc<dyn AgentRegistry>, manager: Arc<AgentRunManager>) -> Self {
        Self {
            agents,
            manager,
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
        scope: Option<&tirea_contract::RunConfig>,
    ) -> String {
        let mut ids = self.agents.ids();
        ids.sort();
        if let Some(caller) = caller_agent {
            ids.retain(|id| id != caller);
        }
        ids.retain(|id| {
            is_scope_allowed(
                scope,
                id,
                SCOPE_ALLOWED_AGENTS_KEY,
                SCOPE_EXCLUDED_AGENTS_KEY,
            )
        });
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
        out.push_str("Run or resume: tool \"agent_run\" with {\"agent_id\":\"<id>\",\"prompt\":\"...\",\"fork_context\":false,\"background\":false}.\n");
        out.push_str("Resume existing run: tool \"agent_run\" with {\"run_id\":\"...\",\"prompt\":\"optional\",\"background\":false}.\n");
        out.push_str(
            "Stop running background run: tool \"agent_stop\" with {\"run_id\":\"...\"}.\n",
        );
        out.push_str("Statuses: running, completed, failed, stopped (stopped can be resumed).\n");
        out.push_str("</agent_tools_usage>");

        if out.len() > self.max_chars {
            out.truncate(self.max_chars);
        }

        out.trim_end().to_string()
    }

    async fn render_reminder(&self, thread_id: &str) -> Option<String> {
        let runs = self
            .manager
            .running_or_stopped_for_owner(thread_id)
            .await;
        if runs.is_empty() {
            return None;
        }

        let mut s = String::new();
        s.push_str("<agent_runs>\n");
        let total = runs.len();
        let mut shown = 0usize;
        for r in runs.into_iter().take(self.max_entries) {
            s.push_str(&format!(
                "<run id=\"{}\" agent=\"{}\" status=\"{}\"/>\n",
                r.run_id,
                r.target_agent_id,
                r.status.as_str(),
            ));
            shown += 1;
            if s.len() >= self.max_chars {
                break;
            }
        }
        s.push_str("</agent_runs>\n");
        if shown < total {
            s.push_str(&format!(
                "Note: agent_runs truncated (total={}, shown={}).\n",
                total, shown
            ));
        }
        s.push_str(
            "Use tool \"agent_run\" with run_id to resume/check, and \"agent_stop\" to stop running runs.",
        );
        if s.len() > self.max_chars {
            s.truncate(self.max_chars);
        }
        Some(s)
    }

    async fn maybe_reminder(&self, step: &mut AfterToolExecuteContext<'_, '_>) {
        if let Some(s) = self.render_reminder(step.thread_id()).await {
            step.add_system_reminder(s);
        }
    }
}

#[async_trait]
impl AgentPlugin for AgentToolsPlugin {
    fn id(&self) -> &str {
        AGENT_TOOLS_PLUGIN_ID
    }

    async fn before_inference(&self, step: &mut BeforeInferenceContext<'_, '_>) {
        let caller_agent = step
            .run_config()
            .value(SCOPE_CALLER_AGENT_ID_KEY)
            .and_then(|v| v.as_str());
        let rendered = self.render_available_agents(caller_agent, Some(step.run_config()));
        if !rendered.is_empty() {
            step.add_system_context(rendered);
        }
    }

    async fn after_tool_execute(&self, step: &mut AfterToolExecuteContext<'_, '_>) {
        // Inject system reminders after tool execution so the reminder is persisted
        // as internal-system history for subsequent turns.
        self.maybe_reminder(step).await;
    }
}

#[async_trait]
impl AgentBehavior for AgentToolsPlugin {
    fn id(&self) -> &str {
        AGENT_TOOLS_PLUGIN_ID
    }

    async fn before_inference(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let caller_agent = ctx
            .run_config()
            .value(SCOPE_CALLER_AGENT_ID_KEY)
            .and_then(|v| v.as_str());
        let rendered = self.render_available_agents(caller_agent, Some(ctx.run_config()));
        if rendered.is_empty() {
            PhaseOutput::default()
        } else {
            PhaseOutput::new().system_context(rendered)
        }
    }

    async fn after_tool_execute(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        match self.render_reminder(ctx.thread_id()).await {
            Some(s) => PhaseOutput::new().system_reminder(s),
            None => PhaseOutput::default(),
        }
    }
}
