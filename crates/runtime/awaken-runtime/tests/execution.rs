// @formal evidence model_operation.coverage_commit_strictly_precedes_model_entry
// @formal evidence model_operation.ordinal_never_wraps_or_reuses_a_coordinate
// @formal evidence model_operation.reentry_requires_positive_execution_evidence
// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
//! The execution loop runs one model step, commits durable facts, and streams
//! live progress that is independent of committed truth (G1/G13).

#[path = "execution/cancelled_model_observation.rs"]
mod cancelled_model_observation;
#[path = "execution/checkpoint_observation.rs"]
mod checkpoint_observation;
#[path = "execution/tool_domain_preparation.rs"]
mod tool_domain_preparation;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::StateKey;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Delta, Fact};
use awaken_agent_contract::observation::Observed;
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::delegation::{
    DelegationExecutionError, DelegationRequest, DelegationResume, DelegationStep,
    RunDelegationService, RunDelegations,
};
use awaken_runtime_contract::execution::{Error, RunExecutor};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ModelRequestAdmission,
    ModelRequestAdmissionRequest, ModelRequestGate, ModelRequestObservation, StopReason,
    ThreadUsage, TokenUsage, ToolCall,
};
use awaken_runtime_contract::permission::{GateDecision, GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::{
    ADVISOR_FAILURE_NOTICE, ADVISOR_TOOL_ID, ADVISOR_UNAVAILABLE_NOTICE, CatalogFingerprint,
    ContextPolicy, ModelBinding, ResolvedModelCandidate, ResolvedSpec, ToolDescriptor, ToolKind,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::{MemoryCommitCoordinator, MemoryStreamSink, replay_latest_state};

async fn interrupted_model_checkpoint() -> (
    Arc<awaken_store_inmem::MemoryStreamCheckpointStore>,
    StreamCheckpoint,
) {
    let store = Arc::new(awaken_store_inmem::MemoryStreamCheckpointStore::new());
    let checkpoint = StreamCheckpoint {
        run_id: "run-1".into(),
        thread_id: "thread-1".into(),
        model: "model-1".into(),
        partial_text: "retained partial".into(),
        partial_tools: Vec::new(),
        retry_count: 0,
    };
    store.put(checkpoint.clone()).await.unwrap();
    (store, checkpoint)
}

async fn commit_terminated_model(durable: &MemoryCommitCoordinator) {
    commit_model_evidence(
        durable,
        awaken_runtime_contract::model_operation::ModelExecutionEvidence::Terminated(
            awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
                "completed",
            ))
            .unwrap(),
        ),
    )
    .await;
}

async fn commit_model_evidence(
    durable: &MemoryCommitCoordinator,
    evidence: awaken_runtime_contract::model_operation::ModelExecutionEvidence,
) {
    use awaken_runtime_contract::model_operation::{ModelOperation, ModelOperationPhase};
    // Persist the selected predecessor fact through the real state log. A local
    // Returned phase alone cannot turn Unknown into permission to replay.
    let mut prior = ModelOperation::next(None, ModelBinding::new("p", "m", "b")).unwrap();
    prior.phase = ModelOperationPhase::Returned;
    prior.execution = evidence;
    commit_model_operations(durable, [prior]).await;
}

async fn commit_model_operations(
    durable: &MemoryCommitCoordinator,
    operations: impl IntoIterator<Item = awaken_runtime_contract::model_operation::ModelOperation>,
) {
    use awaken_runtime_contract::model_operation::ActiveModelOperation;
    for operation in operations {
        // These positive recovery prefixes originate from a current Configured
        // activation. Record its input at the first fixture commit, before any
        // synthetic model effect. Legacy absence is covered by RI22, not guessed
        // into Configured during recovery.
        use awaken_runtime_contract::permission::{RunToolCapability, ToolCapabilityNarrowing};
        let mut view = awaken_tool_contract::tool_batch::store_from_run_commands(
            durable.committed_state(&ThreadId("thread-1".into())),
            &RunId("run-1".into()),
        );
        let mut state = Vec::new();
        RunToolCapability::stage(
            &mut view,
            &mut state,
            ToolCapabilityNarrowing::Configured,
            durable.run(&RunId("run-1".into())).is_none(),
        )
        .unwrap();
        state.push(ActiveModelOperation::write(&Some(operation)));
        awaken_runtime_contract::CommitCoordinator::commit(
            durable,
            awaken_runtime_contract::ThreadCommit::assemble(
                ThreadId("thread-1".into()),
                awaken_runtime_contract::RunDisposition::running(RunId("run-1".into())),
                true,
                Vec::new(),
                state,
                Vec::new(),
            ),
        )
        .await
        .unwrap();
    }
}

fn committed_model_requests(store: &MemoryCommitCoordinator) -> Vec<ModelRequestObservation> {
    store
        .committed()
        .events
        .iter()
        .filter_map(|record| ModelRequestObservation::from_record(record).transpose())
        .collect::<Result<Vec<_>, _>>()
        .expect("canonical model-request audit payload")
}

fn model_operations(
    store: &MemoryCommitCoordinator,
) -> Vec<awaken_runtime_contract::model_operation::ModelOperation> {
    use awaken_agent_contract::agent::state::Action;
    use awaken_runtime_contract::model_operation::ActiveModelOperation;
    store
        .committed_state(&ThreadId("thread-1".into()))
        .into_iter()
        .filter(|command| command.key.0 == ActiveModelOperation::KEY)
        .map(|command| {
            assert_eq!(command.run_id, Some(RunId("run-1".into())));
            let Action::Set(value) = command.action else {
                panic!("model operation must be Set")
            };
            serde_json::from_value(value).unwrap()
        })
        .collect()
}

