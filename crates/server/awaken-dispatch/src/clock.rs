// @formal entrypoint run_ingress.a_deadline_passes_once_it_is_reached
// @formal entrypoint run_ingress.store_deadline_is_monotonic_saturating_and_never_wraps
// @formal entrypoint session_run.reservation_ttl_uses_store_clock
//! The clock at the edge of the durable host.
//!
//! The worker stays clock-free: an edge passes one clock source into each drive,
//! so everything the attempt itself times — credential-binding expiry and the
//! due time of a committed ScheduledAction — shares one timeline. Queue claims,
//! lease renewal, ownership checks and settlement fencing are NOT among them:
//! the store that owns the row judges those against its own clock, so no caller
//! can disagree with it. Long-running services and pools own the drive source;
//! every source is an `awaken_time::WallClock` handle, which direct
//! tests build from `awaken-lease-testkit` and production from
//! `awaken_time::system_clock()`.

/// Largest epoch-millisecond value representable by the SQL backends. The
/// public contract is `u64`; all stores normalize through this one boundary so
/// SQLite/Postgres never turn a far-future value into a negative integer and the
/// memory backend observes the same due/lease semantics.
#[cfg(any(feature = "durable", test, kani))]
pub(crate) const MAX_STORE_MILLIS: u64 = i64::MAX as u64;

#[must_use]
#[cfg(any(feature = "durable", test, kani))]
pub(crate) fn normalize_millis(value: u64) -> u64 {
    value.min(MAX_STORE_MILLIS)
}

#[must_use]
#[cfg(feature = "durable")]
pub(crate) fn db_millis(value: u64) -> i64 {
    normalize_millis(value) as i64
}

/// Decode a persisted millisecond value without allowing a legacy negative
/// integer to wrap into a far-future `u64` deadline.
#[cfg(feature = "durable")]
pub(crate) fn millis_from_db(value: i64) -> Result<u64, &'static str> {
    u64::try_from(value).map_err(|_| "persisted millisecond value is negative")
}

/// The judging half of the deadline rule, owned by the contract.
///
/// [`deadline_millis`] mints a deadline here because clamping it into the SQL
/// domain is a storage concern; deciding whether it has passed is not, so that
/// half lives with the transition rules every backend already shares. Before it
/// was named, the same comparison was spelled four times in Rust --- twice
/// normalizing `now` and twice not --- plus once per SQL query.
#[cfg(any(feature = "durable", test, kani))]
pub(crate) use awaken_dispatch_contract::deadline_has_passed;

#[must_use]
#[cfg(any(feature = "durable", test, kani))]
pub(crate) fn deadline_millis(now_ms: u64, lease_ms: u64) -> u64 {
    normalize_millis(now_ms.saturating_add(lease_ms))
}

#[cfg(kani)]
/// @obligation run_ingress.store_deadline_is_monotonic_saturating_and_never_wraps
/// @obligation session_run.reservation_ttl_uses_store_clock
/// @kani default-features=false
#[kani::proof]
fn store_deadline_is_monotonic_saturating_and_never_wraps() {
    // Cause/effect decision table: C1 now is inside/outside the SQL domain; C2
    // now+lease fits/overflows u64 or the SQL domain. E1 always lies in
    // 0..=MAX_STORE_MILLIS; E2 never precedes normalized now; E3 equals the
    // mathematical sum exactly when it fits both bounds. Arbitrary u64 inputs
    // exhaust all C1/C2 partitions, including both overflow boundaries.
    let now_ms: u64 = kani::any();
    let lease_ms: u64 = kani::any();
    let deadline = deadline_millis(now_ms, lease_ms);
    assert!(deadline <= MAX_STORE_MILLIS);
    assert!(deadline >= normalize_millis(now_ms));
    if now_ms <= MAX_STORE_MILLIS && lease_ms <= MAX_STORE_MILLIS - now_ms {
        assert_eq!(deadline, now_ms + lease_ms);
    }
}
