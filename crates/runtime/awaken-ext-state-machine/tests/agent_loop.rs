//! End-to-end: the state-machine plugin wired into a real runtime loop. A
//! scripted model drives a write-before-read attempt; the gate denies it, the
//! model corrects to read-then-write, and the run ends naturally once the
//! instance reaches its terminal state.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
use awaken_agent_contract::agent::state::StateKey;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_state_machine::{
    Metrics, RunInstances, StateMachineConfig, StateMachinePlugin, ThreadInstances,
};
use awaken_runtime::Runtime;
use awaken_runtime::memory::{MemoryCommitCoordinator, replay_state};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
use serde_json::json;

const READ_BEFORE_WRITE: &str = r#"{"machines":[{
    "name":"rbw","scope":"thread","key":"{file_path}","initial":"unread","terminal":["written"],
    "transitions":[
        {"on":"Read(file_path ~ \"*\")","from":["unread","written","read"],"to":"read"},
        {"on":"Write(file_path ~ \"*\")","from":"read","to":"written",
         "on_violation":{"action":"deny","reason":"Read {file_path} before writing."}}
    ]}],"continuation":{"max_continuations":5,"message":"Finish: {summary}"}}"#;

/// A model that replays a scripted sequence of turns, one per inference.
struct ScriptedLlm {
    turns: Mutex<Vec<AssistantOutput>>,
    step: AtomicUsize,
}

impl ScriptedLlm {
    fn new(turns: Vec<AssistantOutput>) -> Self {
        Self {
            turns: Mutex::new(turns),
            step: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl LlmExecutor for ScriptedLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let idx = self.step.fetch_add(1, Ordering::SeqCst);
        let turns = self.turns.lock().unwrap();
        let output = turns
            .get(idx)
            .cloned()
            .unwrap_or_else(|| AssistantOutput::text("done"));
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A tool that succeeds with fixed content.
struct OkTool(&'static str);

#[async_trait::async_trait]
impl RawTool for OkTool {
    fn id(&self) -> &str {
        self.0
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(call.call_id, "contents"))
    }
}

fn tool_call(id: &str, tool: &str, file: &str) -> ToolCall {
    ToolCall {
        call_id: id.to_string(),
        tool_id: tool.to_string(),
        arguments: json!({ "file_path": file }),
    }
}

fn install(runtime: &Runtime) {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fingerprint.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fingerprint,
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("installs");
}

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                delegation_limits: Default::default(),
                model_binding: ModelBinding {
                    provider_identity_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                tool_descriptors: Vec::new(),
                plugin_ids: vec!["state_machine".to_string()],
                plugin_config: Default::default(),
                context_policy: awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("edit a.rs")],
        }],
        delegation_origin: None,
        model_ref_override: None,
    }
}

#[tokio::test]
async fn deny_then_corrected_read_write_reaches_terminal() {
    // Scripted turns: write (denied) → read → write → (implicit text "done").
    let llm = ScriptedLlm::new(vec![
        AssistantOutput::from_tool_calls(vec![tool_call("c1", "Write", "a.rs")]),
        AssistantOutput::from_tool_calls(vec![tool_call("c2", "Read", "a.rs")]),
        AssistantOutput::from_tool_calls(vec![tool_call("c3", "Write", "a.rs")]),
    ]);
    let plugin = StateMachinePlugin::from_config(
        StateMachineConfig::from_json_str(READ_BEFORE_WRITE).unwrap(),
    )
    .unwrap();

    let runtime = Runtime::new()
        .with_llm(Arc::new(llm))
        .with_tool(Arc::new(OkTool("Read")))
        .with_tool(Arc::new(OkTool("Write")))
        .with_plugin(Arc::new(plugin));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime.execute(activation(), context).await.expect("runs");

    // The run reached its natural end (the instance is terminal, so the
    // continuation guard did not steer).
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    // The denied write's reason is committed and model-visible.
    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.text_content().contains("Read a.rs before writing.")),
        "deny feedback should be committed and visible to the model"
    );

    // Final state: the instance advanced read → written.
    let store = replay_state(&committed);
    assert_eq!(
        ThreadInstances::load_or_default(&store).current("rbw", "a.rs"),
        Some("written")
    );

    // Metrics: one deny, two transitions (read, written).
    let metrics = Metrics::load_or_default(&store);
    assert_eq!(metrics.total.denied, 1);
    assert_eq!(metrics.total.transitioned, 2);
}