/// A deterministic provider that always answers with fixed text.
struct TextLlm(&'static str);

struct ModelEvidenceFault {
    durable: Arc<MemoryCommitCoordinator>,
    phase: awaken_runtime_contract::model_operation::ModelOperationPhase,
    lose_ack: bool,
    pause: Option<Arc<ModelEvidencePause>>,
}

struct ModelEvidencePause {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    entries: AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_runtime_contract::CommitCoordinator for ModelEvidenceFault {
    async fn commit(
        &self,
        commit: awaken_runtime_contract::ThreadCommit,
    ) -> Result<awaken_runtime_contract::CommitRecord, awaken_runtime_contract::CommitError> {
        use awaken_agent_contract::agent::state::Action;
        use awaken_runtime_contract::model_operation::{ActiveModelOperation, ModelOperation};
        let fault = commit.state.iter().any(|command| {
            if command.key.0 != ActiveModelOperation::KEY {
                return false;
            }
            let Action::Set(value) = &command.action else {
                return false;
            };
            serde_json::from_value::<ModelOperation>(value.clone())
                .unwrap()
                .phase
                == self.phase
        });
        if fault {
            if let Some(pause) = &self.pause {
                pause.entries.fetch_add(1, Ordering::SeqCst);
                pause.entered.notify_one();
                pause.release.acquire().await.unwrap().forget();
                return self.durable.commit(commit).await;
            }
            if self.lose_ack {
                self.durable.commit(commit).await?;
            }
            Err(awaken_runtime_contract::CommitError::Rejected(
                "injected model evidence failure".into(),
            ))
        } else {
            self.durable.commit(commit).await
        }
    }
}

#[tokio::test]
async fn model_evidence_write_faults_never_retry_or_fail_over_execution() {
    use awaken_runtime_contract::model_operation::ModelOperationPhase::{Entered, Returned};
    // Cause/effect graph: C1 failure at Entered/Returned; C2 before commit/
    // lost ACK; C3 retry, model-pool failover and outer failure tolerance enabled.
    // E1 fail at the commit boundary, not a provider failure; E2 no additional
    // model calls; E3 exact durable prefix; E4 no invented final Run/usage fact.
    // Decision table:
    // R1 Entered+before => calls=0, phases=[]
    // R2 Entered+lostACK => calls=0, phases=[Entered]
    // R3 Returned+before => calls=1, phases=[Entered]
    // R4 Returned+lostACK => calls=1, phases=[Entered,Returned]
    // All rows include C3; none may be absorbed as a new inference step.
    // C4 a prior stream checkpoint and confirmed predecessor exist => E5
    // preserve them after every evidence failure. R1..R4 list only NEW writes;
    // local error return does not mean recovery completed.
    for (phase, lose_ack, calls, expected) in [
        (Entered, false, 0, vec![]),
        (Entered, true, 0, vec![Entered]),
        (Returned, false, 1, vec![Entered]),
        (Returned, true, 1, vec![Entered, Returned]),
    ] {
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let (checkpoints, prior_checkpoint) = interrupted_model_checkpoint().await;
        commit_terminated_model(&durable).await;
        let model = Arc::new(RetryOnceLlm(AtomicUsize::new(0)));
        let mut request = activation("catalog-a");
        request
            .snapshot
            .resolved_spec
            .model_candidates
            .push(ResolvedModelCandidate::host(ModelBinding::new(
                "fallback-provider",
                "fallback-model",
                "fallback-backend",
            )));
        let result = Runtime::new()
            .with_llm(model.clone())
            .with_max_consecutive_inference_failures(3)
            .execute(
                request,
                RuntimeRunContext::new()
                    .with_reader(durable.clone())
                    .with_stream_checkpoint(checkpoints.clone())
                    .with_commit(Arc::new(ModelEvidenceFault {
                        durable: durable.clone(),
                        phase,
                        lose_ack,
                        pause: None,
                    })),
            )
            .await;
        assert!(
            matches!(result, Err(Error::Commit(_))),
            "{phase:?}/{lose_ack}"
        );
        assert_eq!(model.0.load(Ordering::SeqCst), calls);
        assert_eq!(
            model_operations(&durable)
                .iter()
                .skip(1) // Exclude the confirmed predecessor, not the faulting writes.
                .map(|op| op.phase)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            durable.run(&RunId("run-1".into())).unwrap().state,
            RunState::Running
        );
        assert!(committed_model_requests(&durable).is_empty());
        assert_eq!(
            checkpoints.get("run-1").await,
            Observed::Present(prior_checkpoint),
            "E5"
        );
    }
}

struct BlockedModelEvidence {
    durable: Arc<MemoryCommitCoordinator>,
    entered: Arc<tokio::sync::Notify>,
    scope: awaken_runtime_contract::model_operation::ModelEffectScope,
}

#[tokio::test]
async fn cancellation_drains_model_evidence_commit_before_terminalizing() {
    use awaken_runtime_contract::model_operation::ModelOperationPhase::{Entered, Returned};
    // C1 cancel during entry/return evidence persistence; C2 commit ACK held.
    // E1 retain the owned commit future until its result; E2 no later provider
    // invocation; E3 one ordered Cancelled terminal, no duplicate draft.
    // R10=entry+C2=>calls0,[Entered,Returned(NotDispatched)];
    // R11=return+C2=>calls1,[Entered,Returned]. The live R10 producer knows the
    // executor was never constructed; a crash before this second commit still
    // leaves Entered/Unknown and remains fenced. Cancellation cannot erase a
    // historical unknown or manufacture provider termination.
    // Both cancel before releasing C2. This catches an outer select dropping
    // the whole inference future while its durable write is still in flight.
    for phase in [Entered, Returned] {
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let (checkpoints, prior_checkpoint) = interrupted_model_checkpoint().await;
        commit_terminated_model(&durable).await;
        let pause = Arc::new(ModelEvidencePause {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
            entries: AtomicUsize::new(0),
        });
        let cancellation = tokio_util::sync::CancellationToken::new();
        let model = Arc::new(RetryOnceLlm(AtomicUsize::new(0)));
        let runtime = Runtime::new().with_llm(model.clone());
        let context = RuntimeRunContext::new()
            .with_reader(durable.clone())
            .with_stream_checkpoint(checkpoints.clone())
            .with_cancellation(cancellation.clone())
            .with_commit(Arc::new(ModelEvidenceFault {
                durable: durable.clone(),
                phase,
                lose_ack: false,
                pause: Some(pause.clone()),
            }));
        let driving =
            tokio::spawn(async move { runtime.execute(activation("catalog-a"), context).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), pause.entered.notified())
            .await
            .unwrap();
        cancellation.cancel();
        tokio::task::yield_now().await;
        assert!(!driving.is_finished(), "owned commit has not acknowledged");
        pause.release.add_permits(1);
        let state = tokio::time::timeout(std::time::Duration::from_secs(2), driving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::Cancelled));
        // R10/R11 also preserve interrupted-stream recovery state: cancellation
        // is not a successful consumption/terminal acknowledgement of it.
        assert_eq!(
            checkpoints.get("run-1").await,
            Observed::Present(prior_checkpoint)
        );
        assert_eq!(
            pause.entries.load(Ordering::SeqCst),
            1,
            "the commit future was retained, not dropped and re-created"
        );
        assert_eq!(
            model.0.load(Ordering::SeqCst),
            usize::from(phase == Returned)
        );
        assert_eq!(
            model_operations(&durable)
                .iter()
                .skip(1) // The original confirmed operation survives cancellation.
                .map(|op| op.phase)
                .collect::<Vec<_>>(),
            vec![Entered, Returned]
        );
        if phase == Entered {
            use awaken_runtime_contract::model_operation::{
                ModelExecutionEvidence, PreDispatchStage,
            };
            let latest = model_operations(&durable).last().cloned().unwrap();
            assert!(
                matches!(latest.execution, ModelExecutionEvidence::NotDispatched(proof)
                if proof.stage() == PreDispatchStage::RuntimeAdmission)
            );
        }
        assert_eq!(
            durable
                .committed()
                .run_facts
                .iter()
                .filter(|fact| matches!(fact.state, RunState::Ended(_)))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn restored_model_history_cannot_hide_an_older_unknown_invocation() {
    use awaken_runtime_contract::model_operation::{ModelExecutionEvidence, ModelOperation};
    // Cause/effect table: C1 older invocation Unknown/Terminated; C2 its
    // terminal receipt absent/present after a newer invocation completed.
    // R1 Unknown+absent => Indeterminate, zero model calls, physical ACK veto.
    // R2 Terminated+absent and R3 Unknown+present => normal execution, ordinal
    // strictly above EVERY retained invocation (a late receipt is not a rewind).
    // The same real committed log feeds Native and the protocol projection;
    // these historical inputs do not authorize creating overlapping new calls.
    for (older_stopped, late_receipt) in [(false, false), (true, false), (false, true)] {
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let mut older = ModelOperation::next(None, ModelBinding::new("p", "m", "b")).unwrap();
        if older_stopped {
            older.execution = ModelExecutionEvidence::Terminated(
                awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
                    "completed",
                ))
                .unwrap(),
            );
        }
        let mut newer = ModelOperation::next(Some(&older), older.binding.clone()).unwrap();
        newer.execution = ModelExecutionEvidence::Terminated(
            awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
                "completed",
            ))
            .unwrap(),
        );
        let mut operations = vec![older.clone(), newer];
        if late_receipt {
            older.execution = ModelExecutionEvidence::Terminated(
                awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
                    "completed",
                ))
                .unwrap(),
            );
            operations.push(older);
        }
        commit_model_operations(&durable, operations).await;
        let model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
        let context = RuntimeRunContext::new()
            .with_commit(durable.clone())
            .with_reader(durable.clone());
        let quiescence = context.physical_quiescence.clone();
        let result = Runtime::new()
            .with_llm(model.clone())
            .execute(activation("catalog-a"), context)
            .await
            .unwrap();
        if older_stopped || late_receipt {
            assert_eq!(result, RunState::Ended(EndCause::NaturalEnd));
            assert_eq!(model.0.load(Ordering::SeqCst), 2);
            assert_eq!(model_operations(&durable).last().unwrap().ordinal.get(), 4);
            assert!(quiescence.can_acknowledge_return());
        } else {
            assert_eq!(result, RunState::Ended(EndCause::Indeterminate));
            assert_eq!(model.0.load(Ordering::SeqCst), 0);
            assert!(!quiescence.can_acknowledge_return());
        }
    }
}

#[tokio::test]
async fn restored_model_history_rejects_damage_hidden_by_a_valid_latest_value() {
    use awaken_agent_contract::agent::state::Action;
    use awaken_runtime_contract::model_operation::{
        ActiveModelOperation, ModelExecutionEvidence, ModelOperation,
    };
    use awaken_runtime_contract::{CommitCoordinator, RunDisposition, ThreadCommit};
    // D1 malformed/zero/null/Remove/rebinding followed by a valid latest value;
    // D2 damage owned by this Run or another Run. R1 owned => Commit error,
    // no model entry and sticky physical veto; R2 foreign => ordinary execution.
    // Cross all five damage kinds with D2. Contract tests additionally cover
    // every binding field, merge mode and historical commit-prefix coordinate.
    for foreign in [false, true] {
        for damage in 0..5 {
            let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
            let mut first = ModelOperation::next(None, ModelBinding::new("p", "m", "b")).unwrap();
            first.execution = ModelExecutionEvidence::Terminated(
                awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
                    "completed",
                ))
                .unwrap(),
            );
            let mut broken = ActiveModelOperation::write(&Some(first.clone()));
            match damage {
                0 => broken.action = Action::Set(serde_json::json!("malformed")),
                1 => {
                    let Action::Set(value) = &mut broken.action else {
                        unreachable!()
                    };
                    value["ordinal"] = 0.into();
                }
                2 => broken.action = Action::Set(serde_json::Value::Null),
                3 => broken.action = Action::Remove,
                _ => {
                    let mut changed = first.clone();
                    let (provider, model, backend) = changed.binding.identity();
                    changed.binding =
                        ModelBinding::parse(provider, format!("{model}x"), backend).unwrap();
                    broken = ActiveModelOperation::write(&Some(changed));
                }
            }
            for command in [
                ActiveModelOperation::write(&Some(first.clone())),
                broken,
                ActiveModelOperation::write(&Some(first)),
            ] {
                durable
                    .commit(ThreadCommit::assemble(
                        ThreadId("thread-1".into()),
                        RunDisposition::running(RunId(
                            if foreign { "foreign" } else { "run-1" }.into(),
                        )),
                        true,
                        Vec::new(),
                        vec![command],
                        Vec::new(),
                    ))
                    .await
                    .unwrap();
            }
            let model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
            let context = RuntimeRunContext::new()
                .with_commit(durable.clone())
                .with_reader(durable);
            let quiescence = context.physical_quiescence.clone();
            let result = Runtime::new()
                .with_llm(model.clone())
                .execute(activation("catalog-a"), context)
                .await;
            if foreign {
                assert_eq!(result.unwrap(), RunState::Ended(EndCause::NaturalEnd));
                assert_eq!(model.0.load(Ordering::SeqCst), 2);
                assert!(quiescence.can_acknowledge_return());
            } else {
                assert!(matches!(result, Err(Error::Commit(_))), "damage {damage}");
                assert_eq!(model.0.load(Ordering::SeqCst), 0);
                assert!(!quiescence.can_acknowledge_return());
            }
        }
    }
}

