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

/// A persisted waiting row is usable only when its closed ticket decodes and
/// names the same Run/Thread as the row and recovery partition. Keeping this
/// invariant in the neutral recovery contract prevents each durable adapter or
/// protocol projection from inventing a different corruption policy.
#[derive(Debug, thiserror::Error)]
pub enum ResumeTicketRecoveryError {
    #[error("persisted ResumeTicket is unreadable: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("ResumeTicket run_id does not match its waiting row")]
    RunMismatch,
    #[error("ResumeTicket thread_id does not match its recovery partition")]
    ThreadMismatch,
}

pub fn validate_resume_ticket_owner(
    ticket: &ResumeTicket,
    row_run_id: &RunId,
    expected_thread_id: &ThreadId,
) -> Result<(), ResumeTicketRecoveryError> {
    if &ticket.run_id != row_run_id {
        return Err(ResumeTicketRecoveryError::RunMismatch);
    }
    if &ticket.thread_id != expected_thread_id {
        return Err(ResumeTicketRecoveryError::ThreadMismatch);
    }
    Ok(())
}

pub fn decode_resume_ticket_value_for_owner(
    value: serde_json::Value,
    row_run_id: &RunId,
    expected_thread_id: &ThreadId,
) -> Result<ResumeTicket, ResumeTicketRecoveryError> {
    let ticket = serde_json::from_value(value)?;
    validate_resume_ticket_owner(&ticket, row_run_id, expected_thread_id)?;
    Ok(ticket)
}

pub fn decode_resume_ticket_json_for_owner(
    raw: &str,
    row_run_id: &RunId,
    expected_thread_id: &ThreadId,
) -> Result<ResumeTicket, ResumeTicketRecoveryError> {
    let ticket = serde_json::from_str(raw)?;
    validate_resume_ticket_owner(&ticket, row_run_id, expected_thread_id)?;
    Ok(ticket)
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

    #[test]
    fn persisted_resume_ticket_requires_exact_row_and_partition_ownership() {
        // Cause/effect graph: C1=wire decodes vs is malformed; C2=ticket Run
        // matches its waiting-row Run; C3=ticket Thread matches the recovery
        // partition. Effect E1=return the typed reply authority only for the
        // exact product; E2=reject every damaged product without mutation.
        // Decision table: T1=C1+C2+C3=>E1; T2=!C1=>E2; T3=C1+!C2=>E2;
        // T4=C1+C2+!C3=>E2. This is the single invariant shared by stores and
        // strict protocol admission.
        let run = RunId("ticket-owner-run".into());
        let thread = ThreadId("ticket-owner-thread".into());
        let ticket = ResumeTicket::new(
            "ticket-owner-correlation",
            run.clone(),
            thread.clone(),
            "ticket-owner-snapshot",
            "ticket-owner-catalog",
            crate::agent::awaiting::AwaitTarget::Pause(crate::agent::awaiting::PauseReason::Manual),
        );
        let value = serde_json::to_value(&ticket).unwrap();
        assert_eq!(
            decode_resume_ticket_value_for_owner(value.clone(), &run, &thread).expect("T1/E1"),
            ticket
        );
        assert!(
            decode_resume_ticket_json_for_owner("{}", &run, &thread).is_err(),
            "T2/E2"
        );
        assert!(
            decode_resume_ticket_value_for_owner(
                value.clone(),
                &RunId("another-run".into()),
                &thread,
            )
            .is_err(),
            "T3/E2"
        );
        assert!(
            decode_resume_ticket_value_for_owner(value, &run, &ThreadId("another-thread".into()),)
                .is_err(),
            "T4/E2"
        );
    }
}
