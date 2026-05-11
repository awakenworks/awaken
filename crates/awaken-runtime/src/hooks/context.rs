use std::sync::Arc;

use serde_json::Value;

use crate::cancellation::CancellationToken;
use crate::registry::RegistrySnapshot;
use crate::state::{Snapshot, StateKey};
use awaken_contract::StateError;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::inference::LLMResponse;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_contract::contract::tool::ToolResult;
use awaken_contract::contract::tool_intercept::{
    AdapterKind, RunMode, ToolKind, ToolPolicyContext,
};
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
    pub run_mode: RunMode,
    pub adapter: AdapterKind,
    pub tool_kind: ToolKind,

    // LLM response (set during AfterInference)
    pub llm_response: Option<LLMResponse>,

    // Resume decision (set during BeforeToolExecute when resuming a suspended tool call)
    pub resume_input: Option<ToolCallResume>,
    pub suspension_id: Option<String>,
    pub suspension_reason: Option<String>,

    /// Optional cancellation token for cooperative cancellation at phase boundaries.
    pub cancellation_token: Option<CancellationToken>,

    /// Optional profile access for cross-run persistence.
    pub profile_access: Option<Arc<crate::profile::ProfileAccess>>,

    /// Registry snapshot at the time this context was built. Populated by the
    /// runtime runner when a registry handle is present; `None` in minimal
    /// test contexts that don't carry a registry.
    pub registry_snapshot: Option<Arc<RegistrySnapshot>>,

    /// Content-addressed ids of the tools that were actually presented to
    /// the LLM on this turn (i.e. **after**
    /// `apply_tool_filter_payloads` and frontend-tool injection). Set by
    /// the loop runner immediately before the inference call so the
    /// `AfterInference` hook stamps the recorded `GenAISpan` with the
    /// real wire list instead of an approximation derived from
    /// `agent_spec.allowed_tools`. `None` in minimal test contexts and on
    /// phases other than `AfterInference`.
    pub effective_tool_ids: Option<Vec<String>>,
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
            run_mode: RunMode::Foreground,
            adapter: AdapterKind::Internal,
            tool_kind: ToolKind::Other,
            llm_response: None,
            resume_input: None,
            suspension_id: None,
            suspension_reason: None,
            cancellation_token: None,
            profile_access: None,
            registry_snapshot: None,
            effective_tool_ids: None,
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
        self.run_mode = identity.run_mode();
        self.adapter = identity.adapter();
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
        let name = name.into();
        self.tool_kind = ToolKind::infer_name(&name);
        self.tool_name = Some(name);
        self.tool_call_id = Some(call_id.into());
        self.tool_args = args;
        self
    }

    #[must_use]
    pub fn with_run_mode(mut self, mode: RunMode) -> Self {
        self.run_mode = mode;
        self
    }

    #[must_use]
    pub fn with_adapter(mut self, adapter: AdapterKind) -> Self {
        self.adapter = adapter;
        self
    }

    #[must_use]
    pub fn with_tool_kind(mut self, kind: ToolKind) -> Self {
        self.tool_kind = kind;
        self
    }

    /// Build typed policy context for ToolGate/ToolPolicy hooks.
    pub fn tool_policy_context(&self) -> Option<ToolPolicyContext> {
        Some(ToolPolicyContext {
            thread_id: self.run_identity.thread_id.clone(),
            run_id: self.run_identity.run_id.clone(),
            dispatch_id: self.run_identity.trace.dispatch_id.clone(),
            run_mode: self.run_mode,
            adapter: self.adapter,
            tool_name: self.tool_name.clone()?,
            tool_kind: self.tool_kind,
            arguments: self.tool_args.clone().unwrap_or(Value::Null),
        })
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
    pub fn with_resume_input(mut self, resume: ToolCallResume) -> Self {
        self.resume_input = Some(resume);
        self
    }

    #[must_use]
    pub fn with_suspension(
        mut self,
        suspension_id: Option<String>,
        suspension_reason: Option<String>,
    ) -> Self {
        self.suspension_id = suspension_id;
        self.suspension_reason = suspension_reason;
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

    /// Attach the registry snapshot active at the time this context was built.
    /// Hooks that need content-addressed tool/prompt ids read from this.
    #[must_use]
    pub fn with_registry_snapshot(mut self, snapshot: Arc<RegistrySnapshot>) -> Self {
        self.registry_snapshot = Some(snapshot);
        self
    }

    /// Attach the set of tool descriptor ids that were actually sent to the
    /// LLM on this turn. Used by the `AfterInference` observability hook
    /// to stamp `GenAISpan.context.tool_desc_ids` with the post-filter
    /// list, not the agent's pre-filter `allowed_tools`.
    #[must_use]
    pub fn with_effective_tool_ids(mut self, ids: Vec<String>) -> Self {
        self.effective_tool_ids = Some(ids);
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
                .with_model_id("opus")
                .with_hook_filter("perm"),
        );
        let ctx = PhaseContext::new(Phase::RunStart, empty_snapshot()).with_agent_spec(spec);
        assert_eq!(ctx.agent_spec.id, "reviewer");
        assert_eq!(ctx.agent_spec.model_id, "opus");
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
        assert_eq!(ctx.tool_kind, ToolKind::Search);
        let policy = ctx.tool_policy_context().expect("policy context");
        assert_eq!(policy.tool_name, "search");
        assert_eq!(policy.tool_kind, ToolKind::Search);
        assert_eq!(policy.arguments["q"], "rust");
    }

    #[test]
    fn phase_context_tool_policy_context_carries_trace_and_mode() {
        let identity = RunIdentity::new(
            "t1".into(),
            None,
            "r1".into(),
            None,
            "agent".into(),
            RunOrigin::User,
        )
        .with_dispatch_id("dispatch-1")
        .with_run_mode(RunMode::Scheduled)
        .with_adapter(AdapterKind::Acp);
        let ctx = PhaseContext::new(Phase::ToolGate, empty_snapshot())
            .with_run_identity(identity)
            .with_tool_info(
                "bash",
                "call-1",
                Some(serde_json::json!({"cmd": "echo ok"})),
            );

        let policy = ctx.tool_policy_context().expect("policy context");
        assert_eq!(policy.thread_id, "t1");
        assert_eq!(policy.run_id, "r1");
        assert_eq!(policy.dispatch_id.as_deref(), Some("dispatch-1"));
        assert_eq!(policy.run_mode, RunMode::Scheduled);
        assert_eq!(policy.adapter, AdapterKind::Acp);
        assert_eq!(policy.tool_kind, ToolKind::Execute);
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
