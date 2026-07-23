//! Committed-thread read access needed to resume an awaiting run.
//!
//! Resume reconstructs the transcript from committed messages and validates the
//! resume against the committed [`ResumeTicket`]. This is an after-commit read
//! port (G1/G13) — it never creates or erases runtime truth.

use crate::agent::awaiting::ResumeTicket;
use crate::agent::message::Message;
use crate::agent::run::{Id as RunId, RunState};
use crate::agent::state::Command as StateCommand;
use crate::agent::thread::Id as ThreadId;
use crate::thread::read::transcript::{
    TranscriptError, TranscriptSlice, TranscriptSliceSpec, TranscriptSnapshot,
    TranscriptSnapshotRef, TranscriptView,
};

pub trait ThreadReader: Send + Sync {
    /// Committed messages for a thread, in commit order.
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message>;

    /// The active awaiting ticket for a run, if it is currently awaiting. A run
    /// that has reached a terminal or resumed state returns `None`, so a stale
    /// resume against an old correlation fails closed.
    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket>;

    /// Latest committed lifecycle state for this Run. Used by stable-identity
    /// durable requests to reconnect to an existing child instead of creating a
    /// second execution. Readers without a run projection fail closed with None.
    fn run_state(&self, _run_id: &RunId) -> Option<RunState> {
        None
    }

    /// Committed state commands for a thread, in commit order. A run rebuilds the
    /// materialized `Store` from these to read accumulated state during
    /// execution (G1/G13); the default is empty for readers that hold no state.
    fn committed_state(&self, _thread_id: &ThreadId) -> Vec<StateCommand> {
        Vec::new()
    }

    /// Freeze the latest committed transcript as an immutable, content-addressed
    /// snapshot. The default implementation deliberately builds on the existing
    /// after-commit read so every store gains the neutral snapshot contract before
    /// storage-specific range/index optimizations are introduced.
    fn transcript_snapshot(
        &self,
        thread_id: &ThreadId,
        view: TranscriptView,
    ) -> TranscriptSnapshot {
        TranscriptSnapshot::new(thread_id.clone(), view, self.committed_messages(thread_id))
    }

    /// Reconstruct a previously frozen append-only prefix and select ranges from
    /// it. A later append is harmless: `end_seq` freezes the old prefix and the
    /// snapshot identity rejects a reference for another thread or view.
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
