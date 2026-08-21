//! Resume command and the shared `ResumeValidator`.
//!
//! A awaiting run resumes only through a `ResumeCommand` that the runtime validates
//! against the committed `ResumeTicket`: correlation, run/thread, executable
//! snapshot, catalog fingerprint, and deadline must all match, or the resume
//! fails closed (G5/G28). The clock is supplied by the caller so the runtime
//! core stays deterministic and replayable.

pub use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::awaiting::{AwaitTarget, ResumeTicket, ToolAwaitReason};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::resolved::CatalogFingerprint;
use crate::snapshot::ExecutableAgentSnapshotId;
use crate::tool::ToolOutput;

/// What a resume delivers back into the awaiting run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ResumeResult {
    /// A tool result for the call the run was awaiting on.
    ToolResult(ToolOutput),
    /// A permission decision for the pending operation.
    Permission(PermissionDecision),
    /// Free-form input delivered to the run (e.g. a user answer).
    Input(String),
}

impl ResumeResult {
    /// Approve the pending tool call.
    pub fn allow() -> Self {
        Self::Permission(PermissionDecision::Allow { note: None })
    }

    /// Reject the pending tool call, optionally with a reason for the model.
    pub fn deny(reason: Option<String>) -> Self {
        Self::Permission(PermissionDecision::Deny { reason })
    }
}

/// Neutral resume input. Pure data so it can cross an ingress boundary, be
/// logged, and be validated before any execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeCommand {
    pub correlation_id: String,
    pub run_id: RunId,
    pub thread_id: ThreadId,
    /// The awaiting run's executable snapshot identity — the same newtype the
    /// snapshot and resolution paths carry, so the resume's identity check is
    /// type-safe (a fingerprint can never be passed where a snapshot id is meant).
    pub snapshot_id: ExecutableAgentSnapshotId,
    pub catalog_fingerprint: CatalogFingerprint,
    pub result: ResumeResult,
    /// Caller-supplied clock (epoch millis) used to enforce the ticket deadline.
    pub now_ms: u64,
}