const WARN_ON_UNREAD: &str = r#"{"machines":[{
    "name":"m","scope":"run","key":"{file_path}","initial":"unread",
    "transitions":[{"on":"Write(file_path ~ \"*\")","from":"read","to":"written",
        "on_violation":{"action":"warn","reason":"writing unread {file_path}"}}]}]}"#;

#[tokio::test]
async fn warn_message_reaches_the_next_model_turn() {
    let llm = ScriptedLlm::new(vec![AssistantOutput::from_tool_calls(vec![tool_call(
        "c1", "Write", "a.rs",
    )])]);
    let plugin =
        StateMachinePlugin::from_config(StateMachineConfig::from_json_str(WARN_ON_UNREAD).unwrap())
            .unwrap();
    let runtime = Runtime::new()
        .with_llm(Arc::new(llm))
        .with_tool(Arc::new(OkTool("Write")))
        .with_plugin(Arc::new(plugin));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    // The warn call was allowed (the write executed) and a warning reached the
    // transcript for the next turn.
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.text_content() == "writing unread a.rs"),
        "warn guidance should be committed to the transcript"
    );
    let store = replay_state(&committed);
    assert_eq!(Metrics::load_or_default(&store).total.warned, 1);
}

const WORK_FLOW: &str = r#"{"machines":[{
    "name":"work","scope":"run","key":"","initial":"idle","terminal":["done"],
    "transitions":[
        {"on":"Start","from":["idle"],"to":"pending"},
        {"on":"Finish","from":["pending"],"to":"done"}
    ]}],"continuation":{"max_continuations":5,"message":"Finish the work: {summary}"}}"#;

#[tokio::test]
async fn continuation_nudge_keeps_running_until_terminal() {
    // Start (idle→pending), then a premature text turn (guard steers because the
    // instance is non-terminal), then Finish (pending→done), then end.
    let llm = ScriptedLlm::new(vec![
        AssistantOutput::from_tool_calls(vec![ToolCall {
            call_id: "c1".into(),
            tool_id: "Start".into(),
            arguments: json!({}),
        }]),
        AssistantOutput::text("I think I am done"),
        AssistantOutput::from_tool_calls(vec![ToolCall {
            call_id: "c2".into(),
            tool_id: "Finish".into(),
            arguments: json!({}),
        }]),
    ]);
    let plugin =
        StateMachinePlugin::from_config(StateMachineConfig::from_json_str(WORK_FLOW).unwrap())
            .unwrap();
    let runtime = Runtime::new()
        .with_llm(Arc::new(llm))
        .with_tool(Arc::new(OkTool("Start")))
        .with_tool(Arc::new(OkTool("Finish")))
        .with_plugin(Arc::new(plugin));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    // The guard steered the premature end with the interpolated summary.
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.text_content() == "Finish the work: work[]=pending"),
        "the continuation guard should have nudged the run to keep going"
    );
    // It ended only after reaching the terminal state.
    let store = replay_state(&committed);
    assert_eq!(
        RunInstances::load_or_default(&store).current("work", ""),
        Some("done")
    );
}

// ---------------------------------------------------------------------------
// Config-driven: the machines come from the agent's `plugin_config` section,
// not baked into the plugin. The plugin is registered once as `empty()`.
// ---------------------------------------------------------------------------

fn activation_configured(plugin_config: BTreeMap<String, serde_json::Value>) -> RunActivation {
    let mut activation = activation();
    activation.snapshot.resolved_spec.plugin_config = plugin_config;
    activation
}

fn section(id: &str, config_json: &str) -> BTreeMap<String, serde_json::Value> {
    let mut map = BTreeMap::new();
    map.insert(id.to_string(), serde_json::from_str(config_json).unwrap());
    map
}

#[tokio::test]
async fn config_section_drives_the_machine_set() {
    // One empty plugin; the read-before-write machine arrives via the agent's
    // config section. Same deny→correct→terminal behavior as the baked plugin.
    let llm = ScriptedLlm::new(vec![
        AssistantOutput::from_tool_calls(vec![tool_call("c1", "Write", "a.rs")]),
        AssistantOutput::from_tool_calls(vec![tool_call("c2", "Read", "a.rs")]),
        AssistantOutput::from_tool_calls(vec![tool_call("c3", "Write", "a.rs")]),
    ]);
    let runtime = Runtime::new()
        .with_llm(Arc::new(llm))
        .with_tool(Arc::new(OkTool("Read")))
        .with_tool(Arc::new(OkTool("Write")))
        .with_plugin(Arc::new(StateMachinePlugin::empty()));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .execute(
            activation_configured(section("state_machine", READ_BEFORE_WRITE)),
            context,
        )
        .await
        .expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.text_content().contains("Read a.rs before writing.")),
    );
    let store = replay_state(&committed);
    assert_eq!(
        ThreadInstances::load_or_default(&store).current("rbw", "a.rs"),
        Some("written")
    );
}

