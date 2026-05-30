//! Run dispatch data vocabulary for the unified run queue.
//!
//! The `MailboxStore` persistence trait, the `DispatchSignalReceipt`/
//! `DispatchSignalEntry` durable-signal pair, and the
//! `MailboxStore`→`LiveRunCommandSource` bridge are a server/store concern and
//! moved to `awaken-server-contract`. The dispatch data types stay here.

use serde::{Deserialize, Serialize};

use super::lifecycle::{RunStatus, TerminationReason};

// ── RunDispatchStatus ───────────────────────────────────────────────

/// Six-state lifecycle for a dispatch attempt.
///
/// ```text
/// Queued ──claim──> Claimed ──ack──> Acked (terminal)
///   |                  |
///   |               nack(retry) ──> Queued (attempt_count++, available_at = retry_at)
///   |                  |
///   |               nack(permanent) ──> DeadLetter (terminal)
///   |
///   |── cancel ──> Cancelled (terminal)
///   └── interrupt(dispatch epoch bump) ──> Superseded (terminal)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunDispatchStatus {
    Queued,
    Claimed,
    Acked,
    Cancelled,
    Superseded,
    DeadLetter,
}

impl RunDispatchStatus {
    /// Returns `true` for terminal states that cannot transition further.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Acked | Self::Cancelled | Self::Superseded | Self::DeadLetter
        )
    }
}

// ── RunDispatchResult ────────────────────────────────────────────────

/// Durable runtime-result projection for the dispatch that consumed a run.
///
/// `RunRecord` remains the source of truth for business outcome. This compact
/// projection exists on the queue record so operators can inspect what happened
/// to a claimed dispatch without treating `Acked` as agent success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDispatchResult {
    /// Runtime run ID used by the execution engine.
    pub run_id: String,
    /// Dispatch attempt ID that links a queue claim to a runtime invocation.
    pub dispatch_instance_id: String,
    /// Durable runtime status reached by this run.
    pub status: RunStatus,
    /// Structured terminal reason, if the runtime reached a terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<TerminationReason>,
    /// Final response text, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    /// Runtime error text, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── RunDispatch ─────────────────────────────────────────────────────

/// A run dispatch persisted in the mailbox queue.
///
/// This record owns delivery/lease/retry state only. Business request,
/// message, and outcome semantics live on `RunRecord` and the thread message
/// log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDispatch {
    // ── identity ──
    /// UUID v7, globally unique.
    pub dispatch_id: String,
    /// Thread ID, routing anchor.
    pub thread_id: String,
    /// Canonical runtime run ID this dispatch activates.
    pub run_id: String,

    // ── queue semantics ──
    /// 0 = highest, 255 = lowest, default 128.
    pub priority: u8,
    /// Idempotent delivery key.
    pub dedupe_key: Option<String>,
    /// Thread dispatch epoch captured when this dispatch was created.
    pub dispatch_epoch: u64,

    // ── lifecycle ──
    /// Current status.
    pub status: RunDispatchStatus,
    /// Unix millis; future value = delayed delivery.
    pub available_at: u64,
    /// Number of claim attempts so far.
    pub attempt_count: u32,
    /// Maximum attempts before dead-lettering (default 5).
    pub max_attempts: u32,
    /// Last error message.
    pub last_error: Option<String>,

    // ── lease ──
    /// UUID set on claim.
    pub claim_token: Option<String>,
    /// Consumer identifier (process) that claimed this dispatch.
    pub claimed_by: Option<String>,
    /// Unix millis, extended by heartbeat.
    pub lease_until: Option<u64>,

    // ── runtime trace ──
    /// Dispatch attempt ID associated with the current/latest claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_instance_id: Option<String>,
    /// Runtime status associated with this dispatch's current/latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_status: Option<RunStatus>,
    /// Structured terminal reason for the current/latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<TerminationReason>,
    /// Final response text for the current/latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_response: Option<String>,
    /// Runtime error text for the current/latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_error: Option<String>,
    /// Unix millis when the runtime result was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u64>,

    // ── timestamps ──
    /// Unix millis when the dispatch was created.
    pub created_at: u64,
    /// Unix millis of the last update.
    pub updated_at: u64,
}

// ── MailboxInterrupt ────────────────────────────────────────────────

