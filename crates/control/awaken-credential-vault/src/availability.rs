//! Credential availability: a time-based cooldown ledger over credential sources.
//!
//! When a provider answers a rate/quota signal (a `Disposition::Quota` in
//! `awaken-runtime-contract`), the caller cools the *identity that hit it* until a
//! deadline. A later selection skips a cooled source and rotates to another pool
//! member — this is the mid-run credential rotation the engine's candidate loop
//! rides. Auto-resume is pure time: past `retry_at_ms` a cooled source reads as
//! available again, with no timer.
//!
//! Neutral by construction: the ledger names no `Disposition` and no clock — the
//! caller maps a failure to a deadline and passes `now_ms` in. Aligns with
//! awaken-next's `AvailabilityState` (five states trimmed to the three P1 needs).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::CredentialSourceId;

/// The availability of one credential source at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvailabilityState {
    /// Selectable now.
    Available,
    /// Cooled by a transient quota/rate signal until `retry_at_ms`; selectable
    /// again at or after that wall-clock deadline (auto-resume, no timer).
    CooledDown { retry_at_ms: u64 },
    /// Cooled by a hard exhaustion with no known reset — excluded until explicitly
    /// [`cleared`](AvailabilityLedger::clear) (e.g. a fresh availability check
    /// passes).
    Exhausted,
}

impl AvailabilityState {
    /// Whether a selector may pick this source.
    #[must_use]
    pub fn is_available(self) -> bool {
        matches!(self, AvailabilityState::Available)
    }
}

#[derive(Debug, Clone, Copy)]
enum Entry {
    Cooled { retry_at_ms: u64 },
    Exhausted,
}

/// A concurrent ledger of credential-source cooldowns. Cheap to share; the only
/// mutable state is the cooldown map. Absent sources are [`Available`].
///
/// [`Available`]: AvailabilityState::Available
#[derive(Default)]
pub struct AvailabilityLedger {
    entries: Mutex<HashMap<String, Entry>>,
}

impl AvailabilityLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().expect("availability ledger poisoned")
    }

    /// Cool a source until `retry_at_ms`. A later cool-down for the same source
    /// wins (the freshest signal sets the deadline).
    pub fn cool_down(&self, id: &CredentialSourceId, retry_at_ms: u64) {
        self.lock()
            .insert(id.0.clone(), Entry::Cooled { retry_at_ms });
    }

    /// Mark a source hard-exhausted (no known reset) until explicitly cleared.
    pub fn exhaust(&self, id: &CredentialSourceId) {
        self.lock().insert(id.0.clone(), Entry::Exhausted);
    }

    /// Clear any cooldown/exhaustion — the source is available again now.
    pub fn clear(&self, id: &CredentialSourceId) {
        self.lock().remove(&id.0);
    }

    /// The state of `id` at `now_ms`. A cooled entry whose deadline has passed
    /// reads as [`Available`](AvailabilityState::Available) (auto-resume); the entry
    /// is left in place (a later cool-down overwrites it), so this is a pure read.
    #[must_use]
    pub fn state(&self, id: &CredentialSourceId, now_ms: u64) -> AvailabilityState {
        match self.lock().get(&id.0) {
            None => AvailabilityState::Available,
            Some(Entry::Exhausted) => AvailabilityState::Exhausted,
            Some(Entry::Cooled { retry_at_ms }) => {
                if now_ms >= *retry_at_ms {
                    AvailabilityState::Available
                } else {
                    AvailabilityState::CooledDown {
                        retry_at_ms: *retry_at_ms,
                    }
                }
            }
        }
    }

    /// Whether `id` may be selected at `now_ms`.
    #[must_use]
    pub fn is_available(&self, id: &CredentialSourceId, now_ms: u64) -> bool {
        self.state(id, now_ms).is_available()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> CredentialSourceId {
        CredentialSourceId(s.into())
    }

    #[test]
    fn absent_source_is_available() {
        let l = AvailabilityLedger::new();
        assert_eq!(l.state(&id("a"), 0), AvailabilityState::Available);
        assert!(l.is_available(&id("a"), 0));
    }

    #[test]
    fn cooled_source_is_unavailable_until_the_deadline_then_auto_resumes() {
        let l = AvailabilityLedger::new();
        l.cool_down(&id("a"), 1_000);
        assert_eq!(
            l.state(&id("a"), 500),
            AvailabilityState::CooledDown { retry_at_ms: 1_000 }
        );
        assert!(!l.is_available(&id("a"), 500));
        // At and past the deadline it is available again, no timer.
        assert!(l.is_available(&id("a"), 1_000));
        assert!(l.is_available(&id("a"), 5_000));
    }

    #[test]
    fn exhausted_stays_unavailable_until_cleared() {
        let l = AvailabilityLedger::new();
        l.exhaust(&id("a"));
        assert_eq!(l.state(&id("a"), u64::MAX), AvailabilityState::Exhausted);
        l.clear(&id("a"));
        assert!(l.is_available(&id("a"), 0));
    }

    #[test]
    fn a_fresher_cooldown_overwrites_the_deadline() {
        let l = AvailabilityLedger::new();
        l.cool_down(&id("a"), 1_000);
        l.cool_down(&id("a"), 3_000);
        assert!(!l.is_available(&id("a"), 2_000));
        assert!(l.is_available(&id("a"), 3_000));
    }
}
