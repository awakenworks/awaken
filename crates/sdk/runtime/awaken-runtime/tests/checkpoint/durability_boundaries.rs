//! Optional preview/checkpoint loss cannot weaken the committed message boundary.
use super::*;
use awaken_agent_contract::event::AgentEvent;
use awaken_agent_contract::stream::event::{Event, Observation};
use awaken_agent_contract::stream::sink::{Error as SinkError, Sink};
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use std::sync::atomic::{AtomicUsize, Ordering};

struct LossyPreview {
    accepted_deltas: usize,
    attempts: AtomicUsize,
    text: std::sync::Mutex<String>,
}

#[async_trait::async_trait]
impl Sink for LossyPreview {
    async fn send(&self, event: Event) -> std::result::Result<(), SinkError> {
        self.send_observation(event.into()).await
    }

    async fn send_observation(
        &self,
        observation: Observation,
    ) -> std::result::Result<(), SinkError> {
        if let AgentEvent::Delta(awaken_agent_contract::event::Delta::TextDelta { delta }) =
            observation.event.kind
        {
            if self.attempts.fetch_add(1, Ordering::SeqCst) >= self.accepted_deltas {
                return Err(SinkError::Closed);
            }
            self.text.lock().unwrap().push_str(&delta);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum PutOutcome {
    Saved,
    Rejected,
    AcknowledgementLost,
}

struct CheckpointWriteFault {
    inner: MemoryStreamCheckpointStore,
    outcome: PutOutcome,
    puts: AtomicUsize,
}

#[async_trait::async_trait]
impl StreamCheckpointStore for CheckpointWriteFault {
    async fn get(&self, run: &str) -> Observed<StreamCheckpoint> {
        self.inner.get(run).await
    }

    async fn put(&self, value: StreamCheckpoint) -> std::result::Result<(), StreamCheckpointError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if matches!(self.outcome, PutOutcome::Rejected) {
            return Err(StreamCheckpointError::Storage(
                "injected optional write failure".into(),
            ));
        }
        self.inner.put(value.clone()).await?;
        if matches!(self.outcome, PutOutcome::AcknowledgementLost) {
            assert_eq!(
                self.inner.get(&value.run_id).await,
                Observed::Present(value)
            );
            return Err(StreamCheckpointError::Storage(
                "injected checkpoint acknowledgement loss".into(),
            ));
        }
        Ok(())
    }

    async fn delete(&self, run: &str) -> std::result::Result<(), StreamCheckpointError> {
        self.inner.delete(run).await
    }
}

#[tokio::test]
async fn preview_loss_and_checkpoint_write_failure_preserve_the_complete_final_message() {
    // P1 closed/gapped/healthy preview x P2 checkpoint saved/rejected/ACK lost.
    // All nine rules retain a positively terminated model attempt, continue
    // once in-process and commit the complete response. No failed put is an
    // acknowledged recovery point; checkpoint read/retirement failures retain
    // their separate fail-closed coverage in the parent suite.
    for accepted_deltas in [0, 1, usize::MAX] {
        for outcome in [
            PutOutcome::Saved,
            PutOutcome::Rejected,
            PutOutcome::AcknowledgementLost,
        ] {
            let preview = Arc::new(LossyPreview {
                accepted_deltas,
                attempts: AtomicUsize::new(0),
                text: Default::default(),
            });
            let checkpoints = Arc::new(CheckpointWriteFault {
                inner: MemoryStreamCheckpointStore::new(),
                outcome,
                puts: AtomicUsize::new(0),
            });
            let model = Arc::new(DropOnceLlm {
                calls: Default::default(),
                first: "Hel",
                rest: "lo world",
            });
            let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
            let state = runtime_with(model.clone())
                .execute(
                    activation(),
                    RuntimeRunContext::new()
                        .with_commit(commit.clone())
                        .with_stream_sink(preview.clone())
                        .with_stream_checkpoint(checkpoints.clone()),
                )
                .await
                .unwrap();
            assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
            assert_eq!(assistant_text(&commit), "Hello world");
            assert_eq!(*model.calls.lock().unwrap(), 2);
            assert_eq!(checkpoints.puts.load(Ordering::SeqCst), 1);
            assert_eq!(checkpoints.get(RUN_ID).await, Observed::Absent);
            assert_eq!(preview.attempts.load(Ordering::SeqCst), 2);
            assert_eq!(
                preview.text.lock().unwrap().as_str(),
                match accepted_deltas {
                    0 => "",
                    1 => "Hel",
                    _ => "Hello world",
                },
                "the injected loss must actually remove preview content"
            );
        }
    }
}

struct RejectFinalCommit(Arc<MemoryCommitCoordinator>, AtomicUsize);

#[async_trait::async_trait]
impl Coordinator for RejectFinalCommit {
    async fn commit(&self, value: ThreadCommit) -> std::result::Result<CommitRecord, CommitError> {
        if value
            .messages
            .iter()
            .any(|message| message.role == Role::Assistant)
        {
            self.1.fetch_add(1, Ordering::SeqCst);
            return Err(CommitError::CapacityExhausted(
                "injected final commit failure".into(),
            ));
        }
        self.0.commit(value).await
    }
}

#[tokio::test]
async fn tolerating_preview_loss_does_not_tolerate_a_failed_final_commit() {
    // A closed preview is optional, but rejecting the actual message commit
    // must return an execution error and publish neither assistant nor success.
    let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let fault = Arc::new(RejectFinalCommit(durable.clone(), AtomicUsize::new(0)));
    let preview = Arc::new(LossyPreview {
        accepted_deltas: 0,
        attempts: AtomicUsize::new(0),
        text: Default::default(),
    });
    let result = runtime_with(Arc::new(AlwaysLlm {
        text: "complete response",
        calls: Default::default(),
    }))
    .execute(
        activation(),
        RuntimeRunContext::new()
            .with_commit(fault.clone())
            .with_reader(durable.clone())
            .with_stream_sink(preview.clone()),
    )
    .await;
    assert!(
        result.is_err(),
        "durable publication cannot degrade into success"
    );
    assert!(fault.1.load(Ordering::SeqCst) > 0);
    assert!(preview.attempts.load(Ordering::SeqCst) > 0);
    assert!(
        durable
            .committed()
            .messages
            .iter()
            .all(|message| message.role != Role::Assistant)
    );
    assert!(
        durable
            .committed()
            .latest_run
            .is_none_or(|run| run.state != RunState::Ended(EndCause::NaturalEnd))
    );
}
