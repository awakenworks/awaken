//! The committed answerable ticket of the fixture Run `awaiting-run`.
//!
//! Event-batch tests build tool replies to one awaited call. Aggregate
//! admission checks each reply against the committed ticket of its target
//! Thread, so the fake Runtime exposes that call as the Run's one answerable
//! ticket while keeping every other committed fact of its prefix.

use super::*;

/// The committed Awaiting prefix of the fixture Run `awaiting-run`: one
/// answerable permission ticket for `call_id` under `awaiting-correlation`.
fn awaiting_snapshot(
    thread_id: &str,
    call_id: &str,
) -> awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
    let run_id = RunId("awaiting-run".into());
    let thread_id = ThreadId(thread_id.to_string());
    let ticket = awaken_agent_contract::agent::awaiting::ResumeTicket::new(
        "awaiting-correlation",
        run_id.clone(),
        thread_id.clone(),
        "awaiting-snapshot",
        "awaiting-catalog",
        awaken_agent_contract::agent::awaiting::AwaitTarget::RemoteInput {
            reason: awaken_agent_contract::agent::awaiting::RemoteInputReason::ExternalEvent,
            call_id: call_id.to_string(),
        },
    );
    awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
        commit_timestamps: Default::default(),
        thread_id: thread_id.clone(),
        claimed_run_id: run_id.clone(),
        runs: vec![awaken_agent_contract::agent::run::Record {
            id: run_id.clone(),
            thread_id,
            state: RunState::Awaiting,
        }],
        latest_run_id: Some(run_id.clone()),
        messages: Vec::new(),
        message_commit_cursors: Vec::new(),
        state: Vec::new(),
        state_commit_cursors: Vec::new(),
        events: Vec::new(),
        resume_tickets: vec![
            awaken_agent_contract::thread::read::recovery::RunResumeTicket { run_id, ticket },
        ],
        thread_version: 1,
        store_cursor: 1,
        next_commit_ordinal: 1,
    }
}

pub(super) fn tool_reply_input(tool_use_id: &str) -> SessionEventInput {
    tool_reply_input_for_target(tool_use_id, SessionThreadTarget::Primary)
}

thread_local! {
    /// The call the fixture Run `awaiting-run` waits on. A reply built by
    /// `tool_reply_input_for_target` answers exactly this call, so the fake
    /// Runtime exposes it as that Run's committed answerable ticket.
    static AWAITED_CALL: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn tool_reply_input_for_target(
    tool_use_id: &str,
    target: SessionThreadTarget,
) -> SessionEventInput {
    AWAITED_CALL.with(|call| *call.borrow_mut() = Some(tool_use_id.to_string()));
    SessionEventInput::ToolReply(SessionEventToolReply {
        tool_request_event_id: format!("evt_{tool_use_id}"),
        target,
        expected_run_id: RunId("awaiting-run".into()),
        expected_correlation_id: "awaiting-correlation".into(),
        expected_thread_version: Some(1),
        answered_pending_commit_cursor: Some(777),
        runtime_tool_use_id: tool_use_id.into(),
        reply: SessionEventToolReplyKind::ToolResult {
            content: Some(vec![ContentBlock::text("tool result")]),
            is_error: false,
        },
    })
}

/// Merge the fixture's Awaiting Run and ticket into the selected prefix of
/// `awaiting-run`, or supply them when no prefix is committed yet.
pub(super) fn with_awaited_ticket(
    selected: Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    thread_id: &str,
    run_id: &RunId,
) -> Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot> {
    let Some(call_id) = AWAITED_CALL.with(|call| call.borrow().clone()) else {
        return selected;
    };
    if run_id.0 != "awaiting-run" {
        return selected;
    }
    let awaiting = awaiting_snapshot(thread_id, &call_id);
    Some(match selected {
        Some(mut snapshot) if snapshot.resume_tickets.is_empty() => {
            snapshot.runs.retain(|run| run.id != *run_id);
            snapshot.runs.extend(awaiting.runs);
            snapshot.latest_run_id = awaiting.latest_run_id;
            snapshot.resume_tickets = awaiting.resume_tickets;
            snapshot
        }
        Some(snapshot) => snapshot,
        None => awaiting,
    })
}
