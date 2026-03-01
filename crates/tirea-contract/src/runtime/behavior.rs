use crate::runtime::action::Action;
use crate::runtime::inference::response::{LLMResponse, StreamResult};
use crate::runtime::plugin::phase::step::StepContext;
use crate::runtime::plugin::phase::Phase;
use crate::runtime::tool_call::gate::ToolGate;
use crate::runtime::tool_call::{ToolCallResume, ToolResult};
use crate::thread::Message;
use crate::RunConfig;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tirea_state::{get_at_path, parse_path, DocCell, State, TireaResult};

/// Immutable snapshot of step context passed to [`AgentBehavior`] phase hooks.
///
/// The loop builds a `ReadOnlyContext` from the current `StepContext` before
/// each phase hook and passes it by shared reference. Agents read data from
/// this snapshot and return a `Vec<Box<dyn Action>>` describing effects to apply.
pub struct ReadOnlyContext<'a> {
    phase: Phase,
    thread_id: &'a str,
    messages: &'a [Arc<Message>],
    run_config: &'a RunConfig,
    doc: &'a DocCell,
    response: Option<&'a StreamResult>,
    tool_name: Option<&'a str>,
    tool_call_id: Option<&'a str>,
    tool_args: Option<&'a Value>,
    tool_result: Option<&'a ToolResult>,
    resume_input: Option<ToolCallResume>,
}

impl<'a> ReadOnlyContext<'a> {
    pub fn new(
        phase: Phase,
        thread_id: &'a str,
        messages: &'a [Arc<Message>],
        run_config: &'a RunConfig,
        doc: &'a DocCell,
    ) -> Self {
        Self {
            phase,
            thread_id,
            messages,
            run_config,
            doc,
            response: None,
            tool_name: None,
            tool_call_id: None,
            tool_args: None,
            tool_result: None,
            resume_input: None,
        }
    }

    #[must_use]
    pub fn with_response(mut self, response: &'a StreamResult) -> Self {
        self.response = Some(response);
        self
    }

    #[must_use]
    pub fn with_tool_info(
        mut self,
        name: &'a str,
        call_id: &'a str,
        args: Option<&'a Value>,
    ) -> Self {
        self.tool_name = Some(name);
        self.tool_call_id = Some(call_id);
        self.tool_args = args;
        self
    }

    #[must_use]
    pub fn with_tool_result(mut self, result: &'a ToolResult) -> Self {
        self.tool_result = Some(result);
        self
    }

    #[must_use]
    pub fn with_resume_input(mut self, resume: ToolCallResume) -> Self {
        self.resume_input = Some(resume);
        self
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn thread_id(&self) -> &str {
        self.thread_id
    }

    pub fn messages(&self) -> &[Arc<Message>] {
        self.messages
    }

    pub fn run_config(&self) -> &RunConfig {
        self.run_config
    }

    pub fn doc(&self) -> &DocCell {
        self.doc
    }

    pub fn config_value(&self, key: &str) -> Option<&Value> {
        self.run_config.value(key)
    }

    pub fn response(&self) -> Option<&StreamResult> {
        self.response
    }

    pub fn tool_name(&self) -> Option<&str> {
        self.tool_name
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id
    }

    pub fn tool_args(&self) -> Option<&Value> {
        self.tool_args
    }

    pub fn tool_result(&self) -> Option<&ToolResult> {
        self.tool_result
    }

    pub fn resume_input(&self) -> Option<&ToolCallResume> {
        self.resume_input.as_ref()
    }

    pub fn snapshot(&self) -> Value {
        self.doc.snapshot()
    }

    pub fn snapshot_of<T: State>(&self) -> TireaResult<T> {
        let val = self.doc.snapshot();
        let at = get_at_path(&val, &parse_path(T::PATH)).unwrap_or(&Value::Null);
        T::from_value(at)
    }
}

/// Behavioral abstraction for agent phase hooks.
///
/// Hooks receive an immutable [`ReadOnlyContext`] snapshot and return a
/// `Vec<Box<dyn Action>>` describing effects to apply. The loop engine
/// validates and applies these actions after each hook returns.
#[async_trait]
pub trait AgentBehavior: Send + Sync {
    fn id(&self) -> &str;

