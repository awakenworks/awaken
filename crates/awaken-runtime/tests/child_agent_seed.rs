//! Integration tests for `run_child_agent` parent → child state seeding.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};

use awaken_contract::StateError;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event_sink::NullEventSink;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult};
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::message::{Message, Role, ToolCall};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::state::{PersistedState, StateKey, StateKeyOptions};

use awaken_runtime::backend::{
    BackendDelegateRunRequest, BackendParentContext, BackendRunResult, BackendRunStatus,
    ExecutionBackend, ExecutionBackendError,
};
use awaken_runtime::child_agent::{
    ChildAgentError, ChildAgentParams, run_child_agent, run_child_agent_checked,
};
use awaken_runtime::loop_runner::build_agent_env;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::registry::{AgentResolver, ResolvedAgent, ResolvedBackendAgent};
use awaken_runtime::resolution::ExecutionPlan;
use awaken_runtime::{RuntimeError, StateStore};

struct SeedKey;

impl StateKey for SeedKey {
    const KEY: &'static str = "test.seed_value";
    type Value = i64;
    type Update = i64;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }
}

struct SeedTestPlugin;

impl Plugin for SeedTestPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "seed-test-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<SeedKey>(StateKeyOptions {
            persistent: true,
            ..Default::default()
        })
    }
}

/// A tool that reads `SeedKey` from the child's `ToolCallContext.snapshot`
/// and reports the observed value as its tool result. Lets the test verify
/// the seed was visible to a tool *during* the child run, not just after.
struct ObserveSeedTool;

#[async_trait]
impl Tool for ObserveSeedTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("observe_seed", "observe_seed", "read SeedKey via ctx")
    }

    async fn execute(&self, _args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let observed = ctx.state::<SeedKey>().copied();
        Ok(ToolResult::success("observe_seed", json!({"observed": observed})).into())
    }
}

struct ScriptedLlm {
    responses: std::sync::Mutex<Vec<StreamResult>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<StreamResult>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmExecutor for ScriptedLlm {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let mut responses = self.responses.lock().expect("lock poisoned");
        Ok(responses.remove(0))
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

struct FixedResolver {
    agent: ResolvedAgent,
    plugins: Vec<Arc<dyn Plugin>>,
}

impl AgentResolver for FixedResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        let mut agent = self.agent.clone();
        agent.env = build_agent_env(&self.plugins, &agent)?;
        Ok(agent)
    }
}

fn make_resolver(llm: Arc<ScriptedLlm>) -> FixedResolver {
    let mut agent = ResolvedAgent::new("child", "m", "sys", llm).with_max_rounds(2);
    agent
        .tools
        .insert("observe_seed".into(), Arc::new(ObserveSeedTool));
    FixedResolver {
        agent,
        plugins: vec![Arc::new(SeedTestPlugin)],
    }
}

fn seed_with(value: i64) -> PersistedState {
    let mut extensions = std::collections::HashMap::new();
    extensions.insert(SeedKey::KEY.to_string(), json!(value));
    PersistedState {
        revision: 0,
        extensions,
    }
}

