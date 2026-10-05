//! The one persisted-expiry law and its dual.

use std::time::Duration;

use crate::UtcTime;

/// A persisted deadline: the instant from which a grant, lease or credential no
/// longer holds.
///
/// **Law: an expiry is live while `now < expiry` and expired from
/// `now >= expiry` on.** The instant *at* the deadline already belongs to
/// whoever may take over, so a holder and a successor evaluating the same
/// reading can never both hold. Every judgement goes through
/// [`Expiry::is_live_at`] / [`Expiry::is_expired_at`]; a caller that compares
/// raw milliseconds is where the two boundary conventions this type replaces
/// (`deadline < now` beside `expires <= now`) came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Expiry(UtcTime);

impl Expiry {
    #[must_use]
    pub const fn at(deadline: UtcTime) -> Self {
        Self(deadline)
    }

    /// The deadline `ttl` after `now`, saturating rather than wrapping.
    #[must_use]
    pub const fn after(now: UtcTime, ttl: Duration) -> Self {
        Self(now.saturating_add(ttl))
    }

    /// The deadline at an unsigned epoch millisecond, the unit the dispatch
    /// stores and lease records carry.
    #[must_use]
    pub const fn from_epoch_ms(epoch_ms: u64) -> Self {
        Self(UtcTime::from_epoch_ms(epoch_ms))
    }

    /// Decode a persisted epoch millisecond. A negative value decodes to the
    /// epoch and is therefore already expired (the crate's pre-epoch rule).
    #[must_use]
    pub fn from_persisted_ms(epoch_ms: i64) -> Self {
        Self(UtcTime::from_epoch_ms_i64(epoch_ms))
    }

    /// The persisted epoch millisecond, clamped to `i64::MAX` so the SQL column
    /// never receives a wrapped negative deadline.
    #[must_use]
    pub fn to_persisted_ms(self) -> i64 {
        self.0.as_epoch_ms_i64()
    }

    /// The unsigned epoch millisecond: the exact inverse of
    /// [`Expiry::from_epoch_ms`] for the `u64` expiry fence and lease wires
    /// carry. Unlike [`Expiry::to_persisted_ms`] and `UtcTime::as_epoch_ms`
    /// (both clamped to the signed SQL domain), every value decoded by
    /// [`Expiry::from_epoch_ms`] round-trips exactly, including `u64::MAX`.
    #[must_use]
    pub fn to_epoch_ms(self) -> u64 {
        u64::try_from(self.0.duration_since_epoch().as_millis()).unwrap_or(u64::MAX)
    }

    #[must_use]
    pub const fn deadline(self) -> UtcTime {
        self.0
    }

    /// `now < expiry`. Const so that fixed-width kernels (and their proofs)
    /// judge through this one rule rather than restating it.
    #[must_use]
    pub const fn is_live_at(self, now: UtcTime) -> bool {
        now.as_unix_nanos() < self.0.as_unix_nanos()
    }

    #[must_use]
    pub const fn is_expired_at(self, now: UtcTime) -> bool {
        !self.is_live_at(now)
    }

    /// The time left at `now`, or `None` once expired. A live expiry always has
    /// a non-zero remainder, so `Some` is exactly "still held".
    #[must_use]
    pub fn remaining_at(self, now: UtcTime) -> Option<Duration> {
        self.is_live_at(now)
            .then_some(self.0.saturating_duration_since(now))
    }

    /// A child grant minted at `now` for `ttl`, never outliving `parent`:
    /// `min(now + ttl, parent)`. `None` when that child would already be expired
    /// at `now` (the parent is dead, or `ttl` is zero), so a dead parent can
    /// never mint a live child.
    #[must_use]
    pub fn child_within(parent: Self, now: UtcTime, ttl: Duration) -> Option<Self> {
        Self::after(now, ttl).within(parent, now)
    }

    /// This deadline narrowed so it never outlives `parent`: `min(self,
    /// parent)`. `None` when the narrowed deadline is already expired at
    /// `now`, so a dead parent (or a dead proposal) never yields a live grant.
    /// The absolute-deadline form of [`Expiry::child_within`], for a child
    /// whose requested deadline arrives as a persisted instant.
    #[must_use]
    pub fn within(self, parent: Self, now: UtcTime) -> Option<Self> {
        let child = self.min(parent);
        child.is_live_at(now).then_some(child)
    }
}

/// The earliest instant at which a deferred action becomes due.
///
/// The dual of [`Expiry`] under the same boundary: due from `now >= not_before`
/// on, so the instant an expiry ends is the instant a successor scheduled at
/// that deadline becomes due, with no gap and no overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotBefore(UtcTime);

impl NotBefore {
    #[must_use]
    pub const fn at(instant: UtcTime) -> Self {
        Self(instant)
    }

    #[must_use]
    pub const fn after(now: UtcTime, delay: Duration) -> Self {
        Self(now.saturating_add(delay))
    }

