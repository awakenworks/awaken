// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The shared clock port supplies durable owners with wall-time observations; ManualClock
// @formal reason: provides deterministic atomic tests. Time neither owns state transitions nor orders
// @formal reason: replay. This is a product-boundary classification, not a direct proof of wall-clock
// @formal reason: behavior.
//! Shared wall-clock port. Owners sample once at their durable boundary;
//! pure reducers receive values and replay never consults this clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub mod utc;

/// A source of wall-clock time in epoch milliseconds.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;

    /// This clock's current reading, spelled as the RFC-3339 UTC instant a
    /// durable record carries in an observation field (`created_at`,
    /// `started_at`, `archived_at`, ...).
    ///
    /// The rendering is a total function of [`Self::now_ms`], so an owner that
    /// stamps a record and an owner that does lease arithmetic from the same
    /// reading cannot disagree. Overriding it can only change the spelling, not
    /// the fact: there is no weaker sibling for an implementation to fall back
    /// to, which is why this default is a rendering and not a degrade.
    fn observed_at(&self) -> String {
        utc::epoch_millis_to_rfc3339(self.now_ms())
    }
}

/// The real system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis().min(i64::MAX as u128) as u64)
            .unwrap_or(0)
    }
}

/// The real system clock as an injectable handle.
///
/// A composition root calls this once per store it builds; nothing below the
/// root reaches for the system clock, which is what keeps every `*_at` a value
/// a test can name.
#[must_use]
pub fn system_clock() -> std::sync::Arc<dyn Clock> {
    std::sync::Arc::new(SystemClock)
}

/// A hand-driven clock for deterministic tests.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    pub fn new(start_ms: u64) -> Self {
        Self {
            now: AtomicU64::new(start_ms.min(i64::MAX as u64)),
        }
    }

    /// Set the current time.
    pub fn set(&self, now_ms: u64) {
        self.now
            .store(now_ms.min(i64::MAX as u64), Ordering::SeqCst);
    }

    /// Advance the current time by `delta_ms`.
    pub fn advance(&self, delta_ms: u64) {
        let _ = self
            .now
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |now| {
                Some(now.saturating_add(delta_ms).min(i64::MAX as u64))
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
}