#[tokio::test]
async fn restored_tool_history_rejects_damage_hidden_by_a_valid_latest_batch() {
    use awaken_agent_contract::agent::state::{Action, MergePolicy};
    use awaken_runtime_contract::{CommitCoordinator, RunDisposition, ThreadCommit};
    use awaken_tool_contract::tool::{ToolOutput, ToolRecoveryPolicy};
    use awaken_tool_contract::tool_batch::{ActiveToolBatch, ToolBatch};
    // T1 owned/foreign history; T2 malformed/wrong merge followed by the
    // exact valid latest batch. R1 owned+damage => Commit error,
    // zero model entry, no state append, sticky physical-return veto. R2 foreign
    // damage => ordinary execution. R3 typed write(None) and Remove are both
    // legitimate clear boundaries and permit execution for either owner.
    // The real commit owner retains all three
    // writes; a materialized latest-value read alone would miss every R1 row.
    for foreign in [false, true] {
        for damage in 0..4 {
            let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
            let mut batch = ToolBatch::for_step(
                RunId("run-1".into()),
                0,
                [(
                    ToolCall {
                        call_id: "retained".into(),
                        tool_id: "tool".into(),
                        arguments: serde_json::json!({}),
                    },
                    ToolRecoveryPolicy::replay_safe(),
                )],
            )
            .unwrap();
            batch
                .complete_immediate(ToolOutput::ok("retained", "done"))
                .unwrap();
            batch.finalize().unwrap();
            let valid = ActiveToolBatch::write(&Some(batch));
            let mut broken = valid.clone();
            match damage {
                0 => broken.action = Action::Set(serde_json::json!("malformed")),
                1 => broken = ActiveToolBatch::write(&None),
                2 => broken = ActiveToolBatch::remove(),
                _ => broken.merge = MergePolicy::Exclusive,
            }
            for command in [valid.clone(), broken, valid] {
                durable
                    .commit(ThreadCommit::assemble(
                        ThreadId("thread-1".into()),
                        RunDisposition::running(RunId(
                            if foreign { "foreign" } else { "run-1" }.into(),
                        )),
                        true,
                        Vec::new(),
                        vec![command],
                        Vec::new(),
                    ))
                    .await
                    .unwrap();
            }
            let before = durable.committed().state;
            let model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
            let context = RuntimeRunContext::new()
                .with_commit(durable.clone())
                .with_reader(durable.clone());
            let quiescence = context.physical_quiescence.clone();
            let outcome = Runtime::new()
                .with_llm(model.clone())
                .execute(activation("catalog-a"), context)
                .await;
            if foreign || matches!(damage, 1 | 2) {
                assert_eq!(
                    outcome.unwrap(),
                    RunState::Ended(EndCause::NaturalEnd),
                    "R2/{damage}"
                );
                assert_eq!(model.0.load(Ordering::SeqCst), 2, "R2/{damage}");
                assert!(quiescence.can_acknowledge_return(), "R2/{damage}");
            } else {
                assert!(matches!(outcome, Err(Error::Commit(_))), "R1/{damage}");
                assert_eq!(model.0.load(Ordering::SeqCst), 0, "R1/{damage}");
                assert_eq!(durable.committed().state, before, "R1 no append/{damage}");
                assert!(!quiescence.can_acknowledge_return(), "R1/{damage}");
            }
        }
    }
}

#[tokio::test]
async fn restored_model_evidence_is_run_scoped_and_invalid_coordinates_fail_closed() {
    use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope};
    use awaken_runtime_contract::model_operation::{
        ActiveModelOperation, ModelOperationPhase::Returned,
    };
    use awaken_runtime_contract::{RunDisposition, ThreadCommit};
    // C1 prior state belongs to this/another Run; C2 ordinal valid/zero/MAX;
    // E1 next exact ordinal; E2 foreign state cannot advance this Run;
    // E3 malformed/exhausted owned state admits no provider invocation.
    // R7=owned+valid=>E1; R8=foreign+any=>E2;
    // R9=owned+(zero|MAX)=>E3. Reconstruction must reuse store_from_run_commands.
    for (owner, ordinal, expected) in [
        ("run-1", 7, Some(8)),
        ("foreign-run", 7, Some(1)),
        ("foreign-run", 0, Some(1)),
        ("run-1", 0, None),
        ("run-1", u64::MAX, None),
    ] {
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        awaken_runtime_contract::CommitCoordinator::commit(
            durable.as_ref(),
            ThreadCommit::assemble(
                ThreadId("thread-1".into()),
                RunDisposition::running(RunId(owner.into())),
                true,
                Vec::new(),
                vec![Command::set(
                    Scope::Run,
                    MergePolicy::Disjoint,
                    ActiveModelOperation::KEY,
                    serde_json::json!({
                        "ordinal":ordinal,
                        "binding":ModelBinding::new("old-provider", "old-model", "old-backend"),
                        "phase":Returned,
                        "execution":"Terminated"
                    }),
                )],
                Vec::new(),
            ),
        )
        .await
        .unwrap();
        let model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
        let result = Runtime::new()
            .with_llm(model.clone())
            .execute(
                activation("catalog-a"),
                RuntimeRunContext::new()
                    .with_commit(durable.clone())
                    .with_reader(durable.clone()),
            )
            .await;
        if let Some(expected) = expected {
            assert!(result.is_ok(), "{owner}/{ordinal}");
            let commands = durable.committed_state(&ThreadId("thread-1".into()));
            let own = awaken_tool_contract::tool_batch::store_from_run_commands(
                commands,
                &RunId("run-1".into()),
            );
            let latest = ActiveModelOperation::load(&own).unwrap().unwrap();
            // MaxTokensThenTextLlm requires two calls; both continue this Run's
            // coordinate, independent of model step ids and another Run's state.
            assert_eq!(latest.ordinal.get(), expected + 1);
            assert_eq!(latest.phase, Returned);
            assert_eq!(model.0.load(Ordering::SeqCst), 2);
        } else {
            assert!(matches!(result, Err(Error::Commit(_))));
            assert_eq!(model.0.load(Ordering::SeqCst), 0);
        }
    }
}

#[async_trait::async_trait]
impl LlmExecutor for BlockedModelEvidence {
    fn effect_scope(
        &self,
        _: &ChatRequest,
    ) -> awaken_runtime_contract::model_operation::ModelEffectScope {
        self.scope
    }

    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::model_operation::ModelOperationPhase::Entered;
        // An assertion at the actual provider boundary, not after execution.
        let operations = model_operations(&self.durable);
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].phase, Entered);
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn dropped_and_timed_out_model_futures_preserve_distinct_durable_evidence() {
    use awaken_runtime_contract::model_operation::ModelOperationPhase::{Entered, TimedOut};
    // C1 abort while the real executor is pending / C2 watchdog expiry.
    // E1 start evidence precedes entry; E2 abort leaves Entered; E3 timeout
    // records TimedOut, never Returned or provider quiescence. Rules:
    // R5=C1=>E1+E2; R6=C2=>E1+E3. The existing Dispatch SIGKILL test owns
    // physical exclusion; this test owns the new model evidence boundary.
    // R7: retry + failover + outer tolerance enabled must still admit only one
    // invocation after an abandoned Future; its existing quiescence latch vetoes
    // the Worker ACK. Timeout ends Indeterminate, not a known provider failure.
    // R8 scoped response-only + either abandonment cause => Unknown remains,
    // continuation/retry/failover stay forbidden, but a drained local consumer
    // may ACK governed return. R9 opaque + either cause => physical veto.
    // This crosses the actual journal/executor/watchdog seam for all four rows;
    // the scope is a test adapter contract, not provider stop evidence.
    for (timeout, scope) in [false, true].into_iter().flat_map(|timeout| {
        [
            awaken_runtime_contract::model_operation::ModelEffectScope::Opaque,
            awaken_runtime_contract::model_operation::ModelEffectScope::ResponseOnly,
        ]
        .into_iter()
        .map(move |scope| (timeout, scope))
    }) {
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let entered = Arc::new(tokio::sync::Notify::new());
        let model = Arc::new(BlockedModelEvidence {
            durable: durable.clone(),
            entered: entered.clone(),
            scope,
        });
        let runtime = Runtime::new()
            .with_llm(model)
            .with_retry_policy(awaken_runtime::RetryPolicy {
                max_retries: 3,
                // Watchdog = response window + return allowance: 30ms or 5s.
                budget: if timeout {
                    awaken_runtime_contract::llm::ModelInferenceBudget::new(
                        std::time::Duration::from_millis(90),
                        std::time::Duration::from_millis(20),
                        std::time::Duration::from_millis(20),
                        std::time::Duration::from_millis(10),
                    )
                    .unwrap()
                } else {
                    awaken_runtime_contract::llm::ModelInferenceBudget::new(
                        std::time::Duration::from_millis(15000),
                        std::time::Duration::from_millis(4990),
                        std::time::Duration::from_millis(4990),
                        std::time::Duration::from_millis(10),
                    )
                    .unwrap()
                },
                ..Default::default()
            })
            .with_max_consecutive_inference_failures(3);
        let context = RuntimeRunContext::new().with_commit(durable.clone());
        let quiescence = context.physical_quiescence.clone();
        let mut request = activation("catalog-a");
        request
            .snapshot
            .resolved_spec
            .model_candidates
            .push(ResolvedModelCandidate::host(ModelBinding::new(
                "fallback", "fallback", "fallback",
            )));
        let driving = tokio::spawn(async move { runtime.execute(request, context).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        if timeout {
            assert_eq!(
                driving.await.unwrap().unwrap(),
                RunState::Ended(EndCause::Indeterminate)
            );
        } else {
            driving.abort();
            assert!(driving.await.unwrap_err().is_cancelled());
        }
        assert_eq!(
            quiescence.can_acknowledge_return(),
            scope == awaken_runtime_contract::model_operation::ModelEffectScope::ResponseOnly,
            "R8/R9 local ACK is distinct from provider termination"
        );
        assert!(!quiescence.permits_execution(), "R8/R9 no continuation");
        for operation in model_operations(&durable) {
            assert_eq!(operation.effect_scope, scope, "scope pinned before entry");
            assert_eq!(
                operation.execution,
                awaken_runtime_contract::model_operation::ModelExecutionEvidence::Unknown
            );
        }
        assert_eq!(
            model_operations(&durable)
                .iter()
                .map(|op| op.phase)
                .collect::<Vec<_>>(),
            if timeout {
                vec![Entered, TimedOut]
            } else {
                vec![Entered]
            }
        );
    }
}

#[tokio::test]
async fn provider_operation_context_follows_committed_invocations_across_retry_and_restore() {
    use awaken_runtime_contract::model_operation::{
        ModelExecutionEvidence, ModelOperationContext, ModelOperationPhase,
        current_model_operation_context,
    };
    // J1 fresh Run; J2 reconstructed terminal predecessor; J3 retry after a
    // proven NotDispatched error. Every actual provider entry must observe its
    // already committed Entered record, exact Run/Thread/binding and advancing
    // ordinal. The same context survives adapter awaits; it never escapes the
    // invocation. Existing rejected/lost-entry-write cases assert zero entries.
    struct RecordingModel {
        commit: Arc<MemoryCommitCoordinator>,
        seen: std::sync::Mutex<Vec<ModelOperationContext>>,
    }
    #[async_trait::async_trait]
    impl LlmExecutor for RecordingModel {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            let identity = current_model_operation_context().expect("Runtime supplies identity");
            let recorded = model_operations(&self.commit).pop().unwrap();
            assert_eq!(recorded.phase, ModelOperationPhase::Entered);
            assert_eq!(recorded.binding, request.model_binding);
            assert_eq!(
                identity,
                ModelOperationContext::from_operation(
                    &ThreadId("thread-1".into()),
                    &RunId("run-1".into()),
                    &recorded
                )
            );
            tokio::task::yield_now().await;
            assert_eq!(current_model_operation_context(), Some(identity.clone()));
            let first = {
                let mut seen = self.seen.lock().unwrap();
                seen.push(identity);
                seen.len() == 1
            };
            if first {
                Err(
                    awaken_runtime_contract::llm::Error::Provider("before send".into())
                        .with_execution_evidence(ModelExecutionEvidence::NotDispatched(awaken_runtime_contract::model_operation::NoDispatchProof::request_projection())),
                )
            } else {
                TextLlm("done").infer(request).await
            }
        }
    }
    for restored in [false, true] {
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        if restored {
            commit_terminated_model(&commit).await;
        }
        let model = Arc::new(RecordingModel {
            commit: commit.clone(),
            seen: Default::default(),
        });
        let runtime =
            Runtime::new()
                .with_llm(model.clone())
                .with_retry_policy(awaken_runtime::RetryPolicy {
                    max_retries: 1,
                    backoff_base_ms: 0,
                    overloaded_backoff_base_ms: 0,
                    ..Default::default()
                });
        let result = runtime
            .execute(
                activation("catalog-a"),
                RuntimeRunContext::new()
                    .with_commit(commit.clone())
                    .with_reader(commit.clone()),
            )
            .await
            .unwrap();
        assert_eq!(result, RunState::Ended(EndCause::NaturalEnd));
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_ne!(seen[0].operation_id(), seen[1].operation_id());
        let entries: Vec<_> = model_operations(&commit)
            .into_iter()
            .filter(|op| op.phase == ModelOperationPhase::Entered)
            .map(|op| op.ordinal.get())
            .collect();
        assert_eq!(entries, if restored { vec![2, 3] } else { vec![1, 2] });
        assert_eq!(current_model_operation_context(), None);
    }
}

#[tokio::test]
async fn unjournaled_runtime_masks_an_outer_model_identity() {
    use awaken_runtime_contract::model_operation::{
        ModelOperation, ModelOperationContext, current_model_operation_context,
        with_model_operation_context,
    };
    // J4 no commit owner => no reconstructible invocation identity. Even when
    // a caller has an outer model scope, this direct Runtime must mask it at
    // provider entry. Returning from the inner scope restores the caller only.
    struct DirectModel;
    #[async_trait::async_trait]
    impl LlmExecutor for DirectModel {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            assert_eq!(current_model_operation_context(), None);
            TextLlm("done").infer(request).await
        }
    }
    let record = ModelOperation::next(None, ModelBinding::new("outer", "outer", "outer")).unwrap();
    let outer = ModelOperationContext::from_operation(
        &ThreadId("outer".into()),
        &RunId("outer".into()),
        &record,
    );
    with_model_operation_context(Some(outer.clone()), async {
        let result = Runtime::new()
            .with_llm(Arc::new(DirectModel))
            .execute(activation("catalog-a"), RuntimeRunContext::new())
            .await
            .unwrap();
        assert_eq!(result, RunState::Ended(EndCause::NaturalEnd));
        assert_eq!(current_model_operation_context(), Some(outer));
    })
    .await;
    assert_eq!(current_model_operation_context(), None);
}

