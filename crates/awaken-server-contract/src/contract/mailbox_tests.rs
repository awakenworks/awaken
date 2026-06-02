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
    dispatch
        .claim("worker", "token", 1500, 1000)
        .expect("claim should be valid");
    dispatch
        .record_run_result(
            &RunDispatchResult {
                run_id: "run-001".into(),
                dispatch_instance_id: "dispatch-1".into(),
                status: RunStatus::Done,
                termination: Some(TerminationReason::NaturalEnd),
                response: Some("done".into()),
                error: None,
            },
            2000,
        )
        .expect("run result should be valid");

    let json = serde_json::to_string(&dispatch).unwrap();
    let parsed: RunDispatch = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.run_id(), "run-001");
    assert_eq!(parsed.dispatch_instance_id(), Some("dispatch-1"));
    assert_eq!(parsed.run_status(), Some(RunStatus::Done));
    assert_eq!(parsed.termination(), Some(&TerminationReason::NaturalEnd));
    assert_eq!(parsed.run_response(), Some("done"));
    assert_eq!(parsed.completed_at(), Some(2000));
    assert_eq!(parsed.status(), RunDispatchStatus::Claimed);
}

#[test]
fn run_dispatch_transition_api_enforces_lifecycle_shape() {
    let mut dispatch = RunDispatch::queued("dispatch-001", "thread-abc", "run-001", 1000)
        .with_dedupe_key(Some("req-xyz".to_string()));

    assert_eq!(dispatch.status(), RunDispatchStatus::Queued);
    assert!(dispatch.claim_token().is_none());

    dispatch
        .claim("consumer-1", "claim-1", 2000, 1100)
        .expect("queued dispatch can be claimed");
    assert_eq!(dispatch.status(), RunDispatchStatus::Claimed);
    assert_eq!(dispatch.claimed_by(), Some("consumer-1"));
    assert_eq!(dispatch.claim_token(), Some("claim-1"));
    assert_eq!(dispatch.lease_until(), Some(2000));

    dispatch.mark_acked(3000).expect("claimed dispatch can ack");
    assert_eq!(dispatch.status(), RunDispatchStatus::Acked);
    assert!(dispatch.claim_token().is_none());
    assert!(dispatch.claimed_by().is_none());
    assert!(dispatch.lease_until().is_none());
    assert_eq!(dispatch.completed_at(), Some(3000));
}

#[test]
fn run_dispatch_transition_api_rejects_claiming_terminal_dispatch() {
    let mut dispatch = RunDispatch::queued("dispatch-001", "thread-abc", "run-001", 1000);
    dispatch
        .mark_cancelled(2000)
        .expect("queued dispatch can be cancelled");

    let error = dispatch
        .claim("consumer-1", "claim-1", 3000, 2500)
        .unwrap_err();
    assert!(
        matches!(error, StorageError::Validation(ref message) if message.contains("must be Queued")),
        "unexpected error: {error}"
    );
}

#[test]
fn run_dispatch_transition_api_rejects_terminalizing_unclaimed_dispatch() {
    let mut dispatch = RunDispatch::queued("dispatch-001", "thread-abc", "run-001", 1000);

    let error = dispatch.mark_acked(2000).unwrap_err();
    assert!(
        matches!(error, StorageError::Validation(ref message) if message.contains("must be Claimed")),
        "unexpected ack error: {error}"
    );

    let error = dispatch.mark_dead_letter(2000, "failed").unwrap_err();
    assert!(
        matches!(error, StorageError::Validation(ref message) if message.contains("must be Claimed")),
        "unexpected dead-letter error: {error}"
    );

    let error = dispatch
        .mark_nack_result(2000, 3000, "retry later")
        .unwrap_err();
    assert!(
        matches!(error, StorageError::Validation(ref message) if message.contains("must be Claimed")),
        "unexpected nack error: {error}"
    );

    let error = dispatch.mark_expired_lease(2000, "expired").unwrap_err();
    assert!(
        matches!(error, StorageError::Validation(ref message) if message.contains("must be Claimed")),
        "unexpected expired-lease error: {error}"
    );
}

