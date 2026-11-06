// @formal evidence checkpoint.recovery_never_precedes_commit
//! Checkpoint observation is an execution precondition, not best-effort absence.

use super::*;
use awaken_agent_contract::observation::{Observed, UnknownCause};
use awaken_agent_contract::stream::checkpoint::StreamCheckpointError;

enum ReadOutcome {
    Storage,
    Fenced,
    Value(Option<StreamCheckpoint>),
    DelayedAbsence(std::time::Duration),
    Pending,
}

struct RecoveryTool(AtomicUsize);

#[async_trait::async_trait]
impl awaken_tool_contract::tool::RawTool for RecoveryTool {
    fn id(&self) -> &str {
        "recovery-tool"
    }

    async fn invoke(
        &self,
        call: ToolCall,
    ) -> Result<awaken_tool_contract::tool::ToolOutput, awaken_tool_contract::tool::ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(awaken_tool_contract::tool::ToolOutput::ok(
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
    ) -> GateDecision {
        GateDecision::plugin_gate(GateOutcome::Allow)
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
    async fn get(&self, run_id: &str) -> Observed<StreamCheckpoint> {
        assert_eq!(run_id, "run-1");
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        match &self.outcome {
            ReadOutcome::Storage => Observed::Unknown(UnknownCause::Storage("injected".into())),
            ReadOutcome::Fenced => Observed::Unknown(UnknownCause::Fenced("injected".into())),
            ReadOutcome::Value(Some(checkpoint)) => Observed::Present(checkpoint.clone()),
            ReadOutcome::Value(None) => Observed::Absent,
            ReadOutcome::DelayedAbsence(delay) => {
                tokio::time::sleep(*delay).await;
                Observed::Absent
            }
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
            let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
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
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
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
    let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
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
    let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
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

#[tokio::test(start_paused = true)]
async fn stalled_checkpoint_observation_has_a_deadline_without_cancelling_the_run() {
    // C1 execution/cancel/stop; C2 absent/Terminated predecessor; C3 no token/
    // live uncancelled token; C4 read stays Pending. R1..R12=C1 x C2 x C3:
    // bounded local decision, physical veto, zero model calls or checkpoint
    // writes, unchanged journal. Logical cancel/stop retain their own outcome;
    // execution ends Indeterminate. The read deadline must not cancel the Run
    // token or imply remote stop. Virtual time makes the deadline deterministic.
    for entry in ["execute", "cancel", "stop"] {
        for terminated in [false, true] {
            for with_token in [false, true] {
                let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
                if terminated {
                    commit_terminated_model(&durable).await;
                }
                let before = model_operations(&durable);
                let checkpoints = Arc::new(CheckpointObservation::new(ReadOutcome::Pending));
                let model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
                let token = tokio_util::sync::CancellationToken::new();
                let mut context = RuntimeRunContext::new()
                    .with_commit(durable.clone())
                    .with_reader(durable.clone())
                    .with_stream_checkpoint(checkpoints.clone());
                if with_token {
                    context = context.with_cancellation(token.clone());
                }
                let quiescence = context.physical_quiescence.clone();
                let runtime = Runtime::new().with_llm(model.clone());
                let began = tokio::time::Instant::now();
                let result = tokio::time::timeout(std::time::Duration::from_secs(40), async {
                    match entry {
                        "execute" => runtime.execute(activation("catalog-a"), context).await,
                        "cancel" => {
                            runtime
                                .cancel_run(
                                    RunId("run-1".into()),
                                    ThreadId("thread-1".into()),
                                    context,
                                )
                                .await
                        }
                        _ => {
                            runtime
                                .stop_run(
                                    RunId("run-1".into()),
                                    ThreadId("thread-1".into()),
                                    "budget".into(),
                                    context,
                                )
                                .await
                        }
                    }
                })
                .await
                .expect("recovery read must settle without an external cancellation")
                .unwrap();
                assert_eq!(began.elapsed(), std::time::Duration::from_secs(30));
                assert_eq!(
                    result,
                    RunState::Ended(match entry {
                        "execute" => EndCause::Indeterminate,
                        "cancel" => EndCause::Cancelled,
                        _ => EndCause::Stopped("budget".into()),
                    })
                );
                assert!(!token.is_cancelled());
                assert!(!quiescence.can_acknowledge_return());
                assert_eq!(model.0.load(Ordering::SeqCst), 0);
                assert_eq!(model_operations(&durable), before);
                checkpoints.assert_untouched();
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn successful_slow_checkpoint_observation_preserves_execution_admission() {
    // Positive rules D1..D3: confirmed absence before the 30s deadline is not
    // denied merely for being asynchronous. The response itself ends the read,
    // no fixed sleep, polling, journal fabrication or premature model entry.
    for seconds in [0, 5, 29] {
        let checkpoints = Arc::new(CheckpointObservation::new(ReadOutcome::DelayedAbsence(
            std::time::Duration::from_secs(seconds),
        )));
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let context = RuntimeRunContext::new()
            .with_commit(durable.clone())
            .with_reader(durable.clone())
            .with_stream_checkpoint(checkpoints.clone());
        let quiescence = context.physical_quiescence.clone();
        let began = tokio::time::Instant::now();
        let result = Runtime::new()
            .with_llm(Arc::new(TextLlm("done")))
            .execute(activation("catalog-a"), context)
            .await
            .unwrap();
        assert_eq!(result, RunState::Ended(EndCause::NaturalEnd));
        assert!(began.elapsed() < std::time::Duration::from_secs(30));
        assert!(began.elapsed() >= std::time::Duration::from_secs(seconds));
        assert!(quiescence.can_acknowledge_return());
        assert_eq!(model_operations(&durable).last().unwrap().ordinal.get(), 1);
        assert_eq!(checkpoints.reads.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn checkpoint_observation_precedes_committed_tool_batch_recovery() {
    use awaken_tool_contract::tool_batch::{ActiveToolBatch, ToolBatch};
    // C1 durable Requested tool (safe to enter, not NeverReplay ambiguity);
    // C2 prior model Terminated; C3 checkpoint read Storage/confirmed None.
    // R1 C1+C2+Storage => zero tool effects and no new model ordinal.
    // R2 C1+C2+None => exactly one tool effect and normal completion.
    // R3 prior model Unknown + readable None => no tool effect: checkpoint
    // absence cannot override independent model evidence.
    // R2 proves a missing capability/gate cannot make R1 vacuously pass.
    for (failed, unknown) in [(true, false), (false, false), (false, true)] {
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        commit_model_evidence(
            &durable,
            if unknown {
                awaken_runtime_contract::model_operation::ModelExecutionEvidence::Unknown
            } else {
                awaken_runtime_contract::model_operation::ModelExecutionEvidence::Terminated(
                    awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(
                        Some("completed"),
                    )
                    .unwrap(),
                )
            },
        )
        .await;
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
        awaken_agent_contract::thread::commit::coordinator::Coordinator::commit(
            durable.as_ref(),
            awaken_agent_contract::thread::commit::staged::ThreadCommit::assemble(
                ThreadId("thread-1".into()),
                awaken_agent_contract::thread::commit::staged::RunDisposition::running(RunId(
                    "run-1".into(),
                )),
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
        assert_eq!(
            tool.0.load(Ordering::SeqCst),
            usize::from(!failed && !unknown)
        );
        if failed || unknown {
            assert_eq!(result, RunState::Ended(EndCause::Indeterminate));
            assert_eq!(model_operations(&durable).last().unwrap().ordinal.get(), 1);
            assert_eq!(checkpoints.writes.load(Ordering::SeqCst), 0);
            assert_eq!(checkpoints.deletes.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(result, RunState::Ended(EndCause::NaturalEnd));
            assert_eq!(model_operations(&durable).last().unwrap().ordinal.get(), 2);
        }
    }
}

#[tokio::test]
async fn early_cancellation_preserves_recovered_model_uncertainty() {
    use awaken_runtime_contract::model_operation::ModelExecutionEvidence::Unknown;
    // The provider said so, through the same door an adapter uses.
    let terminated = awaken_runtime_contract::model_operation::ModelExecutionEvidence::Terminated(
        awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
            "completed",
        ))
        .unwrap(),
    );
    // R1 no journal, R2 terminated journal => Cancelled without an invented veto.
    // R3 unknown journal => Cancelled but no physical ACK. Cancellation precedes
    // inference, so the inference-only guard cannot satisfy this test.
    for evidence in [None, Some(terminated), Some(Unknown)] {
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        if let Some(evidence) = evidence {
            commit_model_evidence(&durable, evidence).await;
        }
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        let context = RuntimeRunContext::new()
            .with_reader(durable.clone())
            .with_commit(durable.clone())
            .with_cancellation(token);
        let quiescence = context.physical_quiescence.clone();
        let model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
        let result = Runtime::new()
            .with_llm(model.clone())
            .execute(activation("catalog-a"), context)
            .await
            .unwrap();
        assert_eq!(result, RunState::Ended(EndCause::Cancelled));
        assert_eq!(
            quiescence.can_acknowledge_return(),
            evidence != Some(Unknown)
        );
        assert_eq!(model.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn control_completion_never_supplies_predecessor_stop_evidence() {
    use awaken_runtime_contract::model_operation::ModelExecutionEvidence::Unknown;
    // The provider said so, through the same door an adapter uses.
    let terminated = awaken_runtime_contract::model_operation::ModelExecutionEvidence::Terminated(
        awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
            "completed",
        ))
        .unwrap(),
    );
    // C1 cancel Run / cancel activation / budget stop; C2 absent/terminated/
    // unknown journal; C3 single record / followed by a different completed
    // invocation. Eighteen rules C1 x C2 x C3: preserve the logical outcome and
    // all journal writes; a later completion cannot erase the Unknown veto.
    for entry in ["cancel-run", "cancel-activation", "stop"] {
        for (evidence, later_completed) in [None, Some(terminated), Some(Unknown)]
            .into_iter()
            .flat_map(|evidence| [false, true].map(|later| (evidence, later)))
        {
            let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
            if let Some(evidence) = evidence {
                commit_model_evidence(&durable, evidence).await;
            }
            if later_completed {
                let previous = model_operations(&durable).last().cloned();
                let mut later = awaken_runtime_contract::model_operation::ModelOperation::next(
                    previous.as_ref(),
                    ModelBinding::new("p", "m", "b"),
                )
                .unwrap();
                later.execution = terminated;
                commit_model_operations(&durable, [later]).await;
            }
            let before = model_operations(&durable);
            let context = RuntimeRunContext::new()
                .with_reader(durable.clone())
                .with_commit(durable.clone());
            let quiescence = context.physical_quiescence.clone();
            let runtime = Runtime::new();
            let result = match entry {
                "cancel-run" => {
                    runtime
                        .cancel_run(RunId("run-1".into()), ThreadId("thread-1".into()), context)
                        .await
                }
                "cancel-activation" => {
                    runtime
                        .cancel_activation(activation("catalog-a"), context)
                        .await
                }
                _ => {
                    runtime
                        .stop_run(
                            RunId("run-1".into()),
                            ThreadId("thread-1".into()),
                            "budget".into(),
                            context,
                        )
                        .await
                }
            }
            .unwrap();
            assert_eq!(
                result,
                RunState::Ended(if entry == "stop" {
                    EndCause::Stopped("budget".into())
                } else {
                    EndCause::Cancelled
                })
            );
            assert_eq!(
                quiescence.can_acknowledge_return(),
                evidence != Some(Unknown)
            );
            assert_eq!(model_operations(&durable), before);
        }
    }
}