struct ReturnedUnknownModel {
    calls: AtomicUsize,
    timeout: bool,
}

#[async_trait::async_trait]
impl LlmExecutor for ReturnedUnknownModel {
    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn awaken_runtime_contract::llm::DeltaSink,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        sink.on_text("partial answer").await;
        // Complete JSON is not permission to execute a tool while the model's
        // physical completion is unknown.
        sink.on_tool_call_delta("call-unknown", "must-not-execute", "{}")
            .await;
        self.infer(request).await
    }

    async fn infer(&self, _: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // The adapter future RETURNS normally; the remote operation need not
        // have ended. This is distinct from the adjacent dropped-future test.
        Err(if self.timeout {
            awaken_runtime_contract::llm::Error::Timeout("adapter deadline".into())
        } else {
            awaken_runtime_contract::llm::Error::Provider("EOF before protocol end".into())
        })
    }
}

#[tokio::test]
async fn returned_unknown_model_error_blocks_retry_failover_and_outer_tolerance() {
    // Cause/effect: C1 adapter returns timeout or transport error normally;
    // C2 no provider termination proof; C3 retries, failover and outer failure
    // tolerance all enabled. R1=C1+C2+C3 => one invocation, Indeterminate,
    // retained physical veto. The healthy-return observer alone cannot prove
    // remote quiescence. R2 existing checkpoint => retained, not consumed.
    for timeout in [false, true] {
        let model = Arc::new(ReturnedUnknownModel {
            calls: AtomicUsize::new(0),
            timeout,
        });
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let (checkpoints, checkpoint) = interrupted_model_checkpoint().await;
        commit_terminated_model(&durable).await;
        let runtime = Runtime::new()
            .with_llm(model.clone())
            .with_retry_policy(awaken_runtime::RetryPolicy {
                max_retries: 2,
                backoff_base_ms: 0,
                overloaded_backoff_base_ms: 0,
                ..Default::default()
            })
            .with_max_consecutive_inference_failures(3);
        let mut request = activation("catalog-a");
        request
            .snapshot
            .resolved_spec
            .model_candidates
            .push(ResolvedModelCandidate::host(ModelBinding::new(
                "fallback", "fallback", "fallback",
            )));
        let context = RuntimeRunContext::new()
            .with_commit(durable.clone())
            .with_reader(durable.clone())
            .with_stream_checkpoint(checkpoints.clone());
        let quiescence = context.physical_quiescence.clone();
        let result = runtime.execute(request, context).await.unwrap();
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            1,
            "R1: no hidden re-entry"
        );
        assert_eq!(result, RunState::Ended(EndCause::Indeterminate));
        assert!(!quiescence.can_acknowledge_return());
        assert_eq!(
            model_operations(&durable).last().unwrap().execution,
            awaken_runtime_contract::model_operation::ModelExecutionEvidence::Unknown
        );
        assert_eq!(
            checkpoints.get(&checkpoint.run_id).await,
            Observed::Present(checkpoint)
        );
    }
}

#[tokio::test]
async fn restored_unknown_invocation_blocks_checkpoint_salvage_and_provider_reentry() {
    use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope};
    use awaken_runtime_contract::model_operation::ActiveModelOperation;
    use awaken_runtime_contract::{RunDisposition, ThreadCommit};
    // U1 journal absent, or legacy phase Entered/Returned/TimedOut with
    // absent/explicit Unknown;
    // U2 complete tool JSON in checkpoint. Effects: zero new model invocations,
    // no salvaged tool, retained checkpoint, Indeterminate and physical veto.
    // U1+U2 must behave identically in a fresh Runtime (no surviving local latch).
    // Adjacent coordinate test covers explicit Terminated and cross-Run isolation.
    for phase in ["Missing", "Entered", "Returned", "TimedOut"] {
        for explicit in [false, true] {
            if phase == "Missing" && explicit {
                continue; // No record also means no field; this is one rule.
            }
            let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
            let mut value = serde_json::json!({"ordinal":7,
                "binding":ModelBinding::new("p", "m", "b"), "phase":phase});
            if explicit {
                value["execution"] = serde_json::json!("Unknown");
            }
            if phase != "Missing" {
                awaken_runtime_contract::CommitCoordinator::commit(
                    durable.as_ref(),
                    ThreadCommit::assemble(
                        ThreadId("thread-1".into()),
                        RunDisposition::running(RunId("run-1".into())),
                        true,
                        Vec::new(),
                        vec![Command::set(
                            Scope::Run,
                            MergePolicy::Disjoint,
                            ActiveModelOperation::KEY,
                            value,
                        )],
                        Vec::new(),
                    ),
                )
                .await
                .unwrap();
            }
            let (checkpoints, mut checkpoint) = interrupted_model_checkpoint().await;
            checkpoint.partial_tools.push(
                awaken_agent_contract::stream::checkpoint::PartialToolCall {
                    call_id: "old-call".into(),
                    tool_id: "must-not-execute".into(),
                    raw_arguments: "{}".into(),
                },
            );
            checkpoints.put(checkpoint.clone()).await.unwrap();
            let model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
            let context = RuntimeRunContext::new()
                .with_commit(durable.clone())
                .with_reader(durable.clone())
                .with_stream_checkpoint(checkpoints.clone());
            let quiescence = context.physical_quiescence.clone();
            let state = Runtime::new()
                .with_llm(model.clone())
                .execute(activation("catalog-a"), context)
                .await
                .unwrap();
            assert_eq!(
                state,
                RunState::Ended(EndCause::Indeterminate),
                "{phase}/{explicit}"
            );
            assert_eq!(model.0.load(Ordering::SeqCst), 0);
            assert!(!quiescence.can_acknowledge_return());
            assert_eq!(
                checkpoints.get(&checkpoint.run_id).await,
                Observed::Present(checkpoint)
            );
            if phase == "Missing" {
                assert!(
                    model_operations(&durable).is_empty(),
                    "no fabricated evidence"
                );
            } else {
                assert_eq!(model_operations(&durable).last().unwrap().ordinal.get(), 7);
            }
        }
    }
}