#[tokio::test]
async fn no_section_leaves_calls_unconstrained() {
    // The same empty plugin with no config section imposes no constraint: the
    // write executes on the first turn and the run ends naturally.
    let llm = ScriptedLlm::new(vec![AssistantOutput::from_tool_calls(vec![tool_call(
        "c1", "Write", "a.rs",
    )])]);
    let runtime = Runtime::new()
        .with_llm(Arc::new(llm))
        .with_tool(Arc::new(OkTool("Write")))
        .with_plugin(Arc::new(StateMachinePlugin::empty()));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert!(
        !commit
            .committed()
            .messages
            .iter()
            .any(|m| m.text_content().contains("before writing")),
        "no section ⇒ no gate ⇒ no denial"
    );
}

#[tokio::test]
async fn malformed_section_fails_the_run_closed() {
    // A section that cannot resolve fails the run closed before any model call.
    let llm = ScriptedLlm::new(vec![AssistantOutput::text("hi")]);
    let runtime = Runtime::new()
        .with_llm(Arc::new(llm))
        .with_plugin(Arc::new(StateMachinePlugin::empty()));
    install(&runtime);

    let bad = section(
        "state_machine",
        r#"{"machines":[{"name":"m","initial":"a","transitions":[{"on":"Read(","from":"a","to":"b"}]}]}"#,
    );
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .execute(activation_configured(bad), context)
        .await
        .expect("runs");
    assert_eq!(
        state,
        RunState::Ended(EndCause::Error(Failure::CapabilityBound)),
        "a malformed plugin config fails the run closed"
    );
}

// ---------------------------------------------------------------------------
// Direct model-visibility: capture each inference request and assert an emitted
// message is actually in the message list the model is shown on the next turn
// (not merely committed to the transcript).
// ---------------------------------------------------------------------------

/// A scripted model that also records the message list of every request it is
/// given, so a test can assert exactly what the model saw on each turn.
struct RecordingLlm {
    turns: Mutex<Vec<AssistantOutput>>,
    step: AtomicUsize,
    seen: Mutex<Vec<Vec<Message>>>,
}

impl RecordingLlm {
    fn new(turns: Vec<AssistantOutput>) -> Self {
        Self {
            turns: Mutex::new(turns),
            step: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// The neutral message list shown to the model on inference `turn` (0-based).
    fn request_texts(&self, turn: usize) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .get(turn)
            .map(|msgs| msgs.iter().map(Message::text_content).collect())
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl LlmExecutor for RecordingLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        // Record the neutral messages the runtime assembled for this turn.
        let messages = request
            .messages
            .iter()
            .map(|m| Message {
                id: MessageId(String::new()),
                role: Role::User,
                content: m.content.clone(),
            })
            .collect();
        self.seen.lock().unwrap().push(messages);

        let idx = self.step.fetch_add(1, Ordering::SeqCst);
        let turns = self.turns.lock().unwrap();
        let output = turns
            .get(idx)
            .cloned()
            .unwrap_or_else(|| AssistantOutput::text("done"));
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

const EMIT_ON_WRITE: &str = r#"{"machines":[{
    "name":"m","scope":"run","key":"","initial":"start","terminal":["done"],
    "transitions":[{"on":"Write(file_path ~ \"*\")","from":"start","to":"done",
        "emit":{"target":"conversation","content":"saved {file_path}; read it to verify"}}]}]}"#;

#[tokio::test]
async fn warn_emit_is_in_the_models_next_request() {
    // A warn violation on `Write` emits guidance; the model must literally see it
    // on the turn after the tool call — not just have it committed.
    let llm = Arc::new(RecordingLlm::new(vec![AssistantOutput::from_tool_calls(
        vec![tool_call("c1", "Write", "a.rs")],
    )]));
    let plugin =
        StateMachinePlugin::from_config(StateMachineConfig::from_json_str(WARN_ON_UNREAD).unwrap())
            .unwrap();
    let runtime = Runtime::new()
        .with_llm(llm.clone())
        .with_tool(Arc::new(OkTool("Write")))
        .with_plugin(Arc::new(plugin));
    install(&runtime);

    let context = RuntimeRunContext::new().with_commit(Arc::new(MemoryCommitCoordinator::new()));
    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    // Turn 0 (before the tool call) must NOT contain the emit; turn 1 (right after
    // the Write) MUST — proving it was injected between the two model calls.
    let emit = "writing unread a.rs";
    assert!(
        !llm.request_texts(0).iter().any(|t| t == emit),
        "the emit must not exist before the tool call"
    );
    assert!(
        llm.request_texts(1).iter().any(|t| t == emit),
        "the emit must be in the model's very next request: {:?}",
        llm.request_texts(1)
    );
}