#[tokio::test]
async fn child_observes_seeded_state_via_tool_context() {
    let llm = Arc::new(ScriptedLlm::new(vec![
        // Step 1: child calls observe_seed
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("call-1", "observe_seed", json!({}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        // Step 2: child wraps up after observing
        StreamResult {
            content: vec![ContentBlock::text("done")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));
    let resolver = make_resolver(llm);

    let result = run_child_agent(
        ChildAgentParams::new(
            &resolver,
            "child",
            vec![Message::user("go")],
            BackendParentContext {
                parent_run_id: Some("parent-run".into()),
                parent_thread_id: Some("parent-thread".into()),
                parent_tool_call_id: Some("parent-call".into()),
            },
            Arc::new(NullEventSink),
        )
        .with_initial_state_seed(seed_with(42)),
    )
    .await
    .expect("child agent run should succeed");

    assert!(matches!(result.status, BackendRunStatus::Completed));

    // Final persisted state should round-trip the seed.
    let final_state = result.state.expect("local backend populates final state");
    let observed_json = final_state
        .extensions
        .get(SeedKey::KEY)
        .expect("seed key should round-trip through child final state");
    assert_eq!(observed_json, &json!(42));
}

#[tokio::test]
async fn missing_seed_yields_no_value_in_child() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("no seed")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));
    let resolver = make_resolver(llm);

    let result = run_child_agent(ChildAgentParams::new(
        &resolver,
        "child",
        vec![Message::user("hi")],
        BackendParentContext {
            parent_run_id: None,
            parent_thread_id: None,
            parent_tool_call_id: None,
        },
        Arc::new(NullEventSink),
    ))
    .await
    .expect("child run should succeed without a seed");

    assert!(matches!(result.status, BackendRunStatus::Completed));
    let final_state = result
        .state
        .expect("final state present even when unseeded");
    assert!(
        !final_state.extensions.contains_key(SeedKey::KEY),
        "unseeded key should not appear in final state"
    );
}

#[tokio::test]
async fn tool_round_trips_child_state_back_to_parent_store() {
    // Demonstrates the full developer pattern: a tool's `execute` seeds the
    // child with parent-derived state, runs it, decodes child terminal state,
    // and returns a `StateCommand` that the loop runner would commit to the
    // parent store. Here we commit it manually to a stand-in parent store
    // and assert it landed.

    use awaken_runtime::{MutationBatch, StateCommand, StateStore};

    // —— Parent-side plugin: registers a key the tool will write into ——
    struct ParentSummaryKey;
    impl StateKey for ParentSummaryKey {
        const KEY: &'static str = "test.parent_summary";
        type Value = String;
        type Update = String;
        fn apply(value: &mut Self::Value, update: Self::Update) {
            *value = update;
        }
    }
    struct ParentSummaryPlugin;
    impl Plugin for ParentSummaryPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "parent-summary-plugin",
            }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_key::<ParentSummaryKey>(StateKeyOptions {
                persistent: true,
                ..Default::default()
            })
        }
    }

    // —— Child runs once and ends naturally; the seed value flows through to
    //    its final persisted state ——
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("done")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));
    let resolver = make_resolver(llm);

    // —— Tool author code path begins here ——
    // ① Build the seed (typed) from "parent-derived" inputs.
    let seed_value: i64 = 7;
    let seed = seed_with(seed_value);

    // ② Run the child.
    let outcome = run_child_agent(
        ChildAgentParams::new(
            &resolver,
            "child",
            vec![Message::user("kickoff")],
            BackendParentContext::default(),
            Arc::new(NullEventSink),
        )
        .with_initial_state_seed(seed),
    )
    .await
    .expect("child run should succeed");
    assert!(matches!(outcome.status, BackendRunStatus::Completed));

    // ③ Read child terminal state, decode, build the parent StateCommand.
    let mut cmd = StateCommand::new();
    if matches!(outcome.status, BackendRunStatus::Completed)
        && let Some(state) = &outcome.state
        && let Some(json) = state.extensions.get(SeedKey::KEY)
    {
        let observed: i64 = serde_json::from_value(json.clone()).unwrap();
        let mut batch = MutationBatch::new();
        batch.update::<ParentSummaryKey>(format!("child observed {observed}"));
        cmd.patch.extend(batch).unwrap();
    }
    assert!(
        !cmd.is_empty(),
        "tool should have produced a non-empty StateCommand"
    );

    // —— Stand in for the loop runner: commit the StateCommand to a parent
    //    StateStore and read back the typed value. ——
    let parent_store = StateStore::new();
    parent_store.install_plugin(ParentSummaryPlugin).unwrap();
    parent_store.commit(cmd.patch).unwrap();

    assert_eq!(
        parent_store.read::<ParentSummaryKey>().as_deref(),
        Some("child observed 7"),
        "child's terminal state should round-trip into parent state",
    );
}

#[tokio::test]
async fn unknown_seed_key_fails_the_child_run() {
    let llm = Arc::new(ScriptedLlm::new(vec![]));
    let resolver = make_resolver(llm);

    // Build a seed referencing a key the child's plugins do not register.
    let mut bad_seed = std::collections::HashMap::new();
    bad_seed.insert(
        "no.such.key".to_string(),
        serde_json::json!("never decoded"),
    );
    let seed = PersistedState {
        revision: 0,
        extensions: bad_seed,
    };

    let err = run_child_agent(
        ChildAgentParams::new(
            &resolver,
            "child",
            vec![Message::user("doomed")],
            BackendParentContext::default(),
            Arc::new(NullEventSink),
        )
        .with_initial_state_seed(seed),
    )
    .await
    .expect_err("unknown seed key must surface as an error");

    let message = err.to_string();
    assert!(
        message.contains("no.such.key") || message.to_ascii_lowercase().contains("unknown"),
        "error should mention the unknown key: {message}"
    );
}

