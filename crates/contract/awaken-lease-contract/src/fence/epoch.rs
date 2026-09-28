//! The monotonic lease generation.

use std::fmt;
use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

/// One monotonic generation of a lease: the coordinate a reassignment
/// advances and a renewal keeps.
///
/// An epoch is never zero. Zero is what an unset integer, a `#[serde(default)]`
/// field and a missing SQL column all decode to, so a zero epoch is exactly the
/// value that reads as "some lease" without any grant having minted it. There
/// is no `Default`, and decoding refuses `0` and a missing field alike.
/// [`Epoch::next`] is the only way to advance one, and it refuses to wrap.
///
/// Consumers: every [`super::Tenure`], and through it every [`super::Lease`]
/// and [`super::EffectFence`]; the work-store lease authority mints successors
/// with [`Epoch::next`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct Epoch(NonZeroU64);

impl Epoch {
    /// The generation of a lease's first grant.
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    /// The generation after this one, or `None` once the counter is exhausted.
    /// A wrapped epoch would compare lower than every generation it succeeds,
    /// so exhaustion is refused rather than saturated.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }

    /// Decode a persisted generation. `None` for zero, which no grant mints.
    #[must_use]
    pub const fn from_persisted(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(epoch) => Some(Self(epoch)),
            None => None,
        }
    }

    /// The persisted generation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for Epoch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A persisted epoch of zero: no grant ever minted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZeroEpoch;

impl fmt::Display for ZeroEpoch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a lease epoch is never zero")
    }
}

impl std::error::Error for ZeroEpoch {}

impl TryFrom<u64> for Epoch {
    type Error = ZeroEpoch;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::from_persisted(value).ok_or(ZeroEpoch)
    }
}

impl From<Epoch> for u64 {
    fn from(epoch: Epoch) -> Self {
        epoch.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct Holder {
        epoch: Epoch,
    }

    #[test]
    fn epoch_decoding_and_succession_decision_table() {
        // | Rule | input                 | effect                     |
        // | E1   | persisted 0           | refused (no grant mints 0) |
        // | E2   | persisted 7           | Epoch 7                    |
        // | E3   | FIRST.next()          | Epoch 2                    |
        // | E4   | u64::MAX .next()      | None (never wraps)         |
        // | E5   | JSON `0`              | decode error               |
        // | E6   | JSON field missing    | decode error               |
        // | E7   | JSON `7`              | round-trips as `7`         |
        assert_eq!(Epoch::from_persisted(0), None, "E1");
        assert_eq!(Epoch::try_from(0), Err(ZeroEpoch), "E1");
        assert_eq!(Epoch::from_persisted(7).map(Epoch::get), Some(7), "E2");
        assert_eq!(Epoch::FIRST.next().map(Epoch::get), Some(2), "E3");
        assert_eq!(Epoch::from_persisted(u64::MAX).unwrap().next(), None, "E4");
        assert!(serde_json::from_str::<Epoch>("0").is_err(), "E5");
        assert!(serde_json::from_str::<Holder>("{}").is_err(), "E6");
        let seven = serde_json::from_str::<Epoch>("7").expect("E7");
        assert_eq!(serde_json::to_string(&seven).unwrap(), "7", "E7");
        assert!(Epoch::FIRST < seven, "epochs order by generation");
    }
}
