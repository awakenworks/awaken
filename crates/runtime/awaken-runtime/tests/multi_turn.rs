//! Multi-turn conversation: a fresh run on a thread continues the conversation
//! when a history reader is wired. The runtime loads the thread's
//! committed messages — the caller never assembles history.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

const FP: &str = "catalog-a";

/// Replies with every user message it was shown, joined — so the reply reveals
/// exactly which history reached the model.
struct EchoUserLlm;
#[async_trait::async_trait]
impl LlmExecutor for EchoUserLlm {
    async fn infer(&self, r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let seen = r
            .messages
            .iter()
            .filter(|m| matches!(m.role, ChatRole::User))
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|");
        Ok(ChatResponse {
            output: AssistantOutput::text(seen),
            usage: None,
            stop_reason: None,
        })
    }
}

fn snapshot() -> ExecutableAgentSnapshot {
    let fp = CatalogFingerprint(FP.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId("a".to_string()),
        root_agent_id: AgentId("a".to_string()),
        resolved_spec: ResolvedSpec {
            model_candidates: Vec::new(),
            catalog_fingerprint: fp.clone(),
            instructions: String::new(),
            max_steps: 8,
            model_binding: ModelBinding {
                provider_instance_ref: "p".to_string(),
                model_ref: "m".to_string(),
                backend_ref: "b".to_string(),
            },
            tool_descriptors: vec![ToolDescriptor::pinned(
                "t",
                "noop",
                "",
                serde_json::json!({}),
            )],
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
        },
        fingerprint: fp,
    }
}

fn runtime() -> Runtime {
    let runtime = Runtime::new().with_llm(Arc::new(EchoUserLlm));
    let fp = CatalogFingerprint(FP.to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub".to_string(),
            fingerprint: fp.clone(),
            source_revisions: vec!["r".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fp,
                runtime_version: "t".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("install");
    runtime.register_snapshot(snapshot());
    runtime
}

fn turn(message_id: &str, text: &str) -> RunActivation {
    RunActivation {
        run_id: RunId(message_id.to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: snapshot(),
        input: vec![Message {
            id: MessageId(message_id.to_string()),
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }],
        trace: Default::default(),
    }
}

#[tokio::test]
async fn a_fresh_turn_continues_the_thread_with_a_reader() {
    let runtime = runtime();
    let commit: Arc<MemoryCommitCoordinator> = Arc::new(MemoryCommitCoordinator::new());
    let reader: Arc<dyn ThreadReader> = commit.clone();

    // Turn 1: the model sees only this turn's input.
    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(reader.clone());
    let phase = runtime
        .execute(turn("t1", "My name is Sam."), ctx)
        .await
        .unwrap();
    assert!(matches!(phase, Phase::Ended(_)));

    // Turn 2: a fresh run on the same thread; the runtime loads turn 1 from the
    // committed history, so the model sees both user turns.
    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(reader.clone());
    runtime
        .execute(turn("t2", "What is my name?"), ctx)
        .await
        .unwrap();

    let reply = commit
        .committed_messages(&ThreadId("thread-1".to_string()))
        .into_iter()
        .rfind(|m| m.role == Role::Assistant)
        .expect("assistant reply");
    assert_eq!(
        reply.text_content(),
        "My name is Sam.|What is my name?",
        "turn 2 saw turn 1's user message from committed history"
    );
}

#[tokio::test]
async fn without_a_reader_a_fresh_turn_starts_clean() {
    // No reader: the run sees only its own input (single-turn). This is the
    // backward-compatible default.
    let runtime = runtime();
    let commit: Arc<MemoryCommitCoordinator> = Arc::new(MemoryCommitCoordinator::new());

    let ctx = RuntimeRunContext::new().with_commit(commit.clone());
    runtime.execute(turn("t1", "First."), ctx).await.unwrap();
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());
    runtime.execute(turn("t2", "Second."), ctx).await.unwrap();

    let reply = commit
        .committed_messages(&ThreadId("thread-1".to_string()))
        .into_iter()
        .rfind(|m| m.role == Role::Assistant)
        .expect("assistant reply");
    assert_eq!(
        reply.text_content(),
        "Second.",
        "no history without a reader"
    );
}
