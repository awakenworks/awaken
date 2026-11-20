//! ACP previews are disposable; the committed response and terminal observer are not.
use super::*;
use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::{Event, Observation};
use awaken_agent_contract::stream::sink::{Error as SinkError, Sink};

/// Cause/effect design: C1 ACP emits `working`, `done`, then a natural terminal;
/// C2 the activation supplies user input `do it`. Effects: E1 the Run ends
/// NaturalEnd; E2 one commit contains the user message plus one assistant message
/// with concatenated text; E3 that commit carries the terminal state. Decision
/// rule D1=C1+C2=>E1+E2+E3.
#[tokio::test]
async fn drives_a_step_commits_messages_and_returns_natural_end() {
    let e = exec(vec![
        r#"{"type":"message","text":"working"}"#.into(),
        r#"{"type":"message","text":"done"}"#.into(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let coord = Arc::new(RecordingCoordinator::default());
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    let commits = coord.settled_commits();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].messages.len(), 2);
    assert_eq!(commits[0].messages[0].text_content(), "do it");
    assert_eq!(commits[0].messages[1].text_content(), "workingdone");
    assert_eq!(
        commits[0].run_state(),
        RunState::Ended(EndCause::NaturalEnd)
    );
}

struct LossyPreview {
    accepted_deltas: usize,
    attempts: AtomicUsize,
    text: Mutex<String>,
}

#[async_trait]
impl Sink for LossyPreview {
    async fn send(&self, event: Event) -> std::result::Result<(), SinkError> {
        self.send_observation(event.into()).await
    }

    async fn send_observation(
        &self,
        observation: Observation,
    ) -> std::result::Result<(), SinkError> {
        if let AgentEvent::Delta(Delta::TextDelta { delta }) = observation.event.kind {
            if self.attempts.fetch_add(1, Ordering::SeqCst) >= self.accepted_deltas {
                return Err(SinkError::Closed);
            }
            self.text.lock().unwrap().push_str(&delta);
        }
        Ok(())
    }
}

struct CommitBoundary {
    durable: Arc<RecordingCoordinator>,
    reject_final: bool,
    rejected: AtomicUsize,
}

#[async_trait]
impl Coordinator for CommitBoundary {
    async fn commit(&self, value: ThreadCommit) -> std::result::Result<CommitRecord, CommitError> {
        if self.reject_final
            && value
                .messages
                .iter()
                .any(|message| message.role == Role::Assistant)
        {
            self.rejected.fetch_add(1, Ordering::SeqCst);
            return Err(CommitError::CapacityExhausted(
                "injected final commit failure".into(),
            ));
        }
        self.durable.commit(value).await
    }
}

#[tokio::test]
async fn preview_loss_is_optional_but_final_publication_and_terminal_delivery_are_required() {
    // Six rules: closed/gapped/healthy preview x admitted/rejected final
    // commit. Preview loss never truncates a successful message. Rejection at
    // the real Coordinator port withholds both NaturalEnd and its observer.
    for accepted_deltas in [0, 1, usize::MAX] {
        for reject_final in [false, true] {
            let durable = Arc::new(RecordingCoordinator::default());
            let boundary = Arc::new(CommitBoundary {
                durable: durable.clone(),
                reject_final,
                rejected: AtomicUsize::new(0),
            });
            let preview = Arc::new(LossyPreview {
                accepted_deltas,
                attempts: AtomicUsize::new(0),
                text: Default::default(),
            });
            let terminal = Arc::new(TerminalRecordingObserver::default());
            let executor = exec(vec![
                r#"{"type":"message","text":"working"}"#.into(),
                r#"{"type":"message","text":"done"}"#.into(),
                r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
            ]);
            let result = executor
                .execute(
                    activation(),
                    RuntimeRunContext::new()
                        .with_commit(boundary.clone())
                        .with_reader(durable.clone())
                        .with_stream_sink(preview.clone())
                        .with_terminal_observer(terminal.clone()),
                )
                .await;
            assert_eq!(preview.attempts.load(Ordering::SeqCst), 2);
            assert_eq!(
                preview.text.lock().unwrap().as_str(),
                match accepted_deltas {
                    0 => "",
                    1 => "working",
                    _ => "workingdone",
                }
            );
            if reject_final {
                assert!(result.is_err(), "a failed commit cannot become success");
                assert!(boundary.rejected.load(Ordering::SeqCst) > 0);
                assert!(
                    durable
                        .messages()
                        .iter()
                        .all(|message| message.role != Role::Assistant)
                );
                assert!(terminal.events.lock().unwrap().is_empty());
                assert!(
                    durable
                        .settled_commits()
                        .iter()
                        .all(|value| value.run_state() != RunState::Ended(EndCause::NaturalEnd))
                );
            } else {
                assert_eq!(result.unwrap(), RunState::Ended(EndCause::NaturalEnd));
                let settled = durable.settled_commits();
                assert_eq!(settled.len(), 1);
                assert_eq!(settled[0].messages[1].text_content(), "workingdone");
                assert_eq!(terminal.events.lock().unwrap().len(), 1);
            }
        }
    }
}
