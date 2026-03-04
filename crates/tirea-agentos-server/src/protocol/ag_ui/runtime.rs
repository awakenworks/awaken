//! Runtime wiring for AG-UI requests.
//!
//! Applies AG-UI–specific extensions to a [`ResolvedRun`]:
//! frontend tool descriptor stubs, frontend suspended-call strategy,
//! and context injection.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tirea_agentos::orchestrator::compose_behaviors;
use tirea_agentos::runtime::loop_runner::{
    BaseAgent, ParallelToolExecutor, ResolvedRun, SequentialToolExecutor,
};
use tirea_contract::runtime::behavior::ReadOnlyContext;
use tirea_contract::runtime::phase::{
    ActionSet, BeforeInferenceAction, BeforeToolExecuteAction, SuspendTicket,
};
use tirea_contract::runtime::tool_call::{Tool, ToolDescriptor, ToolError, ToolResult};
use tirea_contract::runtime::AgentBehavior;
use tirea_contract::runtime::{PendingToolCall, ToolCallResumeMode};
use tirea_contract::ToolCallContext;

use tirea_protocol_ag_ui::{build_context_addendum, RunAgentInput};

/// Run-config key carrying AG-UI request `config`.
pub const AGUI_CONFIG_KEY: &str = "agui_config";
/// Run-config key carrying AG-UI request `forwardedProps`.
pub const AGUI_FORWARDED_PROPS_KEY: &str = "agui_forwarded_props";

/// Apply AG-UI–specific extensions to a [`ResolvedRun`].
///
/// Injects frontend tool stubs, suspended-call plugins, context
/// injection, and request model/config overrides.
pub fn apply_agui_extensions(
    resolved: &mut ResolvedRun,
    request: &RunAgentInput,
) -> Result<(), tirea_contract::RunConfigError> {
    if let Some(model) = request.model.as_ref().filter(|m| !m.trim().is_empty()) {
        resolved.agent.model = model.clone();
    }
    if let Some(system_prompt) = request
        .system_prompt
        .as_ref()
        .filter(|prompt| !prompt.trim().is_empty())
    {
        resolved.agent.system_prompt = system_prompt.clone();
    }
    if let Some(config) = request.config.clone() {
        apply_agui_tool_execution_mode_override(resolved, &config);
        apply_agui_chat_options_overrides(resolved, &config);
        resolved.run_config.set(AGUI_CONFIG_KEY, config)?;
    }
    if let Some(forwarded_props) = request.forwarded_props.clone() {
        resolved
            .run_config
            .set(AGUI_FORWARDED_PROPS_KEY, forwarded_props)?;
    }

    let frontend_defs = request.frontend_tools();
    let frontend_tool_names: HashSet<String> =
        frontend_defs.iter().map(|tool| tool.name.clone()).collect();

    // Frontend tools → insert into resolved.tools (overlay semantics)
    for tool in frontend_defs {
        let stub = Arc::new(FrontendToolStub::new(
            tool.name.clone(),
            tool.description.clone(),
            tool.parameters.clone(),
        ));
        let id = stub.descriptor().id.clone();
        resolved.tools.entry(id).or_insert(stub as Arc<dyn Tool>);
    }

    // Run-scoped behaviors
    if !frontend_tool_names.is_empty() {
        add_behavior_mut(
            &mut resolved.agent,
            Arc::new(FrontendToolPendingPlugin::new(frontend_tool_names)),
        );
    }

    // Context injection: forward useCopilotReadable context to the agent's system prompt.
    if let Some(addendum) = build_context_addendum(request) {
        add_behavior_mut(
            &mut resolved.agent,
            Arc::new(ContextInjectionPlugin::new(addendum)),
        );
    }

    Ok(())
}

