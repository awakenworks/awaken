use awaken_server_contract::contract::mailbox::RunDispatch;

pub(crate) const REASON_CLAIMED_SUPERSEDED_BY_EPOCH: &str =
    "claimed dispatch superseded by newer dispatch epoch";
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
pub(crate) const REASON_CLAIMED_SUPERSEDED_BEFORE_ACK: &str =
    "claimed dispatch superseded before ack";
pub(crate) const REASON_CLAIMED_SUPERSEDED_BEFORE_START: &str =
    "claimed dispatch superseded before runtime start";
pub(crate) const REASON_CLAIMED_SUPERSEDED_BEFORE_RESULT: &str =
    "claimed dispatch superseded before run result";
pub(crate) const REASON_CLAIMED_SUPERSEDED_BEFORE_NACK: &str =
    "claimed dispatch superseded before nack";
pub(crate) const REASON_CLAIMED_SUPERSEDED_BEFORE_DEAD_LETTER: &str =
    "claimed dispatch superseded before dead letter";
pub(crate) const REASON_CLAIMED_SUPERSEDED_DURING_LEASE_RENEWAL: &str =
    "claimed dispatch superseded during lease renewal";
pub(crate) const REASON_CLAIMED_LEASE_EXPIRED_AFTER_INTERRUPT: &str =
    "claimed dispatch lease expired after interrupt";
pub(crate) const REASON_QUEUED_SUPERSEDED_BY_INTERRUPT: &str =
    "queued dispatch superseded by interrupt";
pub(crate) const REASON_QUEUED_SUPERSEDED_BY_EPOCH: &str =
    "queued dispatch superseded by newer dispatch epoch";
pub(crate) const REASON_LEASE_EXPIRED_MAX_ATTEMPTS: &str = "lease expired; max attempts reached";

pub(crate) fn mark_superseded(dispatch: &mut RunDispatch, now: u64, reason: Option<&str>) {
    dispatch
        .mark_superseded(now, reason)
        .expect("superseded dispatch transition must preserve invariants");
}

#[cfg_attr(not(feature = "nats"), allow(dead_code))]
pub(crate) fn mark_superseded_at_epoch(
    dispatch: &mut RunDispatch,
    now: u64,
    epoch: u64,
    reason: Option<&str>,
) {
    dispatch
        .mark_superseded_at_epoch(now, epoch, reason)
        .expect("superseded dispatch transition must preserve invariants");
}

pub(crate) fn mark_acked(dispatch: &mut RunDispatch, now: u64) {
    dispatch
        .mark_acked(now)
        .expect("acked dispatch transition must preserve invariants");
}

pub(crate) fn mark_cancelled(dispatch: &mut RunDispatch, now: u64) {
    dispatch
        .mark_cancelled(now)
        .expect("cancelled dispatch transition must preserve invariants");
}

pub(crate) fn mark_dead_letter(dispatch: &mut RunDispatch, now: u64, error: &str) {
    dispatch
        .mark_dead_letter(now, error)
        .expect("dead-letter dispatch transition must preserve invariants");
}

pub(crate) fn mark_nack_result(dispatch: &mut RunDispatch, now: u64, retry_at: u64, error: &str) {
    dispatch
        .mark_nack_result(now, retry_at, error)
        .expect("nack dispatch transition must preserve invariants");
}

pub(crate) fn mark_expired_lease(dispatch: &mut RunDispatch, now: u64) {
    dispatch
        .mark_expired_lease(now, REASON_LEASE_EXPIRED_MAX_ATTEMPTS)
        .expect("expired-lease dispatch transition must preserve invariants");
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_server_contract::contract::mailbox::RunDispatchStatus;

    fn dispatch() -> RunDispatch {
        let mut dispatch = RunDispatch::queued(
            "dispatch-1".to_string(),
            "thread-1".to_string(),
            "run-1".to_string(),
            1000,
        )
        .with_dispatch_epoch(1)
        .with_max_attempts(2);
        dispatch
            .claim("worker", "token", 2000, 1000)
            .expect("test dispatch claim is valid");
        dispatch
    }

    #[test]
    fn terminal_transitions_clear_claim_fields() {
        let mut dispatch = dispatch();
        mark_acked(&mut dispatch, 3000);

        assert_eq!(dispatch.status(), RunDispatchStatus::Acked);
        assert_eq!(dispatch.completed_at(), Some(3000));
        assert!(dispatch.claim_token().is_none());
        assert!(dispatch.claimed_by().is_none());
        assert!(dispatch.lease_until().is_none());
    }

    #[test]
    fn nack_requeues_until_attempts_are_exhausted() {
        let mut dispatch = dispatch();
        mark_nack_result(&mut dispatch, 3000, 4000, "temporary failure");

        assert_eq!(dispatch.status(), RunDispatchStatus::Queued);
        assert_eq!(dispatch.attempt_count(), 1);
        assert_eq!(dispatch.available_at(), 4000);
        assert_eq!(dispatch.last_error(), Some("temporary failure"));

        dispatch
            .claim("worker", "token-2", 5000, 5000)
            .expect("requeued dispatch can be claimed again");
        mark_nack_result(&mut dispatch, 6000, 7000, "final failure");

        assert_eq!(dispatch.status(), RunDispatchStatus::DeadLetter);
        assert_eq!(dispatch.attempt_count(), 2);
        assert_eq!(dispatch.completed_at(), Some(6000));
        assert!(dispatch.claim_token().is_none());
    }

    #[test]
    fn expired_lease_records_dead_letter_reason() {
        let mut dispatch = dispatch();
        dispatch = dispatch.with_attempt_count(1);
        mark_expired_lease(&mut dispatch, 3000);

        assert_eq!(dispatch.status(), RunDispatchStatus::DeadLetter);
        assert_eq!(
            dispatch.last_error(),
            Some(REASON_LEASE_EXPIRED_MAX_ATTEMPTS)
        );
    }
}