#[tokio::test]
async fn restored_checkpoint_with_confirmed_termination_continues_once() {
    // P1 persisted predecessor Terminated; P2 retained text checkpoint.
    // P1+P2 => exact next ordinal, joined response, checkpoint consumed.
    // This positive control excludes a blanket checkpoint-recovery shutdown;
    // the adjacent table denies missing/unknown evidence even with tool JSON.
    let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    commit_terminated_model(&durable).await;
    let (checkpoints, checkpoint) = interrupted_model_checkpoint().await;
    let state = Runtime::new()
        .with_llm(Arc::new(TextLlm("continued")))
        .execute(
            activation("catalog-a"),
            RuntimeRunContext::new()
                .with_commit(durable.clone())
                .with_reader(durable.clone())
                .with_stream_checkpoint(checkpoints.clone()),
        )
        .await
        .unwrap();
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    let messages = durable.committed().messages;
    let assistant: Vec<_> = messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .collect();
    assert_eq!(assistant.len(), 1);
    assert_eq!(
        assistant[0].text_content(),
        format!("{}continued", checkpoint.partial_text)
    );
    let operations = model_operations(&durable);
    assert_eq!(operations.last().unwrap().ordinal.get(), 2);
    assert_eq!(checkpoints.get(&checkpoint.run_id).await, Observed::Absent);
}

struct AdvisorThenTextLlm {
    primary_calls: AtomicUsize,
    expected_tool_result: &'static str,
}

struct FailedAdvisorThenTextLlm {
    primary_calls: AtomicUsize,
}

struct SuspendAdvisorGate;

#[async_trait::async_trait]
impl ToolGateHook for SuspendAdvisorGate {
    async fn gate(
        &self,
        call: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateDecision {
        assert_eq!(call.tool_id, ADVISOR_TOOL_ID, "D4/C1");
        GateDecision::plugin_gate(GateOutcome::RequireConfirmation {
            correlation_id: "advisor-approval".into(),
        })
    }
}

struct SequencedModelRequestGate {
    decisions: std::sync::Mutex<std::collections::VecDeque<ModelRequestAdmission>>,
    requests: std::sync::Mutex<Vec<ModelRequestAdmissionRequest>>,
}

impl SequencedModelRequestGate {
    fn new(decisions: impl IntoIterator<Item = ModelRequestAdmission>) -> Self {
        Self {
            decisions: std::sync::Mutex::new(decisions.into_iter().collect()),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ModelRequestAdmissionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ModelRequestGate for SequencedModelRequestGate {
    async fn admit_model_request(
        &self,
        request: ModelRequestAdmissionRequest,
    ) -> Result<ModelRequestAdmission, String> {
        self.requests.lock().unwrap().push(request);
        self.decisions
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "test gate received an unexpected logical model request".into())
    }
}

struct MaxTokensThenTextLlm(AtomicUsize);

#[async_trait::async_trait]
impl LlmExecutor for MaxTokensThenTextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let call = self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            output: AssistantOutput::text(if call == 0 {
                "completed request at the cap"
            } else {
                "must not be sampled"
            }),
            usage: Some(TokenUsage {
                prompt_tokens: 7,
                completion_tokens: 3,
                ..Default::default()
            }),
            stop_reason: Some(if call == 0 {
                StopReason::MaxTokens
            } else {
                StopReason::NaturalEnd
            }),
        })
    }
}

struct PrimaryFailureThenTextLlm {
    models: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl LlmExecutor for PrimaryFailureThenTextLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let model = request.model_binding.identity().1.to_owned();
        self.models.lock().unwrap().push(model.clone());
        if model == "model-1" {
            return Err(awaken_runtime_contract::llm::Error::Provider(
                "primary rejected before dispatch".into(),
            )
            .with_execution_evidence(
                awaken_runtime_contract::model_operation::ModelExecutionEvidence::NotDispatched(
                    awaken_runtime_contract::model_operation::NoDispatchProof::request_projection(),
                ),
            ));
        }
        Ok(ChatResponse {
            output: AssistantOutput::text("fallback must not be sampled"),
            usage: None,
            stop_reason: Some(StopReason::NaturalEnd),
        })
    }
}

struct RetryOnceLlm(AtomicUsize);

#[async_trait::async_trait]
impl LlmExecutor for RetryOnceLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(awaken_runtime_contract::llm::Error::Timeout(
                "provider-confirmed terminal deadline".into(),
            )
            .with_execution_evidence(
                awaken_runtime_contract::model_operation::ModelExecutionEvidence::Terminated(
                    awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(
                        Some("completed"),
                    )
                    .unwrap(),
                ),
            ));
        }
        Ok(ChatResponse {
            output: AssistantOutput::text("retry completed"),
            usage: None,
            stop_reason: Some(StopReason::NaturalEnd),
        })
    }
}

/// Runtime-only proof that advisor is routed through the same durable
/// delegation capability as every other child Run. The Host integration tests
/// own child dispatch/Thread persistence; this fixture only verifies the
/// runtime boundary and usage fold.
struct AdvisorDelegation {
    starts: AtomicUsize,
    terminal_failure: Option<&'static str>,
}

#[async_trait::async_trait]
impl RunDelegationService for AdvisorDelegation {
    fn tool_id(&self) -> &str {
        ADVISOR_TOOL_ID
    }

    fn target_agent_id(
        &self,
        _arguments: &serde_json::Value,
    ) -> Result<AgentId, DelegationExecutionError> {
        Ok(AgentId("__managed_advisor__".into()))
    }

    fn handles_tool(&self, tool_id: &str) -> bool {
        tool_id == ADVISOR_TOOL_ID || tool_id == "agent_run"
    }

    fn target_agent_id_for(
        &self,
        tool_id: &str,
        _arguments: &serde_json::Value,
    ) -> Result<AgentId, DelegationExecutionError> {
        match tool_id {
            ADVISOR_TOOL_ID => Ok(AgentId("__managed_advisor__".into())),
            "agent_run" => Ok(AgentId("worker".into())),
            other => Err(DelegationExecutionError::new(format!(
                "unexpected tool {other}"
            ))),
        }
    }

    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.target_agent_id.0, "__managed_advisor__", "A1");
        if let Some(message) = self.terminal_failure {
            return Err(DelegationExecutionError::new(message));
        }
        let mut usage = ThreadUsage::default();
        usage.record(
            "advisor-model",
            TokenUsage {
                prompt_tokens: 7,
                completion_tokens: 3,
                ..Default::default()
            },
        );
        Ok(DelegationStep::Ended {
            text: "independent advice".into(),
            usage,
        })
    }

    async fn resume(
        &self,
        _request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        unreachable!("advisor consultations are terminal-only")
    }
}

struct HiddenDelegationThenTextLlm {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for HiddenDelegationThenTextLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "hidden-agent-run".into(),
                tool_id: "agent_run".into(),
                arguments: serde_json::json!({"agent_id":"worker","input":"do work"}),
            }])
        } else {
            assert!(request.messages.iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(block, ContentBlock::ToolResult { content, .. }
                        if awaken_agent_contract::agent::content::extract_text(content)
                            .contains("unknown tool"))
                })
            }));
            AssistantOutput::text("primary survived")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[async_trait::async_trait]