/// Add a behavior to a `BaseAgent` by reference, composing with any existing behavior.
fn add_behavior_mut(agent: &mut BaseAgent, behavior: Arc<dyn AgentBehavior>) {
    if agent.behavior.id() == "noop" {
        agent.behavior = behavior;
    } else {
        let id = format!("{}+{}", agent.behavior.id(), behavior.id());
        agent.behavior = compose_behaviors(id, vec![agent.behavior.clone(), behavior]);
    }
}

fn apply_agui_tool_execution_mode_override(resolved: &mut ResolvedRun, config: &Value) {
    let mode = config
        .get("toolExecutionMode")
        .or_else(|| config.get("tool_execution_mode"))
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase());

    match mode.as_deref() {
        Some("sequential") => {
            resolved.agent.tool_executor = Arc::new(SequentialToolExecutor);
        }
        Some("parallel_batch_approval") => {
            resolved.agent.tool_executor = Arc::new(ParallelToolExecutor::batch_approval());
        }
        Some("parallel_streaming") => {
            resolved.agent.tool_executor = Arc::new(ParallelToolExecutor::streaming());
        }
        _ => {}
    }
}

fn apply_agui_chat_options_overrides(resolved: &mut ResolvedRun, config: &Value) {
    let mut chat_options = resolved.agent.chat_options.clone().unwrap_or_default();
    let mut changed = false;

    if let Some(map) = config.as_object() {
        if let Some(value) = get_bool(map, "captureReasoningContent", "capture_reasoning_content") {
            chat_options.capture_reasoning_content = Some(value);
            changed = true;
        }
        if let Some(value) = get_bool(
            map,
            "normalizeReasoningContent",
            "normalize_reasoning_content",
        ) {
            chat_options.normalize_reasoning_content = Some(value);
            changed = true;
        }
    }

    if let Some(map) = config
        .get("chatOptions")
        .and_then(Value::as_object)
        .or_else(|| config.get("chat_options").and_then(Value::as_object))
    {
        if let Some(value) = get_bool(map, "captureReasoningContent", "capture_reasoning_content") {
            chat_options.capture_reasoning_content = Some(value);
            changed = true;
        }
        if let Some(value) = get_bool(
            map,
            "normalizeReasoningContent",
            "normalize_reasoning_content",
        ) {
            chat_options.normalize_reasoning_content = Some(value);
            changed = true;
        }
    }

    if changed {
        resolved.agent.chat_options = Some(chat_options);
    }
}

fn get_bool(map: &serde_json::Map<String, Value>, primary: &str, alias: &str) -> Option<bool> {
    map.get(primary)
        .or_else(|| map.get(alias))
        .and_then(Value::as_bool)
}

/// Runtime-only frontend tool descriptor stub.
///
/// The frontend pending plugin intercepts configured frontend tools before
/// backend execution. This stub exists only to expose tool descriptors to the model.
struct FrontendToolStub {
    descriptor: ToolDescriptor,
}

impl FrontendToolStub {
    fn new(name: String, description: String, parameters: Option<Value>) -> Self {
        let mut descriptor = ToolDescriptor::new(&name, &name, description);
        if let Some(parameters) = parameters {
            descriptor = descriptor.with_parameters(parameters);
        }
        Self { descriptor }
    }
}

#[async_trait]
impl Tool for FrontendToolStub {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::error(
            &self.descriptor.id,
            "frontend tool stub should be intercepted before backend execution",
        ))
    }
}

/// Run-scoped plugin that injects AG-UI context (from `useCopilotReadable`)
/// into the agent's system prompt before inference.
struct ContextInjectionPlugin {
    addendum: String,
}

impl ContextInjectionPlugin {
    fn new(addendum: String) -> Self {
        Self { addendum }
    }
}

#[async_trait]
impl AgentBehavior for ContextInjectionPlugin {
    fn id(&self) -> &str {
        "agui_context_injection"
    }

    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        ActionSet::single(BeforeInferenceAction::AddSystemContext(
            self.addendum.clone(),
        ))
    }
}

