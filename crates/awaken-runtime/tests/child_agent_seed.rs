//! Integration tests for `run_child_agent` parent → child state seeding.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use awaken_contract::StateError;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event_sink::NullEventSink;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult};
use awaken_contract::contract::message::{Message, ToolCall};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::state::{PersistedState, StateKey, StateKeyOptions};

use awaken_runtime::backend::{
    BackendControl, BackendDelegatePolicy, BackendParentContext, BackendRunStatus,
};
use awaken_runtime::child_agent::{ChildAgentParams, run_child_agent};
use awaken_runtime::loop_runner::build_agent_env;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::registry::{
    AgentResolver, ExecutionResolver, ResolvedAgent, ResolvedExecution,
};
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

impl ExecutionResolver for FixedResolver {
    fn resolve_execution(&self, agent_id: &str) -> Result<ResolvedExecution, RuntimeError> {
        self.resolve(agent_id).map(ResolvedExecution::local)
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

    let result = run_child_agent(ChildAgentParams {
        resolver: &resolver,
        agent_id: "child",
        messages: vec![Message::user("go")],
        parent: BackendParentContext {
            parent_run_id: Some("parent-run".into()),
            parent_thread_id: Some("parent-thread".into()),
            parent_tool_call_id: Some("parent-call".into()),
        },
        initial_state_seed: Some(seed_with(42)),
        sink: Arc::new(NullEventSink),
        control: BackendControl::default(),
        policy: BackendDelegatePolicy::default(),
    })
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

    let result = run_child_agent(ChildAgentParams {
        resolver: &resolver,
        agent_id: "child",
        messages: vec![Message::user("hi")],
        parent: BackendParentContext {
            parent_run_id: None,
            parent_thread_id: None,
            parent_tool_call_id: None,
        },
        initial_state_seed: None,
        sink: Arc::new(NullEventSink),
        control: BackendControl::default(),
        policy: BackendDelegatePolicy::default(),
    })
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

    let err = run_child_agent(ChildAgentParams {
        resolver: &resolver,
        agent_id: "child",
        messages: vec![Message::user("doomed")],
        parent: BackendParentContext::default(),
        initial_state_seed: Some(seed),
        sink: Arc::new(NullEventSink),
        control: BackendControl::default(),
        policy: BackendDelegatePolicy::default(),
    })
    .await
    .expect_err("unknown seed key must surface as an error");

    let message = err.to_string();
    assert!(
        message.contains("no.such.key") || message.to_ascii_lowercase().contains("unknown"),
        "error should mention the unknown key: {message}"
    );
}

// Silence "unused import" warnings — included for the test to look like a
// real downstream caller would write it (mirrors the developer pattern).
#[allow(dead_code)]
fn _doc_imports_compile() {
    let _: Option<&dyn AgentResolver> = None;
    let _: Option<&StateStore> = None;
}
