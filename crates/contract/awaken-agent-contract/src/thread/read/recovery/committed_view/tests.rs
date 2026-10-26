use super::*;
use crate::agent::awaiting::{AwaitTarget, PauseReason};
use crate::agent::message::{Id as MessageId, Role};
use crate::agent::run::{EndCause, RunState};
use crate::agent::state::{MergePolicy, Scope};
use crate::thread::read::recovery::RunResumeTicket;
use crate::thread::read::transcript::TranscriptView;

fn prefix() -> RunRecoverySnapshot {
    let thread = ThreadId("thread".into());
    let head = RunId("head".into());
    RunRecoverySnapshot {
        thread_id: thread.clone(),
        claimed_run_id: head.clone(),
        runs: vec![
            RunRecord {
                id: RunId("earlier".into()),
                thread_id: thread.clone(),
                state: RunState::Ended(EndCause::NaturalEnd),
            },
            RunRecord {
                id: head.clone(),
                thread_id: thread.clone(),
                state: RunState::Awaiting,
            },
        ],
        latest_run_id: Some(head.clone()),
        messages: vec![Message::text(
            MessageId("message".into()),
            Role::User,
            "remember",
        )],
        message_commit_cursors: vec![1],
        commit_timestamps: Default::default(),
        state: vec![StateCommand::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            "key",
            true.into(),
        )],
        state_commit_cursors: vec![1],
        events: Vec::new(),
        resume_tickets: vec![RunResumeTicket {
            run_id: head.clone(),
            ticket: ResumeTicket::new(
                "correlation",
                head,
                thread,
                "snapshot",
                "catalog",
                AwaitTarget::Pause(PauseReason::Manual),
            ),
        }],
        thread_version: 1,
        store_cursor: 1,
        next_commit_ordinal: 1,
    }
}

#[test]
fn immutable_recovery_view_keeps_exact_thread_prefix_and_distinct_run_and_head_queries() {
    // P1 supplied prefix + exact Thread => its messages/state and exact raw
    // transcript; P2 foreign Thread => empty data and no head/wait; P3 any
    // retained Run can be addressed, but only latest_run_id selects the head.
    // The view is usable directly behind the existing trait, with no cache,
    // provider or commit capability manufactured from the supplied data.
    let snapshot = prefix();
    let view: std::sync::Arc<dyn CommittedThreadView> = std::sync::Arc::new(snapshot.clone());
    let thread = &snapshot.thread_id;
    assert_eq!(view.committed_messages(thread), snapshot.messages, "P1");
    assert_eq!(view.committed_state(thread), snapshot.state, "P1");
    let transcript = view.transcript_snapshot(thread, TranscriptView::RawCommitted);
    assert_eq!(transcript.messages(), snapshot.messages, "P1");
    assert_eq!(transcript.reference().end_seq, 1, "P1");
    assert_eq!(
        view.run(&RunId("earlier".into())),
        Some(snapshot.runs[0].clone()),
        "P3"
    );
    assert_eq!(
        view.latest_run(thread),
        Some(snapshot.runs[1].clone()),
        "P3"
    );
    assert_eq!(
        view.run_state(&RunId("head".into())),
        Some(RunState::Awaiting),
        "P3"
    );
    let foreign = ThreadId("foreign".into());
    assert!(view.committed_messages(&foreign).is_empty(), "P2");
    assert!(view.committed_state(&foreign).is_empty(), "P2");
    assert!(view.latest_run(&foreign).is_none(), "P2");
    assert!(view.open_wait_for_thread(&foreign).is_none(), "P2");
    assert!(view.run(&RunId("absent".into())).is_none(), "P3");
}

#[test]
fn immutable_recovery_view_uses_canonical_run_and_wait_selectors() {
    // R1 unique owned head + Awaiting + exact ticket => one open wait.
    // R2 duplicate/foreign head => no Run/head/wait projection; R3 missing
    // or omitted head coordinate => no head/wait, while another exact Run
    // remains addressable. R4 Running/Ended head => no open wait despite a
    // retained ticket. The mutable cache separately refuses malformed input
    // before installation, so these Option reads cannot mask an install error.
    let run = RunId("head".into());
    for cause in [
        "exact",
        "duplicate",
        "foreign",
        "missing-head",
        "omitted-head",
    ] {
        let mut snapshot = prefix();
        match cause {
            "duplicate" => snapshot.runs.push(snapshot.runs[1].clone()),
            "foreign" => snapshot.runs[1].thread_id = ThreadId("foreign".into()),
            "missing-head" => snapshot.latest_run_id = Some(RunId("missing".into())),
            "omitted-head" => snapshot.latest_run_id = None,
            _ => {}
        }
        assert_eq!(
            snapshot.run(&run).is_some(),
            !matches!(cause, "duplicate" | "foreign"),
            "R1-R3 {cause}"
        );
        assert_eq!(
            snapshot.latest_run(&snapshot.thread_id).is_some(),
            cause == "exact",
            "R1-R3 {cause}"
        );
        assert_eq!(
            snapshot.open_wait_for_thread(&snapshot.thread_id).is_some(),
            cause == "exact",
            "R1-R3 {cause}"
        );
    }
    for state in [
        RunState::Running,
        RunState::Awaiting,
        RunState::Ended(EndCause::NaturalEnd),
    ] {
        let mut snapshot = prefix();
        snapshot.runs[1].state = state.clone();
        assert_eq!(
            snapshot.open_wait_for_thread(&snapshot.thread_id).is_some(),
            state == RunState::Awaiting,
            "R1/R4"
        );
    }
}

#[test]
fn immutable_recovery_view_uses_one_unique_owned_ticket_row() {
    // T1 unique exact row => ticket and wait; T2 absent, equal duplicate,
    // foreign inner Run or foreign inner Thread => neither. A row owned by
    // another Run does not change T1/T2 selection, and Run truth stays intact.
    let run = RunId("head".into());
    for unrelated in [false, true] {
        for cause in [
            "exact",
            "absent",
            "duplicate",
            "foreign-run",
            "foreign-thread",
        ] {
            let mut snapshot = prefix();
            match cause {
                "absent" => snapshot.resume_tickets.clear(),
                "duplicate" => snapshot
                    .resume_tickets
                    .push(snapshot.resume_tickets[0].clone()),
                "foreign-run" => snapshot.resume_tickets[0].ticket.run_id = RunId("foreign".into()),
                "foreign-thread" => {
                    snapshot.resume_tickets[0].ticket.thread_id = ThreadId("foreign".into())
                }
                _ => {}
            }
            if unrelated {
                let mut other = prefix().resume_tickets.remove(0);
                other.run_id = RunId("other".into());
                other.ticket.run_id = other.run_id.clone();
                snapshot.resume_tickets.push(other);
            }
            assert_eq!(
                snapshot.resume_ticket(&run).is_some(),
                cause == "exact",
                "T1/T2 {cause}"
            );
            assert_eq!(
                snapshot.open_wait_for_thread(&snapshot.thread_id).is_some(),
                cause == "exact",
                "T1/T2 {cause}"
            );
            assert!(snapshot.run(&run).is_some(), "T1/T2 retains Run");
        }
    }
}
