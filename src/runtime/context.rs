use std::sync::Arc;

use serde_json::Value;

use crate::contract::identity::RunIdentity;
use crate::contract::inference::LLMResponse;
use crate::contract::message::Message;
use crate::contract::profile::{AgentProfile, RunInput};
use crate::contract::tool::ToolResult;
use crate::model::Phase;
use crate::state::{Snapshot, StateKey};

/// Execution context passed to phase hooks and action handlers.
///
/// Three input sources per ADR-0009:
/// - `profile`: immutable agent configuration (model, active_plugins, sections)
/// - `snapshot`: shared runtime state (StateKeys)
/// - `run_input`: per-run caller input (overrides, identity)
#[derive(Clone)]
pub struct PhaseContext {
    pub phase: Phase,
    pub snapshot: Snapshot,

    /// Active agent profile (resolved from registry at each phase boundary).
    pub profile: Arc<AgentProfile>,

    /// Per-run caller input (overrides, identity). Immutable for the run.
    pub run_input: Arc<RunInput>,

    /// Messages accumulated in the current run.
    pub messages: Arc<[Arc<Message>]>,

    // Tool-call context (set during BeforeToolExecute / AfterToolExecute)
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_args: Option<Value>,
    pub tool_result: Option<ToolResult>,

    // LLM response (set during AfterInference)
    pub llm_response: Option<LLMResponse>,
}

impl PhaseContext {
    /// Create a minimal context (for testing or phases without extra data).
    pub fn new(phase: Phase, snapshot: Snapshot) -> Self {
        Self {
            phase,
            snapshot,
            profile: Arc::new(AgentProfile::default()),
            run_input: Arc::new(RunInput::default()),
            messages: Arc::from([]),
            tool_name: None,
            tool_call_id: None,
            tool_args: None,
            tool_result: None,
            llm_response: None,
        }
    }

    /// Read a state key from the snapshot.
    pub fn state<K: StateKey>(&self) -> Option<&K::Value> {
        self.snapshot.get::<K>()
    }

    // -- Builder methods --

    #[must_use]
    pub fn with_snapshot(mut self, snapshot: Snapshot) -> Self {
        self.snapshot = snapshot;
        self
    }

    #[must_use]
    pub fn with_profile(mut self, profile: Arc<AgentProfile>) -> Self {
        self.profile = profile;
        self
    }

    #[must_use]
    pub fn with_run_input(mut self, run_input: Arc<RunInput>) -> Self {
        self.run_input = run_input;
        self
    }

    #[must_use]
    pub fn with_run_identity(mut self, identity: RunIdentity) -> Self {
        Arc::make_mut(&mut self.run_input).identity = identity;
        self
    }

    #[must_use]
    pub fn with_messages(mut self, messages: Vec<Arc<Message>>) -> Self {
        self.messages = Arc::from(messages);
        self
    }

    #[must_use]
    pub fn with_tool_info(
        mut self,
        name: impl Into<String>,
        call_id: impl Into<String>,
        args: Option<Value>,
    ) -> Self {
        self.tool_name = Some(name.into());
        self.tool_call_id = Some(call_id.into());
        self.tool_args = args;
        self
    }

    #[must_use]
    pub fn with_tool_result(mut self, result: ToolResult) -> Self {
        self.tool_result = Some(result);
        self
    }

    #[must_use]
    pub fn with_llm_response(mut self, response: LLMResponse) -> Self {
        self.llm_response = Some(response);
        self
    }
}

// Backward compat alias
impl PhaseContext {
    pub fn get<K: StateKey>(&self) -> Option<&K::Value> {
        self.state::<K>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::identity::RunOrigin;
    use crate::contract::inference::{StopReason, StreamResult, TokenUsage};
    use crate::contract::tool::ToolResult;
    use crate::state::StateMap;

    fn empty_snapshot() -> Snapshot {
        Snapshot::new(0, std::sync::Arc::new(StateMap::default()))
    }

    #[test]
    fn phase_context_new_has_defaults() {
        let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
        assert_eq!(ctx.phase, Phase::BeforeInference);
        assert!(ctx.messages.is_empty());
        assert!(ctx.tool_name.is_none());
        assert!(ctx.llm_response.is_none());
        assert_eq!(ctx.profile.id, "");
    }

    #[test]
    fn phase_context_with_profile() {
        let profile = Arc::new(
            AgentProfile::new("reviewer")
                .with_model("opus")
                .with_active_plugin("perm"),
        );
        let ctx = PhaseContext::new(Phase::RunStart, empty_snapshot()).with_profile(profile);
        assert_eq!(ctx.profile.id, "reviewer");
        assert_eq!(ctx.profile.model.as_deref(), Some("opus"));
        assert!(ctx.profile.active_plugins.contains("perm"));
    }

    #[test]
    fn phase_context_with_run_input() {
        let input = Arc::new(RunInput {
            model_override: Some("gpt-4o-mini".into()),
            identity: RunIdentity::new(
                "t1".into(),
                None,
                "r1".into(),
                None,
                "agent".into(),
                RunOrigin::User,
            ),
            ..Default::default()
        });
        let ctx = PhaseContext::new(Phase::RunStart, empty_snapshot()).with_run_input(input);
        assert_eq!(ctx.run_input.model_override.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(ctx.run_input.identity.thread_id, "t1");
    }

    #[test]
    fn phase_context_with_run_identity() {
        let ctx = PhaseContext::new(Phase::RunStart, empty_snapshot()).with_run_identity(
            RunIdentity::new(
                "t1".into(),
                None,
                "r1".into(),
                None,
                "a".into(),
                RunOrigin::User,
            ),
        );
        assert_eq!(ctx.run_input.identity.thread_id, "t1");
    }

    #[test]
    fn phase_context_with_messages() {
        let msgs = vec![
            Arc::new(Message::user("hello")),
            Arc::new(Message::assistant("hi")),
        ];
        let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot()).with_messages(msgs);
        assert_eq!(ctx.messages.len(), 2);
    }

    #[test]
    fn phase_context_with_tool_info() {
        let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
            "search",
            "c1",
            Some(serde_json::json!({"q": "rust"})),
        );
        assert_eq!(ctx.tool_name.as_deref(), Some("search"));
        assert_eq!(ctx.tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn phase_context_with_tool_result() {
        let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot()).with_tool_result(
            ToolResult::success("search", serde_json::json!({"hits": 5})),
        );
        assert!(ctx.tool_result.as_ref().unwrap().is_success());
    }

    #[test]
    fn phase_context_with_llm_response() {
        let response = LLMResponse::success(StreamResult {
            text: "hello".into(),
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
        });
        let ctx =
            PhaseContext::new(Phase::AfterInference, empty_snapshot()).with_llm_response(response);
        assert!(ctx.llm_response.as_ref().unwrap().outcome.is_ok());
    }

    #[test]
    fn phase_context_builder_chains() {
        let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
            .with_run_identity(RunIdentity::for_thread("t1"))
            .with_messages(vec![Arc::new(Message::user("hi"))])
            .with_tool_info("calc", "c1", None)
            .with_tool_result(ToolResult::success("calc", serde_json::json!(42)));

        assert_eq!(ctx.run_input.identity.thread_id, "t1");
        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.tool_name.as_deref(), Some("calc"));
        assert!(ctx.tool_result.is_some());
    }
}
