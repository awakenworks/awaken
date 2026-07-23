use awaken_agent_contract::agent::awaiting::{AwaitReason, ResumeTicket};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, RunResumeTicket};
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_run_ingress::RecoveryProjection;

fn ticket(run_id: &RunId, thread_id: &ThreadId) -> ResumeTicket {
    ResumeTicket {
        correlation_id: "corr".to_string(),
        run_id: run_id.clone(),
        thread_id: thread_id.clone(),
        snapshot_id: "snapshot".to_string(),
        catalog_fingerprint: "catalog".to_string(),
        delegation_origin: None,
        reason: AwaitReason::UserInput,
        call_id: None,
        pending_tool: None,
        deadline_ms: None,
    }
}

fn snapshot() -> RunRecoverySnapshot {
    let thread_id = ThreadId("thread".to_string());
    let run_id = RunId("run".to_string());
    let resume = ticket(&run_id, &thread_id);
    RunRecoverySnapshot {
        thread_id: thread_id.clone(),
        claimed_run_id: run_id.clone(),
        runs: vec![RunRecord {
            id: run_id.clone(),
            thread_id: thread_id.clone(),
            state: RunState::Awaiting,
        }],
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
    assert!(RunStore::get(&projection, &run_id).is_some());
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
fn wrong_claim_cannot_install_or_advance_projection() {
    let projection = RecoveryProjection::new();
    assert!(
        projection
            .install(&RunId("other".to_string()), snapshot())
            .is_err()
    );
    assert!(projection.current().is_none());
}
