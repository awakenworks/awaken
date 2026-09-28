//! Committed evidence of one Session projection candidate.

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent};
use awaken_session_execution_contract::{CommittedOutcomeProjection, CoordinatedThreadLink};

use crate::ProjectionError;

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
pub struct SessionProjectionEvidence {
    /// Terminal Outcomes named by the root's retained `DefineOutcome`
    /// provenance, read before the root fence: pairing an older state prefix
    /// with a newer terminal would manufacture an unanchored public event.
    pub outcome_terminals: Vec<(String, CommittedOutcomeProjection)>,
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
) -> Result<Vec<RunLifecycleEvent>, ProjectionError> {
    let mut lifecycle_streams = Vec::new();
    for snapshot in snapshots {
        let page = snapshot
            .lifecycle_events_after(RunLifecycleCursor::default(), usize::MAX)
            .map_err(|error| ProjectionError::corrupt(error.to_string()))?;
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
