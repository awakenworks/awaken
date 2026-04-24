//! Low-cardinality metrics for NATS buffered thread storage.

pub(crate) fn inc_poison_wal_quarantined(reason: &'static str) {
    ::metrics::counter!(
        "awaken_nats_buffered_poison_wal_total",
        "reason" => reason
    )
    .increment(1);
}

pub(crate) fn inc_poison_wal_quarantine_failure(reason: &'static str) {
    ::metrics::counter!(
        "awaken_nats_buffered_poison_wal_quarantine_failures_total",
        "reason" => reason
    )
    .increment(1);
}

pub(crate) fn inc_poison_wal_quarantine_dropped(reason: &'static str) {
    ::metrics::counter!(
        "awaken_nats_buffered_poison_wal_quarantine_drops_total",
        "reason" => reason
    )
    .increment(1);
}
