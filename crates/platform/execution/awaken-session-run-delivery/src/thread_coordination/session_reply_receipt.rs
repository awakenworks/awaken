// @formal entrypoint tool_result.conflicting_delivery_operation_never_reuses_a_receipt
// @formal entrypoint tool_result.exact_delivery_retry_replays_one_receipt
//! Durable idempotency receipt recovery for Session Thread tool replies.
//!
//! The logical Thread's committed `ResumeApplied` audit fact remains the sole
//! receipt authority. This module only classifies that committed history; it
//! owns no receipt store, cache, or alternate admission state.

use awaken_agent_contract::agent::awaiting::ToolCallAuthority;
use awaken_agent_contract::agent::awaiting::{AwaitTarget, ResumeTicket};
use awaken_agent_contract::agent::state::{StateKey as _, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource as _;
use awaken_runtime_contract::runs::RunError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionReplyReceipt {
    Absent,
    Exact,
    Conflict,
}

/// Validate one committed tool ticket against its explicitly named execution
/// authority. This is the only bridge between the neutral ResumeTicket and the
/// Runtime-owned ToolBatch cell; protocol handlers must not infer ownership
/// from a projected public event.
pub fn validate_awaiting_tool_authority(
    thread_id: &ThreadId,
    ticket: &ResumeTicket,
    state_commands: &[awaken_agent_contract::agent::state::Command],
) -> Result<(), RunError> {
    let AwaitTarget::ToolCall {
        reason, authority, ..
    } = ticket.target()
    else {
        return Err(RunError::internal(
            "tool authority validation requires a tool-call ticket",
        ));
    };
    let state = Store::rebuild(state_commands);
    let batch = awaken_tool_contract::ActiveToolBatch::load(&state)
        .map_err(|error| RunError::internal(error.to_string()))?;
    let validate_batch = |batch: &awaken_tool_contract::ToolBatch| {
        batch
            .validate_awaiting_ticket(thread_id, ticket)
            .map_err(|_| {
                RunError::bad_request(
                    "Session Thread awaiting ticket changed after Event admission",
                )
            })
    };
    match (authority, batch.as_ref()) {
        // Native/client tools remain owned by one durable ToolBatch; its
        // complete identity and payload are mandatory.
        (ToolCallAuthority::RuntimeToolBatch, Some(batch)) => validate_batch(batch),
        (ToolCallAuthority::RuntimeToolBatch, None) => Err(RunError::internal(
            "Runtime-owned awaiting tool Run has no ActiveToolBatch",
        )),
        // ACP and another opaque attempt executor commit the closed ticket
        // themselves. Their continuation validates the same ticket and result
        // kind; a stale Runtime batch is not their authority.
        (ToolCallAuthority::RunAttemptExecutor, _) => Ok(()),
        // Historical tickets did not encode their owner. An exact batch proves
        // Runtime ownership; otherwise only the historically emitted executor
        // permission shape is recoverable. New writers are always explicit.
        (ToolCallAuthority::LegacyUnspecified, Some(batch)) => validate_batch(batch),
        (ToolCallAuthority::LegacyUnspecified, None)
            if *reason == awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission =>
        {
            Ok(())
        }
        (ToolCallAuthority::LegacyUnspecified, None) => Err(RunError::internal(
            "legacy awaiting tool Run has no recoverable execution authority",
        )),
    }
}

/// Fold one committed ResumeApplied fact into the reply-recovery decision.
/// This is the sole classifier used by the history scan and its Kani proof:
/// unrelated facts stutter, a same-correlation/different-operation fact closes
/// the reply as conflicting, and an exact operation receipt is absorbing.
const fn advance_session_reply_receipt(
    current: SessionReplyReceipt,
    same_correlation: bool,
    same_operation: bool,
) -> SessionReplyReceipt {
    if matches!(current, SessionReplyReceipt::Exact) {
        return SessionReplyReceipt::Exact;
    }
    if same_correlation && same_operation {
        SessionReplyReceipt::Exact
    } else if same_correlation {
        SessionReplyReceipt::Conflict
    } else {
        current
    }
}

impl super::CoordinatedThreads {
    /// Classify the committed receipt only after the current active ticket fails
    /// validation. A successful resume intentionally deletes that ticket, while
    /// the audit fact lives in the same ThreadCommit and therefore survives
    /// response loss, Worker settlement, process restart, and projection lag.
    pub(super) async fn session_thread_tool_reply_receipt(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<SessionReplyReceipt, RunError> {
        if !command.has_complete_coordinates() {
            return Err(RunError::bad_request(
                "Session Thread tool reply is incomplete",
            ));
        }
        let thread_id = command.target.thread_id(&command.session_id);
        let commit = self
            .reads
            .commit_for_read(command.session_id.as_str())
            .await?;
        let snapshot = commit
            .recovery_snapshot(&thread_id, &command.expected_run_id)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        let expected_operation = command.delivery_operation_id();
        let mut receipt = SessionReplyReceipt::Absent;
        for event in snapshot.events.iter().filter(|event| {
            event.run_id == command.expected_run_id
                && event.kind == awaken_agent_contract::audit::kind::Kind::ResumeApplied
        }) {
            let correlation = event
                .payload
                .get("correlation_id")
                .and_then(serde_json::Value::as_str);
            let same_correlation = correlation == Some(command.expected_correlation_id.as_str());
            let same_operation = event
                .payload
                .get("operation_id")
                .and_then(serde_json::Value::as_str)
                == Some(expected_operation.as_str());
            receipt = advance_session_reply_receipt(receipt, same_correlation, same_operation);
            if receipt == SessionReplyReceipt::Exact {
                break;
            }
        }
        Ok(receipt)
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const EVERY_RECEIPT: [SessionReplyReceipt; 3] = [
        SessionReplyReceipt::Absent,
        SessionReplyReceipt::Conflict,
        SessionReplyReceipt::Exact,
    ];

    const fn listed_receipt(receipt: SessionReplyReceipt) -> usize {
        match receipt {
            SessionReplyReceipt::Absent => 0,
            SessionReplyReceipt::Conflict => 1,
            SessionReplyReceipt::Exact => 2,
        }
    }

    #[test]
    fn every_reply_receipt_is_listed() {
        for (index, receipt) in EVERY_RECEIPT.into_iter().enumerate() {
            assert_eq!(listed_receipt(receipt), index);
        }
    }

    /// @exhaustive SessionReplyReceipt(3) x bool(2) x bool(2) = 12
    /// @obligation tool_result.conflicting_delivery_operation_never_reuses_a_receipt
    /// @obligation tool_result.exact_delivery_retry_replays_one_receipt
    #[test]
    fn resume_receipt_classification_is_exact_and_conflict_absorbing() {
        // K1=C1+C2=>Exact, K2=C1+!C2+!C3=>Conflict, K3=!C1=>unchanged,
        // K4=C3=>Exact.
        let mut cases = 0;
        for current in EVERY_RECEIPT {
            for same_correlation in [false, true] {
                for same_operation in [false, true] {
                    let next =
                        advance_session_reply_receipt(current, same_correlation, same_operation);
                    if current == SessionReplyReceipt::Exact || (same_correlation && same_operation)
                    {
                        assert!(next == SessionReplyReceipt::Exact, "K1/K4");
                    } else if same_correlation {
                        assert!(next == SessionReplyReceipt::Conflict, "K2");
                    } else {
                        assert!(next == current, "K3");
                    }
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 12);
    }
}
