// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: One answer resumes exactly the Thread's committed awaiting ticket: the asserted call must
// @formal reason: name its pending tool or delegate and the answer kind must match it; a delegated answer is
// @formal reason: routed through the parent relationship. Classification, not a formal-proof claim.
//! Resuming a Thread's awaiting Run: the result one answer resumes it with,
//! and the durable input that delivers the answer to the Worker that resumes
//! the Run.

use awaken_agent_contract::agent::awaiting::{AwaitReason, AwaitTarget, ResumeTicket};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_dispatch_contract::PendingInput;
use awaken_dispatch_execution::AdmissionError;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_session_application_contract::RunResume;
use awaken_session_types::RunError;
use awaken_tool_contract::tool::ToolOutput;

use crate::{
    SessionCommit, authoritative_child_ticket, check_pending, client_result_for_ticket,
    delegation_registry_from_snapshot,
};

/// Whether `resume` answers with a client result.
#[must_use]
pub fn resume_wants_client(resume: &RunResume) -> bool {
    matches!(resume, RunResume::ClientResult { .. })
}

/// The result `resume` resumes `run_id`'s committed `ticket` with.
///
/// Decision table (causes → effect):
/// | Rule | ticket | asserted call / answer | effect |
/// |---|---|---|---|
/// | R1 | remote input | another call | refused |
/// | R2 | remote input | the call, a permission | refused: remote input needs a client result |
/// | R3 | remote input | the call, a client result | the input text |
/// | R4 | delegation with a committed child ticket | the child's pending tool and kind | the child's answer |
/// | R5 | delegation without a child ticket | the delegate call, a client result | the input text |
/// | R6 | delegation without a child ticket | another call or a permission | refused |
/// | R7 | tool call | the pending tool and its kind | the permission, or the tool output |
/// | R8 | tool call | another tool or the other kind | refused |
pub async fn resume_result(
    commit: &SessionCommit,
    awaiting: &RunRecoverySnapshot,
    run_id: &RunId,
    ticket: &ResumeTicket,
    tool_use_id: &str,
    resume: RunResume,
) -> Result<ResumeResult, RunError> {
    if matches!(ticket.target(), AwaitTarget::RemoteInput { .. }) {
        if ticket.call_id() != Some(tool_use_id) {
            return Err(RunError::bad_request(format!(
                "tool_use_id {tool_use_id:?} does not match the pending remote input"
            )));
        }
        let RunResume::ClientResult { content, .. } = resume else {
            return Err(RunError::bad_request(
                "awaiting remote input requires a client result",
            ));
        };
        return Ok(client_result_for_ticket(
            ticket,
            tool_use_id,
            content,
            false,
        ));
    }
    // An awaiting delegation resumes through the kernel resolver with the
    // user's typed answer; the kernel routes it through the parent
    // relationship to the child's own Run service. The user never resumes the
    // child directly.
    if ticket.reason() == AwaitReason::Delegation {
        let registry = delegation_registry_from_snapshot(awaiting, run_id)?;
        let child = authoritative_child_ticket(commit, registry.as_ref(), ticket.call_id()).await?;
        return match child {
            Some(child_ticket) => {
                check_pending(&child_ticket, tool_use_id, resume_wants_client(&resume))?;
                Ok(match resume {
                    RunResume::Permission(decision) => ResumeResult::Permission(decision),
                    RunResume::ClientResult { content, is_error } => {
                        client_result_for_ticket(&child_ticket, tool_use_id, content, is_error)
                    }
                })
            }
            None => {
                // Remote adapters may expose an opaque follow-up without a
                // locally committed child ticket. Keep that adapter boundary
                // as typed user input while still validating the parent call
                // identity.
                if ticket.call_id() != Some(tool_use_id) {
                    return Err(RunError::bad_request(format!(
                        "tool_use_id {tool_use_id:?} does not match the pending delegate"
                    )));
                }
                let RunResume::ClientResult { content, .. } = resume else {
                    return Err(RunError::bad_request(
                        "awaiting remote Agent input requires a client result",
                    ));
                };
                Ok(ResumeResult::Input(
                    awaken_agent_contract::agent::content::extract_text(&content),
                ))
            }
        };
    }
    check_pending(ticket, tool_use_id, resume_wants_client(&resume))?;
    let (call_id, _) = ticket
        .tool_call()
        .ok_or_else(|| RunError::internal("awaiting run has no pending tool call"))?;
    Ok(match resume {
        RunResume::Permission(decision) => ResumeResult::Permission(decision),
        RunResume::ClientResult { content, is_error } => ResumeResult::ToolResult(if is_error {
            ToolOutput::error_blocks(call_id, content)
        } else {
            ToolOutput::ok_blocks(call_id, content)
        }),
    })
}

