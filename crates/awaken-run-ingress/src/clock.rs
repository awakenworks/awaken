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
        self.now.store(now_ms, Ordering::SeqCst);
    }

    /// Advance the current time by `delta_ms`.
    pub fn advance(&self, delta_ms: u64) {
        self.now.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
}