impl LlmExecutor for AdvisorThenTextLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        assert_eq!(
            request.model_binding.identity().1,
            "model-1",
            "M12/K1: Runtime must never sample the advisor provider directly"
        );
        let output = if self.primary_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "advisor-call".into(),
                tool_id: ADVISOR_TOOL_ID.into(),
                arguments: serde_json::json!({}),
            }])
        } else {
            assert!(
                request
                    .messages
                    .iter()
                    .any(|message| message.content.iter().any(|block| matches!(
                        block,
                        ContentBlock::ToolResult { content, .. }
                            if awaken_agent_contract::agent::content::extract_text(content)
                                == self.expected_tool_result
                    ))),
                "M12/E2"
            );
            AssistantOutput::text("primary done")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[async_trait::async_trait]
impl LlmExecutor for FailedAdvisorThenTextLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if self.primary_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "advisor-call".into(),
                tool_id: ADVISOR_TOOL_ID.into(),
                arguments: serde_json::json!({}),
            }])
        } else {
            let result = request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .find_map(|block| match block {
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => Some((
                        awaken_agent_contract::agent::content::extract_text(content),
                        *is_error,
                    )),
                    _ => None,
                })
                .expect("F1 generic advisor ToolResult");
            assert_eq!(result.0, ADVISOR_FAILURE_NOTICE, "F1/E1");
            assert!(result.1, "F1/E1 remains an error result");
            assert!(!result.0.contains("sensitive"), "F1/E2");
            AssistantOutput::text("primary survived advisor failure")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text(self.0.to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

fn activation(fingerprint: &str) -> RunActivation {
    let fingerprint = CatalogFingerprint(fingerprint.to_string());
    RunActivation::new(
        RunId("run-1".to_string()),
        ThreadId("thread-1".to_string()),
        ExecutableAgentSnapshot {
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
                    ModelBinding::new(
                        "provider-1".to_string(),
                        "model-1".to_string(),
                        "backend-1".to_string(),
                    ),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        vec![Message {
            id: MessageId("message-1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("hello")],
        }],
    )
}

#[tokio::test]
async fn native_attempt_coverage_is_committed_only_before_a_fresh_closed_execution_surface() {
    use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
    use awaken_runtime_contract::attempt_termination::{
        ActiveNativeAttemptCoverage, NativeAttemptCoverage,
    };
    use awaken_runtime_contract::model_operation::ActiveModelOperation;
    use awaken_runtime_contract::{CommitCoordinator, RunDisposition, ThreadCommit};

    // Cause/effect decision table:
    // R1 fresh Run + no tools/plugins => one coverage marker is committed before
    //    model entry; terminal model evidence then proves all execution stopped.
    // R2 fresh Run + ordinary tool surface => Journalled marker before entry;
    //    an uncalled tool grants no effect and cannot invent an active participant.
    // R3 pre-existing Run without the marker => Runtime never backfills it after
    //    an earlier execution window; coordinator classification remains Unknown.
    // R4 a pre-existing model-only marker + a wider recovered tool surface =>
    //    reject before model entry; the earlier closed profile remains intact.
    // R5 delegated tool-bearing Run may share a parent's Sandbox => no closed
    //    profile, even with ordinary tools and a complete model response.
    // The same committed snapshot drives the production classifier. No fixture
    // injects a termination receipt or a parallel coverage store.
    for row in [
        "fresh-closed",
        "fresh-tool",
        "delegated-tool",
        "existing",
        "existing-covered-tool",
    ] {
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        if row.starts_with("existing") {
            let state = (row == "existing-covered-tool")
                .then(|| ActiveNativeAttemptCoverage::write(&NativeAttemptCoverage::ModelOnlyV1))
                .into_iter()
                .collect();
            durable
                .commit(ThreadCommit::assemble(
                    ThreadId("thread-1".into()),
                    RunDisposition::running(RunId("run-1".into())),
                    true,
                    Vec::new(),
                    state,
                    Vec::new(),
                ))
                .await
                .unwrap();
        }
        let mut request = activation("coverage");
        if matches!(
            row,
            "fresh-tool" | "delegated-tool" | "existing-covered-tool"
        ) {
            request
                .snapshot
                .resolved_spec
                .tool_descriptors
                .push(ToolDescriptor::pinned(
                    "test",
                    "tool",
                    "tool",
                    serde_json::json!({"type":"object","properties":{}}),
                ));
        }
        if row == "delegated-tool" {
            request.delegation_origin = Some(
                awaken_agent_contract::agent::delegation::DelegationOrigin::root_for_agent(
                    RunId("parent".into()),
                    "call",
                    "parent-agent",
                ),
            );
        }
        let result = Runtime::new()
            .with_llm(Arc::new(TextLlm("done")))
            .execute(
                request,
                RuntimeRunContext::new()
                    .with_reader(durable.clone())
                    .with_commit(durable.clone()),
            )
            .await;
        if row == "existing-covered-tool" {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("exceeds its durable coverage"),
                "R4"
            );
        } else {
            assert_eq!(
                result.unwrap(),
                RunState::Ended(EndCause::NaturalEnd),
                "{row}"
            );
        }
        let snapshot = durable
            .recovery_snapshot(&ThreadId("thread-1".into()), &RunId("run-1".into()))
            .await
            .unwrap();
        let coverage = snapshot
            .state
            .iter()
            .zip(&snapshot.state_commit_cursors)
            .filter(|(command, _)| command.key.0 == ActiveNativeAttemptCoverage::KEY)
            .collect::<Vec<_>>();
        let model_entry_cursor = snapshot
            .state
            .iter()
            .zip(&snapshot.state_commit_cursors)
            .find_map(|(command, cursor)| {
                (command.key.0 == ActiveModelOperation::KEY).then_some(*cursor)
            });
        if row == "fresh-closed" || row == "fresh-tool" {
            assert_eq!(coverage.len(), 1, "R1");
            let profile = serde_json::to_value(if row == "fresh-tool" {
                NativeAttemptCoverage::JournalledV1
            } else {
                NativeAttemptCoverage::ModelOnlyV1
            })
            .unwrap();
            assert!(
                matches!(&coverage[0].0.action, awaken_agent_contract::agent::state::Action::Set(value) if *value == profile)
            );
            assert!(coverage[0].1 < &model_entry_cursor.unwrap(), "R1 ordering");
        } else if row == "existing-covered-tool" {
            assert_eq!(coverage.len(), 1, "R4 retained marker");
            assert!(model_entry_cursor.is_none(), "R4 no model entry");
        } else {
            assert!(coverage.is_empty(), "R3 {row}");
            assert!(model_entry_cursor.is_some(), "R3 model entry");
        }
    }
}

fn advisor_activation(with_candidate: bool) -> RunActivation {
    let mut activation = activation("catalog-a");
    activation.snapshot.resolved_spec.tool_descriptors = vec![
        ToolDescriptor::pinned(
            "managed:advisor",
            ADVISOR_TOOL_ID,
            "consult",
            serde_json::json!({"type":"object"}),
        )
        .with_kind(ToolKind::Advisor)
        .with_recovery(awaken_tool_contract::tool::ToolRecoveryPolicy::durable_request()),
    ];
    if with_candidate {
        activation
            .snapshot
            .resolved_spec
            .plugin_config
            .agent
            .advisor = Some(
            awaken_runtime_contract::agent_bindings::AgentAdvisorBinding {
                model: "claude-opus-5".into(),
                candidate: ResolvedModelCandidate::host(ModelBinding::new(
                    "provider-1",
                    "advisor-model",
                    "backend-1",
                )),
            },
        );
    }
    activation
}

/// Test design for the split engine critical path.
/// Causes: C1=fresh Execute, C2=deterministic text response, C3=commit and live
/// sink configured. Effects: E1=NaturalEnd, E2=input/reply and state facts are
/// committed, E3=live deltas remain separate from replay truth. Rule R1:
/// C1+C2+C3 -> E1+E2+E3. This crosses run_commands, run_loop, inference,
/// progress, and finalize without relying on module-local implementation detail.
/// Constraints/invariants: committed facts are replay authority while live deltas
/// are observational and cannot substitute for the durable commits.
#[tokio::test]
async fn one_model_step_commits_facts_and_streams_progress() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("hi there")));

    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let sink = Arc::new(MemoryStreamSink::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(sink.clone());

    let outcome = runtime
        .execute(activation("catalog-a"), context)
        .await
        .expect("run executes");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    // Committed truth: the input commits at the first step boundary (under a
    // Running fact), the assistant reply with the terminal fact. The user
    // input is committed so the next Step sees it.
    // R1 now includes the two operation-evidence commits around actual model
    // entry. They add no transcript message and no logical usage observation.
    assert_eq!(commit.commit_count(), 4);
    let committed = commit.committed();
    assert_eq!(committed.messages.len(), 2);
    assert_eq!(committed.messages[0].text_content(), "hello");
    assert_eq!(committed.messages[0].role, Role::User);
    assert_eq!(committed.messages[1].text_content(), "hi there");
    assert_eq!(committed.messages[1].role, Role::Assistant);

    // Replay reads committed facts, not the live stream.
    assert_eq!(
        replay_latest_state(&committed, &RunId("run-1".to_string())),
        Some(RunState::Ended(EndCause::NaturalEnd))
    );
    // Lifecycle and model observations share the same committed audit log.
    // Two operation StatePatch facts supplement the existing lifecycle/usage
    // facts; they must not create another model usage observation.
    // The original five lifecycle/model facts remain, plus exactly one
    // StatePatch for the first Run ceiling; no extra model observation.
    assert_eq!(committed.events.len(), 6);
    assert_eq!(
        awaken_runtime_contract::permission::RunToolCapability::recorded(
            &committed.state,
            &RunId("run-1".into()),
        )
        .unwrap(),
        Some(awaken_runtime_contract::permission::ToolCapabilityNarrowing::Configured)
    );
    assert_eq!(
        committed
            .state
            .iter()
            .filter(|command| command.key.0 == "runtime.run_tool_capability.v1")
            .count(),
        1
    );
    assert_eq!(
        committed_model_requests(&commit),
        vec![ModelRequestObservation::default()]
    );

    // Live stream order is RunStarted -> OutputText -> RunFinished.
    let kinds = sink.events();
    assert!(matches!(kinds[0].kind, AgentEvent::Fact(Fact::RunStarted)));
    assert!(matches!(
        kinds[1].kind,
        AgentEvent::Delta(Delta::TextDelta { .. })
    ));
    assert!(matches!(
        kinds[2].kind,
        AgentEvent::Fact(Fact::RunFinished { .. })
    ));
}

#[tokio::test]
async fn model_request_gate_fences_every_logical_request_but_not_provider_retries() {
    // Cause/effect graph: C1 a gate admits the first logical request; C2 that
    // request completes with a MaxTokens continuation and committed usage; C3
    // the next continuation or failover request is denied; C4 a provider retry
    // stays inside one logical request. Effects: E1 C2 remains committed; E2 C3
    // commits one same-Run BudgetReached ticket before any denied Provider call;
    // E3 gate coordinates always name the exact Run/Thread; E4 C4 samples twice
    // behind one gate decision. Provider attempts are not a second admission
    // owner, and an admitted request is never revoked midway through its result.
    //
    // | Rule | First request | Next logical request | Provider attempts | Effect |
    // |---|---|---|---:|---|
    // | G0 | denied | no request | 0 | E2+E3, no model audit |
    // | G1 | admitted MaxTokens | continuation denied | 1 | E1+E2+E3 |
    // | G2 | admitted primary failure | failover denied | 1 | E2+E3 |
    // | G3 | admitted retryable failure | no new request | 2 | E3+E4 |
    // Constraints/invariants: admission occurs once per logical request, before
    // Provider I/O; transparent retries never become separately gated requests.
    let paused_model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
    let paused_gate = Arc::new(SequencedModelRequestGate::new([
        ModelRequestAdmission::Pause(
            awaken_agent_contract::agent::awaiting::PauseReason::BudgetReached,
        ),
    ]));
    let paused_commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let paused = Runtime::new()
        .with_llm(paused_model.clone())
        .execute(
            activation("catalog-a"),
            RuntimeRunContext::new()
                .with_commit(paused_commit.clone())
                .with_model_request_gate(paused_gate.clone()),
        )
        .await
        .expect("G0 pauses before provider sampling");
    assert_eq!(paused, RunState::Awaiting, "G0/E2");
    assert_eq!(paused_model.0.load(Ordering::SeqCst), 0, "G0/E2");
    assert!(
        model_operations(&paused_commit).is_empty(),
        "G0 no invocation evidence"
    );
    assert_eq!(paused_gate.requests().len(), 1, "G0/E3");
    assert!(
        committed_model_requests(&paused_commit).is_empty(),
        "G0/no audit"
    );

    let continuation_model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
    let continuation_gate = Arc::new(SequencedModelRequestGate::new([
        ModelRequestAdmission::Admit,
        ModelRequestAdmission::Pause(
            awaken_agent_contract::agent::awaiting::PauseReason::BudgetReached,
        ),
    ]));
    let continuation_commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let continuation = Runtime::new()
        .with_llm(continuation_model.clone())
        .execute(
            activation("catalog-a"),
            RuntimeRunContext::new()
                .with_commit(continuation_commit.clone())
                .with_model_request_gate(continuation_gate.clone()),
        )
        .await
        .expect("G1 pauses after the completed request");
    assert_eq!(continuation, RunState::Awaiting, "G1/E2");
    assert_eq!(continuation_model.0.load(Ordering::SeqCst), 1, "G1/E1+E2");
    let continuation_requests = continuation_gate.requests();
    assert_eq!(continuation_requests.len(), 2, "G1/E3");
    assert!(continuation_requests.iter().all(|request| {
        request.run_id == RunId("run-1".into()) && request.thread_id == ThreadId("thread-1".into())
    }));
    let continuation_ticket = continuation_commit
        .resume_ticket(&RunId("run-1".into()))
        .expect("G1 committed pause ticket");
    assert_eq!(
        continuation_ticket.reason(),
        awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached,
        "G1/E2"
    );
    assert_eq!(continuation_ticket.run_id, RunId("run-1".into()), "G1/E2");
    assert!(
        continuation_commit
            .committed_messages(&ThreadId("thread-1".into()))
            .iter()
            .any(|message| message.text_content() == "completed request at the cap"),
        "G1/E1"
    );
    let continuation_observations = committed_model_requests(&continuation_commit);
    assert_eq!(continuation_observations.len(), 1, "G1/E1+E2");
    assert!(!continuation_observations[0].is_error, "G1/E1");
    assert_eq!(
        continuation_observations[0].usage,
        TokenUsage {
            prompt_tokens: 7,
            completion_tokens: 3,
            ..Default::default()
        },
        "G1/E1"
    );

    let failover_model = Arc::new(PrimaryFailureThenTextLlm {
        models: Default::default(),
    });
    let failover_gate = Arc::new(SequencedModelRequestGate::new([
        ModelRequestAdmission::Admit,
        ModelRequestAdmission::Pause(
            awaken_agent_contract::agent::awaiting::PauseReason::BudgetReached,
        ),
    ]));
    let failover_commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let mut failover_activation = activation("catalog-a");
    failover_activation
        .snapshot
        .resolved_spec
        .model_candidates
        .push(ResolvedModelCandidate::host(ModelBinding::new(
            "provider-2",
            "model-2",
            "backend-2",
        )));
    let no_retries = awaken_runtime::RetryPolicy {
        max_retries: 0,
        ..Default::default()
    };
    let failover = Runtime::new()
        .with_retry_policy(no_retries)
        .with_llm(failover_model.clone())
        .execute(
            failover_activation,
            RuntimeRunContext::new()
                .with_commit(failover_commit.clone())
                .with_model_request_gate(failover_gate.clone()),
        )
        .await
        .expect("G2 pauses before failover sampling");
    assert_eq!(failover, RunState::Awaiting, "G2/E2");
    assert_eq!(
        failover_model.models.lock().unwrap().as_slice(),
        &["model-1"],
        "G2/E2"
    );
    assert_eq!(failover_gate.requests().len(), 2, "G2/E3");
    let failover_observations = committed_model_requests(&failover_commit);
    assert_eq!(failover_observations.len(), 1, "G2/E2");
    assert!(failover_observations[0].is_error, "G2/E2");

    let retry_model = Arc::new(RetryOnceLlm(AtomicUsize::new(0)));
    let retry_gate = Arc::new(SequencedModelRequestGate::new([
        ModelRequestAdmission::Admit,
    ]));
    let one_retry = awaken_runtime::RetryPolicy {
        max_retries: 1,
        backoff_base_ms: 0,
        overloaded_backoff_base_ms: 0,
        ..Default::default()
    };
    let retry_commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let retried = Runtime::new()
        .with_retry_policy(one_retry)
        .with_llm(retry_model.clone())
        .execute(
            activation("catalog-a"),
            RuntimeRunContext::new()
                .with_commit(retry_commit.clone())
                .with_model_request_gate(retry_gate.clone()),
        )
        .await
        .expect("G3 transparent retry completes");
    assert_eq!(retried, RunState::Ended(EndCause::NaturalEnd), "G3/E4");
    assert_eq!(retry_model.0.load(Ordering::SeqCst), 2, "G3/E4");
    // G3: each real retry has its own monotonic invocation coordinate, but
    // remains under the same logical request and its one usage observation.
    use awaken_runtime_contract::model_operation::ModelOperationPhase::{Entered, Returned};
    assert_eq!(
        model_operations(&retry_commit)
            .iter()
            .map(|operation| (operation.ordinal.get(), operation.phase))
            .collect::<Vec<_>>(),
        [(1, Entered), (1, Returned), (2, Entered), (2, Returned)]
    );
    assert_eq!(retry_gate.requests().len(), 1, "G3/E4");
    assert_eq!(
        committed_model_requests(&retry_commit),
        vec![ModelRequestObservation {
            retry_count: 1,
            ..Default::default()
        }],
        "G3/E4"
    );
}

#[tokio::test]
async fn model_preparation_and_quota_pause_cannot_mask_a_participant_veto() {
    // P0 clean preparation/admission -> ordinary Admit or BudgetReached pause.
    // P1 a participant veto appears during content preparation -> no quota
    // admission, no model intent or provider entry, Indeterminate.
    // P2 veto appears during quota admission -> neither Admit nor Pause can
    // hide it; no intent/provider entry and Indeterminate. Both awaits return
    // normally: their response is not evidence for the independent participant.
    struct Boundary {
        at: u8,
        pause: bool,
        evidence: awaken_runtime_contract::runtime_context::AttemptQuiescence,
        admissions: AtomicUsize,
        inferences: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::ModelContentMaterializer for Boundary {
        async fn materialize(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatRequest> {
            if self.at == 1 {
                tokio::task::yield_now().await;
                self.evidence.mark_uncertain();
            }
            Ok(request)
        }
    }
    #[async_trait::async_trait]
    impl ModelRequestGate for Boundary {
        async fn admit_model_request(
            &self,
            _: ModelRequestAdmissionRequest,
        ) -> Result<ModelRequestAdmission, String> {
            self.admissions.fetch_add(1, Ordering::SeqCst);
            if self.at == 2 {
                tokio::task::yield_now().await;
                self.evidence.mark_uncertain();
            }
            Ok(if self.pause {
                ModelRequestAdmission::Pause(
                    awaken_agent_contract::agent::awaiting::PauseReason::BudgetReached,
                )
            } else {
                ModelRequestAdmission::Admit
            })
        }
    }
    #[async_trait::async_trait]
    impl LlmExecutor for Boundary {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            self.inferences.fetch_add(1, Ordering::SeqCst);
            TextLlm("done").infer(request).await
        }
    }
    for at in 0..=2 {
        for pause in [false, true] {
            let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
            let mut context = RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit.clone());
            let boundary = Arc::new(Boundary {
                at,
                pause,
                evidence: context.physical_quiescence.clone(),
                admissions: AtomicUsize::new(0),
                inferences: AtomicUsize::new(0),
            });
            context.model_content_materializer = Some(boundary.clone());
            context.model_request_gate = Some(boundary.clone());
            let result = Runtime::new()
                .with_llm(boundary.clone())
                .execute(activation("catalog-a"), context)
                .await
                .unwrap();
            assert_eq!(
                result,
                if at != 0 {
                    RunState::Ended(EndCause::Indeterminate)
                } else if pause {
                    RunState::Awaiting
                } else {
                    RunState::Ended(EndCause::NaturalEnd)
                },
                "P{at} pause={pause}"
            );
            assert_eq!(
                boundary.admissions.load(Ordering::SeqCst),
                usize::from(at != 1),
                "P{at} pause={pause}"
            );
            assert_eq!(
                boundary.inferences.load(Ordering::SeqCst),
                usize::from(at == 0 && !pause),
                "P{at} pause={pause}"
            );
            assert_eq!(boundary.evidence.can_acknowledge_return(), at == 0);
            if at != 0 || pause {
                assert!(model_operations(&commit).is_empty());
            }
        }
    }
}

#[tokio::test]
async fn execution_fails_closed_on_fingerprint_mismatch() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("hi")));
    let mut activation = activation("catalog-a");
    activation.snapshot.resolved_spec.catalog_fingerprint =
        CatalogFingerprint("catalog-b".to_string());

    let result = runtime.execute(activation, RuntimeRunContext::new()).await;
    let error = result.unwrap_err();
    assert_eq!(error.code(), "resolution_failed");
    assert!(
        std::error::Error::source(&error)
            .unwrap()
            .is::<awaken_runtime_contract::resolver::Error>()
    );
}