/// Result of a mailbox interrupt operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxInterrupt {
    /// New thread dispatch epoch after bump.
    pub new_dispatch_epoch: u64,
    /// The dispatch that was Claimed (running) at interrupt time, if any.
    /// Caller should cancel the corresponding runtime run.
    pub active_dispatch: Option<RunDispatch>,
    /// Number of Queued dispatches superseded.
    pub superseded_count: usize,
}

/// Detailed result of a mailbox interrupt operation.
///
/// `MailboxInterrupt` intentionally keeps the 0.2 public struct shape so
/// downstream struct literals remain source-compatible. New code that needs the
/// exact superseded dispatch records should use this type via
/// [`MailboxStore::interrupt_detailed`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxInterruptDetails {
    /// New thread dispatch epoch after bump.
    pub new_dispatch_epoch: u64,
    /// The dispatch that was Claimed (running) at interrupt time, if any.
    /// Caller should cancel the corresponding runtime run.
    pub active_dispatch: Option<RunDispatch>,
    /// Number of Queued dispatches superseded.
    pub superseded_count: usize,
    /// Queued dispatches that were atomically superseded by this interrupt.
    ///
    /// This is the authoritative set callers should use to reconcile terminal
    /// dispatch state back to the durable run lifecycle.
    #[serde(default)]
    pub superseded_dispatches: Vec<RunDispatch>,
}

impl MailboxInterruptDetails {
    #[must_use]
    pub fn into_summary(self) -> MailboxInterrupt {
        MailboxInterrupt {
            new_dispatch_epoch: self.new_dispatch_epoch,
            active_dispatch: self.active_dispatch,
            superseded_count: self.superseded_count,
        }
    }

    #[must_use]
    pub fn summary(&self) -> MailboxInterrupt {
        MailboxInterrupt {
            new_dispatch_epoch: self.new_dispatch_epoch,
            active_dispatch: self.active_dispatch.clone(),
            superseded_count: self.superseded_count,
        }
    }
}

impl From<MailboxInterrupt> for MailboxInterruptDetails {
    fn from(interrupt: MailboxInterrupt) -> Self {
        Self {
            new_dispatch_epoch: interrupt.new_dispatch_epoch,
            active_dispatch: interrupt.active_dispatch,
            superseded_count: interrupt.superseded_count,
            superseded_dispatches: Vec::new(),
        }
    }
}

impl From<MailboxInterruptDetails> for MailboxInterrupt {
    fn from(details: MailboxInterruptDetails) -> Self {
        details.into_summary()
    }
}

pub use super::live_control::{
    LiveCommandReceipt, LiveControlError, LiveDeliveryOutcome, LiveRunCommand, LiveRunCommandEntry,
    LiveRunCommandSource, LiveRunCommandStream, LiveRunTarget,
};

#[cfg(test)]
mod tests {
    use super::*;

    // ── Property-based tests ──

    mod proptest_mailbox {
        use super::*;
        use proptest::prelude::*;

        fn arb_dispatch_status() -> impl Strategy<Value = RunDispatchStatus> {
            prop_oneof![
                Just(RunDispatchStatus::Queued),
                Just(RunDispatchStatus::Claimed),
                Just(RunDispatchStatus::Acked),
                Just(RunDispatchStatus::Cancelled),
                Just(RunDispatchStatus::Superseded),
                Just(RunDispatchStatus::DeadLetter),
            ]
        }

        fn arb_dispatch() -> impl Strategy<Value = RunDispatch> {
            (
                arb_dispatch_status(),
                0u32..100,
                0u64..u64::MAX,
                0u64..u64::MAX,
                0u8..=255u8,
                1u32..20,
                0u64..1_000_000,
            )
                .prop_map(
                    |(
                        status,
                        attempt_count,
                        created_at,
                        available_at,
                        priority,
                        max_attempts,
                        dispatch_epoch,
                    )| {
                        let claim_token = match status {
                            RunDispatchStatus::Claimed => Some("token-123".to_string()),
                            _ => None,
                        };
                        let claimed_by = match status {
                            RunDispatchStatus::Claimed => Some("consumer-1".to_string()),
                            _ => None,
                        };
                        RunDispatch {
                            dispatch_id: "dispatch-prop".to_string(),
                            thread_id: "thread-prop".to_string(),
                            run_id: "run-prop".to_string(),
                            priority,
                            dedupe_key: None,
                            dispatch_epoch,
                            status,
                            available_at,
                            attempt_count,
                            max_attempts,
                            last_error: None,
                            claim_token,
                            claimed_by,
                            lease_until: if status == RunDispatchStatus::Claimed {
                                Some(created_at.saturating_add(30_000))
                            } else {
                                None
                            },
                            dispatch_instance_id: None,
                            run_status: None,
                            termination: None,
                            run_response: None,
                            run_error: None,
                            completed_at: None,
                            created_at,
                            updated_at: created_at,
                        }
                    },
                )
        }

