//! The wall-clock port. Owners sample once at their durable boundary; pure
//! reducers receive values and replay never consults a clock.

use std::sync::Arc;
use std::time::SystemTime;

use crate::UtcTime;

/// The only source of wall-clock time in the workspace.
///
/// An owner that stamps a record, mints an [`Expiry`](crate::Expiry) or judges
/// one takes an `Arc<dyn WallClock>` at construction. Two owners of one
/// aggregate must hold the same handle: a record whose `updated_at` and
/// `erased_at` came from two clocks disagrees with itself under test.
pub trait WallClock: Send + Sync {
    /// This clock's current reading.
    fn now(&self) -> UtcTime;
}

/// The spellings of a reading that durable owners write.
///
/// Implemented once, for every [`WallClock`], so no clock can render a reading
/// differently from what its [`WallClock::now`] returned: an owner that stamps
/// `observed_at()` and an owner that does lease arithmetic with `now_ms()` from
/// the same clock cannot disagree.
pub trait WallClockExt {
    /// [`UtcTime::as_epoch_ms`] of the current reading.
    fn now_ms(&self) -> u64;

    /// [`UtcTime::to_rfc3339`] of the current reading: the instant a durable
    /// record carries in an observation field (`created_at`, `started_at`, ...).
    fn observed_at(&self) -> String;
}

impl<C: WallClock + ?Sized> WallClockExt for C {
    fn now_ms(&self) -> u64 {
        self.now().as_epoch_ms()
    }

    fn observed_at(&self) -> String {
        self.now().to_rfc3339()
    }
}

/// The host's real clock. Private: the only way to obtain it is
/// [`system_clock`], which yields a handle, so no code can ask a concrete type
/// for the time and bypass injection.
struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now(&self) -> UtcTime {
        UtcTime::from_system_time(SystemTime::now())
    }
}

/// The real clock as an injectable handle.
///
/// A composition root calls this once and passes the handle down; nothing below
/// the root reaches for the system clock, which is what keeps every `*_at` a
/// value a test can name.
#[must_use]
pub fn system_clock() -> Arc<dyn WallClock> {
    Arc::new(SystemWallClock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Frozen(UtcTime);

    impl WallClock for Frozen {
        fn now(&self) -> UtcTime {
            self.0
        }
    }

    #[test]
    fn every_spelling_is_a_projection_of_the_one_reading() {
        // R1 now_ms is the reading's epoch milliseconds; R2 observed_at is its
        // RFC 3339 spelling; R3 both hold through a trait object, which is how
        // owners hold the port.
        let reading = UtcTime::from_epoch_ms(1_709_164_800_007);
        let clock: Arc<dyn WallClock> = Arc::new(Frozen(reading));
        assert_eq!(clock.now_ms(), 1_709_164_800_007, "R1");
        assert_eq!(clock.observed_at(), "2024-02-29T00:00:00.007Z", "R2");
        assert_eq!(Frozen(reading).now_ms(), clock.now_ms(), "R3");
    }

    #[test]
    fn the_system_clock_reads_the_host_after_the_epoch() {
        // The system handle reports a post-epoch reading that does not run
        // backwards across two consecutive samples by more than host slew.
        let clock = system_clock();
        let first = clock.now();
        let second = clock.now();
        assert!(first > UtcTime::UNIX_EPOCH);
        assert!(second.saturating_add(Duration::from_secs(1)) >= first);
    }
}
