//! Worker-local read projection loaded from committed Control-Node truth.
//!
//! The projection exists only to satisfy the runtime's synchronous read ports on
//! a database-independent Worker. It never commits or persists truth. Each
//! claimed attempt replaces it with a claim-authorized snapshot, and successful
//! remote commits advance it monotonically.

use std::sync::RwLock;

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::state::Command as StateCommand;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::Error as CommitError;
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitReceipt};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, RunResumeTicket};

#[derive(Debug, thiserror::Error)]
pub enum RecoveryProjectionError {
    #[error("recovery projection lock is poisoned")]
    Poisoned,
    #[error("recovery snapshot run {actual} does not match claimed run {expected}")]
    WrongRun { expected: String, actual: String },
}

/// Non-authoritative committed-read cache for one remote Worker session.
#[derive(Debug, Default)]
pub struct RecoveryProjection {
    snapshot: RwLock<Option<RunRecoverySnapshot>>,
}

impl RecoveryProjection {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically install the prefix fetched under `claimed_run_id`'s fence.
    pub fn install(
        &self,
        claimed_run_id: &RunId,
        snapshot: RunRecoverySnapshot,
    ) -> Result<(), RecoveryProjectionError> {
        if &snapshot.claimed_run_id != claimed_run_id {
            return Err(RecoveryProjectionError::WrongRun {
                expected: claimed_run_id.0.clone(),
                actual: snapshot.claimed_run_id.0,
            });
        }
        *self
            .snapshot
            .write()
            .map_err(|_| RecoveryProjectionError::Poisoned)? = Some(snapshot);
        Ok(())
    }

    /// Advance the cache only after the coordinator acknowledged this commit.
    pub fn apply_committed(
        &self,
        commit: ThreadCommit,
        record: &CommitRecord,
    ) -> Result<(), CommitError> {
        let mut guard = self
            .snapshot
            .write()
            .map_err(|_| CommitError::Rejected("recovery projection poisoned".to_string()))?;
        let snapshot = guard.as_mut().ok_or_else(|| {
            CommitError::Rejected("remote commit has no installed recovery snapshot".to_string())
        })?;
        if snapshot.thread_id != commit.thread_id || snapshot.claimed_run_id != *commit.run_id() {
            return Err(CommitError::Rejected(
                "remote commit does not match the installed recovery projection".to_string(),
            ));
        }

        apply_to_snapshot(snapshot, commit, record.sequence)
    }

    /// Apply an authoritative operation receipt exactly once. A duplicate
    /// receipt after response loss advances an unadvanced cache, while a
    /// duplicate after a successful local projection is a verified no-op.
    pub fn apply_receipt(
        &self,
        operation: CommitOperation,
        receipt: &CommitReceipt,
    ) -> Result<(), CommitError> {
        if receipt.operation_id != operation.operation_id
            || receipt.payload_hash != operation.payload_hash
        {
            return Err(CommitError::Rejected(
                "commit receipt does not match the submitted operation".to_string(),
            ));
        }
        let mut guard = self
            .snapshot
            .write()
            .map_err(|_| CommitError::Rejected("recovery projection poisoned".to_string()))?;
        let snapshot = guard.as_mut().ok_or_else(|| {
            CommitError::Rejected("remote commit has no installed recovery snapshot".to_string())
        })?;
        if snapshot.thread_id != operation.commit.thread_id
            || snapshot.claimed_run_id != *operation.commit.run_id()
        {
            return Err(CommitError::Rejected(
                "remote commit does not match the installed recovery projection".to_string(),
            ));
        }
        let next_ordinal = operation
            .operation_id
            .ordinal
            .checked_add(1)
            .ok_or_else(|| CommitError::Rejected("commit ordinal overflow".to_string()))?;
        let next_version = operation
            .expected_thread_version
            .checked_add(1)
            .ok_or_else(|| CommitError::Rejected("Thread version overflow".to_string()))?;
        if snapshot.thread_version == receipt.thread_version
            && snapshot.next_commit_ordinal == next_ordinal
            && snapshot.store_cursor >= receipt.commit_sequence
        {
            return Ok(());
        }
        if snapshot.thread_version != operation.expected_thread_version
            || receipt.thread_version != next_version
            || snapshot.next_commit_ordinal != operation.operation_id.ordinal
        {
            return Err(CommitError::Rejected(format!(
                "commit receipt projection gap: cache version {}, expected {}, receipt {}",
                snapshot.thread_version, operation.expected_thread_version, receipt.thread_version
            )));
        }
        apply_to_snapshot(snapshot, operation.commit, receipt.commit_sequence)
    }

