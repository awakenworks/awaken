use awaken_agent_contract::agent::awaiting::{AwaitTarget, PauseReason, ResumeTicket};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, CommitPayloadHash, CommitReceipt,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, RunResumeTicket};
use awaken_run_ingress::{
    ClaimedCommitCommand, ClaimedCommitCoordinator, ClaimedRunCommit, RecoveryProjection, RunClaim,
};

fn ticket(run_id: &RunId, thread_id: &ThreadId) -> ResumeTicket {
    ResumeTicket::new(
        "corr",
        run_id.clone(),
        thread_id.clone(),
        "snapshot",
        "catalog",
        AwaitTarget::Pause(PauseReason::Manual),
    )
}

fn snapshot() -> RunRecoverySnapshot {
    let thread_id = ThreadId("thread".to_string());
    let run_id = RunId("run".to_string());
    let historical_run_id = RunId("historical-run".to_string());
    let resume = ticket(&run_id, &thread_id);
    RunRecoverySnapshot {
        thread_id: thread_id.clone(),
        claimed_run_id: run_id.clone(),
        runs: vec![
            RunRecord {
                id: historical_run_id,
                thread_id: thread_id.clone(),
                state: RunState::Ended(awaken_agent_contract::agent::run::EndCause::NaturalEnd),
            },
            RunRecord {
                id: run_id.clone(),
                thread_id: thread_id.clone(),
                state: RunState::Awaiting,
            },
        ],
        latest_run_id: Some(run_id.clone()),
        messages: vec![Message::text(
            MessageId("before".to_string()),
            Role::User,
            "before",
        )],
        state: Vec::new(),
        resume_tickets: vec![RunResumeTicket {
            run_id,
            ticket: resume,
        }],
        thread_version: 1,
        store_cursor: 7,
        next_commit_ordinal: 1,
    }
}

#[test]
fn install_exposes_the_claimed_committed_prefix() {
    // Cause/effect graph: C1 a snapshot contains a historical Run and a distinct
    // latest claimed Run; C2 the queried Thread matches; C3 a Run/Thread is
    // unknown. Effects: E1 both known Runs are addressable; E2 latest_run selects
    // only the snapshot head; E3 unknown identities are absent. Decision table:
    // R1 C1+C2 -> E1+E2; R2 C3 -> E3. This pins Worker projection semantics to
    // the same committed view contract used by every authoritative backend.
    let projection = RecoveryProjection::new();
    let run_id = RunId("run".to_string());
    projection
        .install(&run_id, snapshot())
        .expect("install snapshot");

    assert_eq!(
        projection
            .committed_messages(&ThreadId("thread".to_string()))
            .len(),
        1
    );
    assert_eq!(projection.run_state(&run_id), Some(RunState::Awaiting));
    assert!(projection.resume_ticket(&run_id).is_some());
    assert!(CommittedThreadView::run(&projection, &run_id).is_some());
    assert!(
        CommittedThreadView::run(&projection, &RunId("historical-run".to_string())).is_some(),
        "R1/E1"
    );
    assert_eq!(
        projection
            .latest_run(&ThreadId("thread".to_string()))
            .map(|run| run.id),
        Some(run_id),
        "R1/E2"
    );
    assert!(
        projection.run(&RunId("missing-run".to_string())).is_none(),
        "R2/E3"
    );
    assert!(
        projection
            .latest_run(&ThreadId("missing-thread".to_string()))
            .is_none(),
        "R2/E3"
    );
}

#[test]
fn acknowledged_commit_advances_projection_once() {
    let projection = RecoveryProjection::new();
    let run_id = RunId("run".to_string());
    projection
        .install(&run_id, snapshot())
        .expect("install snapshot");
    projection
        .apply_committed(
            ThreadCommit {
                thread_id: ThreadId("thread".to_string()),
                run: RunDisposition::running(run_id.clone()),
                messages: vec![Message::text(
                    MessageId("after".to_string()),
                    Role::Assistant,
                    "after",
                )],
                state: Vec::new(),
                events: Vec::new(),
            },
            &CommitRecord { sequence: 8 },
        )
        .expect("apply receipt");

    let current = projection.current().expect("projection");
    assert_eq!(current.thread_version, 2);
    assert_eq!(current.store_cursor, 8);
    assert_eq!(current.next_commit_ordinal, 2);
    assert_eq!(current.messages.len(), 2);
    assert_eq!(projection.run_state(&run_id), Some(RunState::Running));
    assert!(
        projection.resume_ticket(&run_id).is_none(),
        "non-awaiting commit clears the ticket"
    );
}

