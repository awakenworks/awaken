//! One internally consistent, process-local view of committed Thread truth.
//!
//! This is an execution view, not a persistence repository. A local authority
//! may project it from its committed facts, while a database-independent Worker
//! materializes the same view from one claim-fenced recovery snapshot. Durable
//! reads belong to [`super::checkpoint::CheckpointReader`].

use crate::agent::awaiting::ResumeTicket;
use crate::agent::message::Message;
use crate::agent::run::{Id as RunId, Record as RunRecord, RunState};
use crate::agent::state::Command as StateCommand;
use crate::agent::thread::Id as ThreadId;
use crate::thread::read::transcript::{
    TranscriptError, TranscriptSlice, TranscriptSliceSpec, TranscriptSnapshot,
    TranscriptSnapshotRef, TranscriptView,
};

/// The committed prefix supplied to one Runtime execution.
///
/// `run` addresses any Run retained by the Thread fact prefix; `latest_run`
/// answers the distinct aggregate-head question. Keeping both meanings on this
/// one view prevents the former `RunStore` compatibility port from changing
/// semantics between backends.
pub trait CommittedThreadView: Send + Sync {
    /// Committed messages for a thread, in commit order.
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message>;

    /// The committed record for any Run in this view, if present.
    fn run(&self, run_id: &RunId) -> Option<RunRecord>;

    /// The latest committed Run on a Thread, if present.
    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord>;

    /// The active awaiting ticket for a run, if it is currently awaiting.
    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket>;

    /// Latest committed lifecycle state for this Run.
    fn run_state(&self, run_id: &RunId) -> Option<RunState> {
        self.run(run_id).map(|record| record.state)
    }

    /// Committed state commands for a thread, in commit order.
    fn committed_state(&self, _thread_id: &ThreadId) -> Vec<StateCommand> {
        Vec::new()
    }

    /// Freeze the latest committed transcript as an immutable snapshot.
    fn transcript_snapshot(
        &self,
        thread_id: &ThreadId,
        view: TranscriptView,
    ) -> TranscriptSnapshot {
        TranscriptSnapshot::new(thread_id.clone(), view, self.committed_messages(thread_id))
    }

    /// Reconstruct a previously frozen append-only prefix and select ranges.
    fn transcript_slice(
        &self,
        spec: &TranscriptSliceSpec,
    ) -> Result<TranscriptSlice, TranscriptError> {
        let committed = self.committed_messages(&spec.snapshot.thread_id);
        let end = usize::try_from(spec.snapshot.end_seq)
            .map_err(|_| TranscriptError::SequenceOverflow)?;
        if committed.len() < end {
            return Err(TranscriptError::SnapshotUnavailable {
                requested_end: spec.snapshot.end_seq,
                available_end: u64::try_from(committed.len()).unwrap_or(u64::MAX),
            });
        }
        let snapshot = TranscriptSnapshot::new(
            spec.snapshot.thread_id.clone(),
            spec.snapshot.view,
            committed[..end].to_vec(),
        );
        snapshot.verify(&spec.snapshot)?;
        snapshot.slice(spec)
    }

    /// Validate that a frozen reference still names the same append-only prefix.
    fn verify_transcript_snapshot(
        &self,
        snapshot: &TranscriptSnapshotRef,
    ) -> Result<(), TranscriptError> {
        self.transcript_slice(&TranscriptSliceSpec {
            snapshot: snapshot.clone(),
            ranges: Vec::new(),
        })
        .map(|_| ())
    }
}
