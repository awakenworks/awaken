// @formal evidence checkpoint.recovery_never_precedes_commit
// @formal evidence tool_result.resume_receipt_is_atomic_with_ticket_consumption
use super::*;
use awaken_agent_contract::agent::state::{StateKey, Store};
use awaken_agent_contract::observation::{Observed, UnknownCause};
use awaken_agent_contract::stream::checkpoint::{
    StreamCheckpoint, StreamCheckpointError, StreamCheckpointStore,
};
use awaken_runtime_contract::model_operation::{
    ActiveModelOperation, ModelExecutionEvidence, ModelOperation,
};
use awaken_runtime_contract::{CommitCoordinator, RunDisposition, ThreadCommit};
use awaken_tool_contract::tool_batch::{ActiveToolBatch, ToolBatchPhase};

#[derive(Clone, Copy)]
enum RecoveryFault {
    UnknownModel,
    ShadowedUnknownModel,
    UnavailableCheckpoint,
    StalledCheckpoint,
}

async fn make_model_unknown(fixture: &PermissionHandoffFixture, shadowed: bool) {
    let mut prior = ModelOperation::next(None, ModelBinding::new("p", "m", "b")).unwrap();
    prior.execution = ModelExecutionEvidence::Unknown;
    let mut later = ModelOperation::next(Some(&prior), prior.binding.clone()).unwrap();
    later.execution = ModelExecutionEvidence::Terminated(
        awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
            "completed",
        ))
        .unwrap(),
    );
    let mut commands = vec![ActiveModelOperation::write(&Some(prior))];
    if shadowed {
        commands.push(ActiveModelOperation::write(&Some(later)));
    }
    fixture
        .commit
        .commit(ThreadCommit::assemble(
            fixture.command.thread_id.clone(),
            RunDisposition::awaiting(
                fixture
                    .commit
                    .resume_ticket_for(&fixture.command.run_id)
                    .unwrap(),
            ),
            true,
            Vec::new(),
            commands,
            Vec::new(),
        ))
        .await
        .unwrap();
}

#[async_trait::async_trait]
impl StreamCheckpointStore for RecoveryFault {
    async fn get(&self, _: &str) -> Observed<StreamCheckpoint> {
        if matches!(self, Self::StalledCheckpoint) {
            return std::future::pending().await;
        }
        assert!(matches!(self, Self::UnavailableCheckpoint));
        Observed::Unknown(UnknownCause::Storage("injected read failure".into()))
    }
    async fn put(&self, _: StreamCheckpoint) -> Result<(), StreamCheckpointError> {
        panic!("no write during uncertain recovery")
    }
    async fn delete(&self, _: &str) -> Result<(), StreamCheckpointError> {
        panic!("no delete during uncertain recovery")
    }
}

#[tokio::test(start_paused = true)]
async fn permission_resume_checks_recovery_before_approval_and_tool_entry() {
    // Causes: C1 Native/tool-batch continuation; C2 unknown prior model /
    // older Unknown hidden by later Terminated /
    // terminated prior with failed/stalled checkpoint; C3 context reader absent/current/stale.
    // Effects: Indeterminate, physical veto, sealed batch, no approval or real
    // tool effect. Because the terminal commit consumes the validated ticket,
    // that same commit owns one exact ResumeApplied receipt; a response-loss
    // retry therefore cannot reinterpret the terminalized delivery. The
    // explicit resume reader must own recovery and sealing without context.reader.
    // R1..R24=C1 x C2 x C3. Existing handoff and Native permission tests supply the
    // positive controls with committed Terminated evidence and readable absence.
    for native in [false, true] {
        for (fault, context_reader) in [
            RecoveryFault::UnknownModel,
            RecoveryFault::ShadowedUnknownModel,
            RecoveryFault::UnavailableCheckpoint,
            RecoveryFault::StalledCheckpoint,
        ]
        .into_iter()
        .flat_map(|fault| (0..3).map(move |reader| (fault, reader)))
        {
            let fixture = PermissionHandoffFixture::awaiting(false).await;
            let model_fault = matches!(
                fault,
                RecoveryFault::UnknownModel | RecoveryFault::ShadowedUnknownModel
            );
            if model_fault {
                make_model_unknown(
                    &fixture,
                    matches!(fault, RecoveryFault::ShadowedUnknownModel),
                )
                .await;
            }
            let before = audited_decisions(&fixture.commit);
            let mut context = context(&fixture.commit);
            context = match context_reader {
                0 => context,
                1 => context.with_reader(fixture.commit.clone()),
                _ => context.with_reader(Arc::new(MemoryCommitCoordinator::new(
                    awaken_time::system_clock(),
                ))),
            };
            if !model_fault {
                context = context.with_stream_checkpoint(Arc::new(fault));
            }
            let quiescence = context.physical_quiescence.clone();
            let result = tokio::time::timeout(std::time::Duration::from_secs(40), async {
                if native {
                    fixture
                        .runtime
                        .resume(fixture.command.clone(), fixture.commit.as_ref(), context)
                        .await
                } else {
                    fixture
                        .runtime
                        .resume_tool_batch(
                            fixture.command.clone(),
                            fixture.commit.as_ref(),
                            context,
                        )
                        .await
                }
            })
            .await
            .expect("resume observation is bounded without approving a tool");
            assert_eq!(result.unwrap(), RunState::Ended(EndCause::Indeterminate));
            assert!(fixture.calls.lock().unwrap().is_empty());
            assert_eq!(audited_decisions(&fixture.commit), before);
            assert!(!quiescence.can_acknowledge_return());
            let batch = ActiveToolBatch::load(&Store::rebuild(
                &fixture.commit.committed_state(&fixture.command.thread_id),
            ))
            .unwrap()
            .unwrap();
            assert_eq!(batch.phase(), ToolBatchPhase::Finalized);
            assert!(
                fixture
                    .commit
                    .resume_ticket_for(&fixture.command.run_id)
                    .is_none(),
                "terminal recovery consumes the answered ticket"
            );
            let receipts = fixture
                .commit
                .committed()
                .events
                .into_iter()
                .filter(|event| event.kind == EventKind::ResumeApplied)
                .collect::<Vec<_>>();
            assert_eq!(receipts.len(), 1, "one consumed delivery has one receipt");
            assert_eq!(
                receipts[0].payload["operation_id"],
                fixture.command.operation_id.as_deref().unwrap()
            );
            assert_eq!(
                receipts[0].payload["correlation_id"],
                fixture.command.correlation_id
            );
        }
    }
}