    fn behavior_ids(&self) -> Vec<&str> {
        vec![self.id()]
    }

    async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
        vec![]
    }

    async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
        vec![]
    }

    async fn before_inference(&self, _ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
        vec![]
    }

    async fn after_inference(&self, _ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
        vec![]
    }

    async fn before_tool_execute(&self, _ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
        vec![]
    }

    async fn after_tool_execute(&self, _ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
        vec![]
    }

    async fn step_end(&self, _ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
        vec![]
    }

    async fn run_end(&self, _ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
        vec![]
    }
}

/// A no-op behavior that returns empty action lists for all hooks.
pub struct NoOpBehavior;

#[async_trait]
impl AgentBehavior for NoOpBehavior {
    fn id(&self) -> &str {
        "noop"
    }
}

/// Build a [`ReadOnlyContext`] from step Extensions and doc state.
///
/// Extracts response, tool info, and resume_input from the step's Extensions.
pub fn build_read_only_context_from_step<'a>(
    phase: Phase,
    step: &'a StepContext<'a>,
    doc: &'a DocCell,
) -> ReadOnlyContext<'a> {
    let mut ctx = ReadOnlyContext::new(
        phase,
        step.thread_id(),
        step.messages(),
        step.run_config(),
        doc,
    );
    if let Some(llm) = step.extensions.get::<LLMResponse>() {
        ctx = ctx.with_response(&llm.result);
    }
    if let Some(gate) = step.extensions.get::<ToolGate>() {
        ctx = ctx.with_tool_info(&gate.name, &gate.id, Some(&gate.args));
        if let Some(result) = gate.result.as_ref() {
            ctx = ctx.with_tool_result(result);
        }
    }
    if phase == Phase::BeforeToolExecute {
        if let Some(call_id) = step.tool_call_id() {
            if let Ok(Some(resume)) = step.ctx().resume_input_for(call_id) {
                ctx = ctx.with_resume_input(resume);
            }
        }
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestSystemContext as AddSystemContext;
    use serde_json::json;

    #[tokio::test]
    async fn default_agent_all_phases_noop() {
        let agent = NoOpBehavior;
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::RunStart, "t1", &[], &config, &doc);

        let actions = agent.run_start(&ctx).await;
        assert!(actions.is_empty());

        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let actions = agent.before_inference(&ctx).await;
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn agent_returns_actions() {
        struct ContextBehavior;

        #[async_trait]
        impl AgentBehavior for ContextBehavior {
            fn id(&self) -> &str {
                "ctx"
            }
            async fn before_inference(&self, _ctx: &ReadOnlyContext<'_>) -> Vec<Box<dyn Action>> {
                vec![Box::new(AddSystemContext("from agent".into()))]
            }
        }

        let agent = ContextBehavior;
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions = agent.before_inference(&ctx).await;
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].label(), "add_system_context");
    }

    #[tokio::test]
    async fn read_only_context_accessors() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({"key": "val"}));
        let ctx = ReadOnlyContext::new(Phase::AfterToolExecute, "thread_42", &[], &config, &doc);

        assert_eq!(ctx.phase(), Phase::AfterToolExecute);
        assert_eq!(ctx.thread_id(), "thread_42");
        assert!(ctx.messages().is_empty());
        assert!(ctx.tool_name().is_none());
        assert!(ctx.tool_result().is_none());
        assert!(ctx.response().is_none());
        assert!(ctx.resume_input().is_none());

        let snapshot = ctx.snapshot();
        assert_eq!(snapshot["key"], "val");
    }

    #[tokio::test]
    async fn read_only_context_with_tool_info() {
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let args = json!({"x": 1});
        let ctx = ReadOnlyContext::new(Phase::BeforeToolExecute, "t1", &[], &config, &doc)
            .with_tool_info("my_tool", "call_1", Some(&args));

        assert_eq!(ctx.tool_name(), Some("my_tool"));
        assert_eq!(ctx.tool_call_id(), Some("call_1"));
        assert_eq!(ctx.tool_args().unwrap()["x"], 1);
    }
}
