use std::collections::HashSet;

use async_trait::async_trait;

use awaken_contract::StateError;
use awaken_contract::contract::suspension::{
    PendingToolCall, SuspendTicket, Suspension, ToolCallResumeMode,
};
use awaken_contract::contract::tool::ToolResult;
use awaken_contract::contract::tool_intercept::ToolInterceptPayload;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::{PhaseContext, ToolGateHook};

const FRONTEND_TOOLS_PLUGIN_NAME: &str = "frontend_tools";

/// Plugin that intercepts the askUserQuestion tool call and suspends
/// execution so the frontend can collect user input and send it back.
///
/// `set_background_color` is NOT intercepted — it is registered as a
/// FrontEndTool via the AI SDK `tools` array and executes immediately.
/// The frontend renders its own color picker UI and submits the result
/// via `addToolOutput` / `sendAutomaticallyWhen`.
pub struct FrontendToolPlugin {
    tools: HashSet<&'static str>,
}

impl FrontendToolPlugin {
    pub fn new() -> Self {
        let tools = HashSet::from(["askUserQuestion", "set_background_color"]);
        Self { tools }
    }
}

impl Default for FrontendToolPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for FrontendToolPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: FRONTEND_TOOLS_PLUGIN_NAME,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_tool_gate_hook(
            FRONTEND_TOOLS_PLUGIN_NAME,
            FrontendToolInterceptHook {
                tools: self.tools.clone(),
            },
        )?;
        Ok(())
    }
}

struct FrontendToolInterceptHook {
    tools: HashSet<&'static str>,
}

#[async_trait]
impl ToolGateHook for FrontendToolInterceptHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<Option<ToolInterceptPayload>, StateError> {
        let tool_name = match &ctx.tool_name {
            Some(name) => name.as_str(),
            None => return Ok(None),
        };

        if !self.tools.contains(tool_name) {
            return Ok(None);
        }

        // If resuming after frontend response, use the decision as tool result
        if let Some(resume) = &ctx.resume_input {
            use awaken_contract::contract::suspension::ResumeDecisionAction;
            let result = match resume.action {
                ResumeDecisionAction::Resume => {
                    ToolResult::success(tool_name, resume.result.clone())
                }
                ResumeDecisionAction::Cancel => ToolResult::error(
                    tool_name,
                    resume
                        .reason
                        .clone()
                        .filter(|v| !v.trim().is_empty())
                        .unwrap_or_else(|| "User denied the action".to_string()),
                ),
            };
            return Ok(Some(ToolInterceptPayload::SetResult(result)));
        }

        // First encounter: suspend for frontend handling
        let call_id = ctx.tool_call_id.as_deref().unwrap_or_default().to_string();
        let args = ctx.tool_args.clone().unwrap_or_default();

        let ticket = SuspendTicket::new(
            Suspension {
                id: call_id.clone(),
                action: format!("tool:{tool_name}"),
                message: String::new(),
                parameters: args.clone(),
                ..Default::default()
            },
            PendingToolCall::new(&call_id, tool_name, args),
            ToolCallResumeMode::UseDecisionAsToolResult,
        );

        Ok(Some(ToolInterceptPayload::Suspend(ticket)))
    }
}