        proptest! {
            #[test]
            fn terminal_status_is_terminal(status in arb_dispatch_status()) {
                let expected_terminal = matches!(
                    status,
                    RunDispatchStatus::Acked
                    | RunDispatchStatus::Cancelled
                    | RunDispatchStatus::Superseded
                    | RunDispatchStatus::DeadLetter
                );
                prop_assert_eq!(status.is_terminal(), expected_terminal);
            }

            #[test]
            fn claimed_dispatch_always_has_claim_token(dispatch in arb_dispatch()) {
                if dispatch.status == RunDispatchStatus::Claimed {
                    prop_assert!(
                        dispatch.claim_token.is_some(),
                        "Claimed dispatch must have a claim_token"
                    );
                }
            }

            #[test]
            fn queued_dispatch_never_has_claim_token(dispatch in arb_dispatch()) {
                if dispatch.status == RunDispatchStatus::Queued {
                    prop_assert!(
                        dispatch.claim_token.is_none(),
                        "Queued dispatch must not have a claim_token"
                    );
                }
            }

            #[test]
            fn run_dispatch_serde_roundtrip(dispatch in arb_dispatch()) {
                let json = serde_json::to_string(&dispatch).unwrap();
                let parsed: RunDispatch = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(parsed.dispatch_id, dispatch.dispatch_id);
                prop_assert_eq!(parsed.status, dispatch.status);
                prop_assert_eq!(parsed.attempt_count, dispatch.attempt_count);
                prop_assert_eq!(parsed.priority, dispatch.priority);
                prop_assert_eq!(parsed.dispatch_epoch, dispatch.dispatch_epoch);
                prop_assert_eq!(parsed.claim_token, dispatch.claim_token);
                prop_assert_eq!(parsed.available_at, dispatch.available_at);
                prop_assert_eq!(parsed.max_attempts, dispatch.max_attempts);
            }

            #[test]
            fn run_dispatch_status_serde_roundtrip_prop(status in arb_dispatch_status()) {
                let json = serde_json::to_string(&status).unwrap();
                let parsed: RunDispatchStatus = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(parsed, status);
            }
        }
    }

    #[test]
    fn is_terminal_returns_true_for_terminal_states() {
        assert!(RunDispatchStatus::Acked.is_terminal());
        assert!(RunDispatchStatus::Cancelled.is_terminal());
        assert!(RunDispatchStatus::Superseded.is_terminal());
        assert!(RunDispatchStatus::DeadLetter.is_terminal());
    }

    #[test]
    fn is_terminal_returns_false_for_non_terminal_states() {
        assert!(!RunDispatchStatus::Queued.is_terminal());
        assert!(!RunDispatchStatus::Claimed.is_terminal());
    }

    fn make_run_dispatch() -> RunDispatch {
        RunDispatch {
            dispatch_id: "dispatch-001".to_string(),
            thread_id: "thread-abc".to_string(),
            run_id: "run-001".to_string(),
            priority: 128,
            dedupe_key: Some("req-xyz".to_string()),
            dispatch_epoch: 1,
            status: RunDispatchStatus::Queued,
            available_at: 1000,
            attempt_count: 0,
            max_attempts: 5,
            last_error: None,
            claim_token: None,
            claimed_by: None,
            lease_until: None,
            dispatch_instance_id: None,
            run_status: None,
            termination: None,
            run_response: None,
            run_error: None,
            completed_at: None,
            created_at: 1000,
            updated_at: 1000,
        }
    }

