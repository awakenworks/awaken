use async_trait::async_trait;
use std::collections::HashSet;
use tirea_agentos::contracts::runtime::behavior::ReadOnlyContext;
use tirea_agentos::contracts::runtime::phase::{ActionSet, BeforeToolExecuteAction, SuspendTicket};
use tirea_agentos::contracts::runtime::tool_call::{
    PendingToolCall, ResumeDecisionAction, ToolCallResumeMode, ToolResult,
};
use tirea_agentos::contracts::runtime::AgentBehavior;

pub struct FrontendToolPlugin {
    tools: HashSet<&'static str>,
}

impl FrontendToolPlugin {
    pub fn new() -> Self {
        let tools = HashSet::from(["askUserQuestion", "set_background_color"]);
        Self { tools }
    }
}

#[async_trait]
impl AgentBehavior for FrontendToolPlugin {
    fn id(&self) -> &str {
        "frontend_tools"
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        let Some(tool_name) = ctx.tool_name() else {
            return ActionSet::empty();
        };
        if !self.tools.contains(tool_name) {
            return ActionSet::empty();
        }

        if let Some(resume) = ctx.resume_input() {
            let result = match resume.action {
                ResumeDecisionAction::Resume => {
                    ToolResult::success(tool_name.to_string(), resume.result.clone())
                }
                ResumeDecisionAction::Cancel => ToolResult::error(
                    tool_name.to_string(),
                    resume
                        .reason
                        .clone()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| "User denied the action".to_string()),
                ),
            };
            return ActionSet::single(BeforeToolExecuteAction::SetToolResult(result));
        }

        let Some(call_id) = ctx.tool_call_id().map(str::to_string) else {
            return ActionSet::empty();
        };
        let args = ctx.tool_args().cloned().unwrap_or_default();
        let suspension = tirea_agentos::contracts::Suspension::new(
            &call_id,
            format!("tool:{tool_name}"),
        )
        .with_parameters(args.clone());

        ActionSet::single(BeforeToolExecuteAction::Suspend(SuspendTicket::new(
            suspension,
            PendingToolCall::new(call_id, tool_name.to_string(), args),
            ToolCallResumeMode::UseDecisionAsToolResult,
        )))
    }
}
