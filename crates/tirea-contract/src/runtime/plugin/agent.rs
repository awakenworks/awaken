use crate::runtime::llm::StreamResult;
use crate::runtime::plugin::phase::effect::PhaseOutput;
use crate::runtime::plugin::phase::Phase;
use crate::runtime::tool_call::{ToolCallResume, ToolResult};
use crate::thread::Message;
use crate::RunConfig;
use async_trait::async_trait;
use serde_json::Value;
use std::any::TypeId;
use std::collections::HashSet;
use std::sync::Arc;
use tirea_state::{get_at_path, parse_path, DocCell, State, TireaResult};

/// Immutable snapshot of step context passed to [`Agent`] phase hooks.
///
/// The loop builds a `ReadOnlyContext` from the current `StepContext` before
/// each phase hook and passes it by shared reference. Agents read data from
/// this snapshot and return a [`PhaseOutput`] describing declarative effects.
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
    resume_input: Option<&'a ToolCallResume>,
}

impl<'a> ReadOnlyContext<'a> {
    /// Create a new read-only context with the minimum required fields.
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

    // -- builder methods for optional fields --

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
    pub fn with_resume_input(mut self, resume: &'a ToolCallResume) -> Self {
        self.resume_input = Some(resume);
        self
    }

    // -- accessors --

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
        self.resume_input
    }

    // -- state access --

    /// Raw state snapshot from the document.
    pub fn snapshot(&self) -> Value {
        self.doc.snapshot()
    }

    /// Typed state snapshot at the canonical path for `T`.
    pub fn snapshot_of<T: State>(&self) -> TireaResult<T> {
        let val = self.doc.snapshot();
        let at = get_at_path(&val, &parse_path(T::PATH)).unwrap_or(&Value::Null);
        T::from_value(at)
    }
}

/// Behavioral abstraction for agent phase hooks.
///
/// `AgentBehavior` defines the phase-hook interface that the agent loop
/// dispatches during each run lifecycle phase. Hooks receive an immutable
/// [`ReadOnlyContext`] snapshot and return a [`PhaseOutput`] describing
/// declarative effects to apply. The loop engine validates and applies
/// these effects after each hook returns.
///
/// # Composition
///
/// Multiple `AgentBehavior` implementations can be composed, merging
/// their [`PhaseOutput`]s. All sub-behaviors see the same
/// [`ReadOnlyContext`] snapshot — effects are applied after all hooks
/// return.
#[async_trait]
pub trait AgentBehavior: Send + Sync {
    /// Unique identifier for this agent.
    fn id(&self) -> &str;

    /// State types owned by this agent (used by composite behaviors for union).
    fn owned_states(&self) -> HashSet<TypeId> {
        HashSet::new()
    }

    /// Return the ordered list of leaf behavior IDs.
    ///
    /// For simple behaviors this returns a single-element vec of `self.id()`.
    /// Composite implementations override this to return the IDs of all
    /// child behaviors.
    fn behavior_ids(&self) -> Vec<&str> {
        vec![self.id()]
    }

    async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        PhaseOutput::default()
    }

    async fn step_start(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        PhaseOutput::default()
    }

    async fn before_inference(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        PhaseOutput::default()
    }

    async fn after_inference(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        PhaseOutput::default()
    }

    async fn before_tool_execute(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        PhaseOutput::default()
    }

    async fn after_tool_execute(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        PhaseOutput::default()
    }

    async fn step_end(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        PhaseOutput::default()
    }

    async fn run_end(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        PhaseOutput::default()
    }
}

/// A no-op behavior that returns empty [`PhaseOutput`] for all hooks.
///
/// Used as the default behavior in configurations where no custom behavior is set.
pub struct NoOpBehavior;

#[async_trait]
impl AgentBehavior for NoOpBehavior {
    fn id(&self) -> &str {
        "noop"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::plugin::phase::effect::PhaseEffect;
    use serde_json::json;

    #[tokio::test]
    async fn default_agent_all_phases_noop() {
        struct NoOpBehavior;

        #[async_trait]
        impl AgentBehavior for NoOpBehavior {
            fn id(&self) -> &str {
                "noop"
            }
        }

        let agent = NoOpBehavior;
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::RunStart, "t1", &[], &config, &doc);

        let output = agent.run_start(&ctx).await;
        assert!(output.is_empty());

        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let output = agent.before_inference(&ctx).await;
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn agent_returns_declarative_effects() {
        struct ContextBehavior;

        #[async_trait]
        impl AgentBehavior for ContextBehavior {
            fn id(&self) -> &str {
                "ctx"
            }
            async fn before_inference(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
                PhaseOutput::new().system_context("from agent")
            }
        }

        let agent = ContextBehavior;
        let config = RunConfig::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let output = agent.before_inference(&ctx).await;
        assert_eq!(output.effects.len(), 1);
        assert!(matches!(&output.effects[0], PhaseEffect::SystemContext(s) if s == "from agent"));
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
