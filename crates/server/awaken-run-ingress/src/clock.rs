//! The clock at the edge of the durable host.
//!
//! The worker stays clock-free: an edge passes one clock source into each drive,
//! so queue claims, lease renewal, pre-effect ownership checks, and settlement
//! fencing share a timeline. Long-running services and pools own that source;
//! direct tests pass [`ManualClock`] while production uses [`SystemClock`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A source of wall-clock time in epoch milliseconds.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// The real system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Store-owned wall-clock time for durable authority facts.
///
/// Keeping this edge clock here prevents each storage backend from inventing a
/// subtly different timestamp implementation.
#[cfg(feature = "durable")]
pub(crate) fn system_now_ms() -> u64 {
    SystemClock.now_ms()
}

/// Largest epoch-millisecond value representable by the SQL backends. The
/// public contract is `u64`; all stores normalize through this one boundary so
/// SQLite/Postgres never turn a far-future value into a negative integer and the
/// memory backend observes the same due/lease semantics.
pub(crate) const MAX_STORE_MILLIS: u64 = i64::MAX as u64;

#[must_use]
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

#[must_use]
pub(crate) fn deadline_millis(now_ms: u64, lease_ms: u64) -> u64 {
    normalize_millis(now_ms.saturating_add(lease_ms))
}

#[cfg(kani)]
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

/// A hand-driven clock for deterministic tests.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    pub fn new(start_ms: u64) -> Self {
        Self {
            now: AtomicU64::new(start_ms),
        }
    }

    /// Set the current time.
    pub fn set(&self, now_ms: u64) {
        self.now.store(normalize_millis(now_ms), Ordering::SeqCst);
    }

    /// Advance the current time by `delta_ms`.
    pub fn advance(&self, delta_ms: u64) {
        let _ = self
            .now
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |now| {
                Some(deadline_millis(now, delta_ms))
            });
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_atomic_advance_is_exact_and_saturating() {
        // Cause/effect decision table: C1 current+delta fits the store domain;
        // C2 it exceeds MAX_STORE_MILLIS; C3 multiple threads race advances.
        // E1 C1 adds exactly delta; E2 C2 saturates at the canonical maximum;
        // E3 C3 loses no successful update. R1/R2 cover the numeric boundary
        // and R3 covers CAS retry under contention.
        let clock = ManualClock::new(10);
        clock.advance(7);
        assert_eq!(clock.now_ms(), 17, "R1");
        clock.set(MAX_STORE_MILLIS - 1);
        clock.advance(2);
        assert_eq!(clock.now_ms(), MAX_STORE_MILLIS, "R2");

        let clock = std::sync::Arc::new(ManualClock::new(0));
        let threads = (0..4)
            .map(|_| {
                let clock = clock.clone();
                std::thread::spawn(move || {
                    for _ in 0..250 {
                        clock.advance(1);
                    }
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().expect("R3 clock thread");
        }
        assert_eq!(clock.now_ms(), 1_000, "R3");
    }
}