/// A resume ticket is a one-answer idempotency boundary. Encoding the exact
/// `(run, correlation)` pair with a run-length prefix is collision-free for
/// arbitrary string contents; replaying another payload for that same ticket is
/// rejected by the Inbox's existing idempotency-conflict rule.
#[must_use]
pub fn durable_resume_input(command: ResumeCommand) -> PendingInput {
    let message_id = format!(
        "foreground-resume:{}:{}{}",
        command.run_id.0.len(),
        command.run_id.0,
        command.correlation_id
    );
    PendingInput {
        message_id,
        run_id: command.run_id,
        thread_id: command.thread_id,
        correlation_id: command.correlation_id,
        available_at_ms: None,
        context_messages: Vec::new(),
        result: command.result,
    }
}

/// The resume input that delivers an approval decision to the awaiting Run.
pub fn decision_input(
    thread: ThreadId,
    awaiting: Option<(RunId, ResumeTicket)>,
    allow: bool,
) -> Result<PendingInput, AdmissionError> {
    let (run_id, ticket) = awaiting
        .ok_or_else(|| AdmissionError::rejected("no awaiting run on this thread to deliver to"))?;
    Ok(PendingInput {
        message_id: awaken_agent_contract::fresh_process_id("xthread"),
        run_id,
        thread_id: thread,
        correlation_id: ticket.correlation_id,
        available_at_ms: None,
        context_messages: Vec::new(),
        result: if allow {
            ResumeResult::allow()
        } else {
            ResumeResult::deny(None)
        },
    })
}

/// The resume input that continues a manually paused Run with text.
pub fn manual_resume_input(
    thread: ThreadId,
    awaiting: Option<(RunId, ResumeTicket)>,
    text: String,
) -> Result<PendingInput, AdmissionError> {
    let (run_id, ticket) = awaiting
        .ok_or_else(|| AdmissionError::rejected("no awaiting run on this thread to resume"))?;
    if ticket.reason() != AwaitReason::ManualPause {
        return Err(AdmissionError::rejected(
            "durable text resume requires a manual-pause ticket",
        ));
    }
    Ok(PendingInput {
        message_id: awaken_agent_contract::fresh_process_id("manual-resume"),
        run_id,
        thread_id: thread,
        correlation_id: ticket.correlation_id,
        available_at_ms: None,
        context_messages: Vec::new(),
        result: ResumeResult::Input(text),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(pause: awaken_agent_contract::agent::awaiting::PauseReason) -> ResumeTicket {
        ResumeTicket::new(
            "correlation",
            RunId("run".into()),
            ThreadId("thread".into()),
            "snapshot",
            "fingerprint",
            AwaitTarget::Pause(pause),
        )
    }

    #[test]
    fn resume_inputs_answer_only_their_awaiting_ticket() {
        let thread = ThreadId("thread".into());
        // Nothing awaits: both refuse.
        assert!(decision_input(thread.clone(), None, true).is_err());
        assert!(manual_resume_input(thread.clone(), None, "go".into()).is_err());
        // A decision answers any awaiting ticket.
        let paused = ticket(awaken_agent_contract::agent::awaiting::PauseReason::Manual);
        let decision = decision_input(
            thread.clone(),
            Some((RunId("run".into()), paused.clone())),
            true,
        )
        .expect("decision");
        assert_eq!(decision.correlation_id, paused.correlation_id);
        assert!(matches!(decision.result, ResumeResult::Permission(_)));
        // A text resume answers only a manual pause.
        let input = manual_resume_input(
            thread.clone(),
            Some((RunId("run".into()), paused)),
            "go".into(),
        )
        .expect("manual");
        assert!(matches!(input.result, ResumeResult::Input(ref text) if text == "go"));
        let budget = ticket(awaken_agent_contract::agent::awaiting::PauseReason::BudgetReached);
        assert!(
            manual_resume_input(thread, Some((RunId("run".into()), budget)), "go".into()).is_err()
        );
    }
}