#[tokio::test]
async fn success_transition_emit_is_in_the_models_next_request() {
    // A successful transition's `emit` (with a {file_path} interpolation) is shown
    // to the model on the next inference, carrying the tool's own argument.
    let llm = Arc::new(RecordingLlm::new(vec![AssistantOutput::from_tool_calls(
        vec![tool_call("c1", "Write", "a.rs")],
    )]));
    let plugin =
        StateMachinePlugin::from_config(StateMachineConfig::from_json_str(EMIT_ON_WRITE).unwrap())
            .unwrap();
    let runtime = Runtime::new()
        .with_llm(llm.clone())
        .with_tool(Arc::new(OkTool("Write")))
        .with_plugin(Arc::new(plugin));
    install(&runtime);

    let context = RuntimeRunContext::new().with_commit(Arc::new(MemoryCommitCoordinator::new()));
    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    let emit = "saved a.rs; read it to verify";
    assert!(
        !llm.request_texts(0).iter().any(|t| t == emit),
        "the emit must not exist before the transition fires"
    );
    assert!(
        llm.request_texts(1).iter().any(|t| t == emit),
        "the transition emit must be in the model's next request: {:?}",
        llm.request_texts(1)
    );
}

/// A gate that awaits the `Await` tool pending an out-of-band decision, and allows
/// everything else — so a real tool runs (and the FSM emits) before the await.
struct AwaitTheAwaitTool;

#[async_trait::async_trait]
impl ToolGateHook for AwaitTheAwaitTool {
    async fn gate(
        &self,
        ctx: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        if ctx.tool_id == "Await" {
            GateOutcome::RequireConfirmation {
                correlation_id: "await-ticket".to_string(),
            }
        } else {
            GateOutcome::Allow
        }
    }
}

fn await_resume_command() -> ResumeCommand {
    ResumeCommand {
        correlation_id: "await-ticket".to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId("snapshot-1".to_string()),
        catalog_fingerprint: CatalogFingerprint("catalog-a".to_string()),
        result: ResumeResult::allow(),
        now_ms: 0,
    }
}

#[tokio::test]
async fn emit_survives_a_await_and_is_in_the_resumed_request() {
    // Turn 0 `Write` executes and the FSM emits guidance; turn 1 awaits on a gated
    // `Await` tool. After resume, the emit (committed on turn 0, before the await)
    // must still be in the message list the model is shown on the resumed turn.
    let llm = Arc::new(RecordingLlm::new(vec![
        AssistantOutput::from_tool_calls(vec![tool_call("c1", "Write", "a.rs")]),
        AssistantOutput::from_tool_calls(vec![tool_call("c2", "Await", "a.rs")]),
    ]));
    let plugin =
        StateMachinePlugin::from_config(StateMachineConfig::from_json_str(EMIT_ON_WRITE).unwrap())
            .unwrap();
    let runtime = Runtime::new()
        .with_llm(llm.clone())
        .with_tool(Arc::new(OkTool("Write")))
        .with_tool(Arc::new(OkTool("Await")))
        .with_gate(Arc::new(AwaitTheAwaitTool))
        .with_plugin(Arc::new(plugin));
    install(&runtime);
    // Resume rebuilds the run from the snapshot registry, so register it.
    runtime.register_snapshot(activation().snapshot);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let awaiting = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(
        awaiting,
        RunState::Awaiting,
        "the Await tool awaits the run"
    );

    // Resume: the awaiting tool runs and the loop continues to a fresh inference.
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let ended = runtime
        .resume(await_resume_command(), commit.as_ref(), context)
        .await
        .expect("resume runs");
    assert_eq!(ended, RunState::Ended(EndCause::NaturalEnd));

    // The resumed inference (index 2 across the whole run) still carries the emit
    // committed on turn 0 — proof it survived the await boundary.
    let emit = "saved a.rs; read it to verify";
    let resumed = llm.request_texts(2);
    assert!(
        resumed.iter().any(|t| t == emit),
        "the emit must survive the await and appear in the resumed request: {resumed:?}"
    );
}