/// Run-scoped frontend interaction strategy for AG-UI.
struct FrontendToolPendingPlugin {
    frontend_tools: HashSet<String>,
}

impl FrontendToolPendingPlugin {
    fn new(frontend_tools: HashSet<String>) -> Self {
        Self { frontend_tools }
    }
}

#[async_trait]
impl AgentBehavior for FrontendToolPendingPlugin {
    fn id(&self) -> &str {
        "agui_frontend_tools"
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        let Some(tool_name) = ctx.tool_name() else {
            return ActionSet::empty();
        };
        if !self.frontend_tools.contains(tool_name) {
            return ActionSet::empty();
        }

        if let Some(resume) = ctx.resume_input() {
            let result = match resume.action {
                tirea_contract::io::ResumeDecisionAction::Resume => {
                    ToolResult::success(tool_name.to_string(), resume.result.clone())
                }
                tirea_contract::io::ResumeDecisionAction::Cancel => ToolResult::error(
                    tool_name.to_string(),
                    resume
                        .reason
                        .clone()
                        .filter(|r| !r.trim().is_empty())
                        .unwrap_or_else(|| "User denied the action".to_string()),
                ),
            };
            return ActionSet::single(BeforeToolExecuteAction::SetToolResult(result));
        }
        let Some(call_id) = ctx.tool_call_id().map(str::to_string) else {
            return ActionSet::empty();
        };

        let args = ctx.tool_args().cloned().unwrap_or_default();
        let suspension = tirea_contract::Suspension::new(&call_id, format!("tool:{tool_name}"))
            .with_parameters(args.clone());
        ActionSet::single(BeforeToolExecuteAction::Suspend(SuspendTicket::new(
            suspension,
            PendingToolCall::new(call_id, tool_name.to_string(), args),
            ToolCallResumeMode::UseDecisionAsToolResult,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tirea_agentos::runtime::loop_runner::BaseAgent;
    use tirea_contract::runtime::phase::{Phase, StepContext};
    use tirea_contract::runtime::tool_call::ToolGate;
    use tirea_contract::testing::TestFixture;
    use tirea_contract::thread::ToolCall;
    use tirea_protocol_ag_ui::{Context, Message, ToolExecutionLocation};

    fn empty_resolved() -> ResolvedRun {
        ResolvedRun {
            agent: BaseAgent::default(),
            tools: HashMap::new(),
            run_config: tirea_contract::RunConfig::new(),
        }
    }

    fn build_read_only_ctx<'a>(phase: Phase, step: &'a StepContext<'a>) -> ReadOnlyContext<'a> {
        let mut ctx = ReadOnlyContext::new(
            phase,
            step.thread_id(),
            step.messages(),
            step.run_config(),
            step.ctx().doc(),
        );
        if let Some(gate) = step.gate.as_ref() {
            ctx = ctx.with_tool_info(&gate.name, &gate.id, Some(&gate.args));
            if let Some(result) = gate.result.as_ref() {
                ctx = ctx.with_tool_result(result);
            }
        }
        if let Some(call_id) = step.tool_call_id() {
            if let Ok(Some(resume)) = step.ctx().resume_input_for(call_id) {
                ctx = ctx.with_resume_input(resume);
            }
        }
        ctx
    }

    struct MarkerPlugin;

    #[async_trait]
    impl AgentBehavior for MarkerPlugin {
        fn id(&self) -> &str {
            "marker_plugin"
        }
    }

    #[test]
    fn injects_frontend_tools_into_resolved() {
        let request = RunAgentInput {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            messages: vec![Message::user("hello")],
            tools: vec![
                tirea_protocol_ag_ui::Tool {
                    name: "copyToClipboard".to_string(),
                    description: "copy".to_string(),
                    parameters: Some(json!({
                        "type": "object",
                        "properties": {
                            "text": { "type": "string" }
                        },
                        "required": ["text"]
                    })),
                    execute: ToolExecutionLocation::Frontend,
                },
                tirea_protocol_ag_ui::Tool::backend("search", "backend search"),
            ],
            context: vec![],
            state: None,
            parent_run_id: None,
            parent_thread_id: None,
            model: None,
            system_prompt: None,
            config: None,
            forwarded_props: None,
        };

        let mut resolved = empty_resolved();
        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");
        assert_eq!(resolved.agent.tool_executor.name(), "parallel_streaming");
        assert!(resolved.tools.contains_key("copyToClipboard"));
        // Only 1 frontend tool (backend tools are not stubs)
        assert_eq!(resolved.tools.len(), 1);
        // FrontendToolPendingPlugin behavior
        assert!(resolved.agent.behavior.id().contains("agui_frontend_tools"));
    }

    #[test]
    fn no_behaviors_added_when_only_decisions_are_present() {
        let request = RunAgentInput {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            messages: vec![
                Message::user("hello"),
                Message::tool("true", "interaction_1"),
            ],
            tools: vec![],
            context: vec![],
            state: None,
            parent_run_id: None,
            parent_thread_id: None,
            model: None,
            system_prompt: None,
            config: None,
            forwarded_props: None,
        };

        let mut resolved = empty_resolved();
        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");
        assert!(resolved.tools.is_empty());
        assert_eq!(resolved.agent.behavior.id(), "noop");
    }

    #[test]
    fn no_behaviors_added_for_non_boolean_decision_payload_without_frontend_tools() {
        let request = RunAgentInput {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            messages: vec![
                Message::user("hello"),
                Message::tool(r#"{"todo":"ship starter"}"#, "call_copy_1"),
            ],
            tools: vec![],
            context: vec![],
            state: Some(json!({
                "__suspended_tool_calls": {
                    "calls": {
                        "call_copy_1": {
                            "call_id": "call_copy_1",
                            "tool_name": "copyToClipboard",
                            "suspension": {
                                "id": "call_copy_1",
                                "action": "tool:copyToClipboard"
                            },
                            "arguments": {},
                            "pending": {
                                "id": "call_copy_1",
                                "name": "copyToClipboard",
                                "arguments": {}
                            },
                            "resume_mode": "use_decision_as_tool_result"
                        }
                    }
                }
            })),
            parent_run_id: None,
            parent_thread_id: None,
            model: None,
            system_prompt: None,
            config: None,
            forwarded_props: None,
        };

        let mut resolved = empty_resolved();
        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");
        assert!(resolved.tools.is_empty());
        assert_eq!(resolved.agent.behavior.id(), "noop");
    }

    #[test]
    fn injects_frontend_behavior_even_when_decisions_are_present() {
        let request = RunAgentInput {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            messages: vec![Message::user("hello"), Message::tool("true", "call_1")],
            tools: vec![tirea_protocol_ag_ui::Tool {
                name: "copyToClipboard".to_string(),
                description: "copy".to_string(),
                parameters: None,
                execute: ToolExecutionLocation::Frontend,
            }],
            context: vec![],
            state: None,
            parent_run_id: None,
            parent_thread_id: None,
            model: None,
            system_prompt: None,
            config: None,
            forwarded_props: None,
        };

        let mut resolved = empty_resolved();
        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");
        assert!(resolved.tools.contains_key("copyToClipboard"));
        // FrontendToolPendingPlugin behavior only
        assert_eq!(resolved.agent.behavior.id(), "agui_frontend_tools");
    }

    #[test]
    fn composes_frontend_pending_behavior_with_existing_behavior() {
        let request = RunAgentInput {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            messages: vec![Message::user("hello")],
            tools: vec![tirea_protocol_ag_ui::Tool {
                name: "copyToClipboard".to_string(),
                description: "copy".to_string(),
                parameters: None,
                execute: ToolExecutionLocation::Frontend,
            }],
            context: vec![],
            state: None,
            parent_run_id: None,
            parent_thread_id: None,
            model: None,
            system_prompt: None,
            config: None,
            forwarded_props: None,
        };

        let mut resolved = empty_resolved();
        add_behavior_mut(&mut resolved.agent, Arc::new(MarkerPlugin));

        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");

        let behavior_id = resolved.agent.behavior.id();
        assert!(
            behavior_id.contains("agui_frontend_tools"),
            "behavior should contain frontend tools, got: {behavior_id}"
        );
        assert!(
            behavior_id.contains("marker_plugin"),
            "behavior should contain marker plugin, got: {behavior_id}"
        );
    }

    #[test]
    fn no_changes_without_frontend_or_response_data() {
        let request = RunAgentInput::new("t1", "r1").with_message(Message::user("hello"));
        let mut resolved = empty_resolved();
        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");
        assert_eq!(resolved.agent.tool_executor.name(), "parallel_streaming");
        assert!(resolved.tools.is_empty());
        assert_eq!(resolved.agent.behavior.id(), "noop");
    }

    #[tokio::test]
    async fn frontend_pending_plugin_marks_frontend_call_as_pending() {
        let plugin =
            FrontendToolPendingPlugin::new(["copyToClipboard".to_string()].into_iter().collect());
        let fixture = TestFixture::new();
        let mut step = fixture.step(vec![]);
        let call = ToolCall::new("call_1", "copyToClipboard", json!({"text":"hello"}));
        step.gate = Some(ToolGate::from_tool_call(&call));

        let ctx = build_read_only_ctx(Phase::BeforeToolExecute, &step);
        let actions = plugin.before_tool_execute(&ctx).await;
        tirea_contract::testing::apply_before_tool_for_test(&mut step, actions);

        let gate = step.gate.as_ref().expect("ToolGate should exist");
        assert!(gate.pending, "should be pending (suspended)");
        let ticket = gate
            .suspend_ticket
            .as_ref()
            .expect("should have SuspendTool ticket");
        assert_eq!(ticket.suspension.action, "tool:copyToClipboard");
        assert_eq!(ticket.pending.id, "call_1");
        assert_eq!(ticket.pending.name, "copyToClipboard");
        assert_eq!(ticket.pending.arguments["text"], "hello");
        assert_eq!(
            ticket.resume_mode,
            tirea_contract::runtime::ToolCallResumeMode::UseDecisionAsToolResult
        );
    }

    #[tokio::test]
    async fn frontend_pending_plugin_resume_sets_tool_result() {
        let plugin =
            FrontendToolPendingPlugin::new(["copyToClipboard".to_string()].into_iter().collect());
        let fixture = TestFixture::new_with_state(json!({
            "__tool_call_scope": {
                "call_1": {
                    "tool_call_state": {
                        "call_id": "call_1",
                        "tool_name": "copyToClipboard",
                        "arguments": { "text": "hello" },
                        "status": "resuming",
                        "resume_token": "call_1",
                        "resume": {
                            "decision_id": "d1",
                            "action": "resume",
                            "result": { "accepted": true },
                            "updated_at": 1
                        },
                        "scratch": null,
                        "updated_at": 1
                    }
                }
            }
        }));
        let mut step = fixture.step(vec![]);
        let call = ToolCall::new("call_1", "copyToClipboard", json!({"text":"hello"}));
        step.gate = Some(ToolGate::from_tool_call(&call));

        let ctx = build_read_only_ctx(Phase::BeforeToolExecute, &step);
        let actions = plugin.before_tool_execute(&ctx).await;
        tirea_contract::testing::apply_before_tool_for_test(&mut step, actions);

        let gate = step.gate.as_ref().expect("ToolGate should exist");
        assert!(!gate.blocked, "should not be blocked");
        assert!(!gate.pending, "should not be pending");
        let result = gate
            .result
            .as_ref()
            .expect("resume should produce OverrideToolResult");
        assert_eq!(result.tool_name, "copyToClipboard");
        assert_eq!(
            result.status,
            tirea_contract::runtime::tool_call::ToolStatus::Success
        );
        assert_eq!(result.data, json!({"accepted": true}));
    }

    #[tokio::test]
    async fn frontend_pending_plugin_cancel_sets_error_result() {
        let plugin =
            FrontendToolPendingPlugin::new(["copyToClipboard".to_string()].into_iter().collect());
        let fixture = TestFixture::new_with_state(json!({
            "__tool_call_scope": {
                "call_1": {
                    "tool_call_state": {
                        "call_id": "call_1",
                        "tool_name": "copyToClipboard",
                        "arguments": { "text": "hello" },
                        "status": "resuming",
                        "resume_token": "call_1",
                        "resume": {
                            "decision_id": "d1",
                            "action": "cancel",
                            "result": null,
                            "reason": "user denied",
                            "updated_at": 1
                        },
                        "scratch": null,
                        "updated_at": 1
                    }
                }
            }
        }));
        let mut step = fixture.step(vec![]);
        let call = ToolCall::new("call_1", "copyToClipboard", json!({"text":"hello"}));
        step.gate = Some(ToolGate::from_tool_call(&call));

        let ctx = build_read_only_ctx(Phase::BeforeToolExecute, &step);
        let actions = plugin.before_tool_execute(&ctx).await;
        tirea_contract::testing::apply_before_tool_for_test(&mut step, actions);

        let gate = step.gate.as_ref().expect("ToolGate should exist");
        assert!(!gate.blocked, "should not be blocked");
        assert!(!gate.pending, "should not be pending");
        let result = gate
            .result
            .as_ref()
            .expect("cancel should produce OverrideToolResult");
        assert_eq!(
            result.status,
            tirea_contract::runtime::tool_call::ToolStatus::Error
        );
        assert_eq!(result.message.as_deref(), Some("user denied"));
    }

    #[test]
    fn injects_context_injection_behavior_when_context_present() {
        let request = RunAgentInput {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            messages: vec![Message::user("hello")],
            tools: vec![],
            context: vec![Context {
                description: "Current tasks".to_string(),
                value: json!(["Review PR", "Write tests"]),
            }],
            state: None,
            parent_run_id: None,
            parent_thread_id: None,
            model: None,
            system_prompt: None,
            config: None,
            forwarded_props: None,
        };

        let mut resolved = empty_resolved();
        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");
        assert!(resolved
            .agent
            .behavior
            .id()
            .contains("agui_context_injection"));
    }

    #[tokio::test]
    async fn context_injection_behavior_adds_system_context() {
        let request = RunAgentInput {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            messages: vec![Message::user("hello")],
            tools: vec![],
            context: vec![Context {
                description: "Task list".to_string(),
                value: json!(["Review PR", "Write tests"]),
            }],
            state: None,
            parent_run_id: None,
            parent_thread_id: None,
            model: None,
            system_prompt: None,
            config: None,
            forwarded_props: None,
        };

        let mut resolved = empty_resolved();
        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");
        let behavior = &resolved.agent.behavior;

        let fixture = TestFixture::new();
        let mut step = fixture.step(vec![]);
        let ctx = build_read_only_ctx(Phase::BeforeInference, &step);

        let actions = behavior.before_inference(&ctx).await;
        tirea_contract::testing::apply_before_inference_for_test(&mut step, actions);

        assert!(!step.inference.system_context.is_empty());
        let merged = step.inference.system_context.join("\n");
        assert!(
            merged.contains("Task list"),
            "should contain context description"
        );
        assert!(
            merged.contains("Review PR"),
            "should contain context values"
        );
    }

    #[test]
    fn no_context_injection_behavior_when_context_empty() {
        let request = RunAgentInput::new("t1", "r1").with_message(Message::user("hello"));
        let mut resolved = empty_resolved();
        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");
        assert!(!resolved
            .agent
            .behavior
            .id()
            .contains("agui_context_injection"));
    }

    #[test]
    fn applies_request_model_and_system_prompt_overrides() {
        let request = RunAgentInput::new("t1", "r1")
            .with_message(Message::user("hello"))
            .with_model("gpt-4.1")
            .with_system_prompt("You are precise.");

        let mut resolved = empty_resolved();
        resolved.agent.model = "base-model".to_string();
        resolved.agent.system_prompt = "base-prompt".to_string();

        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");

        assert_eq!(resolved.agent.model, "gpt-4.1");
        assert_eq!(resolved.agent.system_prompt, "You are precise.");
    }

    #[test]
    fn writes_config_and_forwarded_props_into_run_config() {
        let request = RunAgentInput::new("t1", "r1")
            .with_message(Message::user("hello"))
            .with_state(json!({"k":"v"}))
            .with_forwarded_props(json!({"session":"abc"}));
        let mut request = request;
        request.config = Some(json!({"temperature": 0.2}));

        let mut resolved = empty_resolved();
        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");

        assert_eq!(
            resolved.run_config.value(AGUI_CONFIG_KEY),
            Some(&json!({"temperature": 0.2}))
        );
        assert_eq!(
            resolved.run_config.value(AGUI_FORWARDED_PROPS_KEY),
            Some(&json!({"session":"abc"}))
        );
    }

    #[test]
    fn returns_error_when_config_key_is_already_set() {
        let mut request = RunAgentInput::new("t1", "r1").with_message(Message::user("hello"));
        request.config = Some(json!({"temperature": 0.2}));

        let mut resolved = empty_resolved();
        resolved
            .run_config
            .set(AGUI_CONFIG_KEY, json!({"temperature": 0.1}))
            .expect("preset run config");

        let err = apply_agui_extensions(&mut resolved, &request)
            .expect_err("duplicate AG-UI config key should fail");
        assert!(matches!(
            err,
            tirea_contract::RunConfigError::AlreadySet(ref key) if key == AGUI_CONFIG_KEY
        ));
    }

    #[test]
    fn applies_chat_options_overrides_from_agui_config() {
        let mut request = RunAgentInput::new("t1", "r1").with_message(Message::user("hello"));
        request.config = Some(json!({
            "captureReasoningContent": true,
            "normalizeReasoningContent": true,
            "reasoningEffort": "high"
        }));

        let mut resolved = empty_resolved();
        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");

        let options = resolved
            .agent
            .chat_options
            .expect("chat options should exist");
        assert_eq!(options.capture_reasoning_content, Some(true));
        assert_eq!(options.normalize_reasoning_content, Some(true));
        assert_eq!(
            resolved.run_config.value(AGUI_CONFIG_KEY),
            Some(&json!({
                "captureReasoningContent": true,
                "normalizeReasoningContent": true,
                "reasoningEffort": "high"
            }))
        );
    }

    #[test]
    fn applies_tool_execution_mode_override_from_agui_config() {
        let mut request = RunAgentInput::new("t1", "r1").with_message(Message::user("hello"));
        request.config = Some(json!({
            "toolExecutionMode": "parallel_batch_approval"
        }));

        let mut resolved = empty_resolved();
        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");

        assert_eq!(
            resolved.agent.tool_executor.name(),
            "parallel_batch_approval"
        );
    }

    #[test]
    fn applies_nested_chat_options_overrides_from_agui_config() {
        let mut request = RunAgentInput::new("t1", "r1").with_message(Message::user("hello"));
        request.config = Some(json!({
            "chat_options": {
                "capture_reasoning_content": false,
                "reasoning_effort": 256
            }
        }));

        let mut resolved = empty_resolved();
        apply_agui_extensions(&mut resolved, &request).expect("apply AG-UI extensions");

        let options = resolved
            .agent
            .chat_options
            .expect("chat options should exist");
        assert_eq!(options.capture_reasoning_content, Some(false));
        assert_eq!(options.normalize_reasoning_content, None);
    }
}
