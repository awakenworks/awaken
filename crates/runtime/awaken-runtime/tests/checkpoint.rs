//! Durable interrupted-stream checkpoints, driven end-to-end through `execute`
//! (not the inference call in isolation): the engine flushes a partial at an
//! interruption boundary and clears it once the step concludes. A retained
//! partial can resume only with independent invocation evidence.
//! An unjournaled historical checkpoint must not authorize a fresh call.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
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
use awaken_store_inmem::{MemoryCommitCoordinator, MemoryStreamCheckpointStore, MemoryStreamSink};

/// Streams `first` then confirms remote termination on its first call; streams `rest` and
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
            return Err(
                Error::Timeout("provider-confirmed terminal deadline".to_string())
                    .with_execution_evidence(
                    awaken_runtime_contract::model_operation::ModelExecutionEvidence::Terminated,
                ),
            );
        }
        sink.on_text(self.rest).await;
        Ok(ChatResponse {
            output: AssistantOutput::text(self.rest),
            usage: None,
            stop_reason: None,
        })
    }
}

/// Would succeed if invoked; the historical-checkpoint test requires zero calls.
struct AlwaysLlm {
    text: &'static str,
    calls: std::sync::atomic::AtomicUsize,
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
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
            attempt_timeout: std::time::Duration::from_secs(5),
        });
    let fingerprint = CatalogFingerprint("catalog-a".to_string());

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
            metadata: Default::default(),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding {
                        provider_identity_ref: "p".to_string(),
                        model_ref: "m".to_string(),
                        backend_ref: "b".to_string(),
                    },
                ),
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
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
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
    // C1 remote termination confirmed after partial text; C2 retry allowed.
    // R1=C1+C2 => one continuation, joined text, spent checkpoint removed.
    // Unknown returned errors have separate negative coverage in execution.rs.
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
    // The committed response is the whole text, continued across the drop.
    assert_eq!(assistant_text(&commit), "Hello world");
    // The run concluded in-process, so the checkpoint it flushed at the boundary
    // was cleared — nothing lingers to resume.
    assert!(
        checkpoints
            .get(RUN_ID)
            .await
            .expect("checkpoint read")
            .is_none()
    );
}

#[tokio::test]
async fn a_historical_checkpoint_without_invocation_evidence_cannot_resume() {
    // H1 a historical text checkpoint exists; H2 no invocation journal exists.
    // H1+H2 => Indeterminate, no new assistant message, retained checkpoint.
    // Absence of evidence must not be treated as a fresh model operation.
    // Positive confirmed-termination continuation is covered above.
    let checkpoints = Arc::new(MemoryStreamCheckpointStore::new());
    checkpoints
        .put(StreamCheckpoint {
            run_id: RUN_ID.to_string(),
            thread_id: THREAD_ID.to_string(),
            model: "m".to_string(),
            partial_text: "Resumed ".to_string(),
            partial_tools: Vec::new(),
            retry_count: 0,
        })
        .await
        .expect("checkpoint put");

    let model = Arc::new(AlwaysLlm {
        text: "and done",
        calls: Default::default(),
    });
    let (state, commit) = drive(model.clone(), checkpoints.clone()).await;

    assert_eq!(state, RunState::Ended(EndCause::Indeterminate));
    assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .all(|m| m.role != Role::Assistant),
        "historical partial cannot authorize a new assistant response"
    );
    assert!(
        checkpoints
            .get(RUN_ID)
            .await
            .expect("checkpoint read")
            .is_some()
    );
}
