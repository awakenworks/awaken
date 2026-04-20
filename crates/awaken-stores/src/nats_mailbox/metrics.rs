//! Low-cardinality NATS mailbox metrics.

use std::time::Duration;

use awaken_contract::contract::mailbox::{LiveDeliveryOutcome, RunDispatch};

pub(crate) fn inc_claim_attempt(result: &'static str) {
    ::metrics::counter!(
        "awaken_mailbox_claim_attempt_total",
        "result" => result
    )
    .increment(1);
}

pub(crate) fn record_claim_scan(keys: usize, elapsed: Duration) {
    if keys > 0 {
        ::metrics::counter!("awaken_mailbox_claim_scan_keys_total").increment(keys as u64);
    }
    ::metrics::histogram!("awaken_mailbox_claim_scan_duration_ms")
        .record(elapsed.as_secs_f64() * 1000.0);
}

pub(crate) fn record_authoritative_scan(keys: usize, elapsed: Duration) {
    if keys > 0 {
        ::metrics::counter!("awaken_mailbox_authoritative_scan_keys_total").increment(keys as u64);
    }
    ::metrics::histogram!("awaken_mailbox_authoritative_scan_duration_ms")
        .record(elapsed.as_secs_f64() * 1000.0);
}

pub(crate) fn record_queued_without_signal_age(dispatch: &RunDispatch, now: u64) {
    ::metrics::histogram!("awaken_mailbox_queued_without_signal_age_ms")
        .record(now.saturating_sub(dispatch.available_at) as f64);
}

pub(crate) fn inc_dispatch_signal_republish() {
    ::metrics::counter!("awaken_mailbox_dispatch_signal_republish_total").increment(1);
}

pub(crate) fn record_claimed_dispatch_lease_age(now: u64, lease_until: u64) {
    ::metrics::histogram!("awaken_mailbox_claimed_dispatch_lease_age_ms")
        .record(now.saturating_sub(lease_until) as f64);
}

pub(crate) fn inc_expired_claim_reclaimed() {
    ::metrics::counter!("awaken_mailbox_expired_claim_reclaimed_total").increment(1);
}

pub(crate) fn inc_dedupe_lock_reconciled() {
    ::metrics::counter!("awaken_mailbox_dedupe_lock_reconciled_total").increment(1);
}

pub(crate) fn inc_dedupe_lock_conflict() {
    ::metrics::counter!("awaken_mailbox_dedupe_lock_conflict_total").increment(1);
}

pub(crate) fn inc_live_delivery(outcome: &Result<LiveDeliveryOutcome, impl std::fmt::Debug>) {
    let result = match outcome {
        Ok(LiveDeliveryOutcome::Delivered) => "delivered",
        Ok(LiveDeliveryOutcome::NoSubscriber) => "no_subscriber",
        Err(_) => "error",
    };
    ::metrics::counter!(
        "awaken_mailbox_live_delivery_total",
        "result" => result
    )
    .increment(1);
}

pub(crate) fn record_watcher_initial_scan(keys: usize, elapsed: Duration) {
    if keys > 0 {
        ::metrics::counter!("awaken_mailbox_index_rebuild_keys_total").increment(keys as u64);
    }
    ::metrics::histogram!("awaken_mailbox_index_rebuild_duration_ms")
        .record(elapsed.as_secs_f64() * 1000.0);
}
