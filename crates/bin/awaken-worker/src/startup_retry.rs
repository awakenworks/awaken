//! Bounded exponential backoff for transient Worker startup failures.
//!
//! A Worker that cannot reach its Coordinator yet (connection refused, DNS
//! lookup failure, timeout, 429/5xx, slot still held by its predecessor) waits
//! and retries instead of exiting. The wait is bounded: once the budget is
//! spent the process exits non-zero, and its `restartPolicy: Never` Pod is
//! replaced by the Worker fleet controller with a fresh UID. Permanent causes
//! exit at once. The transport classifies each failure once
//! (`WorkerRegistrationError`); this module maps that class to an action.
//!
//! | Registration failure | Typical cause | Class | Action |
//! |---|---|---|---|
//! | `Unavailable` | connect refused, DNS lookup, timeout, 429, 5xx | transient | back off and retry |
//! | `SlotOccupied` | 409: predecessor incarnation still holds the slot | transient | back off and retry |
//! | `Rejected` | other 4xx, malformed request or receipt, credential defect | permanent | exit now |
//! | transient after the budget | Coordinator absent past the budget | exhausted | exit non-zero |

use awaken_coordinator_client::WorkerRegistrationError;
use std::time::Duration;

/// The action for one classified startup failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupFailureClass {
    Transient,
    Permanent,
}

pub(crate) fn classify_registration(error: &WorkerRegistrationError) -> StartupFailureClass {
    match error {
        WorkerRegistrationError::Unavailable(_) | WorkerRegistrationError::SlotOccupied(_) => {
            StartupFailureClass::Transient
        }
        WorkerRegistrationError::Rejected(_) => StartupFailureClass::Permanent,
    }
}

/// Exponential delays from `INITIAL` doubling to `MAX_DELAY`, with equal
/// jitter (each delay lies in `[base/2, base]`) so a fleet restarted together
/// does not retry in lockstep. At most `MAX_RETRIES` delays are granted.
pub(crate) struct StartupBackoff {
    retries: u32,
    seed: u64,
}

impl StartupBackoff {
    pub(crate) const INITIAL: Duration = Duration::from_millis(250);
    pub(crate) const MAX_DELAY: Duration = Duration::from_secs(30);
    /// About nine minutes of waiting in total before the process gives up.
    pub(crate) const MAX_RETRIES: u32 = 24;

    /// The jitter is derived from the incarnation identity: distinct Workers
    /// spread out, while one Worker's schedule is reproducible.
    pub(crate) fn new(incarnation_id: &str) -> Self {
        let seed = incarnation_id
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        Self { retries: 0, seed }
    }

    pub(crate) fn retries(&self) -> u32 {
        self.retries
    }

    /// The one action for a failed registration: the delay before the next
    /// attempt, or the message the process exits with (permanent cause, or a
    /// transient one past the budget; the Never-restart Pod is then replaced).
    pub(crate) fn delay_after(
        &mut self,
        error: &WorkerRegistrationError,
    ) -> Result<Duration, String> {
        if classify_registration(error) == StartupFailureClass::Permanent {
            return Err(error.to_string());
        }
        self.next_delay().ok_or_else(|| {
            format!(
                "worker registration still unavailable after {} retries: {error}",
                self.retries
            )
        })
    }

