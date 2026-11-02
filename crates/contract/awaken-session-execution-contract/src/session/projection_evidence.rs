// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: The Session's committed projection evidence and its two cursor rules, moved from
// @formal reason: awaken-managed-session-events beside the SessionProjectionSource port that reads them:
// @formal reason: the ordered merge of observed lifecycle prefixes and the exact Set+Remove commit of a
// @formal reason: terminal Outcome. Pure; malformed coordinates fail closed. The lifecycle-merge and
// @formal reason: Outcome-terminal decision tests move with them; warm/cold projection tests cover use.
//! The committed evidence one protocol projection candidate is built from,
//! read through [`super::SessionProjectionSource`], and the two cursor rules
//! its readers share: the merged lifecycle of observed prefixes and the exact
//! root commit that makes a terminal Outcome public.
//!
//! Consumers: `SessionApplication`'s projection-evidence reads (producer) and
//! the Managed event derivation (`awaken-managed-session-events`) with its
//! protocol adapter.

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent};

use crate::{CommittedOutcomeProjection, CoordinatedThreadLink};

mod prefix;
pub use prefix::same_thread_projection_prefix;

/// Borrowed committed sources; deriving a protocol candidate or admitting an
/// observational command never creates another retained snapshot authority.
pub struct SessionProjectionPrefix<'a> {
    pub root: Option<&'a RunRecoverySnapshot>,
    pub links: &'a [CoordinatedThreadLink],
    pub children: &'a ProjectionThreadSnapshots,
}

/// A child read follows the validation root only for a coordinated Session.
#[must_use]
pub const fn projection_validation_needs_trailing_root(
    coordinated_links: usize,
    child_snapshots: usize,
) -> bool {
    coordinated_links != 0 || child_snapshots != 0
}

/// Committed projection evidence that contradicts itself. Every reader fails
/// its whole candidate closed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CommittedEvidenceError {
    /// A Thread prefix's lifecycle cannot be read from its own audit rows.
    #[error(transparent)]
    Lifecycle(#[from] awaken_agent_contract::RunLifecycleFeedError),
    /// Outcome state commands were recovered without their commit cursors.
    #[error("Outcome state recovery omitted commit coordinates")]
    OutcomeStateCursorsMissing,
}

/// Coordinated child prefixes keyed by logical Thread id.
pub type ProjectionThreadSnapshots = std::collections::HashMap<String, RunRecoverySnapshot>;

/// Elapsed time at each read boundary since the caller's sampled start, for
/// the projector's diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct SessionProjectionReadTimings {
    pub outcomes: std::time::Duration,
    pub snapshots: std::time::Duration,
    pub lifecycle: std::time::Duration,
}

/// Every committed Runtime fact one projection candidate is built from, read
/// in the fixed order that keeps them one fenced prefix. The Session
/// application reads it through its read-only projection port; consumer: the
/// protocol projector, which validates the same prefixes again after building.
pub struct CommittedSessionFacts {
    /// Terminal Outcomes named by the root's retained `DefineOutcome`
    /// provenance, read after the root snapshot and checked against its exact
    /// terminal state/action cursor before any candidate may publish.
    pub outcome_terminals: Vec<(String, CommittedOutcomeProjection)>,
    /// A query lagged or ran ahead of the borrowed root's terminal state.
    /// The caller retries this source selection before any projection CAS.
    pub outcome_query_incomplete: bool,
    pub root: Option<RunRecoverySnapshot>,
    pub links: Vec<CoordinatedThreadLink>,
    pub children: ProjectionThreadSnapshots,
    /// The complete merged lifecycle of `root` and `children`.
    pub lifecycle: Vec<RunLifecycleEvent>,
    pub timings: SessionProjectionReadTimings,
}

/// Derive the complete lifecycle from already observed atomic prefixes.
/// Admission and projection use this same helper so neither can silently
/// truncate or reinterpret Awaiting commit coordinates.
pub fn committed_lifecycle_prefix<'a>(
    snapshots: impl IntoIterator<Item = &'a RunRecoverySnapshot>,
) -> Result<Vec<RunLifecycleEvent>, CommittedEvidenceError> {
    let mut lifecycle_streams = Vec::new();
    for snapshot in snapshots {
        let page = snapshot.lifecycle_events_after(RunLifecycleCursor::default(), usize::MAX)?;
        lifecycle_streams.push(page.events);
    }
    Ok(merge_ordered_lifecycle_streams(lifecycle_streams))
}

struct LifecycleMergeEntry {
    event: RunLifecycleEvent,
    stream_index: usize,
}

impl PartialEq for LifecycleMergeEntry {
    fn eq(&self, other: &Self) -> bool {
        self.event.cursor == other.event.cursor && self.stream_index == other.stream_index
    }
}

impl Eq for LifecycleMergeEntry {}

impl PartialOrd for LifecycleMergeEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LifecycleMergeEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap; reverse cursor/source comparison to pop the
        // smallest cursor first. Source order preserves sort_by_key's prior
        // stable tie behavior for equal cursors.
        other
            .event
            .cursor
            .cmp(&self.event.cursor)
            .then_with(|| other.stream_index.cmp(&self.stream_index))
    }
}

