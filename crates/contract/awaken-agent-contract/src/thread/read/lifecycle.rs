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
pub struct RunLifecycleCursor(pub u64);

/// Number of durable event slots reserved for one source commit.
///
/// This is the persisted cursor codec used by every commit-store backend. Keep
/// the value stable: changing it would reinterpret existing event sequences.
const RUN_LIFECYCLE_CURSOR_STRIDE: u64 = 1_000;

/// A source commit and event offset could not be represented by the persisted
/// lifecycle cursor domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RunLifecycleCursorCodecError {
    #[error("lifecycle event offset {offset} exceeds the per-commit limit of {max_exclusive}")]
    OffsetOutOfRange { offset: usize, max_exclusive: u64 },
    #[error(
        "lifecycle cursor overflow for source commit {source_commit_cursor} at event offset {offset}"
    )]
    Overflow {
        source_commit_cursor: u64,
        offset: usize,
    },
}

/// Encode one source commit and its zero-based event offset without changing
/// the durable `commit * 1000 + offset` representation.
pub fn encode_run_lifecycle_cursor(
    source_commit_cursor: u64,
    offset: usize,
) -> Result<RunLifecycleCursor, RunLifecycleCursorCodecError> {
    if offset >= RUN_LIFECYCLE_CURSOR_STRIDE as usize {
        return Err(RunLifecycleCursorCodecError::OffsetOutOfRange {
            offset,
            max_exclusive: RUN_LIFECYCLE_CURSOR_STRIDE,
        });
    }
    let cursor = source_commit_cursor
        .checked_mul(RUN_LIFECYCLE_CURSOR_STRIDE)
        .and_then(|cursor| cursor.checked_add(offset as u64))
        .ok_or(RunLifecycleCursorCodecError::Overflow {
            source_commit_cursor,
            offset,
        })?;
    Ok(RunLifecycleCursor(cursor))
}

/// Decode a persisted lifecycle cursor into its source commit and zero-based
/// event offset. Every `u64` has one canonical quotient/remainder pair.
#[must_use]
pub fn decode_run_lifecycle_cursor(cursor: RunLifecycleCursor) -> (u64, usize) {
    (
        cursor.0 / RUN_LIFECYCLE_CURSOR_STRIDE,
        (cursor.0 % RUN_LIFECYCLE_CURSOR_STRIDE) as usize,
    )
}

/// Stable consumer-facing classification of a committed Run transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunLifecycleEventKind {
    Running,
    Awaiting,
    Resumed,
    /// An expired dispatch lease was durably reclaimed and the replacement
    /// claim crossed the Thread commit fence before retrying execution.
    Rescheduled,
    Completed,
    Failed,
    Cancelled,
}

/// One committed lifecycle transition. `state` preserves the complete neutral
/// authority while `kind` provides the common integration classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLifecycleEvent {
    pub cursor: RunLifecycleCursor,
    /// Backend-wide commit sequence whose atomic write produced this event.
    /// Defaults to zero when decoding events serialized before this field was
    /// added; authoritative feeds always populate it.
    #[serde(default)]
    pub source_commit_cursor: u64,
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub kind: RunLifecycleEventKind,
    pub state: RunState,
    /// Awaiting cause captured on the same committed state-transition fact.
    /// Historical consumers can therefore classify a consumed no-input ticket
    /// without consulting a second ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub await_reason: Option<crate::agent::awaiting::AwaitReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLifecyclePage {
    pub events: Vec<RunLifecycleEvent>,
    /// The last returned event cursor, or the requested cursor for an empty page.
    pub next_cursor: RunLifecycleCursor,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum RunLifecycleFeedError {
    #[error("run lifecycle source rejected: {0}")]
    Rejected(String),
    #[error("committed lifecycle event {sequence} has an invalid RunState payload")]
    InvalidState { sequence: u64 },
    #[error("committed lifecycle event {sequence} references an unknown run")]
    UnknownRun { sequence: u64 },
}