    #[test]
    fn run_dispatch_serde_roundtrip() {
        let dispatch = make_run_dispatch();
        let json = serde_json::to_string(&dispatch).unwrap();
        let parsed: RunDispatch = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.dispatch_id, "dispatch-001");
        assert_eq!(parsed.thread_id, "thread-abc");
        assert_eq!(parsed.run_id, "run-001");
        assert_eq!(parsed.priority, 128);
        assert_eq!(parsed.dedupe_key.as_deref(), Some("req-xyz"));
        assert_eq!(parsed.dispatch_epoch, 1);
        assert_eq!(parsed.status, RunDispatchStatus::Queued);
        assert_eq!(parsed.available_at, 1000);
        assert_eq!(parsed.attempt_count, 0);
        assert_eq!(parsed.max_attempts, 5);
        assert!(parsed.last_error.is_none());
        assert!(parsed.claim_token.is_none());
        assert!(parsed.claimed_by.is_none());
        assert!(parsed.lease_until.is_none());
        assert!(parsed.dispatch_instance_id.is_none());
        assert!(parsed.run_status.is_none());
        assert!(parsed.termination.is_none());
        assert!(parsed.run_response.is_none());
        assert!(parsed.run_error.is_none());
        assert!(parsed.completed_at.is_none());
        assert_eq!(parsed.created_at, 1000);
        assert_eq!(parsed.updated_at, 1000);
    }

    #[test]
    fn run_dispatch_runtime_trace_serde_roundtrip() {
        use super::super::lifecycle::TerminationReason;

        let mut dispatch = make_run_dispatch();
        dispatch.dispatch_instance_id = Some("dispatch-1".into());
        dispatch.run_status = Some(RunStatus::Done);
        dispatch.termination = Some(TerminationReason::NaturalEnd);
        dispatch.run_response = Some("done".into());
        dispatch.completed_at = Some(2000);

        let json = serde_json::to_string(&dispatch).unwrap();
        let parsed: RunDispatch = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.run_id, "run-001");
        assert_eq!(parsed.dispatch_instance_id.as_deref(), Some("dispatch-1"));
        assert_eq!(parsed.run_status, Some(RunStatus::Done));
        assert_eq!(parsed.termination, Some(TerminationReason::NaturalEnd));
        assert_eq!(parsed.run_response.as_deref(), Some("done"));
        assert_eq!(parsed.completed_at, Some(2000));
        assert_eq!(parsed.status, RunDispatchStatus::Queued);
    }

    #[test]
    fn run_dispatch_result_serde_roundtrip() {
        use super::super::lifecycle::TerminationReason;

        let result = RunDispatchResult {
            run_id: "run-1".into(),
            dispatch_instance_id: "dispatch-1".into(),
            status: RunStatus::Done,
            termination: Some(TerminationReason::Blocked("needs approval".into())),
            response: None,
            error: Some("needs approval".into()),
        };

        let json = serde_json::to_string(&result).unwrap();
        let parsed: RunDispatchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, result);
    }

    #[test]
    fn run_dispatch_status_serde_roundtrip() {
        for status in [
            RunDispatchStatus::Queued,
            RunDispatchStatus::Claimed,
            RunDispatchStatus::Acked,
            RunDispatchStatus::Cancelled,
            RunDispatchStatus::Superseded,
            RunDispatchStatus::DeadLetter,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: RunDispatchStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn mailbox_interrupt_serde_roundtrip() {
        let interrupt = MailboxInterrupt {
            new_dispatch_epoch: 5,
            active_dispatch: Some(make_run_dispatch()),
            superseded_count: 3,
        };
        let json = serde_json::to_string(&interrupt).unwrap();
        let parsed: MailboxInterrupt = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.new_dispatch_epoch, 5);
        assert!(parsed.active_dispatch.is_some());
        assert_eq!(parsed.superseded_count, 3);
    }

    #[test]
    fn mailbox_interrupt_ignores_detailed_payload_for_legacy_summary() {
        let json = serde_json::json!({
            "new_dispatch_epoch": 5,
            "active_dispatch": null,
            "superseded_count": 3,
            "superseded_dispatches": [make_run_dispatch()]
        });
        let parsed: MailboxInterrupt = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.new_dispatch_epoch, 5);
        assert!(parsed.active_dispatch.is_none());
        assert_eq!(parsed.superseded_count, 3);
    }

    #[test]
    fn mailbox_interrupt_details_serde_roundtrip() {
        let details = MailboxInterruptDetails {
            new_dispatch_epoch: 5,
            active_dispatch: Some(make_run_dispatch()),
            superseded_count: 3,
            superseded_dispatches: vec![make_run_dispatch()],
        };
        let json = serde_json::to_string(&details).unwrap();
        let parsed: MailboxInterruptDetails = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.new_dispatch_epoch, 5);
        assert!(parsed.active_dispatch.is_some());
        assert_eq!(parsed.superseded_count, 3);
        assert_eq!(parsed.superseded_dispatches.len(), 1);
        assert_eq!(parsed.summary().superseded_count, 3);
    }
}