fn merge_ordered_lifecycle_streams(streams: Vec<Vec<RunLifecycleEvent>>) -> Vec<RunLifecycleEvent> {
    debug_assert!(streams.iter().all(|events| {
        events
            .windows(2)
            .all(|pair| pair[0].cursor <= pair[1].cursor)
    }));
    let capacity = streams.iter().map(Vec::len).sum();
    let mut streams = streams.into_iter().map(Vec::into_iter).collect::<Vec<_>>();
    let mut heap = std::collections::BinaryHeap::with_capacity(streams.len());
    for (stream_index, stream) in streams.iter_mut().enumerate() {
        if let Some(event) = stream.next() {
            heap.push(LifecycleMergeEntry {
                event,
                stream_index,
            });
        }
    }
    let mut merged = Vec::with_capacity(capacity);
    while let Some(entry) = heap.pop() {
        let stream_index = entry.stream_index;
        merged.push(entry.event);
        if let Some(event) = streams[stream_index].next() {
            heap.push(LifecycleMergeEntry {
                event,
                stream_index,
            });
        }
    }
    merged
}

/// The terminal state Set and active-pointer Remove must be one committed
/// Thread write. A protocol read may use this key/action/cursor proof without
/// decoding extension-private Outcome JSON or building a second query.
pub fn outcome_terminal_commit_cursor(
    outcome_id: &str,
    snapshot: &RunRecoverySnapshot,
) -> Result<Option<u64>, CommittedEvidenceError> {
    if snapshot.state.len() != snapshot.state_commit_cursors.len() {
        return Err(CommittedEvidenceError::OutcomeStateCursorsMissing);
    }
    let state_key = format!("outcome/{outcome_id}/state");
    let terminal = snapshot
        .state
        .iter()
        .zip(snapshot.state_commit_cursors.iter().copied())
        .rev()
        .find(|(command, _)| command.key.0 == state_key)
        .and_then(|(command, cursor)| {
            matches!(
                command.action,
                awaken_agent_contract::agent::state::Action::Set(_)
            )
            .then_some(cursor)
        });
    Ok(terminal.filter(|terminal| {
        snapshot
            .state
            .iter()
            .zip(snapshot.state_commit_cursors.iter().copied())
            .any(|(command, cursor)| {
                command.key.0 == "outcome/active"
                    && matches!(
                        command.action,
                        awaken_agent_contract::agent::state::Action::Remove
                    )
                    && cursor == *terminal
            })
    }))
}

#[cfg(test)]
mod lifecycle_merge_tests {
    use super::*;

    fn event(cursor: u64, stream: usize) -> RunLifecycleEvent {
        RunLifecycleEvent {
            cursor: RunLifecycleCursor(cursor),
            source_commit_cursor: cursor,
            thread_id: awaken_agent_contract::ThreadId(format!("thread-{stream}")),
            run_id: awaken_agent_contract::RunId(format!("run-{stream}-{cursor}")),
            kind: awaken_agent_contract::RunLifecycleEventKind::Running,
            state: awaken_agent_contract::agent::run::RunState::Running,
            await_reason: None,
        }
    }

