//! Live-inbox consumption: messages queued on the attempt's inbox are drained
//! at the natural-end boundary, folded into the transcript as re-identified
//! turns, and shown to the model before any run-end decision. An empty inbox
//! changes nothing.

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
use awaken_runtime_contract::live_inbox::LiveInbox;
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

/// Replies with every user message it was shown, joined — the reply reveals
/// exactly which turns reached the model.
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

fn turn(run_id: &str, text: &str) -> RunActivation {
    RunActivation {
        run_id: RunId(run_id.to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: snapshot(),
        input: vec![Message {
            id: MessageId(format!("{run_id}-input")),
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }],
        trace: Default::default(),
    }
}

fn queued(text: &str) -> Message {
    // Caller-supplied id: the engine must re-identify, never trust it.
    Message::text(MessageId("client-chosen-id".to_string()), Role::User, text)
}

#[tokio::test]
async fn queued_messages_are_folded_in_before_the_run_ends() {
    let runtime = runtime();
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let inbox = LiveInbox::new();
    let _ = inbox.offer(queued("Queued follow-up."));

    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_live_inbox(inbox.clone());
    let phase = runtime.execute(turn("r1", "First."), ctx).await.unwrap();
    assert!(matches!(phase, Phase::Ended(_)));

    let committed = commit.committed_messages(&ThreadId("thread-1".to_string()));
    // The injected turn is committed with a run-scoped id, not the client's.
    let injected = committed
        .iter()
        .find(|m| m.id.0 == "r1-inbox-0")
        .expect("injected message committed under run-scoped id");
    assert_eq!(injected.text_content(), "Queued follow-up.");
    assert_eq!(injected.role, Role::User);
    assert!(
        !committed.iter().any(|m| m.id.0 == "client-chosen-id"),
        "caller-supplied id never reaches the transcript"
    );

    // The model saw the injection: its final reply echoes both user turns.
    let reply = committed
        .iter()
        .rfind(|m| m.role == Role::Assistant)
        .expect("assistant reply");
    assert_eq!(reply.text_content(), "First.|Queued follow-up.");

    // Consumed: nothing left to list or drain.
    assert!(inbox.list().is_empty());
}

#[tokio::test]
async fn a_batch_drains_in_order_with_sequential_ids() {
    let runtime = runtime();
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let inbox = LiveInbox::new();
    let _ = inbox.offer(queued("one"));
    let _ = inbox.offer(queued("two"));

    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_live_inbox(inbox);
    runtime.execute(turn("r2", "Start."), ctx).await.unwrap();

    let committed = commit.committed_messages(&ThreadId("thread-1".to_string()));
    let ids: Vec<&str> = committed
        .iter()
        .filter(|m| m.id.0.starts_with("r2-inbox-"))
        .map(|m| m.id.0.as_str())
        .collect();
    assert_eq!(ids, ["r2-inbox-0", "r2-inbox-1"]);

    let reply = committed
        .iter()
        .rfind(|m| m.role == Role::Assistant)
        .expect("assistant reply");
    assert_eq!(
        reply.text_content(),
        "Start.|one|two",
        "both queued messages reached the model in offer order"
    );
}

#[tokio::test]
async fn an_empty_or_absent_inbox_leaves_the_run_untouched() {
    let runtime = runtime();
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // Absent inbox (the default context) — single echo turn, natural end.
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());
    let phase = runtime.execute(turn("r3", "Solo."), ctx).await.unwrap();
    assert!(matches!(phase, Phase::Ended(_)));

    // Present but empty inbox — identical outcome.
    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_live_inbox(LiveInbox::new());
    let phase = runtime.execute(turn("r4", "Alone."), ctx).await.unwrap();
    assert!(matches!(phase, Phase::Ended(_)));

    let committed = commit.committed_messages(&ThreadId("thread-1".to_string()));
    assert!(
        !committed.iter().any(|m| m.id.0.contains("-inbox-")),
        "no injected turns appear without queued input"
    );
    let replies: Vec<String> = committed
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .map(Message::text_content)
        .collect();
    // No reader is wired, so each run sees only its own input.
    assert_eq!(replies, ["Solo.", "Alone."]);
}