struct FailingExecutionResolver;

impl AgentResolver for FailingExecutionResolver {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        Err(RuntimeError::AgentNotFound {
            agent_id: agent_id.to_string(),
        })
    }

    fn resolve_execution(&self, _agent_id: &str) -> Result<ExecutionPlan, RuntimeError> {
        Err(RuntimeError::ResolveFailed {
            message: "registry backend timed out".into(),
        })
    }
}

#[tokio::test]
async fn child_agent_preserves_non_missing_resolver_errors() {
    let err = run_child_agent(ChildAgentParams::new(
        &FailingExecutionResolver,
        "child",
        vec![Message::user("go")],
        BackendParentContext::default(),
        Arc::new(NullEventSink),
    ))
    .await
    .expect_err("resolver failure should surface");

    match err {
        ExecutionBackendError::ExecutionFailed(message) => {
            assert!(
                message.contains("failed to resolve agent 'child'"),
                "error should include resolve context: {message}"
            );
            assert!(
                message.contains("registry backend timed out"),
                "error should preserve original resolver failure: {message}"
            );
        }
        other => panic!("resolver infrastructure failure must not become AgentNotFound: {other:?}"),
    }
}

struct SingleResolveLocalResolver {
    agent: ResolvedAgent,
    resolve_calls: AtomicUsize,
    resolve_execution_calls: AtomicUsize,
}

impl SingleResolveLocalResolver {
    fn new(agent: ResolvedAgent) -> Self {
        Self {
            agent,
            resolve_calls: AtomicUsize::new(0),
            resolve_execution_calls: AtomicUsize::new(0),
        }
    }
}

impl AgentResolver for SingleResolveLocalResolver {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        Err(RuntimeError::ResolveFailed {
            message: format!("unexpected second local resolve for {agent_id}"),
        })
    }

    fn resolve_execution(&self, agent_id: &str) -> Result<ExecutionPlan, RuntimeError> {
        self.resolve_execution_calls.fetch_add(1, Ordering::SeqCst);
        if agent_id != self.agent.id() {
            return Err(RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            });
        }
        let mut agent = self.agent.clone();
        agent.env = build_agent_env(&[], &agent)?;
        Ok(ExecutionPlan::from_resolved_agent(&agent))
    }
}

#[tokio::test]
async fn local_child_agent_uses_resolved_execution_without_second_lookup() {
    let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
        content: vec![ContentBlock::text("done")],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }]));
    let agent = ResolvedAgent::new("child", "m", "sys", llm).with_max_rounds(1);
    let resolver = SingleResolveLocalResolver::new(agent);

    let outcome = run_child_agent(ChildAgentParams::new(
        &resolver,
        "child",
        vec![Message::user("go")],
        BackendParentContext::default(),
        Arc::new(NullEventSink),
    ))
    .await
    .expect("canonical child helper should reuse the resolved local agent");

    assert!(matches!(outcome.status, BackendRunStatus::Completed));
    assert_eq!(
        resolver.resolve_execution_calls.load(Ordering::SeqCst),
        1,
        "canonical helper should resolve execution exactly once"
    );
    assert_eq!(
        resolver.resolve_calls.load(Ordering::SeqCst),
        0,
        "local delegate path must not perform a second AgentResolver::resolve lookup"
    );
}

// Silence "unused import" warnings — included for the test to look like a
// real downstream caller would write it (mirrors the developer pattern).
#[allow(dead_code)]
fn _doc_imports_compile() {
    let _: Option<&dyn AgentResolver> = None;
    let _: Option<&StateStore> = None;
}

type CapturedMessages = Vec<(Role, String)>;

struct CaptureMessagesBackend {
    captured: std::sync::Mutex<Option<(CapturedMessages, CapturedMessages)>>,
}

impl CaptureMessagesBackend {
    fn new() -> Self {
        Self {
            captured: std::sync::Mutex::new(None),
        }
    }

    fn captured(&self) -> (CapturedMessages, CapturedMessages) {
        self.captured
            .lock()
            .expect("lock poisoned")
            .clone()
            .expect("delegate request should have been captured")
    }
}