impl ResumeCommand {
    /// Build the command from the committed ticket: every identity field comes
    /// from the ticket, so the caller supplies only the answer (`result`) and the
    /// clock (`now_ms`). Both the in-process driver and the durable worker use
    /// this — the ticket is the single source of the resume's identity.
    pub fn from_ticket(ticket: &ResumeTicket, result: ResumeResult, now_ms: u64) -> Self {
        Self {
            correlation_id: ticket.correlation_id.clone(),
            run_id: ticket.run_id.clone(),
            thread_id: ticket.thread_id.clone(),
            // The ticket stores these as plain strings (its crate sits below the
            // runtime-contract newtypes); wrap them at this one boundary.
            snapshot_id: ExecutableAgentSnapshotId(ticket.snapshot_id.clone()),
            catalog_fingerprint: CatalogFingerprint(ticket.catalog_fingerprint.clone()),
            result,
            now_ms,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResumeError {
    #[error("run is not awaiting (no active ticket)")]
    NotWaiting,
    #[error("resume correlation does not match the ticket")]
    CorrelationMismatch,
    #[error("resume run id does not match the ticket")]
    RunMismatch,
    #[error("resume thread id does not match the ticket")]
    ThreadMismatch,
    #[error("resume snapshot id does not match the ticket")]
    SnapshotMismatch,
    #[error("resume catalog fingerprint does not match the ticket")]
    FingerprintMismatch,
    #[error("resume is past the ticket deadline")]
    Expired,
    #[error("resume result kind is incompatible with the committed wait reason")]
    ResultKindMismatch,
    #[error("tool result call id does not match the committed ticket")]
    ToolCallMismatch,
}

/// Validate a resume against the committed ticket. Every identity must match and
/// the deadline (if any) must not have passed, or the resume fails closed.
pub fn validate_resume(ticket: &ResumeTicket, command: &ResumeCommand) -> Result<(), ResumeError> {
    if ticket.correlation_id != command.correlation_id {
        return Err(ResumeError::CorrelationMismatch);
    }
    if ticket.run_id != command.run_id {
        return Err(ResumeError::RunMismatch);
    }
    if ticket.thread_id != command.thread_id {
        return Err(ResumeError::ThreadMismatch);
    }
    // The ticket carries plain strings; compare against the newtype's inner value
    // at this layer boundary.
    if ticket.snapshot_id != command.snapshot_id.0 {
        return Err(ResumeError::SnapshotMismatch);
    }
    if ticket.catalog_fingerprint != command.catalog_fingerprint.0 {
        return Err(ResumeError::FingerprintMismatch);
    }
    if let Some(deadline) = ticket.deadline_ms
        && command.now_ms > deadline
    {
        return Err(ResumeError::Expired);
    }
    let result_kind_matches = matches!(
        (ticket.target(), &command.result),
        (
            AwaitTarget::ToolCall {
                reason: ToolAwaitReason::Permission | ToolAwaitReason::ScheduledAction,
                ..
            },
            ResumeResult::Permission(_)
        ) | (
            AwaitTarget::ToolCall {
                reason: ToolAwaitReason::ClientExecution,
                ..
            },
            ResumeResult::ToolResult(_)
        ) | (
            AwaitTarget::ToolCall {
                reason: ToolAwaitReason::Delegation,
                ..
            },
            ResumeResult::Input(_) | ResumeResult::ToolResult(_)
        ) | (
            AwaitTarget::RemoteInput { .. } | AwaitTarget::Pause(_),
            ResumeResult::Input(_)
        )
    );
    if !result_kind_matches {
        return Err(ResumeError::ResultKindMismatch);
    }
    if let (AwaitTarget::ToolCall { call_id, .. }, ResumeResult::ToolResult(output)) =
        (ticket.target(), &command.result)
        && call_id != &output.call_id
    {
        return Err(ResumeError::ToolCallMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::awaiting::{
        PauseReason, PendingTool, RemoteInputReason, ToolAwaitReason,
    };

    fn ticket_for(target: AwaitTarget) -> ResumeTicket {
        ResumeTicket::new(
            "c1",
            RunId("run-1".to_string()),
            ThreadId("thread-1".to_string()),
            "snap-1",
            "fp-1",
            target,
        )
        .with_deadline(Some(100))
    }

    fn tool_target(reason: ToolAwaitReason) -> AwaitTarget {
        AwaitTarget::ToolCall {
            reason,
            call_id: "call-1".into(),
            tool: PendingTool {
                tool_id: "tool-1".into(),
                arguments: serde_json::json!({}),
            },
        }
    }

    fn ticket() -> ResumeTicket {
        ticket_for(tool_target(ToolAwaitReason::Permission))
    }

    fn command() -> ResumeCommand {
        ResumeCommand {
            correlation_id: "c1".to_string(),
            run_id: RunId("run-1".to_string()),
            thread_id: ThreadId("thread-1".to_string()),
            snapshot_id: ExecutableAgentSnapshotId("snap-1".to_string()),
            catalog_fingerprint: CatalogFingerprint("fp-1".to_string()),
            result: ResumeResult::allow(),
            now_ms: 50,
        }
    }

    #[test]
    fn matching_resume_is_accepted() {
        assert!(validate_resume(&ticket(), &command()).is_ok());
    }

    #[test]
    fn permission_decision_wire_shape_preserves_only_legal_states() {
        // Partition the closed decision set across allow/deny and absent/present
        // explanations. Serde must round-trip every partition without recreating
        // the former bool/Option product.
        let cases = [
            ResumeResult::allow(),
            ResumeResult::Permission(PermissionDecision::Allow {
                note: Some("reviewed".into()),
            }),
            ResumeResult::deny(None),
            ResumeResult::deny(Some("operator policy".into())),
        ];
        for expected in cases {
            let encoded = serde_json::to_value(&expected).expect("serialize decision");
            let decoded: ResumeResult =
                serde_json::from_value(encoded).expect("deserialize decision");
            assert_eq!(decoded, expected);
        }
    }

    #[test]
    fn result_kind_and_tool_identity_follow_the_closed_target() {
        let input = ResumeCommand {
            result: ResumeResult::Input("not an approval".into()),
            ..command()
        };
        assert_eq!(
            validate_resume(&ticket(), &input),
            Err(ResumeError::ResultKindMismatch)
        );

        let client = ticket_for(tool_target(ToolAwaitReason::ClientExecution));
        let wrong_call = ResumeCommand::from_ticket(
            &client,
            ResumeResult::ToolResult(ToolOutput::ok("other-call", "done")),
            50,
        );
        assert_eq!(
            validate_resume(&client, &wrong_call),
            Err(ResumeError::ToolCallMismatch)
        );

        let legal = [
            (
                ticket_for(tool_target(ToolAwaitReason::ScheduledAction)),
                ResumeResult::allow(),
            ),
            (
                client,
                ResumeResult::ToolResult(ToolOutput::ok("call-1", "done")),
            ),
            (
                ticket_for(tool_target(ToolAwaitReason::Delegation)),
                ResumeResult::Input("answer".into()),
            ),
            (
                ticket_for(AwaitTarget::RemoteInput {
                    reason: RemoteInputReason::UserInput,
                    call_id: "remote".into(),
                }),
                ResumeResult::Input("answer".into()),
            ),
            (
                ticket_for(AwaitTarget::Pause(PauseReason::Manual)),
                ResumeResult::Input("continue".into()),
            ),
        ];
        for (ticket, result) in legal {
            let command = ResumeCommand::from_ticket(&ticket, result, 50);
            assert_eq!(validate_resume(&ticket, &command), Ok(()));
        }
    }

    #[test]
    fn from_ticket_copies_identity_and_validates() {
        // Built from the ticket + an answer, it validates against that same ticket.
        let cmd = ResumeCommand::from_ticket(&ticket(), ResumeResult::allow(), 50);
        assert_eq!(cmd.correlation_id, "c1");
        assert_eq!(cmd.snapshot_id.0, "snap-1");
        assert_eq!(cmd.result, ResumeResult::allow());
        assert!(validate_resume(&ticket(), &cmd).is_ok());
    }

    #[test]
    fn each_mismatch_fails_closed() {
        let cases = [
            (
                ResumeCommand {
                    correlation_id: "x".into(),
                    ..command()
                },
                ResumeError::CorrelationMismatch,
            ),
            (
                ResumeCommand {
                    run_id: RunId("x".into()),
                    ..command()
                },
                ResumeError::RunMismatch,
            ),
            (
                ResumeCommand {
                    thread_id: ThreadId("x".into()),
                    ..command()
                },
                ResumeError::ThreadMismatch,
            ),
            (
                ResumeCommand {
                    snapshot_id: ExecutableAgentSnapshotId("x".into()),
                    ..command()
                },
                ResumeError::SnapshotMismatch,
            ),
            (
                ResumeCommand {
                    catalog_fingerprint: CatalogFingerprint("x".into()),
                    ..command()
                },
                ResumeError::FingerprintMismatch,
            ),
            (
                ResumeCommand {
                    now_ms: 999,
                    ..command()
                },
                ResumeError::Expired,
            ),
        ];
        for (cmd, expected) in cases {
            assert_eq!(validate_resume(&ticket(), &cmd), Err(expected));
        }
    }

    #[test]
    fn no_deadline_never_expires() {
        let mut t = ticket();
        t.deadline_ms = None;
        let cmd = ResumeCommand {
            now_ms: u64::MAX,
            ..command()
        };
        assert!(validate_resume(&t, &cmd).is_ok());
    }
}
