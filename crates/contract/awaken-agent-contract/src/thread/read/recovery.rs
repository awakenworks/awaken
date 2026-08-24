//! Claim-scoped recovery reads for a database-independent Worker.
//!
//! A remote Worker cannot reconstruct a Run by calling the synchronous read
//! ports independently: a commit between those calls could combine values from
//! different committed prefixes. This port returns the complete execution view
//! under one backend consistency boundary. The snapshot is read-only; commit
//! authority remains with the coordinator.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::agent::awaiting::ResumeTicket;
use crate::agent::message::Message;
use crate::agent::run::{Id as RunId, Record as RunRecord};
use crate::agent::state::Command as StateCommand;
use crate::agent::thread::Id as ThreadId;
use crate::audit::record::Record as EventRecord;

/// One active resume ticket paired with the Run whose disposition owns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResumeTicket {
    pub run_id: RunId,
    pub ticket: ResumeTicket,
}

/// One internally consistent prefix of committed Thread truth.
///
/// `thread_version` is the count of commits on this Thread and is therefore the
/// optimistic-concurrency version for the aggregate. `store_cursor` is the
/// backend-wide committed sequence used only for diagnostics/feed backfill.
/// They are intentionally distinct: another Thread advancing `store_cursor`
/// must not conflict this Thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecoverySnapshot {
    pub thread_id: ThreadId,
    pub claimed_run_id: RunId,
    pub runs: Vec<RunRecord>,
    pub latest_run_id: Option<RunId>,
    pub messages: Vec<Message>,
    /// Backend commit coordinate for each message at the same index. Existing
    /// stores already persist this column; exposing it prevents a cold projector
    /// from moving every historical message behind every lifecycle transition.
    /// A legacy/remote snapshot may omit the vector, in which case consumers must
    /// not claim exact warm/cold total ordering for that prefix.
    #[serde(default)]
    pub message_commit_cursors: Vec<u64>,
    pub state: Vec<StateCommand>,
    /// Backend commit coordinate for each state command at the same index.
    /// State-backed public facts (for example context compaction and Outcome
    /// evaluation) use this existing durable coordinate instead of moving to a
    /// Run's earlier lifecycle cursor when discovered by a later projector.
    #[serde(default)]
    pub state_commit_cursors: Vec<u64>,
    /// Committed audit facts for this Thread, ordered by their durable event
    /// sequence and read under the same consistency boundary as messages,
    /// state, tickets, and `store_cursor`.
    #[serde(default)]
    pub events: Vec<EventRecord>,
    pub resume_tickets: Vec<RunResumeTicket>,
    pub thread_version: u64,
    pub store_cursor: u64,
    /// The stable ordinal a recovered Worker uses for the claimed Run's next
    /// logical commit. It is the number of that Run's committed operations.
    pub next_commit_ordinal: u64,
}

/// Failure to materialize an authoritative recovery prefix.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("recovery source rejected: {0}")]
    Rejected(String),
}

/// Asynchronous because a distributed store must read the authoritative
/// database in one repeatable-read (or stronger) transaction.
#[async_trait]
pub trait RunRecoverySource: Send + Sync {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError>;

    /// Recover one logical Thread from the physical Session partition selected by
    /// the guarded dispatch. Partition-bound sources may keep the default; a
    /// process-level source must override it instead of deriving physical
    /// ownership from the logical child id.
    async fn recovery_snapshot_in_session(
        &self,
        _session_thread_id: &ThreadId,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.recovery_snapshot(thread_id, claimed_run_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_snapshot_without_events_remains_readable() {
        // Causes: C1 an older Worker payload omits the newly projected audit
        // prefix. Effect E1: decode succeeds with an empty event list. Decision
        // rule R1=C1=>E1; writers still emit the field for new peers.
        // Constraints/invariants: compatibility defaults only the absent audit
        // prefix and preserves every existing recovery coordinate verbatim.
        let snapshot = RunRecoverySnapshot {
            thread_id: ThreadId("thread".into()),
            claimed_run_id: RunId("run".into()),
            runs: Vec::new(),
            latest_run_id: None,
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 0,
            store_cursor: 0,
            next_commit_ordinal: 0,
        };
        let mut legacy = serde_json::to_value(snapshot).unwrap();
        legacy.as_object_mut().unwrap().remove("events");
        let decoded: RunRecoverySnapshot = serde_json::from_value(legacy).expect("R1/E1");
        assert!(decoded.events.is_empty(), "R1/E1");
    }
}