    /// The delay before the next attempt, or `None` once the budget is spent.
    pub(crate) fn next_delay(&mut self) -> Option<Duration> {
        if self.retries >= Self::MAX_RETRIES {
            return None;
        }
        let doubled = Self::INITIAL.saturating_mul(1_u32 << self.retries.min(16));
        let base = doubled.min(Self::MAX_DELAY);
        let half = base / 2;
        let spread = u64::try_from(half.as_millis()).unwrap_or(u64::MAX).max(1);
        let mix = self
            .seed
            .wrapping_add(u64::from(self.retries).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        self.retries += 1;
        Some(half + Duration::from_millis(mix % (spread + 1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_failures_are_classified_once_into_retry_or_exit() {
        // Decision table (module docs): U Unavailable and O SlotOccupied are
        // transient; R Rejected is permanent. Every variant of the transport's
        // own classification has exactly one row.
        for (error, expected) in [
            (
                WorkerRegistrationError::Unavailable(
                    "worker control transport (connect): failed to lookup address information"
                        .into(),
                ),
                StartupFailureClass::Transient,
            ),
            (
                WorkerRegistrationError::Unavailable(
                    "worker control transport (connect): Connection refused".into(),
                ),
                StartupFailureClass::Transient,
            ),
            (
                WorkerRegistrationError::SlotOccupied("predecessor holds the slot".into()),
                StartupFailureClass::Transient,
            ),
            (
                WorkerRegistrationError::Rejected("invalid Worker manifest".into()),
                StartupFailureClass::Permanent,
            ),
        ] {
            assert_eq!(classify_registration(&error), expected, "{error}");
        }
    }

    #[test]
    fn a_failed_registration_maps_to_one_retry_or_exit_action() {
        // A1 permanent => exit now with the transport's message, no retry spent;
        // A2 transient within budget => the next backoff delay;
        // A3 transient past the budget => exit naming the retry count.
        let mut backoff = StartupBackoff::new("uid-a");
        let rejected = WorkerRegistrationError::Rejected("invalid manifest".into());
        assert_eq!(
            backoff.delay_after(&rejected),
            Err("invalid manifest".into()),
            "A1"
        );
        assert_eq!(backoff.retries(), 0, "A1");
        let refused = WorkerRegistrationError::Unavailable("Connection refused".into());
        for _ in 0..StartupBackoff::MAX_RETRIES {
            assert!(backoff.delay_after(&refused).is_ok(), "A2");
        }
        let exhausted = backoff.delay_after(&refused).unwrap_err();
        assert!(exhausted.contains("after 24 retries"), "A3 {exhausted}");
    }

    #[test]
    fn backoff_is_exponential_jittered_capped_and_bounded() {
        // B1 each delay lies in [base/2, base] with base = 250ms * 2^n capped
        // at 30s; B2 the cap is reached and never exceeded; B3 exactly
        // MAX_RETRIES delays are granted, then None (exit); B4 the same
        // incarnation reproduces its schedule, distinct incarnations differ.
        let mut backoff = StartupBackoff::new("uid-a");
        let mut delays = Vec::new();
        while let Some(delay) = backoff.next_delay() {
            delays.push(delay);
        }
        assert_eq!(delays.len(), StartupBackoff::MAX_RETRIES as usize, "B3");
        assert_eq!(backoff.retries(), StartupBackoff::MAX_RETRIES, "B3");
        assert_eq!(backoff.next_delay(), None, "B3 stays exhausted");
        for (retry, delay) in delays.iter().enumerate() {
            let base =
                (StartupBackoff::INITIAL * (1 << retry.min(16))).min(StartupBackoff::MAX_DELAY);
            assert!(
                *delay >= base / 2 && *delay <= base,
                "B1 retry {retry}: {delay:?}"
            );
        }
        assert!(
            delays
                .iter()
                .any(|delay| *delay >= StartupBackoff::MAX_DELAY / 2),
            "B2 the cap is reached"
        );
        let total: Duration = delays.iter().sum();
        assert!(
            total >= Duration::from_secs(4 * 60) && total <= Duration::from_secs(10 * 60),
            "B3 bounded total wait {total:?}"
        );
        let replay: Vec<_> = {
            let mut again = StartupBackoff::new("uid-a");
            std::iter::from_fn(|| again.next_delay()).collect()
        };
        assert_eq!(replay, delays, "B4 reproducible");
        let other: Vec<_> = {
            let mut other = StartupBackoff::new("uid-b");
            std::iter::from_fn(|| other.next_delay()).collect()
        };
        assert_ne!(other, delays, "B4 distinct incarnations spread out");
    }
}
