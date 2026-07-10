//! A streaming provider pushes text chunks to the live stream as they arrive;
//! the committed message is the assembled whole, and multi-byte UTF-8 content is
//! reassembled without splitting a code point (G10/G13).

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::event::Kind as StreamKind;
use awaken_runtime::Runtime;
use awaken_runtime::memory::{MemoryCommitCoordinator, MemoryStreamSink};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, DeltaSink, LlmExecutor, Result,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

/// Pushes each chunk to the live sink, then returns the assembled text as the
/// committed response.
struct StreamingLlm {
    chunks: Vec<&'static str>,
}

#[async_trait::async_trait]
impl LlmExecutor for StreamingLlm {
    async fn infer(&self, _request: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text(self.chunks.concat()),
            usage: None,
            stop_reason: None,
        })
    }

    async fn infer_streaming(
        &self,
        _request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse> {
        for chunk in &self.chunks {
            sink.on_text(chunk).await;
        }
        Ok(ChatResponse {
            output: AssistantOutput::text(self.chunks.concat()),
            usage: None,
            stop_reason: None,
        })
    }
}

async fn run(chunks: Vec<&'static str>) -> (MemoryCommitCoordinator, MemoryStreamSink) {
    let runtime = Runtime::new().with_llm(Arc::new(StreamingLlm { chunks }));
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fingerprint.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fingerprint.clone(),
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("installs");

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let sink = Arc::new(MemoryStreamSink::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(sink.clone());

    let activation = RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                model_binding: ModelBinding {
                    provider_instance_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("hi")],
        }],
        trace: Default::default(),
    };
    let outcome = runtime.execute(activation, context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
    (
        Arc::try_unwrap(commit).unwrap_or_default(),
        Arc::try_unwrap(sink).unwrap_or_default(),
    )
}

fn streamed_text(sink: &MemoryStreamSink) -> Vec<String> {
    sink.events()
        .into_iter()
        .filter_map(|e| match e.kind {
            StreamKind::OutputText { text } => Some(text),
            _ => None,
        })
        .collect()
}

fn committed_assistant_text(commit: &MemoryCommitCoordinator) -> String {
    commit
        .committed()
        .messages
        .into_iter()
        .find(|m| m.role == Role::Assistant)
        .map(|m| m.text_content())
        .expect("an assistant message is committed")
}

#[tokio::test]
async fn text_streams_chunk_by_chunk_and_commits_the_whole() {
    let (commit, sink) = run(vec!["Hel", "lo, ", "world"]).await;

    // The live stream saw each chunk in order, not one combined blob.
    assert_eq!(streamed_text(&sink), vec!["Hel", "lo, ", "world"]);
    // The committed message is the assembled whole.
    assert_eq!(committed_assistant_text(&commit), "Hello, world");
}

#[tokio::test]
async fn multibyte_utf8_reassembles_without_splitting_a_code_point() {
    // Chunk boundaries fall between code points (each chunk is valid UTF-8); the
    // runtime concatenates verbatim and never indexes by byte, so multi-byte
    // characters and an emoji survive reassembly intact.
    let (commit, sink) = run(vec!["你", "好世", "界", "🌍!"]).await;

    assert_eq!(streamed_text(&sink), vec!["你", "好世", "界", "🌍!"]);
    assert_eq!(committed_assistant_text(&commit), "你好世界🌍!");
}