#[async_trait]
impl ExecutionBackend for CaptureMessagesBackend {
    async fn execute_delegate(
        &self,
        request: BackendDelegateRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        let messages = request
            .messages
            .iter()
            .map(|message| (message.role, message.text()))
            .collect();
        let new_messages = request
            .new_messages
            .iter()
            .map(|message| (message.role, message.text()))
            .collect();
        *self.captured.lock().expect("lock poisoned") = Some((messages, new_messages));

        Ok(BackendRunResult {
            agent_id: request.agent_id.to_string(),
            status: BackendRunStatus::Completed,
            termination: TerminationReason::NaturalEnd,
            status_reason: None,
            response: Some("captured".into()),
            output: Default::default(),
            steps: 0,
            run_id: Some("capture-run".into()),
            inbox: None,
            state: None,
        })
    }
}

struct CaptureMessagesResolver {
    backend: Arc<CaptureMessagesBackend>,
    agent_id: String,
}

impl AgentResolver for CaptureMessagesResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        Err(RuntimeError::ResolveFailed {
            message: "capture backend is non-local; resolve_execution is the canonical path".into(),
        })
    }

    fn resolve_execution(&self, agent_id: &str) -> Result<ExecutionPlan, RuntimeError> {
        if agent_id != self.agent_id {
            return Err(RuntimeError::ResolveFailed {
                message: format!("agent not found: {agent_id}"),
            });
        }
        let spec = Arc::new(awaken_contract::registry_spec::AgentSpec {
            id: self.agent_id.clone(),
            ..Default::default()
        });
        Ok(ExecutionPlan::Remote(ResolvedBackendAgent::with_backend(
            spec,
            self.backend.clone(),
        )))
    }

    fn agent_ids(&self) -> Vec<String> {
        vec![self.agent_id.clone()]
    }
}

#[tokio::test]
async fn child_agent_messages_are_fresh_delegate_input() {
    let backend = Arc::new(CaptureMessagesBackend::new());
    let resolver = CaptureMessagesResolver {
        backend: backend.clone(),
        agent_id: "remote-child".into(),
    };

    let outcome = run_child_agent(ChildAgentParams::new(
        &resolver,
        "remote-child",
        vec![
            Message::system("child system seed"),
            Message::user("current child task"),
        ],
        BackendParentContext::default(),
        Arc::new(NullEventSink),
    ))
    .await
    .expect("delegate request should dispatch");

    assert!(matches!(outcome.status, BackendRunStatus::Completed));
    let expected = vec![
        (Role::System, "child system seed".to_string()),
        (Role::User, "current child task".to_string()),
    ];
    let (messages, new_messages) = backend.captured();
    assert_eq!(
        messages, expected,
        "ChildAgentParams.initial_messages is the child run's full fresh input"
    );
    assert_eq!(
        new_messages, expected,
        "run_child_agent intentionally mirrors fresh input into new_messages"
    );
}

struct FailedStatusBackend;

#[async_trait]
impl ExecutionBackend for FailedStatusBackend {
    async fn execute_delegate(
        &self,
        request: BackendDelegateRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        Ok(BackendRunResult {
            agent_id: request.agent_id.to_string(),
            status: BackendRunStatus::Failed("child failed".into()),
            termination: TerminationReason::Error("child failed".into()),
            status_reason: Some("child failed".into()),
            response: None,
            output: Default::default(),
            steps: 0,
            run_id: Some("failed-run".into()),
            inbox: None,
            state: None,
        })
    }
}

struct FailedStatusResolver {
    backend: Arc<FailedStatusBackend>,
    agent_id: String,
}

impl AgentResolver for FailedStatusResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        Err(RuntimeError::ResolveFailed {
            message: "failed-status backend is non-local".into(),
        })
    }

    fn resolve_execution(&self, agent_id: &str) -> Result<ExecutionPlan, RuntimeError> {
        if agent_id != self.agent_id {
            return Err(RuntimeError::ResolveFailed {
                message: format!("agent not found: {agent_id}"),
            });
        }
        let spec = Arc::new(awaken_contract::registry_spec::AgentSpec {
            id: self.agent_id.clone(),
            ..Default::default()
        });
        Ok(ExecutionPlan::Remote(ResolvedBackendAgent::with_backend(
            spec,
            self.backend.clone(),
        )))
    }

    fn agent_ids(&self) -> Vec<String> {
        vec![self.agent_id.clone()]
    }
}

