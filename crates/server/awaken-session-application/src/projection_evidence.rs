// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: Read-only evidence gathering moved from the Managed committed projector behind the
// @formal reason: SessionProjectionSource port: retained terminal Outcomes before the root fence, the
// @formal reason: root prefix lent to coordinated-link projection, child prefixes, then the ordered
// @formal reason: merge of their lifecycle. It publishes nothing; the Managed projector still re-reads
// @formal reason: and compares the same prefixes before its CAS publication. Warm/cold, read-order and
// @formal reason: prefix-race projection tests plus the lifecycle merge decision table cover behavior;
// @formal reason: this is reducer TCB linked to the existing paired-publication obligation, not a proof.
//! Read-only committed evidence for protocol Session projections.
//!
//! The Managed adapter used to gather this itself, straight from the Runtime.
//! It now reads through the Session application, which in turn sees only the
//! [`SessionProjectionSource`] port: committed Thread prefixes, coordinated
//! links and terminal Outcomes, never an execution context or Environment.

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent};
use awaken_session_error_contract::RunError;
use awaken_session_event_contract::{SessionEventBatch, SessionEventCommand};
use awaken_session_execution_contract::{
    CommittedOutcomeProjection, CoordinatedThreadLink, SessionProjectionSource,
};
use awaken_session_state_contract::PersistedSession;

use super::SessionApplication;

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
/// in the fixed order that keeps them one fenced prefix. Consumer: the Managed
/// committed projector, which validates it again after building.
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

impl SessionApplication {
    fn projection_source(&self) -> &(impl SessionProjectionSource + ?Sized) {
        self.runtime()
    }

    /// Read one logical Thread prefix through the parent Session partition.
    /// A prefix for a different Thread is a Runtime fault, never data.
    pub async fn projection_thread_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<Option<RunRecoverySnapshot>, RunError> {
        let snapshot = self
            .projection_source()
            .projection_thread_snapshot(session_id, thread_id)
            .await?;
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.thread_id.0 != thread_id)
        {
            return Err(RunError::internal(
                "Runtime returned a recovery snapshot for a different Session Thread",
            ));
        }
        Ok(snapshot)
    }

    /// Collect one deterministic coordinated-Thread prefix. Callers compare two
    /// collections around projection reads so independent child commits cannot
    /// form a mixed public prefix. Each link's latest Run is the one its own
    /// snapshot committed.
    pub async fn projection_coordinated_prefix(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&RunRecoverySnapshot>,
    ) -> Result<(Vec<CoordinatedThreadLink>, ProjectionThreadSnapshots), RunError> {
        let mut links = self
            .projection_source()
            .projection_coordinated_threads(session_id, observed_root)
            .await?;
        let mut snapshots = ProjectionThreadSnapshots::new();
        for link in &links {
            // A link may commit before its child has a first Run.
            if let Some(snapshot) = self
                .projection_thread_snapshot(session_id, &link.thread_id.0)
                .await?
            {
                snapshots.insert(link.thread_id.0.clone(), snapshot);
            }
        }
        for link in &mut links {
            if let Some(snapshot) = snapshots.get(&link.thread_id.0) {
                link.latest_run_id = snapshot.latest_run_id.clone();
            }
        }
        links.sort_by(|left, right| left.thread_id.0.cmp(&right.thread_id.0));
        Ok((links, snapshots))
    }

    /// Read the complete Runtime evidence of one projection candidate after
    /// the caller has read the durable Session root (so a Worker-side
    /// realization mutation joins the same candidate) and taken any
    /// process-local publication token it fences on.
    pub async fn read_projection_evidence(
        &self,
        persisted: &PersistedSession,
        started: std::time::Instant,
    ) -> Result<SessionProjectionEvidence, RunError> {
        let session_id = &persisted.session_id;
        let mut outcome_terminals = Vec::new();
        for outcome_id in retained_outcome_ids(&persisted.event_batches) {
            if let Some(terminal) = self
                .projection_source()
                .projection_outcome(session_id.as_str(), &outcome_id)
                .await?
            {
                outcome_terminals.push((outcome_id, terminal));
            }
        }
        let outcomes = started.elapsed();
        // Read the root fence once and lend that atomic prefix to the
        // relationship projection: it includes every link committed at or
        // before this fence, so an unknown lifecycle event is classifiable
        // without guessing from its Thread id.
        let root = self
            .projection_thread_snapshot(session_id, session_id.as_str())
            .await?;
        let (links, children) = self
            .projection_coordinated_prefix(session_id, root.as_ref())
            .await?;
        let snapshots = started.elapsed();
        // Lifecycle is derived from these exact atomic prefixes; reopening the
        // global feed would scan unrelated Sessions and add an unfenced read.
        let lifecycle = committed_lifecycle_prefix(root.iter().chain(children.values()))?;
        Ok(SessionProjectionEvidence {
            outcome_terminals,
            root,
            links,
            children,
            lifecycle,
            timings: SessionProjectionReadTimings {
                outcomes,
                snapshots,
                lifecycle: started.elapsed(),
            },
        })
    }
}

/// Outcome ids named by retained `DefineOutcome` provenance, first-seen order.
fn retained_outcome_ids(batches: &[SessionEventBatch]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    batches
        .iter()
        .flat_map(|batch| &batch.events)
        .filter_map(|entry| match &entry.event {
            SessionEventCommand::DefineOutcome { outcome_id, .. }
                if seen.insert(outcome_id.clone()) =>
            {
                Some(outcome_id.clone())
            }
            _ => None,
        })
        .collect()
}

/// Derive the complete lifecycle from already observed atomic prefixes.
/// Admission and projection use this same helper so neither can silently
/// truncate or reinterpret Awaiting commit coordinates.
pub fn committed_lifecycle_prefix<'a>(
    snapshots: impl IntoIterator<Item = &'a RunRecoverySnapshot>,
) -> Result<Vec<RunLifecycleEvent>, RunError> {
    let mut lifecycle_streams = Vec::new();
    for snapshot in snapshots {
        let page = snapshot
            .lifecycle_events_after(RunLifecycleCursor::default(), usize::MAX)
            .map_err(|error| RunError::internal(error.to_string()))?;
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