#[test]
fn run_dispatch_transition_api_rejects_cancelling_claimed_dispatch() {
    let mut dispatch = RunDispatch::queued("dispatch-001", "thread-abc", "run-001", 1000);
    dispatch
        .claim("consumer-1", "claim-1", 2000, 1100)
        .expect("queued dispatch can be claimed");

    let error = dispatch.mark_cancelled(2000).unwrap_err();
    assert!(
        matches!(error, StorageError::Validation(ref message) if message.contains("must be Queued")),
        "unexpected cancel error: {error}"
    );
}

#[test]
fn run_dispatch_requeue_after_nack_clears_runtime_projection() {
    use super::super::lifecycle::TerminationReason;

    let mut dispatch =
        RunDispatch::queued("dispatch-001", "thread-abc", "run-001", 1000).with_max_attempts(3);
    dispatch
        .claim("consumer-1", "claim-1", 2000, 1100)
        .expect("queued dispatch can be claimed");
    dispatch
        .record_run_result(
            &RunDispatchResult {
                run_id: "run-001".into(),
                dispatch_instance_id: "runtime-1".into(),
                status: RunStatus::Done,
                termination: Some(TerminationReason::NaturalEnd),
                response: Some("done".into()),
                error: None,
            },
            1500,
        )
        .expect("claimed dispatch records result");

    dispatch
        .mark_nack_result(1600, 1700, "retry")
        .expect("retryable nack requeues");

    assert_eq!(dispatch.status(), RunDispatchStatus::Queued);
    assert!(dispatch.dispatch_instance_id().is_none());
    assert!(dispatch.run_status().is_none());
    assert!(dispatch.termination().is_none());
    assert!(dispatch.run_response().is_none());
    assert!(dispatch.run_error().is_none());
    assert!(dispatch.completed_at().is_none());
    dispatch
        .validate_for_persist()
        .expect("queued shape is valid");
}

#[test]
fn run_dispatch_requeue_after_expired_lease_clears_runtime_projection() {
    let mut dispatch =
        RunDispatch::queued("dispatch-001", "thread-abc", "run-001", 1000).with_max_attempts(3);
    dispatch
        .claim("consumer-1", "claim-1", 2000, 1100)
        .expect("queued dispatch can be claimed");
    dispatch
        .record_dispatch_start("runtime-1", 1200)
        .expect("claimed dispatch records runtime start");

    dispatch
        .mark_expired_lease(2100, "expired")
        .expect("retryable lease expiration requeues");

    assert_eq!(dispatch.status(), RunDispatchStatus::Queued);
    assert!(dispatch.dispatch_instance_id().is_none());
    assert!(dispatch.run_status().is_none());
    assert!(dispatch.completed_at().is_none());
    dispatch
        .validate_for_persist()
        .expect("queued shape is valid");
}

#[test]
fn run_dispatch_dead_letter_after_expired_lease_clears_runtime_projection() {
    let mut dispatch =
        RunDispatch::queued("dispatch-001", "thread-abc", "run-001", 1000).with_max_attempts(1);
    dispatch
        .claim("consumer-1", "claim-1", 2000, 1100)
        .expect("queued dispatch can be claimed");
    dispatch
        .record_dispatch_start("runtime-1", 1200)
        .expect("claimed dispatch records runtime start");
    assert_eq!(dispatch.run_status(), Some(RunStatus::Running));
    assert_eq!(dispatch.dispatch_instance_id(), Some("runtime-1"));

    dispatch
        .mark_expired_lease(2100, "expired")
        .expect("max-attempt lease expiration dead-letters");

    assert_eq!(dispatch.status(), RunDispatchStatus::DeadLetter);
    assert_eq!(dispatch.completed_at(), Some(2100));
    // The abandoned attempt must not survive as a Running runtime projection on
    // the terminal dispatch.
    assert!(dispatch.run_status().is_none());
    assert!(dispatch.dispatch_instance_id().is_none());
    assert!(dispatch.termination().is_none());
    assert!(dispatch.run_response().is_none());
    assert!(dispatch.run_error().is_none());
    dispatch
        .validate_for_persist()
        .expect("dead-letter shape is valid");
}