#[test]
fn projection_counter_overflow_is_rejected_without_partial_update() {
    let projection = RecoveryProjection::new();
    let run_id = RunId("run".to_string());
    let mut at_limit = snapshot();
    at_limit.thread_version = u64::MAX;
    projection
        .install(&run_id, at_limit.clone())
        .expect("install snapshot at boundary");

    let result = projection.apply_committed(
        ThreadCommit {
            thread_id: ThreadId("thread".to_string()),
            run: RunDisposition::running(run_id),
            messages: vec![Message::text(
                MessageId("must-not-appear".to_string()),
                Role::Assistant,
                "must not appear",
            )],
            state: Vec::new(),
            events: Vec::new(),
        },
        &CommitRecord { sequence: 8 },
    );

    assert!(matches!(result, Err(CommitError::Rejected(message)) if message.contains("overflow")));
    assert_eq!(
        projection.current().expect("projection remains installed"),
        at_limit,
        "a rejected boundary transition cannot partially mutate the cache"
    );
}

#[test]
fn wrong_claim_cannot_install_or_advance_projection() {
    let projection = RecoveryProjection::new();
    assert!(
        projection
            .install(&RunId("other".to_string()), snapshot())
            .is_err()
    );
    assert!(projection.current().is_none());
}

#[test]
fn duplicate_receipt_advances_after_response_loss_and_then_becomes_a_noop() {
    let projection = RecoveryProjection::new();
    let run_id = RunId("run".to_string());
    projection
        .install(&run_id, snapshot())
        .expect("install snapshot");
    let operation = CommitOperation {
        operation_id: CommitOperationId::new(run_id.clone(), 1),
        expected_thread_version: 1,
        payload_hash: CommitPayloadHash("sha256:operation".to_string()),
        commit: ThreadCommit {
            thread_id: ThreadId("thread".to_string()),
            run: RunDisposition::running(run_id),
            messages: vec![Message::text(
                MessageId("after-loss".to_string()),
                Role::Assistant,
                "after loss",
            )],
            state: Vec::new(),
            events: Vec::new(),
        },
    };
    let receipt = CommitReceipt {
        operation_id: operation.operation_id.clone(),
        commit_sequence: 8,
        thread_version: 2,
        payload_hash: operation.payload_hash.clone(),
        duplicate: true,
    };

    projection
        .apply_receipt(operation.clone(), &receipt)
        .expect("duplicate receipt advances an unadvanced projection");
    projection
        .apply_receipt(operation, &receipt)
        .expect("same receipt is now an idempotent no-op");
    let current = projection.current().unwrap();
    assert_eq!(current.thread_version, 2);
    assert_eq!(current.next_commit_ordinal, 2);
    assert_eq!(current.messages.len(), 2, "facts projected exactly once");
}

struct ReceiptService {
    command: Mutex<Option<ClaimedCommitCommand>>,
}

#[async_trait::async_trait]
impl ClaimedRunCommit for ReceiptService {
    async fn commit(
        &self,
        _claim: &RunClaim,
        _commit: ThreadCommit,
    ) -> Result<CommitRecord, CommitError> {
        Err(CommitError::Rejected(
            "versioned Worker must use commit_operation".to_string(),
        ))
    }

    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, CommitError> {
        let receipt = CommitReceipt {
            operation_id: command.operation.operation_id.clone(),
            commit_sequence: 8,
            thread_version: command.operation.expected_thread_version + 1,
            payload_hash: command.operation.payload_hash.clone(),
            duplicate: true,
        };
        *self.command.lock().unwrap() = Some(command);
        Ok(receipt)
    }
}

#[tokio::test]
async fn claimed_coordinator_builds_stable_operation_from_recovery_prefix() {
    let projection = Arc::new(RecoveryProjection::new());
    projection
        .install(&RunId("run".to_string()), snapshot())
        .unwrap();
    let service = Arc::new(ReceiptService {
        command: Mutex::new(None),
    });
    let coordinator = ClaimedCommitCoordinator::new(
        service.clone(),
        RunClaim {
            run_id: RunId("run".to_string()),
            owner: "worker".to_string(),
            epoch: 7,
        },
    )
    .with_recovery_projection(projection.clone());
    coordinator
        .commit(ThreadCommit {
            thread_id: ThreadId("thread".to_string()),
            run: RunDisposition::running(RunId("run".to_string())),
            messages: vec![Message::text(
                MessageId("coordinated".to_string()),
                Role::Assistant,
                "coordinated",
            )],
            state: Vec::new(),
            events: Vec::new(),
        })
        .await
        .expect("duplicate receipt after response loss is accepted");

    let command = service.command.lock().unwrap().clone().unwrap();
    assert_eq!(command.operation.operation_id.ordinal, 1);
    assert_eq!(command.operation.expected_thread_version, 1);
    assert_eq!(command.claim.epoch, 7);
    let current = projection.current().unwrap();
    assert_eq!(current.thread_version, 2);
    assert_eq!(current.next_commit_ordinal, 2);
    assert_eq!(current.messages.len(), 2);
}
use std::sync::{Arc, Mutex};
