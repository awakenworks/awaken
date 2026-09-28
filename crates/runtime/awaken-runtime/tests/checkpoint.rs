// @formal evidence checkpoint.crash_preserves_durable_truth
// @formal evidence checkpoint.recovery_never_precedes_commit
// @formal evidence checkpoint.terminal_completion_clears_resume
// @formal evidence model_operation.reentry_requires_positive_execution_evidence
//! Durable interrupted-stream checkpoints, driven end-to-end through `execute`
//! (not the inference call in isolation): the engine flushes a partial at an
//! interruption boundary and clears it once the step concludes. A retained
//! partial can resume only with independent invocation evidence.
//! An unjournaled historical checkpoint must not authorize a fresh call.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::StateKey;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::observation::Observed;
use awaken_agent_contract::stream::checkpoint::{
    StreamCheckpoint, StreamCheckpointError, StreamCheckpointStore,
};
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
                    awaken_runtime_contract::model_operation::ModelExecutionEvidence::Terminated(awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some("completed")).unwrap()),
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

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation {
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
                max_steps: Some(16),
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("p".to_string(), "m".to_string(), "b".to_string()),
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
    }
}

fn runtime_with(llm: Arc<dyn LlmExecutor>) -> Runtime {
    // A single retry with no backoff keeps the in-process recovery instant.
    Runtime::new()
        .with_llm(llm)
        .with_retry_policy(awaken_runtime::RetryPolicy {
            max_retries: 1,
            throttled_max_retries: 12,
            backoff_base_ms: 0,
            overloaded_backoff_base_ms: 0,
            budget: awaken_runtime_contract::llm::ModelInferenceBudget::new(
                std::time::Duration::from_millis(15000),
                std::time::Duration::from_millis(5000),
                std::time::Duration::from_millis(5000),
                std::time::Duration::from_millis(1),
            )
            .unwrap(),
        })
}

async fn drive(
    llm: Arc<dyn LlmExecutor>,
    checkpoints: Arc<dyn StreamCheckpointStore>,
) -> (RunState, MemoryCommitCoordinator) {
    let (state, commit) = drive_result(llm, checkpoints).await;
    (state.expect("runs"), commit)
}

async fn drive_result(
    llm: Arc<dyn LlmExecutor>,
    checkpoints: Arc<dyn StreamCheckpointStore>,
) -> (
    awaken_runtime_contract::execution::Result<RunState>,
    MemoryCommitCoordinator,
) {
    let runtime = runtime_with(llm);

    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(Arc::new(MemoryStreamSink::new()))
        .with_stream_checkpoint(checkpoints);

    let state = runtime.execute(activation(), context).await;
    (
        state,
        Arc::try_unwrap(commit).unwrap_or_else(|shared| (*shared).clone()),
    )
}