#[async_trait]
pub trait RunLifecycleFeed: Send + Sync {
    async fn events_after(
        &self,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError>;
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

/// Classify one committed Run transition using the preceding state for that Run.
///
/// Store-native lifecycle feeds use the same classifier as the portable
/// [`CheckpointRunLifecycleFeed`], so an authoritative database reader cannot
/// drift from the backend-neutral projection vocabulary.
#[must_use]
pub fn classify_run_lifecycle_event(
    state: &RunState,
    previous: Option<&RunState>,
) -> RunLifecycleEventKind {
    match state {
        RunState::Running if matches!(previous, Some(RunState::Awaiting)) => {
            RunLifecycleEventKind::Resumed
        }
        RunState::Running => RunLifecycleEventKind::Running,
        RunState::Awaiting => RunLifecycleEventKind::Awaiting,
        RunState::Ended(EndCause::NaturalEnd) => RunLifecycleEventKind::Completed,
        RunState::Ended(EndCause::Cancelled) => RunLifecycleEventKind::Cancelled,
        RunState::Ended(
            EndCause::MaxSteps
            | EndCause::Stopped(_)
            | EndCause::Error(_)
            | EndCause::Indeterminate,
        ) => RunLifecycleEventKind::Failed,
    }
}

/// Classify one audit record which belongs to the neutral Run lifecycle feed.
///
/// `RunStateChanged` retains the existing state-machine classifier;
/// `RunRescheduled` is an observation derived from the dispatch retry authority
/// and cannot alter the preceding Run state. Every feed backend calls this
/// helper so adding the new fact does not create backend-specific vocabulary.
#[must_use]
pub fn classify_run_lifecycle_record(
    kind: &AuditKind,
    state: &RunState,
    previous: Option<&RunState>,
) -> Option<RunLifecycleEventKind> {
    match kind {
        AuditKind::RunStateChanged => Some(classify_run_lifecycle_event(state, previous)),
        AuditKind::RunRescheduled => Some(RunLifecycleEventKind::Rescheduled),
        AuditKind::ModelRequestCompleted
        | AuditKind::StateChanged
        | AuditKind::RunAwaiting
        | AuditKind::RunResumed
        | AuditKind::PermissionDecided
        | AuditKind::Continuation => None,
    }
}

#[async_trait]
impl RunLifecycleFeed for CheckpointRunLifecycleFeed {
    async fn events_after(
        &self,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError> {
        checkpoint_lifecycle_events_after(self.reader.as_ref(), cursor, limit)
    }
}

/// Canonical lifecycle projection for a single-process checkpoint reader.
/// Store adapters with a native cross-process feed override this at their
/// application boundary; all other backends reuse this exact classifier and
/// cursor algorithm without growing another projection path.
pub fn checkpoint_lifecycle_events_after<R>(
    reader: &R,
    cursor: RunLifecycleCursor,
    limit: usize,
) -> Result<RunLifecyclePage, RunLifecycleFeedError>
where
    R: CheckpointReader + ?Sized,
{
    if limit == 0 {
        return Ok(RunLifecyclePage {
            events: Vec::new(),
            next_cursor: cursor,
        });
    }
    let records = reader.list_events(&EventScope::All, None, usize::MAX);
    let mut previous = HashMap::<RunId, RunState>::new();
    let mut events = Vec::with_capacity(limit.min(records.len()));
    for record in records {
        if !matches!(
            record.kind,
            AuditKind::RunStateChanged | AuditKind::RunRescheduled
        ) {
            continue;
        }
        let state = serde_json::from_value::<RunState>(
            record.payload.get("state").cloned().unwrap_or_default(),
        )
        .map_err(|_| RunLifecycleFeedError::InvalidState {
            sequence: record.sequence,
        })?;
        let await_reason = record
            .payload
            .get("await_reason")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| RunLifecycleFeedError::InvalidState {
                sequence: record.sequence,
            })?;
        let Some(kind) =
            classify_run_lifecycle_record(&record.kind, &state, previous.get(&record.run_id))
        else {
            continue;
        };
        if record.kind == AuditKind::RunStateChanged {
            previous.insert(record.run_id.clone(), state.clone());
        }
        if record.sequence <= cursor.0 {
            continue;
        }
        let run = reader
            .run(&record.run_id)
            .ok_or(RunLifecycleFeedError::UnknownRun {
                sequence: record.sequence,
            })?;
        events.push(RunLifecycleEvent {
            cursor: RunLifecycleCursor(record.sequence),
            source_commit_cursor: decode_run_lifecycle_cursor(RunLifecycleCursor(record.sequence))
                .0,
            thread_id: run.thread_id,
            run_id: record.run_id,
            kind,
            state,
            await_reason,
        });
        if events.len() == limit {
            break;
        }
    }
    let next_cursor = events.last().map_or(cursor, |event| event.cursor);
    Ok(RunLifecyclePage {
        events,
        next_cursor,
    })
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
    use crate::thread::read::committed_thread_view::CommittedThreadView;

    struct Reader {
        events: Vec<EventRecord>,
        runs: HashMap<RunId, RunRecord>,
        _sync: Mutex<()>,
    }

    impl CommittedThreadView for Reader {
        fn committed_messages(&self, _thread_id: &ThreadId) -> Vec<Message> {
            Vec::new()
        }

        fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
            None
        }

        fn run(&self, run_id: &RunId) -> Option<RunRecord> {
            self.runs.get(run_id).cloned()
        }

        fn latest_run(&self, _thread_id: &ThreadId) -> Option<RunRecord> {
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

    fn rescheduled_event(
        sequence: u64,
        run_id: &RunId,
        state: RunState,
        claim_epoch: u64,
    ) -> EventRecord {
        EventRecord {
            sequence,
            run_id: run_id.clone(),
            kind: AuditKind::RunRescheduled,
            payload: serde_json::json!({
                "state": state,
                "claim_epoch": claim_epoch,
            }),
        }
    }

    #[test]
    fn persisted_cursor_codec_is_bijective_within_bounds_and_fails_closed() {
        // Cause/effect graph: C1 source commit is representable; C2 event offset
        // is below/at the 1000-slot boundary; C3 multiplication/addition fits
        // u64. Effects: E1 encode preserves the historical cursor; E2 decode
        // recovers commit+offset; E3 invalid offset/overflow is rejected.
        //
        // | Rule | C1 | C2 offset | C3 | Effect |
        // |---|---|---|---|---|
        // | D1 | T | 0 | T | E1 cursor=1000, E2 commit=1 |
        // | D2 | T | 999 | T | E1/E2 upper valid slot |
        // | D3 | T | 1000 | - | E3 offset rejection |
        // | D4 | T | <1000 | F | E3 overflow rejection |
        // Constraints/invariants: one source commit owns exactly 1000 event
        // slots and encoding never wraps the persisted u64 cursor.
        let first = encode_run_lifecycle_cursor(1, 0).expect("D1 first event");
        assert_eq!(first, RunLifecycleCursor(1_000), "D1/E1");
        assert_eq!(decode_run_lifecycle_cursor(first), (1, 0), "D1/E2");

        let upper = encode_run_lifecycle_cursor(1, 999).expect("D2 upper event slot");
        assert_eq!(upper, RunLifecycleCursor(1_999), "D2/E1");
        assert_eq!(decode_run_lifecycle_cursor(upper), (1, 999), "D2/E2");

        assert_eq!(
            encode_run_lifecycle_cursor(1, 1_000),
            Err(RunLifecycleCursorCodecError::OffsetOutOfRange {
                offset: 1_000,
                max_exclusive: RUN_LIFECYCLE_CURSOR_STRIDE,
            }),
            "D3/E3"
        );

        let largest_commit = u64::MAX / RUN_LIFECYCLE_CURSOR_STRIDE;
        let largest_offset = (u64::MAX % RUN_LIFECYCLE_CURSOR_STRIDE) as usize;
        assert_eq!(
            encode_run_lifecycle_cursor(largest_commit, largest_offset),
            Ok(RunLifecycleCursor(u64::MAX)),
            "D4 boundary setup"
        );
        assert_eq!(
            encode_run_lifecycle_cursor(largest_commit, largest_offset + 1),
            Err(RunLifecycleCursorCodecError::Overflow {
                source_commit_cursor: largest_commit,
                offset: largest_offset + 1,
            }),
            "D4/E3 addition overflow"
        );
        assert_eq!(
            encode_run_lifecycle_cursor(largest_commit + 1, 0),
            Err(RunLifecycleCursorCodecError::Overflow {
                source_commit_cursor: largest_commit + 1,
                offset: 0,
            }),
            "D4/E3 multiplication overflow"
        );
    }

    #[test]
    fn lifecycle_event_source_commit_cursor_is_backward_deserialization_compatible() {
        // Cause/effect graph: C1 old serialized event omits the new field;
        // C2 new serialized event includes it. Effects: E1 old payload decodes
        // with the documented zero sentinel; E2 new payload round-trips the
        // authoritative source commit. Decision table: S1=C1=>E1; S2=C2=>E2.
        // Constraints/invariants: the zero sentinel denotes legacy absence only;
        // it cannot replace a source cursor emitted by a current writer.
        let event = RunLifecycleEvent {
            cursor: RunLifecycleCursor(1_000),
            source_commit_cursor: 1,
            thread_id: ThreadId("thread".into()),
            run_id: RunId("run".into()),
            kind: RunLifecycleEventKind::Running,
            state: RunState::Running,
            await_reason: None,
        };
        let mut legacy = serde_json::to_value(&event).expect("serialize event");
        legacy
            .as_object_mut()
            .expect("event object")
            .remove("source_commit_cursor");
        let decoded: RunLifecycleEvent = serde_json::from_value(legacy).expect("S1 legacy decode");
        assert_eq!(decoded.source_commit_cursor, 0, "S1/E1");

        let round_trip: RunLifecycleEvent =
            serde_json::from_value(serde_json::to_value(&event).expect("serialize current event"))
                .expect("S2 current decode");
        assert_eq!(round_trip.source_commit_cursor, 1, "S2/E2");
    }

    #[tokio::test]
    async fn feed_classifies_resume_terminal_causes_and_pages_exclusively() {
        // Cause/effect graph: C1 four encoded checkpoint events span commits
        // 1..=4; C2 pages use an exclusive event cursor; C3 Running follows
        // Awaiting. Effects: E1 page kinds are stable; E2 source commit cursors
        // are decoded from persisted event IDs; E3 page two starts after page
        // one's exact cursor. Decision table: F1=C1=>E1,E2;
        // F2=C1+C2=>E1,E2,E3; F3=C1+C2+C3=>Resumed classification.
        // Constraints/invariants: cursors are exclusive and commit ordered;
        // classification reads committed state without creating lifecycle facts.
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
            .map(|(index, state)| {
                let cursor = encode_run_lifecycle_cursor((index + 1) as u64, 0)
                    .expect("encoded checkpoint event");
                state_event(cursor.0, &run_id, state)
            })
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

        let first = feed.events_after(RunLifecycleCursor(0), 2).await.unwrap();
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                RunLifecycleEventKind::Running,
                RunLifecycleEventKind::Awaiting
            ]
        );
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| event.source_commit_cursor)
                .collect::<Vec<_>>(),
            vec![1, 2],
            "F1/E2"
        );
        let second = feed.events_after(first.next_cursor, 10).await.unwrap();
        assert_eq!(
            second
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                RunLifecycleEventKind::Resumed,
                RunLifecycleEventKind::Failed
            ]
        );
        assert_eq!(
            second
                .events
                .iter()
                .map(|event| event.source_commit_cursor)
                .collect::<Vec<_>>(),
            vec![3, 4],
            "F2/E2/E3"
        );
        assert_eq!(second.next_cursor, RunLifecycleCursor(4_000));
    }

    #[tokio::test]
    async fn reschedule_fact_is_ordered_without_mutating_resume_classification() {
        // Causes: C1 a Run changes Running→Awaiting; C2 dispatch recovery
        // commits RunRescheduled while preserving Awaiting; C3 execution resumes
        // and completes; C4 the consumer retries the same exclusive cursor.
        // Effects: E1 the feed reports one Rescheduled in commit order; E2 the
        // later Running remains Resumed (the observation did not become state);
        // E3 replay after the last cursor is empty and stable.
        //
        // | Rule | C1 | C2 | C3 | Cursor | Effects |
        // |---|---|---|---|---|---|
        // | R1 | T | T | T | 0 | E1,E2 |
        // | R2 | T | T | T | last | E3 |
        // Constraints/invariants: RunRescheduled is an ordered observation, not
        // a state transition, and replay after a cursor is side-effect free.
        let run_id = RunId("rescheduled-run".into());
        let thread_id = ThreadId("rescheduled-thread".into());
        let events = vec![
            state_event(1_000, &run_id, RunState::Running),
            state_event(2_000, &run_id, RunState::Awaiting),
            rescheduled_event(3_000, &run_id, RunState::Awaiting, 2),
            state_event(4_000, &run_id, RunState::Running),
            state_event(5_000, &run_id, RunState::Ended(EndCause::NaturalEnd)),
        ];
        let reader = Reader {
            events,
            runs: HashMap::from([(
                run_id.clone(),
                RunRecord {
                    id: run_id,
                    thread_id,
                    state: RunState::Ended(EndCause::NaturalEnd),
                },
            )]),
            _sync: Mutex::new(()),
        };
        let feed = CheckpointRunLifecycleFeed::new(Arc::new(reader));

        let page = feed.events_after(RunLifecycleCursor(0), 10).await.unwrap();
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                RunLifecycleEventKind::Running,
                RunLifecycleEventKind::Awaiting,
                RunLifecycleEventKind::Rescheduled,
                RunLifecycleEventKind::Resumed,
                RunLifecycleEventKind::Completed,
            ],
            "R1/E1-E2"
        );
        let replay = feed.events_after(page.next_cursor, 10).await.unwrap();
        assert!(replay.events.is_empty(), "R2/E3");
        assert_eq!(replay.next_cursor, page.next_cursor, "R2/E3");
    }

    #[tokio::test]
    async fn zero_limit_keeps_the_requested_cursor_without_reading() {
        let feed = CheckpointRunLifecycleFeed::new(Arc::new(Reader {
            events: Vec::new(),
            runs: HashMap::new(),
            _sync: Mutex::new(()),
        }));

        let page = feed.events_after(RunLifecycleCursor(7), 0).await.unwrap();

        assert!(page.events.is_empty());
        assert_eq!(page.next_cursor, RunLifecycleCursor(7));
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
                .events_after(RunLifecycleCursor(0), 1)
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
                .events_after(RunLifecycleCursor(0), 1)
                .await
                .unwrap_err(),
            RunLifecycleFeedError::UnknownRun { sequence: 2 }
        );
    }

    #[test]
    fn terminal_classification_is_total() {
        assert_eq!(
            classify_run_lifecycle_event(&RunState::Ended(EndCause::NaturalEnd), None),
            RunLifecycleEventKind::Completed
        );
        assert_eq!(
            classify_run_lifecycle_event(&RunState::Ended(EndCause::Cancelled), None),
            RunLifecycleEventKind::Cancelled
        );
        assert_eq!(
            classify_run_lifecycle_event(&RunState::Ended(EndCause::Indeterminate), None),
            RunLifecycleEventKind::Failed
        );
    }
}