#[tokio::test]
async fn permission_resume_keeps_the_validated_model_ordinal_after_a_late_receipt() {
    // C1 first call's receipt arrives after ordinal 2; C2 explicit resume reader
    // with absent/current/stale context.reader. R1..R3 => approve once, execute
    // the real tool once and enter model ordinal 3, never reused ordinal 2.
    // This kills rebuilding the latest-value cell inside drive_resumed after
    // recovery already validated and restored the full authoritative history.
    for context_reader in 0..3 {
        let fixture = PermissionHandoffFixture::awaiting(false).await;
        let prior = ActiveModelOperation::recorded(
            &fixture.commit.committed_state(&fixture.command.thread_id),
            &fixture.command.run_id,
        )
        .unwrap()
        .pop()
        .unwrap();
        let mut later = ModelOperation::next(Some(&prior), prior.binding.clone()).unwrap();
        later.execution = ModelExecutionEvidence::Terminated(
            awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
                "completed",
            ))
            .unwrap(),
        );
        let expected = later.ordinal.get() + 1;
        for operation in [later, prior] {
            fixture
                .commit
                .commit(ThreadCommit::assemble(
                    fixture.command.thread_id.clone(),
                    RunDisposition::awaiting(
                        fixture
                            .commit
                            .resume_ticket_for(&fixture.command.run_id)
                            .unwrap(),
                    ),
                    true,
                    Vec::new(),
                    vec![ActiveModelOperation::write(&Some(operation))],
                    Vec::new(),
                ))
                .await
                .unwrap();
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runtime = fixture.runtime.with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(1),
            requests: requests.clone(),
            tool_calls: Vec::new(),
        }));
        let context = match context_reader {
            0 => context(&fixture.commit),
            1 => context(&fixture.commit).with_reader(fixture.commit.clone()),
            _ => context(&fixture.commit).with_reader(Arc::new(MemoryCommitCoordinator::new(
                awaken_time::system_clock(),
            ))),
        };
        assert_eq!(
            runtime
                .resume(fixture.command.clone(), fixture.commit.as_ref(), context)
                .await
                .unwrap(),
            RunState::Ended(EndCause::NaturalEnd)
        );
        assert_eq!(fixture.calls.lock().unwrap().len(), 1);
        assert_eq!(requests.lock().unwrap().len(), 1);
        let operations = ActiveModelOperation::recorded(
            &fixture.commit.committed_state(&fixture.command.thread_id),
            &fixture.command.run_id,
        )
        .unwrap();
        assert_eq!(operations.last().unwrap().ordinal.get(), expected);
    }
}

#[tokio::test]
async fn tool_interruption_and_terminal_replay_preserve_predecessor_uncertainty() {
    // C1 known/unknown/shadowed-unknown predecessor; C2 first interruption/terminal replay.
    // R1..R6: logical interruption is NaturalEnd with zero actual tool calls;
    // only known evidence permits physical return, including idempotent replay.
    // Terminal state cannot replace predecessor execution evidence.
    for (unknown, shadowed) in [(false, false), (true, false), (true, true)] {
        let fixture = PermissionHandoffFixture::awaiting(false).await;
        if unknown {
            make_model_unknown(&fixture, shadowed).await;
        }
        for _ in 0..2 {
            let context = context(&fixture.commit).with_reader(fixture.commit.clone());
            let quiescence = context.physical_quiescence.clone();
            let state = fixture
                .runtime
                .interrupt_awaiting_tools(
                    fixture.command.run_id.clone(),
                    fixture.command.thread_id.clone(),
                    context,
                )
                .await
                .unwrap();
            assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
            assert!(fixture.calls.lock().unwrap().is_empty());
            assert_eq!(quiescence.can_acknowledge_return(), !unknown);
        }
    }
}
