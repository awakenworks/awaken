//! End-to-end: the state-machine plugin wired into a real runtime loop. A
//! scripted model drives a write-before-read attempt; the gate denies it, the
//! model corrects to read-then-write, and the run ends naturally once the
//! instance reaches its terminal state.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_state_machine::{
    Metrics, RunInstances, StateCell, StateMachineConfig, StateMachinePlugin, ThreadInstances,
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
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
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
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                model_binding: ModelBinding {
                    provider_instance_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                tool_descriptors: Vec::new(),
                plugin_ids: vec!["state_machine".to_string()],
                plugin_config: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("edit a.rs")],
        }],
        trace: Default::default(),
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
    let phase = runtime.execute(activation(), context).await.expect("runs");

    // The run reached its natural end (the instance is terminal, so the
    // continuation guard did not steer).
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

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
        ThreadInstances::load(&store).current("rbw", "a.rs"),
        Some("written")
    );

    // Metrics: one deny, two transitions (read, written).
    let metrics = Metrics::load(&store);
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
    let phase = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

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
    assert_eq!(Metrics::load(&store).total.warned, 1);
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
    let phase = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

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
    assert_eq!(RunInstances::load(&store).current("work", ""), Some("done"));
}