    #[test]
    fn ordered_lifecycle_merge_matches_stable_full_sort() {
        // Causes: C1 zero/one/many source streams; C2 each stream is empty or
        // carries an ordered subset; C3 cursors may tie across streams. Effects:
        // E1 every event is retained once; E2 cursors are globally ordered; E3
        // equal cursors retain input-stream order. Exhaust every assignment of
        // six distinct cursors to three streams, then cover equal cursors.
        assert!(merge_ordered_lifecycle_streams(Vec::new()).is_empty(), "M1");
        for assignment in 0_u32..3_u32.pow(6) {
            let mut value = assignment;
            let mut streams = vec![Vec::new(), Vec::new(), Vec::new()];
            for cursor in 1..=6 {
                let stream = (value % 3) as usize;
                value /= 3;
                streams[stream].push(event(cursor, stream));
            }
            let mut expected = streams.iter().flatten().cloned().collect::<Vec<_>>();
            expected.sort_by_key(|item| item.cursor);
            let merged = merge_ordered_lifecycle_streams(streams);
            assert_eq!(
                merged.iter().map(|item| item.cursor.0).collect::<Vec<_>>(),
                expected
                    .iter()
                    .map(|item| item.cursor.0)
                    .collect::<Vec<_>>(),
                "M2 assignment {assignment}",
            );
        }
        let tied = merge_ordered_lifecycle_streams(vec![
            vec![event(1, 0), event(3, 0)],
            vec![event(1, 1), event(2, 1)],
        ]);
        assert_eq!(
            tied.iter()
                .map(|item| (item.cursor.0, item.thread_id.0.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (1, "thread-0"),
                (1, "thread-1"),
                (2, "thread-1"),
                (3, "thread-0")
            ],
            "M3 stable tie order",
        );
    }

    #[test]
    fn lifecycle_merge_scaling_matrix_preserves_every_ordered_prefix() {
        // Scaling matrix: H=100/1k/10k lifecycle facts x S=1/8/25 ordered
        // Thread streams. Effects: every fact appears once, global cursor order
        // is monotonic, and elapsed observations remain diagnostic rather than a
        // machine-speed correctness threshold. K3D owns the public p95 gate.
        for total in [100_usize, 1_000, 10_000] {
            for source_count in [1_usize, 8, 25] {
                let mut streams = vec![Vec::new(); source_count];
                for cursor in 1..=total {
                    let source = cursor % source_count;
                    streams[source].push(event(cursor as u64, source));
                }
                let started = std::time::Instant::now();
                let merged = merge_ordered_lifecycle_streams(streams);
                let elapsed = started.elapsed();
                assert_eq!(merged.len(), total, "H={total} S={source_count}");
                assert!(
                    merged
                        .windows(2)
                        .all(|pair| pair[0].cursor <= pair[1].cursor),
                    "H={total} S={source_count}"
                );
                eprintln!(
                    "LIFECYCLE_MERGE_BENCH {{\"history\":{total},\"sources\":{source_count},\"elapsed_us\":{}}}",
                    elapsed.as_micros(),
                );
            }
        }
    }
}

#[cfg(test)]
mod outcome_terminal_tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope};
    use awaken_agent_contract::agent::thread::Id as ThreadId;

    #[test]
    fn outcome_terminal_query_must_match_exact_root_state_and_pointer_commit() {
        // Causes: C1 no Outcome state, active state Set, terminal state Set plus
        // same-commit active Remove, or a torn/wrong-action pair; C2 the typed
        // terminal query is None or Some; C3 state/cursor vectors align or not.
        // Effects: E1 only exact Set+Remove shares a terminal cursor; E2
        // root-terminal/query agreement can publish, a lagging None must retry,
        // and a query ahead of the root is withheld by its missing anchor; E3
        // malformed coordinates fail closed. Rules R1 absent/active+None=>E2;
        // R2 exact terminal+Some=>E1+E2; R3 exact terminal+None=>retry;
        // R4 active+Some=>withhold; R5 torn/wrong-action or a later state
        // Remove=>no terminal;
        // R6 !C3=>E3. No private Outcome
        // JSON or wall time participates in the decision.
        let set = |key| {
            Command::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                key,
                serde_json::json!({}),
            )
        };
        let remove = |key| Command::remove(Scope::Thread, MergePolicy::Disjoint, key);
        let snapshot = |entries: Vec<(Command, u64)>| {
            let (state, state_commit_cursors) = entries.into_iter().unzip();
            RunRecoverySnapshot {
                commit_timestamps: Default::default(),
                thread_id: ThreadId("root".into()),
                claimed_run_id: RunId("outcome-run".into()),
                runs: Vec::new(),
                latest_run_id: None,
                messages: Vec::new(),
                message_commit_cursors: Vec::new(),
                state,
                state_commit_cursors,
                events: Vec::new(),
                resume_tickets: Vec::new(),
                thread_version: 1,
                store_cursor: 30,
                next_commit_ordinal: 0,
            }
        };
        let active = snapshot(vec![(set("outcome/id/state"), 10)]);
        let terminal = snapshot(vec![
            (set("outcome/id/state"), 20),
            (remove("outcome/active"), 20),
        ]);
        for (rule, source, terminal_query, expected_cursor, complete) in [
            ("R1 absent", snapshot(Vec::new()), false, None, true),
            ("R1 active", active.clone(), false, None, true),
            ("R2 terminal", terminal.clone(), true, Some(20), true),
            ("R3 stale query", terminal, false, Some(20), false),
            ("R4 ahead query", active, true, None, true),
            (
                "R4 torn",
                snapshot(vec![
                    (set("outcome/id/state"), 20),
                    (remove("outcome/active"), 21),
                ]),
                false,
                None,
                true,
            ),
            (
                "R4 wrong state action",
                snapshot(vec![
                    (remove("outcome/id/state"), 20),
                    (remove("outcome/active"), 20),
                ]),
                false,
                None,
                true,
            ),
            (
                "R5 terminal state removed later",
                snapshot(vec![
                    (set("outcome/id/state"), 20),
                    (remove("outcome/active"), 20),
                    (remove("outcome/id/state"), 21),
                ]),
                false,
                None,
                true,
            ),
        ] {
            let cursor = outcome_terminal_commit_cursor("id", &source).unwrap();
            assert_eq!(cursor, expected_cursor, "{rule} exact source");
            assert_eq!(
                cursor.is_none() || terminal_query,
                complete,
                "{rule} lag coverage"
            );
        }
        let mut corrupt = snapshot(vec![(set("outcome/id/state"), 20)]);
        corrupt.state_commit_cursors.clear();
        assert!(
            outcome_terminal_commit_cursor("id", &corrupt).is_err(),
            "R6/E3"
        );
    }
}
