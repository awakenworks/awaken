//! Checkpoint observation is an execution precondition, not best-effort absence.

use super::*;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointError;

enum ReadOutcome {
    Storage,
    Fenced,
    Value(Option<StreamCheckpoint>),
    Pending,
}

struct RecoveryTool(AtomicUsize);

#[async_trait::async_trait]
impl awaken_runtime_contract::tool::RawTool for RecoveryTool {
    fn id(&self) -> &str {
        "recovery-tool"
    }

    async fn invoke(
        &self,
        call: ToolCall,
    ) -> Result<awaken_runtime_contract::tool::ToolOutput, awaken_runtime_contract::tool::ToolError>
    {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(awaken_runtime_contract::tool::ToolOutput::ok(
            call.call_id,
            "done",
        ))
    }
}

#[async_trait::async_trait]
impl ToolGateHook for RecoveryTool {
    async fn gate(
        &self,
        _: &ToolCall,
        _: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::Allow
    }
}

struct CheckpointObservation {
    outcome: ReadOutcome,
    entered: tokio::sync::Notify,
    reads: AtomicUsize,
    writes: AtomicUsize,
    deletes: AtomicUsize,
}

impl CheckpointObservation {
    fn new(outcome: ReadOutcome) -> Self {
        Self {
            outcome,
            entered: tokio::sync::Notify::new(),
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        }
    }

    fn assert_untouched(&self) {
        assert_eq!(self.reads.load(Ordering::SeqCst), 1);
        assert_eq!(self.writes.load(Ordering::SeqCst), 0);
        assert_eq!(self.deletes.load(Ordering::SeqCst), 0);
    }
}

#[async_trait::async_trait]
impl StreamCheckpointStore for CheckpointObservation {
    async fn get(&self, run_id: &str) -> Result<Option<StreamCheckpoint>, StreamCheckpointError> {
        assert_eq!(run_id, "run-1");
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        match &self.outcome {
            ReadOutcome::Storage => Err(StreamCheckpointError::Storage("injected".into())),
            ReadOutcome::Fenced => Err(StreamCheckpointError::Fenced("injected".into())),
            ReadOutcome::Value(value) => Ok(value.clone()),
            ReadOutcome::Pending => std::future::pending().await,
        }
    }

    async fn put(&self, _: StreamCheckpoint) -> Result<(), StreamCheckpointError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn delete(&self, _: &str) -> Result<(), StreamCheckpointError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn failed_checkpoint_reads_cannot_become_absence() {
    // Causes: C1 Storage/Fenced; C2 journal absent/confirmed Terminated.
    // Graph: C1 -> unread recovery state -> prohibit model entry and checkpoint
    // mutation, regardless of C2. A terminated journal does not validate unread
    // data. Decision table R1..R4 = C1 x C2 => Indeterminate + physical veto,
    // zero model calls, no new invocation evidence, no checkpoint put/delete.
    for terminated in [false, true] {
        for outcome in [ReadOutcome::Storage, ReadOutcome::Fenced] {
            let durable = Arc::new(MemoryCommitCoordinator::new());
            if terminated {
                commit_terminated_model(&durable).await;
            }
            let before = model_operations(&durable);
            let checkpoints = Arc::new(CheckpointObservation::new(outcome));
            let model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
            let context = RuntimeRunContext::new()
                .with_commit(durable.clone())
                .with_reader(durable.clone())
                .with_stream_checkpoint(checkpoints.clone());
            let quiescence = context.physical_quiescence.clone();
            let result = Runtime::new()
                .with_llm(model.clone())
                .execute(activation("catalog-a"), context)
                .await
                .unwrap();
            assert_eq!(result, RunState::Ended(EndCause::Indeterminate));
            assert_eq!(model.0.load(Ordering::SeqCst), 0);
            assert!(!quiescence.can_acknowledge_return());
            assert_eq!(model_operations(&durable), before);
            checkpoints.assert_untouched();
        }
    }
}

#[tokio::test]
async fn checkpoint_identity_must_match_both_run_and_thread() {
    // Causes: C1 run matches; C2 thread matches; C3 prior termination confirmed.
    // C3 is always true so the model journal cannot mask a missing identity gate.
    // R1 !C1+C2, R2 C1+!C2, R3 !C1+!C2 => same veto as a failed read, no text
    // accepted and no new invocation. R4 C1+C2+C3 is the adjacent positive test
    // restored_checkpoint_with_confirmed_termination_continues_once.
    for (run_matches, thread_matches) in [(false, true), (true, false), (false, false)] {
        let durable = Arc::new(MemoryCommitCoordinator::new());
        commit_terminated_model(&durable).await;
        let (_, mut checkpoint) = interrupted_model_checkpoint().await;
        if !run_matches {
            checkpoint.run_id = "foreign-run".into();
        }
        if !thread_matches {
            checkpoint.thread_id = "foreign-thread".into();
        }
        let checkpoints = Arc::new(CheckpointObservation::new(ReadOutcome::Value(Some(
            checkpoint,
        ))));
        let model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
        let context = RuntimeRunContext::new()
            .with_commit(durable.clone())
            .with_reader(durable.clone())
            .with_stream_checkpoint(checkpoints.clone());
        let quiescence = context.physical_quiescence.clone();
        let before = model_operations(&durable);
        let result = Runtime::new()
            .with_llm(model.clone())
            .execute(activation("catalog-a"), context)
            .await
            .unwrap();
        assert_eq!(result, RunState::Ended(EndCause::Indeterminate));
        assert_eq!(model.0.load(Ordering::SeqCst), 0);
        assert!(!quiescence.can_acknowledge_return());
        assert_eq!(model_operations(&durable), before);
        assert!(
            durable
                .committed()
                .messages
                .iter()
                .all(|m| m.role != Role::Assistant)
        );
        checkpoints.assert_untouched();
    }
}

#[tokio::test]
async fn cancellation_interrupts_pending_checkpoint_read_without_erasing_uncertainty() {
    // C1 get has entered and remains Pending; C2 cancellation arrives; C3 no
    // journal survives. R1 C1+C2+C3 => bounded local Cancelled return, physical
    // veto, no model/journal/checkpoint writes. Notify establishes the window;
    // the timeout is only a test failure bound, never termination evidence.
    let durable = Arc::new(MemoryCommitCoordinator::new());
    let checkpoints = Arc::new(CheckpointObservation::new(ReadOutcome::Pending));
    let model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
    let token = tokio_util::sync::CancellationToken::new();
    let context = RuntimeRunContext::new()
        .with_commit(durable.clone())
        .with_reader(durable.clone())
        .with_stream_checkpoint(checkpoints.clone())
        .with_cancellation(token.clone());
    let quiescence = context.physical_quiescence.clone();
    let runtime = Runtime::new().with_llm(model.clone());
    let execute = runtime.execute(activation("catalog-a"), context);
    let cancel = async {
        checkpoints.entered.notified().await;
        token.cancel();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(execute, cancel)
    })
    .await
    .expect("cancel must interrupt a pending read");
    assert_eq!(result.unwrap(), RunState::Ended(EndCause::Cancelled));
    assert_eq!(model.0.load(Ordering::SeqCst), 0);
    assert!(!quiescence.can_acknowledge_return());
    assert!(model_operations(&durable).is_empty());
    checkpoints.assert_untouched();
}

