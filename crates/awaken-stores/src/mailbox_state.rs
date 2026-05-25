use awaken_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};

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

pub(crate) fn clear_claim_fields(dispatch: &mut RunDispatch) {
    dispatch.claim_token = None;
    dispatch.claimed_by = None;
    dispatch.lease_until = None;
}

pub(crate) fn mark_superseded(dispatch: &mut RunDispatch, now: u64, reason: Option<&str>) {
    dispatch.status = RunDispatchStatus::Superseded;
    dispatch.completed_at = Some(now);
    dispatch.updated_at = now;
    if let Some(reason) = reason {
        dispatch.last_error = Some(reason.to_string());
    }
    clear_claim_fields(dispatch);
}

#[cfg_attr(not(feature = "nats"), allow(dead_code))]
pub(crate) fn mark_superseded_at_epoch(
    dispatch: &mut RunDispatch,
    now: u64,
    epoch: u64,
    reason: Option<&str>,
) {
    dispatch.dispatch_epoch = epoch;
    mark_superseded(dispatch, now, reason);
}

pub(crate) fn mark_acked(dispatch: &mut RunDispatch, now: u64) {
    dispatch.status = RunDispatchStatus::Acked;
    dispatch.completed_at = Some(now);
    dispatch.updated_at = now;
    clear_claim_fields(dispatch);
}

pub(crate) fn mark_cancelled(dispatch: &mut RunDispatch, now: u64) {
    dispatch.status = RunDispatchStatus::Cancelled;
    dispatch.completed_at = Some(now);
    dispatch.updated_at = now;
    clear_claim_fields(dispatch);
}

pub(crate) fn mark_dead_letter(dispatch: &mut RunDispatch, now: u64, error: &str) {
    dispatch.status = RunDispatchStatus::DeadLetter;
    dispatch.last_error = Some(error.to_string());
    dispatch.completed_at = Some(now);
    dispatch.updated_at = now;
    clear_claim_fields(dispatch);
}

pub(crate) fn mark_nack_result(dispatch: &mut RunDispatch, now: u64, retry_at: u64, error: &str) {
    dispatch.attempt_count += 1;
    dispatch.last_error = Some(error.to_string());
    dispatch.updated_at = now;
    clear_claim_fields(dispatch);
    if dispatch.attempt_count >= dispatch.max_attempts {
        dispatch.status = RunDispatchStatus::DeadLetter;
        dispatch.completed_at = Some(now);
    } else {
        dispatch.status = RunDispatchStatus::Queued;
        dispatch.available_at = retry_at;
    }
}

pub(crate) fn mark_expired_lease(dispatch: &mut RunDispatch, now: u64) {
    dispatch.attempt_count = dispatch.attempt_count.saturating_add(1);
    dispatch.available_at = now;
    dispatch.updated_at = now;
    clear_claim_fields(dispatch);
    if dispatch.attempt_count >= dispatch.max_attempts {
        dispatch.status = RunDispatchStatus::DeadLetter;
        dispatch.last_error = Some(REASON_LEASE_EXPIRED_MAX_ATTEMPTS.to_string());
        dispatch.completed_at = Some(now);
    } else {
        dispatch.status = RunDispatchStatus::Queued;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch() -> RunDispatch {
        RunDispatch {
            dispatch_id: "dispatch-1".to_string(),
            thread_id: "thread-1".to_string(),
            run_id: "run-1".to_string(),
            priority: 128,
            dedupe_key: None,
            dispatch_epoch: 1,
            status: RunDispatchStatus::Claimed,
            available_at: 1000,
            attempt_count: 0,
            max_attempts: 2,
            last_error: None,
            claim_token: Some("token".to_string()),
            claimed_by: Some("worker".to_string()),
            lease_until: Some(2000),
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
    fn terminal_transitions_clear_claim_fields() {
        let mut dispatch = dispatch();
        mark_acked(&mut dispatch, 3000);

        assert_eq!(dispatch.status, RunDispatchStatus::Acked);
        assert_eq!(dispatch.completed_at, Some(3000));
        assert!(dispatch.claim_token.is_none());
        assert!(dispatch.claimed_by.is_none());
        assert!(dispatch.lease_until.is_none());
    }

    #[test]
    fn nack_requeues_until_attempts_are_exhausted() {
        let mut dispatch = dispatch();
        mark_nack_result(&mut dispatch, 3000, 4000, "temporary failure");

        assert_eq!(dispatch.status, RunDispatchStatus::Queued);
        assert_eq!(dispatch.attempt_count, 1);
        assert_eq!(dispatch.available_at, 4000);
        assert_eq!(dispatch.last_error.as_deref(), Some("temporary failure"));

        dispatch.claim_token = Some("token-2".to_string());
        dispatch.claimed_by = Some("worker".to_string());
        dispatch.lease_until = Some(5000);
        mark_nack_result(&mut dispatch, 6000, 7000, "final failure");

        assert_eq!(dispatch.status, RunDispatchStatus::DeadLetter);
        assert_eq!(dispatch.attempt_count, 2);
        assert_eq!(dispatch.completed_at, Some(6000));
        assert!(dispatch.claim_token.is_none());
    }

    #[test]
    fn expired_lease_records_dead_letter_reason() {
        let mut dispatch = dispatch();
        dispatch.attempt_count = 1;
        mark_expired_lease(&mut dispatch, 3000);

        assert_eq!(dispatch.status, RunDispatchStatus::DeadLetter);
        assert_eq!(
            dispatch.last_error.as_deref(),
            Some(REASON_LEASE_EXPIRED_MAX_ATTEMPTS)
        );
    }
}
