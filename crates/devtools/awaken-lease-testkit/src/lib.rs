//! Deterministic [`WallClock`] doubles.
//!
//! These lived in production crates (`awaken-agent-contract`'s `ManualClock`,
//! the per-context `ManualWorkerClock` and fixed test clocks); a hand-driven
//! clock compiled into a shipped binary is one constructor away from becoming
//! the production clock. They now live only here, in a crate that test and
//! conformance code depend on and no shipped binary links.

use std::sync::atomic::{AtomicU64, Ordering};

use awaken_time::{UtcTime, WallClock};

/// The largest epoch millisecond a manual clock holds: the signed persisted
/// domain every store writes, so a test can never drive a reading a store
/// cannot persist.
const MAX_EPOCH_MS: u64 = i64::MAX as u64;

/// A hand-driven clock at millisecond resolution.
#[derive(Debug, Default)]
pub struct ManualClock {
    now_ms: AtomicU64,
}

impl ManualClock {
    #[must_use]
    pub fn new(start_ms: u64) -> Self {
        Self {
            now_ms: AtomicU64::new(start_ms.min(MAX_EPOCH_MS)),
        }
    }

    /// Set the current time.
    pub fn set(&self, now_ms: u64) {
        self.now_ms
            .store(now_ms.min(MAX_EPOCH_MS), Ordering::SeqCst);
    }

    /// Advance the current time by `delta_ms`, saturating at the persisted
    /// domain's maximum.
    pub fn advance(&self, delta_ms: u64) {
        let _ = self
            .now_ms
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |now| {
                Some(now.saturating_add(delta_ms).min(MAX_EPOCH_MS))
            });
    }
}

impl WallClock for ManualClock {
    fn now(&self) -> UtcTime {
        UtcTime::from_epoch_ms(self.now_ms.load(Ordering::SeqCst))
    }
}

/// A clock that always reads one instant, at full nanosecond precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedClock(UtcTime);

impl FixedClock {
    #[must_use]
    pub const fn at(reading: UtcTime) -> Self {
        Self(reading)
    }
}

impl WallClock for FixedClock {
    fn now(&self) -> UtcTime {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_time::WallClockExt;

    #[test]
    fn manual_clock_atomic_advance_is_exact_and_saturating() {
        // Cause/effect decision table: C1 current+delta fits the store domain;
        // C2 it exceeds (i64::MAX as u64); C3 multiple threads race advances.
        // E1 C1 adds exactly delta; E2 C2 saturates at the canonical maximum;
        // E3 C3 loses no successful update. R1/R2 cover the numeric boundary
        // and R3 covers CAS retry under contention.
        // R4 an out-of-domain starting value is clamped just like set/advance.
        assert_eq!(ManualClock::new(u64::MAX).now_ms(), i64::MAX as u64, "R4");
        let clock = ManualClock::new(10);
        clock.advance(7);
        assert_eq!(clock.now_ms(), 17, "R1");
        clock.set((i64::MAX as u64) - 1);
        clock.advance(2);
        assert_eq!(clock.now_ms(), (i64::MAX as u64), "R2");

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

    #[test]
    fn fixed_clock_keeps_nanosecond_precision() {
        let reading = UtcTime::from_unix_nanos(1_700_000_000_123_456_789);
        let clock = FixedClock::at(reading);
        assert_eq!(clock.now(), reading);
        assert_eq!(clock.now().as_unix_nanos(), 1_700_000_000_123_456_789);
        assert_eq!(clock.now_ms(), 1_700_000_000_123);
    }
}
