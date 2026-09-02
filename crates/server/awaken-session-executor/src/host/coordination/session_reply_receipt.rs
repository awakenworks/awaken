//! Durable idempotency receipt recovery for Session Thread tool replies.
//!
//! The logical Thread's committed `ResumeApplied` audit fact remains the sole
//! receipt authority. This module only classifies that committed history; it
//! owns no receipt store, cache, or alternate admission state.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionReplyReceipt {
    Absent,
    Exact,
    Conflict,
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

#[cfg(kani)]
#[kani::proof]
fn resume_receipt_classification_is_exact_and_conflict_absorbing() {
    // Cause/effect graph: C1 an Event has the expected correlation; C2 it has
    // the expected operation; C3 an exact receipt was already observed.
    // Effects: E1 C1+C2 selects Exact; E2 C1+!C2 selects Conflict unless E1 is
    // already absorbing; E3 !C1 cannot change the accumulated decision.
    // Decision rules K1=C1+C2=>E1, K2=C1+!C2+!C3=>E2,
    // K3=!C1=>E3, K4=C3=>Exact. These are the complete Boolean partitions.
    let current = match kani::any::<u8>() % 3 {
        0 => SessionReplyReceipt::Absent,
        1 => SessionReplyReceipt::Conflict,
        _ => SessionReplyReceipt::Exact,
    };
    let same_correlation = kani::any::<bool>();
    let same_operation = kani::any::<bool>();
    let next = advance_session_reply_receipt(current, same_correlation, same_operation);

    if current == SessionReplyReceipt::Exact || (same_correlation && same_operation) {
        assert!(next == SessionReplyReceipt::Exact, "K1/K4");
    } else if same_correlation {
        assert!(next == SessionReplyReceipt::Conflict, "K2");
    } else {
        assert!(next == current, "K3");
    }
}

impl SharedHost {
    /// Classify the committed receipt only after the current active ticket fails
    /// validation. A successful resume intentionally deletes that ticket, while
    /// the audit fact lives in the same ThreadCommit and therefore survives
    /// response loss, Worker settlement, process restart, and projection lag.
    pub(super) async fn session_thread_tool_reply_receipt(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<SessionReplyReceipt, HostError> {
        if command.session_id.trim().is_empty()
            || command.expected_run_id.0.trim().is_empty()
            || command.expected_correlation_id.trim().is_empty()
            || command.tool_use_id.trim().is_empty()
        {
            return Err(HostError::bad_request(
                "Session Thread tool reply is incomplete",
            ));
        }
        let thread_id = command.target.thread_id(&command.session_id);
        let commit = self.commit_for_read(&command.session_id).await?;
        let snapshot = commit
            .recovery_snapshot(&thread_id, &command.expected_run_id)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
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