#[tokio::test]
async fn confirmed_checkpoint_absence_allows_fresh_execution() {
    // R5 successful None + absent journal => one fresh invocation, ordinary
    // completion. Positive control: fail-closed errors must not disable fresh Runs.
    let checkpoints = Arc::new(CheckpointObservation::new(ReadOutcome::Value(None)));
    let durable = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new()
        .with_commit(durable.clone())
        .with_reader(durable.clone())
        .with_stream_checkpoint(checkpoints.clone());
    let quiescence = context.physical_quiescence.clone();
    let result = Runtime::new()
        .with_llm(Arc::new(TextLlm("fresh")))
        .execute(activation("catalog-a"), context)
        .await
        .unwrap();
    assert_eq!(result, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(model_operations(&durable).last().unwrap().ordinal.get(), 1);
    assert!(quiescence.can_acknowledge_return());
    assert_eq!(checkpoints.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn checkpoint_observation_precedes_committed_tool_batch_recovery() {
    use awaken_runtime_contract::tool_batch::{ActiveToolBatch, ToolBatch};
    // C1 durable Requested tool (safe to enter, not NeverReplay ambiguity);
    // C2 prior model Terminated; C3 checkpoint read Storage/confirmed None.
    // R1 C1+C2+Storage => zero tool effects and no new model ordinal.
    // R2 C1+C2+None => exactly one tool effect and normal completion.
    // R2 proves a missing capability/gate cannot make R1 vacuously pass.
    for failed in [true, false] {
        let durable = Arc::new(MemoryCommitCoordinator::new());
        commit_terminated_model(&durable).await;
        let batch = ToolBatch::for_step(
            RunId("run-1".into()),
            0,
            [(
                ToolCall {
                    call_id: "pending-call".into(),
                    tool_id: "recovery-tool".into(),
                    arguments: serde_json::json!({}),
                },
                Default::default(),
            )],
        )
        .unwrap();
        awaken_runtime_contract::CommitCoordinator::commit(
            durable.as_ref(),
            awaken_runtime_contract::ThreadCommit::assemble(
                ThreadId("thread-1".into()),
                awaken_runtime_contract::RunDisposition::running(RunId("run-1".into())),
                true,
                Vec::new(),
                vec![ActiveToolBatch::write(&Some(batch))],
                Vec::new(),
            ),
        )
        .await
        .unwrap();
        let checkpoints = Arc::new(CheckpointObservation::new(if failed {
            ReadOutcome::Storage
        } else {
            ReadOutcome::Value(None)
        }));
        let tool = Arc::new(RecoveryTool(AtomicUsize::new(0)));
        let mut input = activation("catalog-a");
        input
            .snapshot
            .resolved_spec
            .tool_descriptors
            .push(ToolDescriptor::pinned(
                "test",
                "recovery-tool",
                "Recovery effect probe",
                serde_json::json!({"type":"object"}),
            ));
        let result = Runtime::new()
            .with_llm(Arc::new(TextLlm("done")))
            .with_tool(tool.clone())
            .with_gate(tool.clone())
            .execute(
                input,
                RuntimeRunContext::new()
                    .with_commit(durable.clone())
                    .with_reader(durable.clone())
                    .with_stream_checkpoint(checkpoints.clone()),
            )
            .await
            .unwrap();
        assert_eq!(tool.0.load(Ordering::SeqCst), usize::from(!failed));
        if failed {
            assert_eq!(result, RunState::Ended(EndCause::Indeterminate));
            assert_eq!(model_operations(&durable).last().unwrap().ordinal.get(), 1);
            checkpoints.assert_untouched();
        } else {
            assert_eq!(result, RunState::Ended(EndCause::NaturalEnd));
            assert_eq!(model_operations(&durable).last().unwrap().ordinal.get(), 2);
        }
    }
}
