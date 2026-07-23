//! Durable, backend-neutral Run lifecycle feed projected from committed facts.
//!
//! The feed is deliberately separate from dispatch delivery operations. It
//! classifies only committed `RunStateChanged` records and therefore cannot
//! mistake a claim, lease expiry, or settle operation for agent truth.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::agent::run::{EndCause, Id as RunId, RunState};
use crate::agent::thread::Id as ThreadId;
use crate::audit::kind::Kind as AuditKind;
use crate::thread::read::checkpoint::{CheckpointReader, EventScope};

/// Exclusive cursor in one committed-truth feed partition.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct LifecycleCursor(pub u64);

/// Stable consumer-facing classification of a committed Run transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunLifecycleKind {
    Running,
    Awaiting,
    Resumed,
    Completed,
    Failed,
    Cancelled,
}

/// One committed lifecycle transition. `state` preserves the complete neutral
/// authority while `kind` provides the common integration classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLifecycleEvent {
    pub cursor: LifecycleCursor,
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub kind: RunLifecycleKind,
    pub state: RunState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecyclePage {
    pub events: Vec<RunLifecycleEvent>,
    /// The last returned event cursor, or the requested cursor for an empty page.
    pub next_cursor: LifecycleCursor,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum RunLifecycleFeedError {
    #[error("committed lifecycle event {sequence} has an invalid RunState payload")]
    InvalidState { sequence: u64 },
    #[error("committed lifecycle event {sequence} references an unknown run")]
    UnknownRun { sequence: u64 },
}

#[async_trait]
pub trait RunLifecycleFeed: Send + Sync {
    async fn events_after(
        &self,
        cursor: LifecycleCursor,
        limit: usize,
    ) -> Result<LifecyclePage, RunLifecycleFeedError>;
}

/// Portable feed over the existing committed-event read model.
///
/// P1 favors correctness and backend parity: the adapter folds the partition's
/// lifecycle prefix to distinguish an initial `Running` transition from a
/// post-`Awaiting` resume. P2 may replace that fold with a durable lifecycle
/// index/changefeed without changing this port.
pub struct CheckpointRunLifecycleFeed {
    reader: Arc<dyn CheckpointReader>,
}

impl CheckpointRunLifecycleFeed {
    #[must_use]
    pub fn new(reader: Arc<dyn CheckpointReader>) -> Self {
        Self { reader }
    }
}

fn classify(state: &RunState, previous: Option<&RunState>) -> RunLifecycleKind {
    match state {
        RunState::Running if matches!(previous, Some(RunState::Awaiting)) => {
            RunLifecycleKind::Resumed
        }
        RunState::Running => RunLifecycleKind::Running,
        RunState::Awaiting => RunLifecycleKind::Awaiting,
        RunState::Ended(EndCause::NaturalEnd) => RunLifecycleKind::Completed,
        RunState::Ended(EndCause::Cancelled) => RunLifecycleKind::Cancelled,
        RunState::Ended(
            EndCause::MaxSteps
            | EndCause::Stopped(_)
            | EndCause::Error(_)
            | EndCause::Indeterminate,
        ) => RunLifecycleKind::Failed,
    }
}

#[async_trait]
impl RunLifecycleFeed for CheckpointRunLifecycleFeed {
    async fn events_after(
        &self,
        cursor: LifecycleCursor,
        limit: usize,
    ) -> Result<LifecyclePage, RunLifecycleFeedError> {
        if limit == 0 {
            return Ok(LifecyclePage {
                events: Vec::new(),
                next_cursor: cursor,
            });
        }
        let records = self.reader.list_events(&EventScope::All, None, usize::MAX);
        let mut previous = HashMap::<RunId, RunState>::new();
        let mut events = Vec::with_capacity(limit.min(records.len()));
        for record in records {
            if record.kind != AuditKind::RunStateChanged {
                continue;
            }
            let state = serde_json::from_value::<RunState>(
                record.payload.get("state").cloned().unwrap_or_default(),
            )
            .map_err(|_| RunLifecycleFeedError::InvalidState {
                sequence: record.sequence,
            })?;
            let kind = classify(&state, previous.get(&record.run_id));
            previous.insert(record.run_id.clone(), state.clone());
            if record.sequence <= cursor.0 {
                continue;
            }
            let run = self
                .reader
                .run(&record.run_id)
                .ok_or(RunLifecycleFeedError::UnknownRun {
                    sequence: record.sequence,
                })?;
            events.push(RunLifecycleEvent {
                cursor: LifecycleCursor(record.sequence),
                thread_id: run.thread_id,
                run_id: record.run_id,
                kind,
                state,
            });
            if events.len() == limit {
                break;
            }
        }
        let next_cursor = events.last().map_or(cursor, |event| event.cursor);
        Ok(LifecyclePage {
            events,
            next_cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::agent::awaiting::ResumeTicket;
    use crate::agent::message::Message;
    use crate::agent::run::{Failure, Record as RunRecord};
    use crate::agent::state::Command as StateCommand;
    use crate::audit::record::Record as EventRecord;
    use crate::thread::read::thread_reader::ThreadReader;

    struct Reader {
        events: Vec<EventRecord>,
        runs: HashMap<RunId, RunRecord>,
        _sync: Mutex<()>,
    }

    impl ThreadReader for Reader {
        fn committed_messages(&self, _thread_id: &ThreadId) -> Vec<Message> {
            Vec::new()
        }

        fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
            None
        }

        fn run_state(&self, run_id: &RunId) -> Option<RunState> {
            self.runs.get(run_id).map(|run| run.state.clone())
        }

        fn committed_state(&self, _thread_id: &ThreadId) -> Vec<StateCommand> {
            Vec::new()
        }
    }

    impl CheckpointReader for Reader {
        fn run(&self, id: &RunId) -> Option<RunRecord> {
            self.runs.get(id).cloned()
        }

        fn latest_run(&self, _thread_id: &ThreadId) -> Option<RunRecord> {
            None
        }

        fn list_events(
            &self,
            scope: &EventScope,
            from: Option<u64>,
            limit: usize,
        ) -> Vec<EventRecord> {
            assert_eq!(scope, &EventScope::All);
            let after = from.unwrap_or_default();
            self.events
                .iter()
                .filter(|event| event.sequence > after)
                .take(limit)
                .cloned()
                .collect()
        }
    }

    fn state_event(sequence: u64, run_id: &RunId, state: RunState) -> EventRecord {
        EventRecord {
            sequence,
            run_id: run_id.clone(),
            kind: AuditKind::RunStateChanged,
            payload: serde_json::json!({ "state": state }),
        }
    }

    #[tokio::test]
    async fn feed_classifies_resume_terminal_causes_and_pages_exclusively() {
        let run_id = RunId("lifecycle-run".into());
        let thread_id = ThreadId("lifecycle-thread".into());
        let states = [
            RunState::Running,
            RunState::Awaiting,
            RunState::Running,
            RunState::Ended(EndCause::Error(Failure::StateConflict)),
        ];
        let events = states
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, state)| state_event((index + 1) as u64, &run_id, state))
            .collect();
        let reader = Reader {
            events,
            runs: HashMap::from([(
                run_id.clone(),
                RunRecord {
                    id: run_id,
                    thread_id,
                    state: states.last().unwrap().clone(),
                },
            )]),
            _sync: Mutex::new(()),
        };
        let feed = CheckpointRunLifecycleFeed::new(Arc::new(reader));

        let first = feed.events_after(LifecycleCursor(0), 2).await.unwrap();
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![RunLifecycleKind::Running, RunLifecycleKind::Awaiting]
        );
        let second = feed.events_after(first.next_cursor, 10).await.unwrap();
        assert_eq!(
            second
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![RunLifecycleKind::Resumed, RunLifecycleKind::Failed]
        );
        assert_eq!(second.next_cursor, LifecycleCursor(4));
    }

    #[tokio::test]
    async fn zero_limit_keeps_the_requested_cursor_without_reading() {
        let feed = CheckpointRunLifecycleFeed::new(Arc::new(Reader {
            events: Vec::new(),
            runs: HashMap::new(),
            _sync: Mutex::new(()),
        }));

        let page = feed.events_after(LifecycleCursor(7), 0).await.unwrap();

        assert!(page.events.is_empty());
        assert_eq!(page.next_cursor, LifecycleCursor(7));
    }

    #[tokio::test]
    async fn malformed_state_and_unknown_run_are_not_silently_skipped() {
        let run_id = RunId("missing-run".into());
        let malformed = EventRecord {
            sequence: 1,
            run_id: run_id.clone(),
            kind: AuditKind::RunStateChanged,
            payload: serde_json::json!({ "state": "not-a-run-state" }),
        };
        let malformed_feed = CheckpointRunLifecycleFeed::new(Arc::new(Reader {
            events: vec![malformed],
            runs: HashMap::new(),
            _sync: Mutex::new(()),
        }));
        assert_eq!(
            malformed_feed
                .events_after(LifecycleCursor(0), 1)
                .await
                .unwrap_err(),
            RunLifecycleFeedError::InvalidState { sequence: 1 }
        );

        let unknown_feed = CheckpointRunLifecycleFeed::new(Arc::new(Reader {
            events: vec![state_event(2, &run_id, RunState::Running)],
            runs: HashMap::new(),
            _sync: Mutex::new(()),
        }));
        assert_eq!(
            unknown_feed
                .events_after(LifecycleCursor(0), 1)
                .await
                .unwrap_err(),
            RunLifecycleFeedError::UnknownRun { sequence: 2 }
        );
    }

    #[test]
    fn terminal_classification_is_total() {
        assert_eq!(
            classify(&RunState::Ended(EndCause::NaturalEnd), None),
            RunLifecycleKind::Completed
        );
        assert_eq!(
            classify(&RunState::Ended(EndCause::Cancelled), None),
            RunLifecycleKind::Cancelled
        );
        assert_eq!(
            classify(&RunState::Ended(EndCause::Indeterminate), None),
            RunLifecycleKind::Failed
        );
    }
}