#[tokio::test]
async fn checked_child_agent_rejects_non_completed_status() {
    let resolver = FailedStatusResolver {
        backend: Arc::new(FailedStatusBackend),
        agent_id: "remote-child".into(),
    };

    let err = run_child_agent_checked(ChildAgentParams::new(
        &resolver,
        "remote-child",
        vec![Message::user("go")],
        BackendParentContext::default(),
        Arc::new(NullEventSink),
    ))
    .await
    .expect_err("checked helper should reject failed terminal status");

    match &err {
        ChildAgentError::Terminal(result) => {
            assert!(matches!(result.status, BackendRunStatus::Failed(_)));
        }
        other => panic!("expected terminal child status error, got: {other:?}"),
    }
    assert!(
        err.terminal_result().is_some(),
        "terminal result should remain available for diagnostics"
    );
}

/// Backend that advertises no `delegate_state_seed` capability and panics if
/// `execute_delegate` is reached — the capability check is supposed to
/// reject the request first.
struct NoSeedBackend;

#[async_trait]
impl ExecutionBackend for NoSeedBackend {
    fn capabilities(&self) -> awaken_runtime::resolution::BackendProfile {
        awaken_runtime::resolution::BackendProfile::remote_stateless_text()
    }

    async fn execute_delegate(
        &self,
        _request: BackendDelegateRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        panic!(
            "execute_delegate must not be reached — validate_delegate_execution_request \
             should reject the state-seeded request before dispatch"
        );
    }
}

struct NoSeedResolver {
    backend: Arc<NoSeedBackend>,
    agent_id: String,
}

impl AgentResolver for NoSeedResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        Err(RuntimeError::ResolveFailed {
            message: "no-seed backend is non-local; resolve_execution is the canonical path".into(),
        })
    }

    fn resolve_execution(&self, agent_id: &str) -> Result<ExecutionPlan, RuntimeError> {
        if agent_id != self.agent_id {
            return Err(RuntimeError::ResolveFailed {
                message: format!("agent not found: {agent_id}"),
            });
        }
        let spec = Arc::new(awaken_contract::registry_spec::AgentSpec {
            id: self.agent_id.clone(),
            ..Default::default()
        });
        Ok(ExecutionPlan::Remote(ResolvedBackendAgent::with_backend(
            spec,
            self.backend.clone(),
        )))
    }

    fn agent_ids(&self) -> Vec<String> {
        vec![self.agent_id.clone()]
    }
}

#[tokio::test]
async fn seed_rejected_when_backend_lacks_capability() {
    let resolver = NoSeedResolver {
        backend: Arc::new(NoSeedBackend),
        agent_id: "remote-child".into(),
    };

    let result = run_child_agent(
        ChildAgentParams::new(
            &resolver,
            "remote-child",
            vec![Message::user("go")],
            BackendParentContext::default(),
            Arc::new(NullEventSink),
        )
        .with_initial_state_seed(seed_with(1)),
    )
    .await;

    let err = result.expect_err("seeded delegate against an unsupported backend must error");
    let message = err.to_string();
    assert!(
        message.contains("delegate_state_seed"),
        "error should name the unsupported capability, got: {message}"
    );
}

#[tokio::test]
async fn no_seed_against_no_seed_backend_still_dispatches() {
    // Sanity check: capability rejection is scoped to *seeded* requests; an
    // unsupported backend without a seed should NOT be pre-empted by the
    // capability check (it'll fail later for its own reasons — here, the
    // backend panics, proving dispatch was attempted).
    let resolver = NoSeedResolver {
        backend: Arc::new(NoSeedBackend),
        agent_id: "remote-child".into(),
    };

    let dispatched = std::panic::AssertUnwindSafe(run_child_agent(ChildAgentParams::new(
        &resolver,
        "remote-child",
        vec![Message::user("go")],
        BackendParentContext::default(),
        Arc::new(NullEventSink),
    )));

    let outcome = futures::FutureExt::catch_unwind(dispatched).await;
    assert!(
        outcome.is_err(),
        "execute_delegate panic should reach the caller — capability check must not pre-empt unseeded requests",
    );
}
