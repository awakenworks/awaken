// @formal entrypoint remote_worker.acknowledged_projection_is_monotonic
// @formal entrypoint remote_worker.execution_requires_claimed_snapshot
// @formal entrypoint remote_worker.pending_signal_is_consumed_by_commit
// @formal entrypoint remote_worker.recovery_projection_is_committed_prefix
//! Worker-local read projection loaded from committed Control-Node truth.
//!
//! The projection exists only to satisfy the runtime's synchronous read ports on
//! a database-independent Worker. It never commits or persists truth. Each
//! claimed attempt replaces it with a claim-authorized snapshot, and successful
//! remote commits advance it monotonically.

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::state::Command as StateCommand;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::record::Record as EventRecord;
use awaken_agent_contract::thread::commit::coordinator::Error as CommitError;
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitReceipt, require_commit_operation_receipt,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::lifecycle::encode_run_lifecycle_cursor;
use awaken_agent_contract::thread::read::recovery::{
    AnswerableTicketError, CurrentRunError, RunRecordSelectionError, RunRecoverySnapshot,
    RunResumeTicket,
};
use awaken_sync::InvariantRwLock;

#[derive(Debug, thiserror::Error)]
pub enum RecoveryProjectionError {
    #[error("recovery projection lock is poisoned")]
    Poisoned,
    #[error("recovery snapshot run {actual} does not match claimed run {expected}")]
    WrongRun { expected: String, actual: String },
    #[error("recovery snapshot thread {actual} does not match claimed thread {expected}")]
    WrongThread { expected: String, actual: String },
    #[error("invalid recovery Run record: {0}")]
    InvalidRun(#[from] RunRecordSelectionError),
    #[error("invalid recovery Thread head: {0}")]
    InvalidHead(#[from] CurrentRunError),
    #[error("invalid recovery ticket: {0}")]
    InvalidTicket(#[from] AnswerableTicketError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoverySnapshotAdmission {
    Exact,
    WrongRun,
    WrongThread,
}

/// Classify the complete logical identity returned for one claim-fenced
/// recovery read. Run mismatch wins so a response can never be reinterpreted as
/// another claim merely because its Thread happens to match.
#[must_use]
const fn classify_recovery_snapshot_admission(
    run_matches: bool,
    thread_matches: bool,
) -> RecoverySnapshotAdmission {
    if !run_matches {
        RecoverySnapshotAdmission::WrongRun
    } else if !thread_matches {
        RecoverySnapshotAdmission::WrongThread
    } else {
        RecoverySnapshotAdmission::Exact
    }
}

/// Non-authoritative committed-read cache for one remote Worker session.
#[derive(Debug, Default)]
pub struct RecoveryProjection {
    snapshot: InvariantRwLock<Option<RunRecoverySnapshot>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProjectionAdvance {
    thread_version: u64,
    next_commit_ordinal: u64,
    store_cursor: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptProjectionDisposition {
    Replay,
    Apply(ProjectionAdvance),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptProjectionError {
    ProjectionIdentity,
    ThreadVersionOverflow,
    CommitOrdinalOverflow,
    Gap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReceiptProjectionFacts {
    projection_identity_matches: bool,
    snapshot_thread_version: u64,
    snapshot_next_ordinal: u64,
    snapshot_store_cursor: u64,
    operation_expected_version: u64,
    operation_ordinal: u64,
    receipt_thread_version: u64,
    receipt_commit_sequence: u64,
}

fn classify_receipt_projection(
    facts: ReceiptProjectionFacts,
) -> Result<ReceiptProjectionDisposition, ReceiptProjectionError> {
    if !facts.projection_identity_matches {
        return Err(ReceiptProjectionError::ProjectionIdentity);
    }
    let next_ordinal = facts
        .operation_ordinal
        .checked_add(1)
        .ok_or(ReceiptProjectionError::CommitOrdinalOverflow)?;
    let next_version = facts
        .operation_expected_version
        .checked_add(1)
        .ok_or(ReceiptProjectionError::ThreadVersionOverflow)?;
    if facts.snapshot_thread_version == facts.receipt_thread_version
        && facts.snapshot_next_ordinal == next_ordinal
        && facts.snapshot_store_cursor >= facts.receipt_commit_sequence
    {
        return Ok(ReceiptProjectionDisposition::Replay);
    }
    if facts.snapshot_thread_version != facts.operation_expected_version
        || facts.receipt_thread_version != next_version
        || facts.snapshot_next_ordinal != facts.operation_ordinal
    {
        return Err(ReceiptProjectionError::Gap);
    }
    Ok(ReceiptProjectionDisposition::Apply(ProjectionAdvance {
        thread_version: next_version,
        next_commit_ordinal: next_ordinal,
        store_cursor: facts
            .snapshot_store_cursor
            .max(facts.receipt_commit_sequence),
    }))
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
        claimed_thread_id: &ThreadId,
        snapshot: RunRecoverySnapshot,
    ) -> Result<(), RecoveryProjectionError> {
        match classify_recovery_snapshot_admission(
            &snapshot.claimed_run_id == claimed_run_id,
            &snapshot.thread_id == claimed_thread_id,
        ) {
            RecoverySnapshotAdmission::WrongRun => Err(RecoveryProjectionError::WrongRun {
                expected: claimed_run_id.0.clone(),
                actual: snapshot.claimed_run_id.0,
            }),
            RecoverySnapshotAdmission::WrongThread => Err(RecoveryProjectionError::WrongThread {
                expected: claimed_thread_id.0.clone(),
                actual: snapshot.thread_id.0,
            }),
            RecoverySnapshotAdmission::Exact => {
                // Preserve errors before exposing the Option-valued read port:
                // damaged claim/head truth must not look like unstarted work.
                snapshot.run_record(claimed_run_id)?;
                snapshot.answerable_ticket(claimed_run_id)?;
                if let Some(head) = snapshot.current_run()? {
                    snapshot.answerable_ticket(&head.id)?;
                }
                *self
                    .snapshot
                    .write()
                    .map_err(|_| RecoveryProjectionError::Poisoned)? = Some(snapshot);
                Ok(())
            }
        }
    }

    /// Apply an authoritative operation receipt exactly once. A duplicate
    /// receipt after response loss advances an unadvanced cache, while a
    /// duplicate after a successful local projection is a verified no-op.
    pub fn apply_receipt(
        &self,
        operation: CommitOperation,
        receipt: &CommitReceipt,
    ) -> Result<(), CommitError> {
        require_commit_operation_receipt(&operation, receipt)
            .map_err(|error| CommitError::Rejected(error.to_string()))?;
        let mut guard = self
            .snapshot
            .write()
            .map_err(|_| CommitError::Rejected("recovery projection poisoned".to_string()))?;
        let snapshot = guard.as_mut().ok_or_else(|| {
            CommitError::Rejected("remote commit has no installed recovery snapshot".to_string())
        })?;
        let disposition = classify_receipt_projection(ReceiptProjectionFacts {
            projection_identity_matches: snapshot.thread_id == operation.commit.thread_id
                && snapshot.claimed_run_id == *operation.commit.run_id(),
            snapshot_thread_version: snapshot.thread_version,
            snapshot_next_ordinal: snapshot.next_commit_ordinal,
            snapshot_store_cursor: snapshot.store_cursor,
            operation_expected_version: operation.expected_thread_version,
            operation_ordinal: operation.operation_id.ordinal,
            receipt_thread_version: receipt.thread_version,
            receipt_commit_sequence: receipt.commit_sequence,
        })
        .map_err(|error| match error {
            ReceiptProjectionError::ProjectionIdentity => CommitError::Rejected(
                "remote commit does not match the installed recovery projection".to_string(),
            ),
            ReceiptProjectionError::ThreadVersionOverflow => {
                CommitError::Rejected("Thread version overflow".to_string())
            }
            ReceiptProjectionError::CommitOrdinalOverflow => {
                CommitError::Rejected("commit ordinal overflow".to_string())
            }
            ReceiptProjectionError::Gap => CommitError::Rejected(format!(
                "commit receipt projection gap: cache version {}, expected {}, receipt {}",
                snapshot.thread_version, operation.expected_thread_version, receipt.thread_version
            )),
        })?;
        match disposition {
            ReceiptProjectionDisposition::Replay => Ok(()),
            ReceiptProjectionDisposition::Apply(advance) => {
                apply_to_snapshot(snapshot, operation.commit, receipt.commit_sequence, advance)
            }
        }
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
    advance: ProjectionAdvance,
) -> Result<(), CommitError> {
    // Validate every monotonic counter before mutating the projection. A failed
    // boundary check must not append messages/state and leave a partially advanced
    // cache that no longer matches committed Coordinator truth.
    let run_id = commit.run_id().clone();
    let run_state = commit.run_state();
    let resume_ticket = commit.resume_ticket().cloned();
    let committed_events = commit
        .events
        .iter()
        .enumerate()
        .map(|(offset, draft)| {
            let sequence = encode_run_lifecycle_cursor(commit_sequence, offset)
                .map_err(|error| CommitError::Rejected(error.to_string()))?;
            Ok(EventRecord {
                sequence: sequence.0,
                run_id: run_id.clone(),
                kind: draft.kind.clone(),
                payload: draft.payload.clone(),
            })
        })
        .collect::<Result<Vec<_>, CommitError>>()?;
    snapshot.messages.extend(commit.messages);
    snapshot.state.extend(commit.state);
    snapshot.events.extend(committed_events);
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
    snapshot.thread_version = advance.thread_version;
    snapshot.store_cursor = advance.store_cursor;
    snapshot.next_commit_ordinal = advance.next_commit_ordinal;
    Ok(())
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// @obligation remote_worker.recovery_projection_is_committed_prefix
    /// @kani default-features=false
    #[kani::proof]
    fn recovery_snapshot_admission_requires_exact_claim_identity() {
        // Causes: C1 returned Run is exact/foreign; C2 returned Thread is
        // exact/foreign. Effects: R1 C1+C2 admits; R2 !C1 rejects as WrongRun
        // even when C2 is false; R3 C1+!C2 rejects as WrongThread. The
        // production install door derives both comparisons from these IDs.
        let claimed_run = RunId("run:claimed".into());
        let claimed_thread = ThreadId("thread:claimed".into());
        let returned_run = if kani::any::<bool>() {
            claimed_run.clone()
        } else {
            RunId("run:foreign".into())
        };
        let returned_thread = if kani::any::<bool>() {
            claimed_thread.clone()
        } else {
            ThreadId("thread:foreign".into())
        };
        let admission = classify_recovery_snapshot_admission(
            &returned_run == &claimed_run,
            &returned_thread == &claimed_thread,
        );
        if returned_run != claimed_run {
            assert_eq!(admission, RecoverySnapshotAdmission::WrongRun);
        } else if returned_thread != claimed_thread {
            assert_eq!(admission, RecoverySnapshotAdmission::WrongThread);
        } else {
            assert_eq!(admission, RecoverySnapshotAdmission::Exact);
        }
    }

    /// @obligation remote_worker.acknowledged_projection_is_monotonic
    /// @obligation remote_worker.recovery_projection_is_committed_prefix
    /// @kani default-features=false
    #[kani::proof]
    fn acknowledged_projection_is_exact_monotonic_or_a_noop() {
        let identity_matches: bool = kani::any();
        let snapshot_version: u64 = kani::any();
        let snapshot_ordinal: u64 = kani::any();
        let snapshot_cursor: u64 = kani::any();
        let expected_version: u64 = kani::any();
        let operation_ordinal: u64 = kani::any();
        let receipt_version: u64 = kani::any();
        let receipt_sequence: u64 = kani::any();
        let result = classify_receipt_projection(ReceiptProjectionFacts {
            projection_identity_matches: identity_matches,
            snapshot_thread_version: snapshot_version,
            snapshot_next_ordinal: snapshot_ordinal,
            snapshot_store_cursor: snapshot_cursor,
            operation_expected_version: expected_version,
            operation_ordinal,
            receipt_thread_version: receipt_version,
            receipt_commit_sequence: receipt_sequence,
        });
        let next_version = expected_version.checked_add(1);
        let next_ordinal = operation_ordinal.checked_add(1);
        let is_replay = next_ordinal.is_some_and(|next| snapshot_ordinal == next)
            && snapshot_version == receipt_version
            && snapshot_cursor >= receipt_sequence;
        let is_fresh = snapshot_version == expected_version
            && snapshot_ordinal == operation_ordinal
            && next_version == Some(receipt_version);
        assert_eq!(
            result.is_ok(),
            identity_matches
                && next_version.is_some()
                && next_ordinal.is_some()
                && (is_replay || is_fresh)
        );
        match result {
            Ok(ReceiptProjectionDisposition::Replay) => assert!(is_replay),
            Ok(ReceiptProjectionDisposition::Apply(advance)) => {
                assert!(is_fresh);
                assert_eq!(advance.thread_version, snapshot_version + 1);
                assert_eq!(advance.next_commit_ordinal, snapshot_ordinal + 1);
                assert!(advance.store_cursor >= snapshot_cursor);
                assert!(advance.store_cursor >= receipt_sequence);
            }
            Err(_) => {}
        }
    }
}

impl CommittedThreadView for RecoveryProjection {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        self.snapshot
            .read()
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .as_ref()
                    .map(|prefix| prefix.committed_messages(thread_id))
            })
            .unwrap_or_default()
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.snapshot
            .read()
            .ok()
            .and_then(|snapshot| snapshot.as_ref()?.resume_ticket(run_id))
    }

    fn open_wait_for_thread(&self, thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        self.snapshot
            .read()
            .ok()
            .and_then(|snapshot| snapshot.as_ref()?.open_wait_for_thread(thread_id))
    }

    fn run(&self, run_id: &RunId) -> Option<RunRecord> {
        self.snapshot
            .read()
            .ok()
            .and_then(|snapshot| snapshot.as_ref()?.run(run_id))
    }

    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        self.snapshot
            .read()
            .ok()
            .and_then(|snapshot| snapshot.as_ref()?.latest_run(thread_id))
    }

    fn committed_state(&self, thread_id: &ThreadId) -> Vec<StateCommand> {
        self.snapshot
            .read()
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .as_ref()
                    .map(|prefix| prefix.committed_state(thread_id))
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod install_tests {
    use super::*;

    fn awaiting_snapshot(run: &RunId, thread: &ThreadId) -> RunRecoverySnapshot {
        let mut prefix = snapshot(run.clone(), thread.clone(), 1);
        prefix.runs.push(RunRecord {
            id: run.clone(),
            thread_id: thread.clone(),
            state: RunState::Awaiting,
        });
        prefix.latest_run_id = Some(run.clone());
        prefix.resume_tickets.push(RunResumeTicket {
            run_id: run.clone(),
            ticket: ResumeTicket::new(
                "correlation",
                run.clone(),
                thread.clone(),
                "snapshot",
                "catalog",
                awaken_agent_contract::agent::awaiting::AwaitTarget::Pause(
                    awaken_agent_contract::agent::awaiting::PauseReason::Manual,
                ),
            ),
        });
        prefix
    }

    fn snapshot(run_id: RunId, thread_id: ThreadId, thread_version: u64) -> RunRecoverySnapshot {
        RunRecoverySnapshot {
            thread_id,
            claimed_run_id: run_id,
            runs: Vec::new(),
            latest_run_id: None,
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            commit_timestamps: Default::default(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version,
            store_cursor: 0,
            next_commit_ordinal: 0,
        }
    }

    #[test]
    fn install_rejects_foreign_identity_without_replacing_the_committed_prefix() {
        // Causes: C1 returned Run exact/foreign; C2 returned Thread
        // exact/foreign; C3 an earlier committed prefix is installed.
        // Effects: R1 !C1 rejects as WrongRun, including !C1+!C2; R2
        // C1+!C2 rejects as WrongThread; both preserve C3; R3 C1+C2 installs
        // the new prefix. Run mismatch has stable precedence over Thread.
        let claimed_run = RunId("run:claimed".into());
        let claimed_thread = ThreadId("thread:claimed".into());
        let foreign_run = RunId("run:foreign".into());
        let foreign_thread = ThreadId("thread:foreign".into());
        let projection = RecoveryProjection::new();
        let previous = snapshot(claimed_run.clone(), claimed_thread.clone(), 1);
        projection
            .install(&claimed_run, &claimed_thread, previous.clone())
            .expect("R3 first committed prefix");

        for (run, thread, expected_error) in [
            (foreign_run.clone(), claimed_thread.clone(), "run"),
            (foreign_run, foreign_thread.clone(), "run"),
            (claimed_run.clone(), foreign_thread, "thread"),
        ] {
            let error = projection
                .install(&claimed_run, &claimed_thread, snapshot(run, thread, 2))
                .expect_err("R1/R2 foreign snapshot cannot replace the prefix");
            match (expected_error, error) {
                ("run", RecoveryProjectionError::WrongRun { expected, actual }) => {
                    assert_eq!(
                        (expected.as_str(), actual.as_str()),
                        ("run:claimed", "run:foreign")
                    );
                }
                ("thread", RecoveryProjectionError::WrongThread { expected, actual }) => {
                    assert_eq!(
                        (expected.as_str(), actual.as_str()),
                        ("thread:claimed", "thread:foreign")
                    );
                }
                (_, other) => panic!("wrong identity error: {other}"),
            }
            assert_eq!(
                projection.current(),
                Some(previous.clone()),
                "R1/R2 C3 retained"
            );
        }

        let replacement = snapshot(claimed_run.clone(), claimed_thread.clone(), 2);
        projection
            .install(&claimed_run, &claimed_thread, replacement.clone())
            .expect("R3 exact replacement");
        assert_eq!(projection.current(), Some(replacement), "R3 new prefix");
    }

    #[test]
    fn recovery_projection_rejects_ambiguous_and_foreign_run_records() {
        // C1 the response's claim/Thread metadata is exact; C2 the selected
        // Run is unique and belongs to that Thread; C3 a prior good prefix is
        // installed. R1 C1+C2 => read the exact Awaiting head. R2 C1+!C2
        // (equal duplicate, foreign owner or missing head) => installation
        // refuses before replacement and preserves C3. Corruption must never
        // become an absent Run that could authorize execution again.
        let run = RunId("run".into());
        let thread = ThreadId("thread".into());
        let projection = RecoveryProjection::new();
        let valid = awaiting_snapshot(&run, &thread);
        projection.install(&run, &thread, valid.clone()).unwrap();
        assert_eq!(projection.run_state(&run), Some(RunState::Awaiting), "R1");
        assert!(projection.open_wait_for_thread(&thread).is_some(), "R1");
        for cause in ["duplicate", "foreign-owner", "missing-head"] {
            let mut corrupt = valid.clone();
            match cause {
                "duplicate" => corrupt.runs.push(corrupt.runs[0].clone()),
                "foreign-owner" => corrupt.runs[0].thread_id = ThreadId("foreign".into()),
                _ => corrupt.latest_run_id = Some(RunId("missing".into())),
            }
            assert!(
                projection.install(&run, &thread, corrupt).is_err(),
                "R2 {cause}"
            );
            assert_eq!(projection.current(), Some(valid.clone()), "R2 {cause}");
        }
    }

    #[test]
    fn recovery_projection_rejects_ambiguous_and_foreign_ticket_rows() {
        // T1 exact claim, unique ticket and exact inner owner => admitted.
        // T2 equal duplicate rows, an inner foreign Run or a foreign Thread
        // => reject at installation, retaining the prior good prefix. The
        // existing answerable_ticket selector owns this distinction; the
        // cache must not invent a first-row recovery rule.
        let run = RunId("run".into());
        let thread = ThreadId("thread".into());
        let projection = RecoveryProjection::new();
        let valid = awaiting_snapshot(&run, &thread);
        projection.install(&run, &thread, valid.clone()).unwrap();
        assert!(projection.resume_ticket(&run).is_some(), "T1");
        for cause in ["duplicate", "foreign-run", "foreign-thread"] {
            let mut corrupt = valid.clone();
            match cause {
                "duplicate" => corrupt
                    .resume_tickets
                    .push(corrupt.resume_tickets[0].clone()),
                "foreign-run" => corrupt.resume_tickets[0].ticket.run_id = RunId("foreign".into()),
                _ => corrupt.resume_tickets[0].ticket.thread_id = ThreadId("foreign".into()),
            }
            assert!(
                projection.install(&run, &thread, corrupt).is_err(),
                "T2 {cause}"
            );
            assert_eq!(projection.current(), Some(valid.clone()), "T2 {cause}");
        }
    }
}