#[derive(Default)]
struct DeleteFailingCheckpointStore {
    inner: MemoryStreamCheckpointStore,
    deletes: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl StreamCheckpointStore for DeleteFailingCheckpointStore {
    async fn get(&self, run_id: &str) -> Observed<StreamCheckpoint> {
        self.inner.get(run_id).await
    }

    async fn put(
        &self,
        checkpoint: StreamCheckpoint,
    ) -> std::result::Result<(), StreamCheckpointError> {
        self.inner.put(checkpoint).await
    }

    async fn delete(&self, run_id: &str) -> std::result::Result<(), StreamCheckpointError> {
        self.deletes.lock().unwrap().push(run_id.to_string());
        Err(StreamCheckpointError::Storage(
            "injected delete failure".to_string(),
        ))
    }
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
    assert_eq!(checkpoints.get(RUN_ID).await, Observed::Absent);
}

#[tokio::test]
async fn checkpoint_delete_failure_withholds_terminal_completion_and_retains_resume() {
    // Causes: C1 a confirmed interruption creates a Run-scoped checkpoint; C2
    // the continued provider request terminates successfully; C3 deletion
    // succeeds or fails. Effects: E1 success permits NaturalEnd with no resume;
    // E2 failure returns the existing Commit error before terminal `finish`,
    // targets the exact Run id, and retains the checkpoint. Decision rules:
    // R1=C1+C2+delete-ok=>E1 (covered above); R2=C1+C2+delete-fail=>E2.
    // Constraints/invariants: the checkpoint store is the existing runtime port;
    // inference tolerance and model failover cannot absorb retirement failure.
    let checkpoints = Arc::new(DeleteFailingCheckpointStore::default());
    let (result, commit) = drive_result(
        Arc::new(DropOnceLlm {
            calls: Default::default(),
            first: "Hel",
            rest: "lo world",
        }),
        checkpoints.clone(),
    )
    .await;

    assert!(
        matches!(
            result,
            Err(awaken_runtime_contract::execution::Error::Commit(ref error))
                if error.contains("injected delete failure")
        ),
        "R2/E2: checkpoint retirement failure must escape terminal handling"
    );
    assert_eq!(
        *checkpoints.deletes.lock().unwrap(),
        vec![RUN_ID.to_string()],
        "R2/E2: retire only the current Run checkpoint"
    );
    assert!(
        matches!(checkpoints.inner.get(RUN_ID).await, Observed::Present(_)),
        "R2/E2: failed retirement preserves resumable truth"
    );
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .all(|message| message.role != Role::Assistant),
        "R2/E2: no terminal assistant output is committed"
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
    assert!(matches!(
        checkpoints.get(RUN_ID).await,
        Observed::Present(_)
    ));
}

#[tokio::test]
async fn filesystem_recovery_waits_for_reopened_committed_predecessor() {
    use awaken_agent_contract::thread::commit::coordinator::Coordinator;
    use awaken_agent_contract::thread::commit::staged::ThreadCommit;
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
    use awaken_runtime_contract::model_operation::{
        ActiveModelOperation, ModelExecutionEvidence, ModelOperation, ModelOperationPhase,
    };
    use awaken_store_fs::{FsCommitCoordinator, FsStreamCheckpointStore};

    // Causes: C1 the exact checkpoint survives a fresh store instance; C2 the
    // same reopened commit log contains no predecessor / a Terminated
    // predecessor; C3 Runtime receives both reopened ports. Effects: E1 no
    // predecessor vetoes recovery with zero model calls and retains the partial;
    // E2 committed termination admits one continuation, commits the stitched
    // output and retires the partial. Decision rules R1=C1+!C2+C3=>E1 and
    // R2=C1+C2+C3=>E2. The positive row prevents a vacuous fail-closed test.
    // Constraint: this is real filesystem persistence and adapter replay, not a
    // power-loss, arbitrary crash-point, kernel/fsync or universal refinement
    // proof; the direct production evidence gate has an independent Kani proof.
    for predecessor_committed in [false, true] {
        let root = tempfile::tempdir().expect("temporary persistence root");
        let commit_dir = root.path().join("commits");
        let checkpoint_dir = root.path().join("checkpoints");
        {
            let checkpoint = FsStreamCheckpointStore::open(&checkpoint_dir).expect("checkpoint");
            checkpoint
                .put(StreamCheckpoint {
                    run_id: RUN_ID.to_string(),
                    thread_id: THREAD_ID.to_string(),
                    model: "m".to_string(),
                    partial_text: "Resumed ".to_string(),
                    partial_tools: Vec::new(),
                    retry_count: 0,
                })
                .await
                .expect("durable checkpoint");
        }
        {
            let commit =
                FsCommitCoordinator::open(&commit_dir, awaken_lease_contract::system_clock())
                    .await
                    .expect("initial commit store");
            if predecessor_committed {
                let mut operation =
                    ModelOperation::next(None, ModelBinding::new("p", "m", "b")).unwrap();
                operation.phase = ModelOperationPhase::Returned;
                operation.execution = ModelExecutionEvidence::Terminated(
                    awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(
                        Some("completed"),
                    )
                    .unwrap(),
                );
                commit
                    .commit(ThreadCommit::assemble(
                        ThreadId(THREAD_ID.to_string()),
                        awaken_runtime_contract::RunDisposition::running(RunId(RUN_ID.to_string())),
                        true,
                        Vec::new(),
                        vec![ActiveModelOperation::write(&Some(operation))],
                        Vec::new(),
                    ))
                    .await
                    .expect("acknowledged predecessor commit");
            }
        }

        let commit = Arc::new(
            FsCommitCoordinator::open(&commit_dir, awaken_lease_contract::system_clock())
                .await
                .expect("reopened commit store"),
        );
        let checkpoints = Arc::new(
            FsStreamCheckpointStore::open(&checkpoint_dir).expect("reopened checkpoint store"),
        );
        let model = Arc::new(AlwaysLlm {
            text: "and done",
            calls: Default::default(),
        });
        let context = RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(commit.clone())
            .with_stream_checkpoint(checkpoints.clone());
        let state = runtime_with(model.clone())
            .execute(activation(), context)
            .await
            .expect("bounded recovery outcome");

        if predecessor_committed {
            assert_eq!(state, RunState::Ended(EndCause::NaturalEnd), "R2/E2");
            assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(
                commit
                    .committed_messages(&ThreadId(THREAD_ID.to_string()))
                    .into_iter()
                    .find(|message| message.role == Role::Assistant)
                    .expect("R2/E2 committed assistant")
                    .text_content(),
                "Resumed and done"
            );
            assert_eq!(checkpoints.get(RUN_ID).await, Observed::Absent, "R2/E2");
        } else {
            assert_eq!(state, RunState::Ended(EndCause::Indeterminate), "R1/E1");
            assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(
                matches!(checkpoints.get(RUN_ID).await, Observed::Present(_)),
                "R1/E1"
            );
        }
    }
}