    #[must_use]
    pub fn current(&self) -> Option<RunRecoverySnapshot> {
        self.snapshot
            .read()
            .ok()
            .and_then(|snapshot| snapshot.clone())
    }

    pub fn clear(&self) {
        if let Ok(mut snapshot) = self.snapshot.write() {
            *snapshot = None;
        }
    }
}

fn apply_to_snapshot(
    snapshot: &mut RunRecoverySnapshot,
    commit: ThreadCommit,
    commit_sequence: u64,
) -> Result<(), CommitError> {
    // Validate every monotonic counter before mutating the projection. A failed
    // boundary check must not append messages/state and leave a partially advanced
    // cache that no longer matches committed Coordinator truth.
    let next_thread_version = snapshot
        .thread_version
        .checked_add(1)
        .ok_or_else(|| CommitError::Rejected("Thread version overflow".to_string()))?;
    let next_commit_ordinal = snapshot
        .next_commit_ordinal
        .checked_add(1)
        .ok_or_else(|| CommitError::Rejected("commit ordinal overflow".to_string()))?;
    let run_id = commit.run_id().clone();
    let run_state = commit.run_state();
    let resume_ticket = commit.resume_ticket().cloned();
    snapshot.messages.extend(commit.messages);
    snapshot.state.extend(commit.state);
    let record_value = RunRecord {
        id: run_id.clone(),
        thread_id: commit.thread_id,
        state: run_state.clone(),
    };
    if let Some(existing) = snapshot.runs.iter_mut().find(|run| run.id == run_id) {
        *existing = record_value;
    } else {
        snapshot.runs.push(record_value);
    }
    snapshot.latest_run_id = Some(run_id.clone());
    snapshot
        .resume_tickets
        .retain(|entry| entry.run_id != run_id);
    if let Some(ticket) = resume_ticket
        && matches!(run_state, RunState::Awaiting)
    {
        snapshot
            .resume_tickets
            .push(RunResumeTicket { run_id, ticket });
    }
    snapshot.thread_version = next_thread_version;
    snapshot.store_cursor = snapshot.store_cursor.max(commit_sequence);
    snapshot.next_commit_ordinal = next_commit_ordinal;
    Ok(())
}

impl CommittedThreadView for RecoveryProjection {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        self.snapshot
            .read()
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .as_ref()
                    .filter(|snapshot| &snapshot.thread_id == thread_id)
                    .map(|snapshot| snapshot.messages.clone())
            })
            .unwrap_or_default()
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.snapshot.read().ok().and_then(|snapshot| {
            snapshot
                .as_ref()?
                .resume_tickets
                .iter()
                .find(|entry| &entry.run_id == run_id)
                .map(|entry| entry.ticket.clone())
        })
    }

    fn run(&self, run_id: &RunId) -> Option<RunRecord> {
        self.snapshot.read().ok().and_then(|snapshot| {
            snapshot
                .as_ref()?
                .runs
                .iter()
                .find(|run| &run.id == run_id)
                .cloned()
        })
    }

    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        self.snapshot.read().ok().and_then(|snapshot| {
            let snapshot = snapshot.as_ref()?;
            if &snapshot.thread_id != thread_id {
                return None;
            }
            let run_id = snapshot.latest_run_id.as_ref()?;
            snapshot.runs.iter().find(|run| &run.id == run_id).cloned()
        })
    }

    fn run_state(&self, run_id: &RunId) -> Option<RunState> {
        self.snapshot.read().ok().and_then(|snapshot| {
            snapshot
                .as_ref()?
                .runs
                .iter()
                .find(|run| &run.id == run_id)
                .map(|run| run.state.clone())
        })
    }

    fn committed_state(&self, thread_id: &ThreadId) -> Vec<StateCommand> {
        self.snapshot
            .read()
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .as_ref()
                    .filter(|snapshot| &snapshot.thread_id == thread_id)
                    .map(|snapshot| snapshot.state.clone())
            })
            .unwrap_or_default()
    }
}