#[tokio::test]
async fn execution_does_not_require_an_installed_catalog() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("hi")));
    let outcome = runtime
        .execute(activation("catalog-a"), RuntimeRunContext::new())
        .await
        .expect("snapshot is the execution authority");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
}

#[tokio::test]
async fn execution_requires_a_model_provider() {
    // Test design — Cause: a valid frozen activation is executed without an
    // installed model provider. Effect: execution fails with the Runtime
    // configuration diagnostic before inference. Constraints/invariants: the snapshot is
    // configuration authority but cannot fabricate an executable provider.
    // Decision rule M1=no provider=>fail closed; the installed-provider success
    // partition is covered by `execution_does_not_require_an_installed_catalog`.
    let runtime = Runtime::new();
    let result = runtime
        .execute(activation("catalog-a"), RuntimeRunContext::new())
        .await;
    let error = result.unwrap_err();
    assert_eq!(error.code(), "missing_model_provider");
    assert_eq!(
        error.kind(),
        awaken_runtime_contract::diagnostic::RuntimeErrorKind::InvalidConfiguration
    );
}

#[tokio::test]
async fn advisor_call_uses_the_durable_delegation_capability_and_folds_usage() {
    // M12 cause/effect graph: C1=the frozen snapshot publishes Advisor and its
    // candidate, C2=one Host delegation service accepts that capability, and
    // C3=the child ends with text and usage. Effects: E1=the Host service starts
    // exactly once, E2=the text becomes the ordered ToolResult, E3=the primary
    // reaches NaturalEnd, and E4=the generic child-usage fold remains intact.
    //
    // Decision table:
    // | Rule | descriptor | Host service | child boundary | effects     |
    // | D1   | Advisor    | accepts      | text + usage   | E1+E2+E3+E4 |
    // Constraint K1: the Host service is the sole Advisor executor; Runtime
    // samples only the primary model and keeps the generic delivery/usage path.
    let model = Arc::new(AdvisorThenTextLlm {
        primary_calls: AtomicUsize::new(0),
        expected_tool_result: "independent advice",
    });
    let service = Arc::new(AdvisorDelegation {
        starts: AtomicUsize::new(0),
        terminal_failure: None,
    });
    let runtime = Runtime::new()
        .with_llm(model.clone())
        .with_run_delegation(service.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let result = runtime
        .execute(
            advisor_activation(true),
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("D1");
    assert_eq!(result, RunState::Ended(EndCause::NaturalEnd), "D1/E3");
    assert_eq!(service.starts.load(Ordering::SeqCst), 1, "D1/E1");
    assert_eq!(model.primary_calls.load(Ordering::SeqCst), 2, "D1/K1");
    assert_eq!(
        commit
            .committed_messages(&ThreadId("thread-1".into()))
            .last()
            .unwrap()
            .text_content(),
        "primary done"
    );
    let usage =
        ThreadUsage::from_committed_state(&commit.committed_state(&ThreadId("thread-1".into())));
    assert_eq!(usage.by_model["advisor-model"].prompt_tokens, 7, "D1/E4");
    assert_eq!(
        usage.by_model["advisor-model"].completion_tokens, 3,
        "D1/E4"
    );
}

#[tokio::test]
async fn advisor_without_a_host_service_is_consistently_unavailable_without_provider_sampling() {
    // M12 cause/effect graph: C1=the exact Advisor descriptor and candidate are
    // frozen, C2=no Host delegation service accepts the call, C3=authorization
    // is either immediate or suspended then approved, and C4=the primary can
    // consume a ToolResult. Effects: E1=no child relationship is created,
    // E2=no advisor provider request is sampled, E3=the result is exactly
    // ADVISOR_UNAVAILABLE_NOTICE, and E4=the primary reaches NaturalEnd.
    //
    // | Rule | authorization         | Host service | Effects     |
    // | D2   | immediate             | absent       | E1+E2+E3+E4 |
    // | D4   | permission -> approve | absent       | E1+E2+E3+E4 |
    // Constraints K1/K2: a published candidate is configuration, not a second
    // Runtime execution path; approval changes authorization only and cannot
    // install an Advisor executor. ScheduledAction shares the same approved seam
    // and its wait-kind/precommit partition is owned by the scheduled suite.
    for (rule, requires_approval) in [("D2", false), ("D4", true)] {
        let model = Arc::new(AdvisorThenTextLlm {
            primary_calls: AtomicUsize::new(0),
            expected_tool_result: ADVISOR_UNAVAILABLE_NOTICE,
        });
        let runtime = Runtime::new().with_llm(model.clone());
        let runtime = if requires_approval {
            runtime.with_gate(Arc::new(SuspendAdvisorGate))
        } else {
            runtime
        };
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));

        let initial = runtime
            .execute(
                advisor_activation(true),
                RuntimeRunContext::new().with_commit(commit.clone()),
            )
            .await
            .expect(rule);
        let state = if requires_approval {
            assert_eq!(initial, RunState::Awaiting, "{rule}/C3");
            assert_eq!(model.primary_calls.load(Ordering::SeqCst), 1, "{rule}/C3");
            let ticket = commit
                .resume_ticket_for(&RunId("run-1".into()))
                .expect("D4 approval ticket");
            assert_eq!(
                ticket.reason(),
                awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission,
                "{rule}/C3"
            );
            runtime
                .resume(
                    ResumeCommand::from_ticket(&ticket, ResumeResult::allow(), 0),
                    commit.as_ref(),
                    RuntimeRunContext::new().with_commit(commit.clone()),
                )
                .await
                .expect("D4 approved Advisor remains model-visible and non-fatal")
        } else {
            initial
        };

        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd), "{rule}/E4");
        assert_eq!(model.primary_calls.load(Ordering::SeqCst), 2, "{rule}/E2");
        assert_eq!(committed_model_requests(&commit).len(), 2, "{rule}/E2");
        let committed = commit.committed();
        assert!(
            committed
                .messages
                .iter()
                .any(|message| message.role == Role::Tool
                    && message.text_content() == ADVISOR_UNAVAILABLE_NOTICE),
            "{rule}/E3"
        );
        assert!(
            committed
                .state
                .iter()
                .all(|command| command.key.0 != RunDelegations::KEY),
            "{rule}/E1"
        );
    }
}

