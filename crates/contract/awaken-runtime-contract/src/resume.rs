//! Resume command and the shared `ResumeValidator`.
//!
//! A awaiting run resumes only through a `ResumeCommand` that the runtime validates
//! against the committed `ResumeTicket`: correlation, run/thread, executable
//! snapshot, catalog fingerprint, and deadline must all match, or the resume
//! fails closed (G5/G28). The clock is supplied by the caller so the runtime
//! core stays deterministic and replayable.

pub use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::awaiting::{
    AwaitTarget, PauseReason, ResumeTicket, ToolAwaitReason,
};
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
    /// Continue the same Run without injecting a user or tool message. This is
    /// accepted only for a committed `BudgetReached` ticket after the Session
    /// authority has made model admission available again.
    Continue,
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
    /// Stable context accepted atomically with this resume. Session tool
    /// replies use Role::System Messages only; the runtime validates that
    /// restriction and commits them immediately before the resumed result.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_messages: Vec<awaken_agent_contract::agent::message::Message>,
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
            context_messages: Vec::new(),
            now_ms,
        }
    }

    /// Attach already-stable context frozen by durable ingress.
    #[must_use]
    pub fn with_context_messages(
        mut self,
        context_messages: Vec<awaken_agent_contract::agent::message::Message>,
    ) -> Self {
        self.context_messages = context_messages;
        self
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
    #[error("resume context contains a non-System Message")]
    InvalidContextRole,
    #[error("resume context contains an incomplete or duplicate Message")]
    InvalidContextMessage,
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
            ResumeResult::Input(_) | ResumeResult::ToolResult(_) | ResumeResult::Permission(_)
        ) | (
            AwaitTarget::RemoteInput { .. }
                | AwaitTarget::Pause(PauseReason::Manual | PauseReason::RateLimit),
            ResumeResult::Input(_)
        ) | (
            AwaitTarget::Pause(PauseReason::BudgetReached),
            ResumeResult::Continue
        )
    );
    if !result_kind_matches {
        return Err(ResumeError::ResultKindMismatch);
    }
    if let (
        AwaitTarget::ToolCall {
            reason, call_id, ..
        },
        ResumeResult::ToolResult(output),
    ) = (ticket.target(), &command.result)
        && !matches!(reason, ToolAwaitReason::Delegation)
        && call_id != &output.call_id
    {
        return Err(ResumeError::ToolCallMismatch);
    }
    let mut context_ids = std::collections::HashSet::new();
    for message in &command.context_messages {
        if message.role != awaken_agent_contract::agent::message::Role::System {
            return Err(ResumeError::InvalidContextRole);
        }
        if message.id.0.trim().is_empty()
            || message.content.is_empty()
            || !context_ids.insert(message.id.clone())
        {
            return Err(ResumeError::InvalidContextMessage);
        }
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
            context_messages: Vec::new(),
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
        // Cause/effect graph: C1 the committed target is an ordinary Permission
        // or ClientExecution ticket versus a parent Delegation ticket; C2 the
        // Delegation result is Input, Permission, child ToolResult, or Continue;
        // C3 a ToolResult id names this ticket or the delegated child's ticket.
        // Effects: E1 ordinary targets retain their exact result/call-id checks;
        // E2 a Delegation carries its typed child answer unchanged for validation
        // against the child's committed ticket; E3 unsupported pairings reject.
        // Constraints/invariants: the parent ticket authenticates the delegation
        // relationship, never reclassifies the child's answer; the child resume
        // validator remains the sole owner of that answer's kind and call id.
        // Decision rules: D1 Delegation+Input=>E2; D2 Delegation+Permission=>E2;
        // D3 Delegation+child ToolResult=>E2; D4 Delegation+Continue=>E3; ordinary
        // Permission/ClientExecution cases remain E1.
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
                ticket_for(tool_target(ToolAwaitReason::Delegation)),
                ResumeResult::allow(),
            ),
            (
                ticket_for(tool_target(ToolAwaitReason::Delegation)),
                ResumeResult::ToolResult(ToolOutput::ok("child-call", "done")),
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
        let delegation = ticket_for(tool_target(ToolAwaitReason::Delegation));
        assert_eq!(
            validate_resume(
                &delegation,
                &ResumeCommand::from_ticket(&delegation, ResumeResult::Continue, 50)
            ),
            Err(ResumeError::ResultKindMismatch)
        );
    }

    #[test]
    fn message_free_continue_is_owned_only_by_budget_reached_waits() {
        // Cause/effect graph: C1 the committed reason is BudgetReached or an
        // externally answerable wait; C2 the delivery is Continue or ordinary
        // input. Effects: E1 only BudgetReached+Continue resumes without adding
        // transcript content; E2 every crossed pairing fails closed.
        //
        // | Rule | Ticket | Result | Effect |
        // |---|---|---|---|
        // | B1 | BudgetReached | Continue | E1 accept |
        // | B2 | BudgetReached | Input | E2 reject |
        // | B3 | ToolPermission | Continue | E2 reject |
        // Constraints/invariants: message-free Continue has exactly one owner;
        // every externally answerable wait still requires correlated content.
        let budget_ticket = ticket_for(AwaitTarget::Pause(PauseReason::BudgetReached));
        let continue_command =
            ResumeCommand::from_ticket(&budget_ticket, ResumeResult::Continue, 50);
        assert!(
            validate_resume(&budget_ticket, &continue_command).is_ok(),
            "B1/E1"
        );
        assert_eq!(
            validate_resume(
                &budget_ticket,
                &ResumeCommand::from_ticket(
                    &budget_ticket,
                    ResumeResult::Input("wrong".into()),
                    50,
                ),
            ),
            Err(ResumeError::ResultKindMismatch),
            "B2/E2"
        );
        assert_eq!(
            validate_resume(
                &ticket(),
                &ResumeCommand::from_ticket(&ticket(), ResumeResult::Continue, 50),
            ),
            Err(ResumeError::ResultKindMismatch),
            "B3/E2"
        );
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
        // Test design — Causes: each immutable ResumeTicket identity axis or its
        // deadline is changed independently. Effects: validation returns the
        // corresponding typed mismatch. Constraints/invariants: no nearby ticket
        // can authorize another Run/Thread/snapshot/catalog/correlation. Decision
        // rule M1-M6: mutate one axis=>its exact error and no accepted resume.
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
                    context_messages: Vec::new(),
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
        // Test design — Causes: a ticket deliberately has no deadline and now is
        // u64::MAX. Effects: validation still accepts the otherwise exact command.
        // Constraints/invariants: absence means unbounded, not deadline zero.
        // Decision rule D1: None deadline+exact identity=>not Expired.
        let mut t = ticket();
        t.deadline_ms = None;
        let cmd = ResumeCommand {
            context_messages: Vec::new(),
            now_ms: u64::MAX,
            ..command()
        };
        assert!(validate_resume(&t, &cmd).is_ok());
    }

    #[test]
    fn resume_context_is_backward_compatible_and_system_only() {
        use awaken_agent_contract::agent::content::ContentBlock;
        use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};

        // Cause/effect graph: C1 context is omitted/System/non-System; C2 ids
        // and content are complete/duplicate. Effects: E1 legacy omission
        // decodes empty; E2 one stable System is accepted; E3 any other role or
        // incomplete/duplicate Message fails before Runtime execution.
        //
        // | Rule | Context | Complete/unique | Effect |
        // |---|---|---|---|
        // | C1 | omitted | - | E1 empty/accept |
        // | C2 | System | yes | E2 accept |
        // | C3 | non-System | yes | E3 role error |
        // | C4 | System | no | E3 message error |
        // Constraints/invariants: compatibility defaults only omission; accepted
        // context is complete, unique, System-role data with stable ids.
        let mut legacy = serde_json::to_value(command()).expect("serialize command");
        legacy
            .as_object_mut()
            .expect("object")
            .remove("context_messages");
        let legacy: ResumeCommand = serde_json::from_value(legacy).expect("C1");
        assert!(legacy.context_messages.is_empty(), "C1/E1");
        assert!(validate_resume(&ticket(), &legacy).is_ok(), "C1/E1");

        let system = Message::new(
            MessageId("system-1".into()),
            Role::System,
            vec![ContentBlock::text("context")],
        );
        let valid = command().with_context_messages(vec![system.clone()]);
        assert!(validate_resume(&ticket(), &valid).is_ok(), "C2/E2");

        let mut wrong_role = system.clone();
        wrong_role.role = Role::User;
        assert_eq!(
            validate_resume(
                &ticket(),
                &command().with_context_messages(vec![wrong_role])
            ),
            Err(ResumeError::InvalidContextRole),
            "C3/E3"
        );
        assert_eq!(
            validate_resume(
                &ticket(),
                &command().with_context_messages(vec![system.clone(), system])
            ),
            Err(ResumeError::InvalidContextMessage),
            "C4/E3"
        );
    }
}