    #[must_use]
    pub const fn instant(self) -> UtcTime {
        self.0
    }

    #[must_use]
    pub fn is_due_at(self, now: UtcTime) -> bool {
        now >= self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> UtcTime {
        UtcTime::from_epoch_ms(value)
    }

    #[test]
    fn the_boundary_instant_belongs_to_the_successor() {
        // Cause/effect decision table over one expiry at t=1000.
        // | now  | live | expired | remaining | a NotBefore(1000) is due |
        // | 999  | yes  | no      | Some(1ms) | no                       |
        // | 1000 | no   | yes     | None      | yes                      |
        // | 1001 | no   | yes     | None      | yes                      |
        // The row at 1000 is the law: holder and successor never overlap and
        // never leave a gap.
        let expiry = Expiry::at(ms(1_000));
        let successor = NotBefore::at(expiry.deadline());
        for (now, live, remaining, due) in [
            (999, true, Some(Duration::from_millis(1)), false),
            (1_000, false, None, true),
            (1_001, false, None, true),
        ] {
            assert_eq!(expiry.is_live_at(ms(now)), live, "live at {now}");
            assert_eq!(expiry.is_expired_at(ms(now)), !live, "expired at {now}");
            assert_eq!(
                expiry.remaining_at(ms(now)),
                remaining,
                "remaining at {now}"
            );
            assert_eq!(successor.is_due_at(ms(now)), due, "due at {now}");
            assert_ne!(expiry.is_live_at(ms(now)), successor.is_due_at(ms(now)));
        }
    }

    #[test]
    fn persisted_millis_round_trip_and_clamp() {
        // R1 an ordinary deadline round-trips; R2 a deadline beyond i64
        // persists as i64::MAX; R3 a negative persisted value decodes to the
        // epoch and is expired at every reading; R4 `after` saturates.
        let expiry = Expiry::after(ms(1_000), Duration::from_millis(500));
        assert_eq!(expiry.to_persisted_ms(), 1_500, "R1");
        assert_eq!(Expiry::from_persisted_ms(1_500), expiry, "R1");
        let far = Expiry::after(ms(u64::MAX), Duration::from_millis(1));
        assert_eq!(far.to_persisted_ms(), i64::MAX, "R2");
        let negative = Expiry::from_persisted_ms(-5);
        assert_eq!(negative.deadline(), UtcTime::UNIX_EPOCH, "R3");
        assert!(negative.is_expired_at(UtcTime::UNIX_EPOCH), "R3");
        let top = UtcTime::from_duration_since_epoch(Duration::MAX);
        assert_eq!(
            Expiry::after(top, Duration::from_secs(1)).deadline(),
            top,
            "R4"
        );
        assert_eq!(
            NotBefore::after(ms(10), Duration::from_millis(5)).instant(),
            ms(15)
        );
        // R5 the unsigned wire domain round-trips exactly, including u64::MAX,
        // where the signed SQL domain clamps.
        for wire in [0, 1_500, i64::MAX as u64, u64::MAX] {
            assert_eq!(Expiry::from_epoch_ms(wire).to_epoch_ms(), wire, "R5 {wire}");
        }
    }

    #[test]
    fn a_child_never_outlives_its_parent() {
        // Cause/effect decision table, parent expiring at t=1000.
        // | now  | ttl  | child                     |
        // | 100  | 200  | Some(300)  (own ttl binds) |
        // | 900  | 500  | Some(1000) (parent binds)  |
        // | 100  | 0    | None       (born expired)  |
        // | 1000 | 50   | None       (parent dead)   |
        // | 1200 | 50   | None       (parent dead)   |
        let parent = Expiry::at(ms(1_000));
        for (now, ttl, child) in [
            (100, 200, Some(300)),
            (900, 500, Some(1_000)),
            (100, 0, None),
            (1_000, 50, None),
            (1_200, 50, None),
        ] {
            let minted = Expiry::child_within(parent, ms(now), Duration::from_millis(ttl));
            assert_eq!(
                minted,
                child.map(|at| Expiry::at(ms(at))),
                "now {now} ttl {ttl}"
            );
            if let Some(minted) = minted {
                assert!(minted <= parent && minted.is_live_at(ms(now)));
            }
        }
        // The absolute form narrows a requested deadline the same way.
        // | requested | now  | within(parent 1000) |
        // | 800       | 100  | Some(800)           |
        // | 1500      | 100  | Some(1000)          |
        // | 1500      | 1000 | None (parent dead)  |
        // | 50        | 100  | None (request dead) |
        for (requested, now, narrowed) in [
            (800, 100, Some(800)),
            (1_500, 100, Some(1_000)),
            (1_500, 1_000, None),
            (50, 100, None),
        ] {
            assert_eq!(
                Expiry::at(ms(requested)).within(parent, ms(now)),
                narrowed.map(|at| Expiry::at(ms(at))),
                "requested {requested} now {now}"
            );
        }
    }
}