#[tokio::test]
async fn advisor_terminal_failure_is_generic_and_the_primary_run_continues() {
    // Cause/effect graph: C1 the exact Advisor capability is published and
    // accepted; C2 its child service ends with a terminal provider detail; C3
    // the primary model can consume an error ToolResult. Effects: E1 the model
    // sees the one generic advisor failure notice with `is_error`; E2 provider
    // detail never enters the primary transcript; E3 the primary reaches a
    // natural end; E4 the stable call starts exactly once.
    //
    // Decision table:
    // | Rule | published | child terminal failure | model continues | Effects |
    // | F1   | yes       | yes                    | yes             | E1-E4   |
    // Constraints/invariants: provider detail is redacted at the ToolResult
    // boundary and child failure cannot terminate the still-runnable primary.
    let model = Arc::new(FailedAdvisorThenTextLlm {
        primary_calls: AtomicUsize::new(0),
    });
    let service = Arc::new(AdvisorDelegation {
        starts: AtomicUsize::new(0),
        terminal_failure: Some("sensitive provider detail"),
    });
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let result = Runtime::new()
        .with_llm(model.clone())
        .with_run_delegation(service.clone())
        .execute(
            advisor_activation(true),
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("F1 primary remains runnable");
    assert_eq!(result, RunState::Ended(EndCause::NaturalEnd), "F1/E3");
    assert_eq!(service.starts.load(Ordering::SeqCst), 1, "F1/E4");
    assert_eq!(model.primary_calls.load(Ordering::SeqCst), 2, "F1/E3");
    let transcript =
        serde_json::to_string(&commit.committed_messages(&ThreadId("thread-1".into()))).unwrap();
    assert!(transcript.contains(ADVISOR_FAILURE_NOTICE), "F1/E1");
    assert!(!transcript.contains("sensitive provider detail"), "F1/E2");
}

#[tokio::test]
async fn advisor_is_primary_only_even_when_a_child_shares_the_host_service() {
    // M12 cause/effect graph: C1=the child snapshot contains Advisor, C2=the
    // process shares a Host service that could execute Advisor for a primary,
    // and C3=the current activation has a delegation origin. Effects: E1=the
    // service starts zero children, E2=the exact unavailable ToolResult is fed
    // back, and E3=the child model may finish normally.
    // Constraint K1: Advisor is primary-only; sharing the sole Host executor
    // cannot create recursive advice. Decision rule D3: C1+C2+C3 -> E1+E2+E3.
    let model = Arc::new(AdvisorThenTextLlm {
        primary_calls: AtomicUsize::new(0),
        expected_tool_result: ADVISOR_UNAVAILABLE_NOTICE,
    });
    let service = Arc::new(AdvisorDelegation {
        starts: AtomicUsize::new(0),
        terminal_failure: None,
    });
    let runtime = Runtime::new()
        .with_llm(model.clone())
        .with_run_delegation(service.clone());
    let mut activation = advisor_activation(true);
    activation.delegation_origin = Some(
        awaken_agent_contract::agent::delegation::DelegationOrigin::root(
            RunId("parent".into()),
            "parent-call",
        ),
    );

    let result = runtime
        .execute(activation, RuntimeRunContext::new())
        .await
        .expect("D3");

    assert_eq!(result, RunState::Ended(EndCause::NaturalEnd), "D3/E3");
    assert_eq!(service.starts.load(Ordering::SeqCst), 0, "D3/E1");
    assert_eq!(model.primary_calls.load(Ordering::SeqCst), 2, "D3/E2");
}

#[tokio::test]
async fn an_unpublished_delegation_tool_cannot_use_a_shared_host_service() {
    // Cause/effect graph: C1=the process-wide Host service knows `agent_run`,
    // C2=this exact snapshot publishes no AgentDelegation descriptor, and C3=the
    // model hallucinates that tool id. Effects: E1=no child service entry,
    // E2=ordinary unknown-tool output, E3=the primary may finish. This is the
    // security boundary that lets Managed remove `agent_run` while sharing the
    // same Host with native SDK sessions.
    //
    // | Rule | Host handles | snapshot publishes | call | effects  |
    // | H1   | yes          | no                 | yes  | E1+E2+E3 |
    // Constraints/invariants: immutable snapshot publication gates capability;
    // a process-wide service cannot make an unpublished tool executable.
    let service = Arc::new(AdvisorDelegation {
        starts: AtomicUsize::new(0),
        terminal_failure: None,
    });
    let runtime = Runtime::new()
        .with_llm(Arc::new(HiddenDelegationThenTextLlm {
            calls: AtomicUsize::new(0),
        }))
        .with_run_delegation(service.clone());
    let outcome = runtime
        .execute(activation("catalog-a"), RuntimeRunContext::new())
        .await
        .expect("H1");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd), "H1/E3");
    assert_eq!(service.starts.load(Ordering::SeqCst), 0, "H1/E1");
}

#[tokio::test]
async fn run_without_commit_coordinator_still_completes() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("ok")));
    let outcome = runtime
        .execute(activation("catalog-a"), RuntimeRunContext::new())
        .await
        .expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
}
