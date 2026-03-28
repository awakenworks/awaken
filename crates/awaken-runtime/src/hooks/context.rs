use std::sync::Arc;

use serde_json::Value;

use crate::cancellation::CancellationToken;
use crate::state::{Snapshot, StateKey};
use awaken_contract::StateError;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::inference::LLMResponse;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::tool::ToolResult;
use awaken_contract::model::Phase;
use awaken_contract::registry_spec::{AgentSpec, PluginConfigKey};

/// Execution context passed to phase hooks and action handlers.
///
/// Three input sources per ADR-0009:
/// - `agent_spec`: immutable agent configuration (model, active_hook_filter, sections)
/// - `snapshot`: shared runtime state (StateKeys)
/// - `run_identity`: per-run identity (thread_id, run_id, etc.)
#[derive(Clone)]
pub struct PhaseContext {
    pub phase: Phase,
    pub snapshot: Snapshot,

    /// Active agent spec (resolved from registry at each phase boundary).
    pub agent_spec: Arc<AgentSpec>,

    /// Per-run identity (thread_id, run_id, etc.). Immutable for the run.
    pub run_identity: RunIdentity,

    /// Messages accumulated in the current run.
    pub messages: Arc<[Arc<Message>]>,

    // Tool-call context (set during BeforeToolExecute / AfterToolExecute)
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_args: Option<Value>,
    pub tool_result: Option<ToolResult>,

    // LLM response (set during AfterInference)
    pub llm_response: Option<LLMResponse>,

    // Resume decision (set during BeforeToolExecute when resuming a suspended tool call)
    pub resume_input: Option<awaken_contract::contract::suspension::ToolCallResume>,

    /// Optional cancellation token for cooperative cancellation at phase boundaries.
    pub cancellation_token: Option<CancellationToken>,

    /// Optional profile access for cross-run persistence.
    pub profile_access: Option<Arc<crate::profile::ProfileAccess>>,
}

impl PhaseContext {
    /// Create a minimal context (for testing or phases without extra data).
    pub fn new(phase: Phase, snapshot: Snapshot) -> Self {
        Self {
            phase,
            snapshot,
            agent_spec: Arc::new(AgentSpec::default()),
            run_identity: RunIdentity::default(),
            messages: Arc::from([]),
            tool_name: None,
            tool_call_id: None,
            tool_args: None,
            tool_result: None,
            llm_response: None,
            resume_input: None,
            cancellation_token: None,
            profile_access: None,
        }
    }

    /// Read a state key from the snapshot.
    pub fn state<K: StateKey>(&self) -> Option<&K::Value> {
        self.snapshot.get::<K>()
    }

    /// Read a typed plugin config from the active agent spec.
    /// Returns `Config::default()` if the section is missing.
    pub fn config<K: PluginConfigKey>(&self) -> Result<K::Config, StateError> {
        self.agent_spec.config::<K>()
    }

    // -- Builder methods --

    #[must_use]
    pub fn with_snapshot(mut self, snapshot: Snapshot) -> Self {
        self.snapshot = snapshot;
        self
    }

    #[must_use]
    pub fn with_agent_spec(mut self, spec: Arc<AgentSpec>) -> Self {
        self.agent_spec = spec;
        self
    }

    #[must_use]
    pub fn with_run_identity(mut self, identity: RunIdentity) -> Self {
        self.run_identity = identity;
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

    #[must_use]
    pub fn with_resume_input(
        mut self,
        resume: awaken_contract::contract::suspension::ToolCallResume,
    ) -> Self {
        self.resume_input = Some(resume);
        self
    }

    #[must_use]
    pub fn with_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }

    /// Get profile access, if configured.
    pub fn profile(&self) -> Option<&crate::profile::ProfileAccess> {
        self.profile_access.as_deref()
    }

    #[must_use]
    pub fn with_profile_access(mut self, access: Arc<crate::profile::ProfileAccess>) -> Self {
        self.profile_access = Some(access);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateMap;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::identity::RunOrigin;
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::contract::tool::ToolResult;

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
        assert_eq!(ctx.agent_spec.id, "");
    }

    #[test]
    fn phase_context_with_agent_spec() {
        let spec = Arc::new(
            AgentSpec::new("reviewer")
                .with_model("opus")
                .with_hook_filter("perm"),
        );
        let ctx = PhaseContext::new(Phase::RunStart, empty_snapshot()).with_agent_spec(spec);
        assert_eq!(ctx.agent_spec.id, "reviewer");
        assert_eq!(ctx.agent_spec.model, "opus");
        assert!(ctx.agent_spec.active_hook_filter.contains("perm"));
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
        assert_eq!(ctx.run_identity.thread_id, "t1");
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
            content: vec![ContentBlock::text("hello")],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
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

        assert_eq!(ctx.run_identity.thread_id, "t1");
        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.tool_name.as_deref(), Some("calc"));
        assert!(ctx.tool_result.is_some());
    }

    #[test]
    fn phase_context_profile_none_by_default() {
        let ctx = PhaseContext::new(Phase::RunStart, empty_snapshot());
        assert!(ctx.profile().is_none());
    }
}
