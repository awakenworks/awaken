use super::*;
use awaken_agent_contract::agent::state::{StateKey, Store};
use awaken_agent_contract::stream::checkpoint::{
    StreamCheckpoint, StreamCheckpointError, StreamCheckpointStore,
};
use awaken_runtime_contract::model_operation::{
    ActiveModelOperation, ModelExecutionEvidence, ModelOperation,
};
use awaken_runtime_contract::tool_batch::{ActiveToolBatch, ToolBatchPhase};
use awaken_runtime_contract::{CommitCoordinator, RunDisposition, ThreadCommit};

#[derive(Clone, Copy)]
enum RecoveryFault {
    UnknownModel,
    UnavailableCheckpoint,
    StalledCheckpoint,
}

async fn make_model_unknown(fixture: &PermissionHandoffFixture) {
    let mut prior = ModelOperation::next(None, ModelBinding::new("p", "m", "b")).unwrap();
    prior.execution = ModelExecutionEvidence::Unknown;
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
            vec![ActiveModelOperation::write(&Some(prior))],
            Vec::new(),
        ))
        .await
        .unwrap();
}

#[async_trait::async_trait]
impl StreamCheckpointStore for RecoveryFault {
    async fn get(&self, _: &str) -> Result<Option<StreamCheckpoint>, StreamCheckpointError> {
        if matches!(self, Self::StalledCheckpoint) {
            return std::future::pending().await;
        }
        assert!(matches!(self, Self::UnavailableCheckpoint));
        Err(StreamCheckpointError::Storage(
            "injected read failure".into(),
        ))
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
    // terminated prior with failed/stalled checkpoint; C3 context reader absent/current/stale.
    // Effects: Indeterminate, physical veto, sealed batch, no approval/
    // ResumeApplied event and zero real tool effects. The explicit resume reader
    // must own both recovery and terminal sealing, even without context.reader.
    // R1..R18=C1 x C2 x C3. Existing handoff and Native permission tests supply the
    // positive controls with committed Terminated evidence and readable absence.
    for native in [false, true] {
        for (fault, context_reader) in [
            RecoveryFault::UnknownModel,
            RecoveryFault::UnavailableCheckpoint,
            RecoveryFault::StalledCheckpoint,
        ]
        .into_iter()
        .flat_map(|fault| (0..3).map(move |reader| (fault, reader)))
        {
            let fixture = PermissionHandoffFixture::awaiting(false).await;
            if matches!(fault, RecoveryFault::UnknownModel) {
                make_model_unknown(&fixture).await;
            }
            let before = audited_decisions(&fixture.commit);
            let mut context = context(&fixture.commit);
            context = match context_reader {
                0 => context,
                1 => context.with_reader(fixture.commit.clone()),
                _ => context.with_reader(Arc::new(MemoryCommitCoordinator::new())),
            };
            if !matches!(fault, RecoveryFault::UnknownModel) {
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
                    .committed()
                    .events
                    .iter()
                    .all(|event| event.kind != EventKind::ResumeApplied)
            );
        }
    }
}

#[tokio::test]
async fn tool_interruption_and_terminal_replay_preserve_predecessor_uncertainty() {
    // C1 known/unknown predecessor; C2 first interruption/terminal replay.
    // R1..R4: logical interruption is NaturalEnd with zero actual tool calls;
    // only known evidence permits physical return, including idempotent replay.
    // Terminal state cannot replace predecessor execution evidence.
    for unknown in [false, true] {
        let fixture = PermissionHandoffFixture::awaiting(false).await;
        if unknown {
            make_model_unknown(&fixture).await;
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
