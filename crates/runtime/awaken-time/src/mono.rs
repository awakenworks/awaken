//! The single monotonic read point.
//!
//! Wall time answers "when did this happen" and is persisted; monotonic time
//! answers "how long has this taken" and never is. Timeouts, retry pacing and
//! elapsed-duration metrics read [`now`] here instead of `Instant::now()`
//! wherever they are: it is tokio's clock, so a test that pauses its runtime
//! (`tokio::time::pause`) controls every deadline built on it, and outside a
//! runtime it reads the host's monotonic clock.

use std::time::Duration;

pub use tokio::time::Instant;

/// The current monotonic instant.
#[must_use]
#[expect(
    clippy::disallowed_methods,
    reason = "awaken-time owns the monotonic clock; this is its one read"
)]
pub fn now() -> Instant {
    Instant::now()
}

/// A process-local deadline for a wait or a timeout. Never persisted: a restart
/// gets a new monotonic epoch, which is why durable deadlines are
/// [`Expiry`](crate::Expiry) values instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonoDeadline(Instant);

impl MonoDeadline {
    /// The deadline at an exact instant, for a caller that derives it from a
    /// start it already sampled (an attempt window bounded by a request
    /// deadline). Unrepresentable arithmetic stays the caller's typed refusal.
    #[must_use]
    pub const fn at(instant: Instant) -> Self {
        Self(instant)
    }

    /// The deadline `timeout` from now. A timeout too large to represent is a
    /// deadline that never passes.
    #[must_use]
    pub fn after(timeout: Duration) -> Self {
        let start = now();
        Self(
            start
                .checked_add(timeout)
                .unwrap_or_else(|| start + Duration::from_secs(u64::from(u32::MAX))),
        )
    }

    #[must_use]
    pub const fn instant(self) -> Instant {
        self.0
    }

    /// Passed from the deadline instant on, the same boundary as
    /// [`Expiry`](crate::Expiry).
    #[must_use]
    pub fn has_passed(self) -> bool {
        now() >= self.0
    }

    /// Time left before the deadline; zero once it has passed.
    #[must_use]
    pub fn remaining(self) -> Duration {
        self.0.saturating_duration_since(now())
    }
}

/// Elapsed time since a start instant, for durations that are reported rather
/// than enforced (phase timings, metrics).
#[derive(Debug, Clone, Copy)]
pub struct Stopwatch(Instant);

impl Stopwatch {
    #[must_use]
    pub fn start() -> Self {
        Self(now())
    }

    #[must_use]
    pub fn elapsed(self) -> Duration {
        now().saturating_duration_since(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_paused_runtime_controls_every_monotonic_reading() {
        // Cause/effect decision table under a paused tokio clock.
        // | advanced by | deadline(100ms) passed | remaining | stopwatch |
        // | 0           | no                     | 100ms     | 0         |
        // | 99ms        | no                     | 1ms       | 99ms      |
        // | 100ms       | yes (boundary)         | 0         | 100ms     |
        // | 150ms       | yes                    | 0         | 150ms     |
        let deadline = MonoDeadline::after(Duration::from_millis(100));
        let stopwatch = Stopwatch::start();
        let mut advanced = 0;
        for (at, passed, remaining) in [
            (0, false, 100),
            (99, false, 1),
            (100, true, 0),
            (150, true, 0),
        ] {
            tokio::time::advance(Duration::from_millis(at - advanced)).await;
            advanced = at;
            assert_eq!(deadline.has_passed(), passed, "passed at {at}");
            assert_eq!(
                deadline.remaining(),
                Duration::from_millis(remaining),
                "remaining at {at}"
            );
            assert_eq!(
                stopwatch.elapsed(),
                Duration::from_millis(at),
                "elapsed at {at}"
            );
        }
        assert!(deadline.instant() <= now());
    }

    #[test]
    fn an_unrepresentable_timeout_never_passes() {
        let deadline = MonoDeadline::after(Duration::MAX);
        assert!(!deadline.has_passed());
        assert!(deadline.remaining() > Duration::from_secs(1));
    }
}
