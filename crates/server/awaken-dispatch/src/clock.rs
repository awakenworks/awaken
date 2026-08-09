//! The clock at the edge of the durable host.
//!
//! The worker stays clock-free: every method that needs "now" takes it as a
//! parameter, so the core is deterministic and replayable. Only the long-running
//! [`DispatchService`](crate::DispatchService) reads a real clock, through this
//! port — so a test can drive time by hand with [`ManualClock`] while production
//! uses [`SystemClock`].

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
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |now| {
                Some(deadline_millis(now, delta_ms))
            });
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
}