#[test]
fn run_dispatch_rejects_persisted_queued_runtime_projection() {
    let mut parts =
        RunDispatch::queued("dispatch-001", "thread-abc", "run-001", 1000).to_persisted_parts();
    parts.dispatch_instance_id = Some("runtime-1".into());
    parts.run_status = Some(RunStatus::Running);

    let error = RunDispatch::from_persisted_parts(parts)
        .expect_err("queued dispatch with runtime projection must be rejected");
    assert!(
        matches!(error, StorageError::Validation(ref message) if message.contains("runtime result fields")),
        "unexpected error: {error}"
    );
}

#[test]
fn run_dispatch_supersede_transitions_are_explicit() {
    let mut queued = RunDispatch::queued("dispatch-001", "thread-abc", "run-001", 1000);
    queued
        .mark_superseded(1100, Some("replaced"))
        .expect("queued dispatch can be superseded");
    assert_eq!(queued.status(), RunDispatchStatus::Superseded);

    let mut claimed = RunDispatch::queued("dispatch-002", "thread-abc", "run-002", 1000);
    claimed
        .claim("consumer-1", "claim-1", 2000, 1100)
        .expect("queued dispatch can be claimed");
    let error = claimed
        .mark_superseded(1200, Some("generic supersede"))
        .expect_err("claimed dispatch needs the explicit epoch supersede path");
    assert!(
        matches!(error, StorageError::Validation(ref message) if message.contains("must be Queued")),
        "unexpected error: {error}"
    );
    claimed
        .record_dispatch_start("runtime-1", 1250)
        .expect("claimed dispatch records runtime start");
    claimed
        .mark_superseded_at_epoch(1300, 2, Some("stale epoch"))
        .expect("claimed dispatch can be explicitly superseded by epoch");
    assert_eq!(claimed.status(), RunDispatchStatus::Superseded);
    assert!(claimed.claim_token().is_none());
    assert!(claimed.dispatch_instance_id().is_none());
    assert!(claimed.run_status().is_none());

    let mut acked = RunDispatch::queued("dispatch-003", "thread-abc", "run-003", 1000);
    acked.claim("consumer-1", "claim-1", 2000, 1100).unwrap();
    acked.mark_acked(1200).unwrap();
    let error = acked
        .mark_superseded(1300, None)
        .expect_err("acked dispatch must remain terminal");
    assert!(
        matches!(error, StorageError::Validation(ref message) if message.contains("must be Queued")),
        "unexpected acked error: {error}"
    );

    let mut cancelled = RunDispatch::queued("dispatch-004", "thread-abc", "run-004", 1000);
    cancelled.mark_cancelled(1200).unwrap();
    let error = cancelled
        .mark_superseded(1300, None)
        .expect_err("cancelled dispatch must remain terminal");
    assert!(
        matches!(error, StorageError::Validation(ref message) if message.contains("must be Queued")),
        "unexpected cancelled error: {error}"
    );

    let mut dead_letter = RunDispatch::queued("dispatch-005", "thread-abc", "run-005", 1000);
    dead_letter
        .claim("consumer-1", "claim-1", 2000, 1100)
        .unwrap();
    dead_letter.mark_dead_letter(1200, "failed").unwrap();
    let error = dead_letter
        .mark_superseded(1300, None)
        .expect_err("dead-letter dispatch must remain terminal");
    assert!(
        matches!(error, StorageError::Validation(ref message) if message.contains("must be Queued")),
        "unexpected dead-letter error: {error}"
    );
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
