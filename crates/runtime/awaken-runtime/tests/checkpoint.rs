//! Durable interrupted-stream checkpoints, driven end-to-end through `execute`
//! (not the inference call in isolation): the engine flushes a partial at an
//! interruption boundary and clears it once the step concludes, and a partial
//! pre-seeded into the store (as a crash mid-recovery would leave) resumes the
//! run's first step from that partial instead of regenerating it.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_runtime::Runtime;
use awaken_runtime::memory::{
    MemoryCommitCoordinator, MemoryStreamCheckpointStore, MemoryStreamSink,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, DeltaSink, Error, LlmExecutor, Result,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

/// Streams `first` then drops (retryable) on its first call; streams `rest` and
/// succeeds on any later call. Continuation stitches `first + rest`.
struct DropOnceLlm {
    calls: std::sync::Mutex<usize>,
    first: &'static str,
    rest: &'static str,
}
#[async_trait::async_trait]
impl LlmExecutor for DropOnceLlm {
    async fn infer(&self, _request: ChatRequest) -> Result<ChatResponse> {
        unreachable!("streaming-only")
    }
    async fn infer_streaming(
        &self,
        _request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse> {
        let n = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls - 1
        };
        if n == 0 {
            sink.on_text(self.first).await;
            return Err(Error::Timeout("connection reset".to_string()));
        }
        sink.on_text(self.rest).await;
        Ok(ChatResponse {
            output: AssistantOutput::text(self.rest),
            usage: None,
            stop_reason: None,
        })
    }
}

/// Always streams `text` and succeeds — used to observe how a pre-seeded
/// checkpoint reshapes the run's first inference.
struct AlwaysLlm {
    text: &'static str,
}
#[async_trait::async_trait]
impl LlmExecutor for AlwaysLlm {
    async fn infer(&self, _request: ChatRequest) -> Result<ChatResponse> {
        unreachable!("streaming-only")
    }
    async fn infer_streaming(
        &self,
        _request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse> {
        sink.on_text(self.text).await;
        Ok(ChatResponse {
            output: AssistantOutput::text(self.text),
            usage: None,
            stop_reason: None,
        })
    }
}

const RUN_ID: &str = "run-1";
const THREAD_ID: &str = "thread-1";

async fn drive(
    llm: Arc<dyn LlmExecutor>,
    checkpoints: Arc<MemoryStreamCheckpointStore>,
) -> (RunState, MemoryCommitCoordinator) {
    // A single retry with no backoff keeps the in-process recovery instant.
    let runtime = Runtime::new()
        .with_llm(llm)
        .with_retry_policy(awaken_runtime::LlmRetryPolicy {
            max_retries: 1,
            backoff_base_ms: 0,
            overloaded_backoff_base_ms: 0,
        });
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
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(Arc::new(MemoryStreamSink::new()))
        .with_stream_checkpoint(checkpoints);

    let activation = RunActivation {
        run_id: RunId(RUN_ID.to_string()),
        thread_id: ThreadId(THREAD_ID.to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                model_binding: ModelBinding {
                    provider_identity_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("hi")],
        }],
        model_ref_override: None,
    };
    let state = runtime.execute(activation, context).await.expect("runs");
    (state, Arc::try_unwrap(commit).unwrap_or_default())
}

fn assistant_text(commit: &MemoryCommitCoordinator) -> String {
    commit
        .committed()
        .messages
        .into_iter()
        .find(|m| m.role == Role::Assistant)
        .map(|m| m.text_content())
        .expect("an assistant message is committed")
}

#[tokio::test]
async fn an_interrupted_step_recovers_in_process_and_leaves_no_checkpoint() {
    let checkpoints = Arc::new(MemoryStreamCheckpointStore::new());
    let (state, commit) = drive(
        Arc::new(DropOnceLlm {
            calls: Default::default(),
            first: "Hel",
            rest: "lo world",
        }),
        checkpoints.clone(),
    )
    .await;

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    // The committed turn is the whole text, continued across the drop.
    assert_eq!(assistant_text(&commit), "Hello world");
    // The run concluded in-process, so the checkpoint it flushed at the boundary
    // was cleared — nothing lingers to resume.
    assert!(checkpoints.get(RUN_ID).await.is_none());
}

#[tokio::test]
async fn a_pre_seeded_checkpoint_resumes_the_first_step() {
    // A crash mid-recovery would leave a checkpoint keyed by this run. On the next
    // execution the engine reads it and continues the first step from the partial.
    let checkpoints = Arc::new(MemoryStreamCheckpointStore::new());
    checkpoints
        .put(StreamCheckpoint {
            run_id: RUN_ID.to_string(),
            thread_id: THREAD_ID.to_string(),
            model: "m".to_string(),
            partial_text: "Resumed ".to_string(),
            partial_tools: Vec::new(),
        })
        .await;

    let (state, commit) = drive(
        Arc::new(AlwaysLlm { text: "and done" }),
        checkpoints.clone(),
    )
    .await;

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    // The committed turn carries the recovered prefix stitched onto the fresh text.
    assert_eq!(assistant_text(&commit), "Resumed and done");
    // The consumed checkpoint is cleared once the step concludes.
    assert!(checkpoints.get(RUN_ID).await.is_none());
}
