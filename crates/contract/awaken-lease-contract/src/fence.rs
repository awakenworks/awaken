// @formal entrypoint sandbox.effect_fence_succession_is_renewal_or_epoch_advance
//! The one ownership-fence kernel.
//!
//! [`FenceToken`] names exactly the coordinates and the successor law that a
//! realization lease and every effect fence projected from it share: owner,
//! runtime incarnation, monotonic epoch and renewable expiry. Operation
//! identity is deliberately outside the trait; the typed effect that owns a
//! transition compares it.
//!
//! The kernel lives beside the clock and the expiry law so that every contract
//! that fences an effect takes both from one depth-0 leaf (see the crate
//! documentation for why the leaf is shared).

/// How a successor fence relates to the fence it takes over from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceSuccession {
    /// The exact owner/incarnation/epoch lease, with an equal or later expiry.
    Renewal,
    /// The aggregate advanced its monotonic epoch and reassigned the lease.
    Reassignment,
}

/// The one successor law over realization-lease coordinates. It is a
/// representation-free kernel so the bounded proof exercises the same code the
/// provided [`FenceToken`] methods run: a strictly higher epoch reassigns;
/// otherwise only the exact lease may renew, and only without shortening expiry.
#[must_use]
pub const fn fence_succession(
    predecessor_epoch: u64,
    predecessor_expires_at_unix_ms: u64,
    successor_epoch: u64,
    successor_expires_at_unix_ms: u64,
    same_realization_lease: bool,
) -> Option<FenceSuccession> {
    if successor_epoch > predecessor_epoch {
        Some(FenceSuccession::Reassignment)
    } else if same_realization_lease
        && successor_expires_at_unix_ms >= predecessor_expires_at_unix_ms
    {
        Some(FenceSuccession::Renewal)
    } else {
        None
    }
}

/// Coordinates of one aggregate-owned realization lease as projected onto a
/// physical effect. Implementors expose the coordinates only; the successor law
/// is provided once here and must not be restated by callers.
pub trait FenceToken {
    fn owner(&self) -> &str;
    fn runtime_incarnation(&self) -> &str;
    fn epoch(&self) -> u64;
    fn expires_at_unix_ms(&self) -> u64;

    /// Compare the realization lease independently of any operation. A
    /// successor effect may reuse process-local state only while all three
    /// lease coordinates remain exact.
    #[must_use]
    fn same_realization_lease(&self, other: &Self) -> bool
    where
        Self: Sized,
    {
        self.owner() == other.owner()
            && self.runtime_incarnation() == other.runtime_incarnation()
            && self.epoch() == other.epoch()
    }

    /// Classify how `successor` takes over from this fence, or `None` when it
    /// may not: a reassignment must advance the epoch, and a renewal must keep
    /// the exact lease without shortening expiry.
    #[must_use]
    fn succession(&self, successor: &Self) -> Option<FenceSuccession>
    where
        Self: Sized,
    {
        fence_succession(
            self.epoch(),
            self.expires_at_unix_ms(),
            successor.epoch(),
            successor.expires_at_unix_ms(),
            self.same_realization_lease(successor),
        )
    }

    /// Whether an aggregate-authorized successor may take over a participant
    /// created under this fence. Operation identity is checked separately by
    /// the typed effect that owns the transition.
    #[must_use]
    fn authorizes_successor(&self, successor: &Self) -> bool
    where
        Self: Sized,
    {
        self.succession(successor).is_some()
    }

    /// Whether this token still authorizes anything at `now_unix_ms`. Expiry
    /// is exclusive: a token expiring at `t` is dead at `t`.
    #[must_use]
    fn is_live_at(&self, now_unix_ms: u64) -> bool {
        fence_is_live_at(self.expires_at_unix_ms(), now_unix_ms)
    }
}

/// Liveness law shared by every fence token: live strictly before expiry.
#[must_use]
pub const fn fence_is_live_at(expires_at_unix_ms: u64, now_unix_ms: u64) -> bool {
    expires_at_unix_ms > now_unix_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_kernel_is_strictly_before_expiry() {
        // C1 now before/at/after expiry. R1 live only strictly before.
        for (now, live) in [
            (0, true),
            (99, true),
            (100, false),
            (101, false),
            (u64::MAX, false),
        ] {
            assert_eq!(fence_is_live_at(100, now), live, "kernel now={now}");
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// @obligation sandbox.effect_fence_succession_is_renewal_or_epoch_advance
    #[kani::proof]
    fn fence_succession_is_exactly_renewal_or_epoch_advance() {
        let predecessor_epoch: u64 = kani::any();
        let predecessor_expiry: u64 = kani::any();
        let successor_epoch: u64 = kani::any();
        let successor_expiry: u64 = kani::any();
        // `same_realization_lease` is derived exactly as production derives
        // it (`FenceToken::same_realization_lease`): same owner, same runtime
        // incarnation, and the same epoch.
        let same_owner: bool = kani::any();
        let same_runtime_incarnation: bool = kani::any();
        let same_lease =
            same_owner && same_runtime_incarnation && successor_epoch == predecessor_epoch;
        let succession = fence_succession(
            predecessor_epoch,
            predecessor_expiry,
            successor_epoch,
            successor_expiry,
            same_lease,
        );
        match succession {
            Some(FenceSuccession::Reassignment) => {
                assert!(successor_epoch > predecessor_epoch);
            }
            Some(FenceSuccession::Renewal) => {
                assert!(same_lease);
                assert!(successor_epoch == predecessor_epoch);
                assert!(successor_expiry >= predecessor_expiry);
            }
            None => {
                assert!(successor_epoch <= predecessor_epoch);
                assert!(!same_lease || successor_expiry < predecessor_expiry);
            }
        }
        if successor_epoch < predecessor_epoch {
            assert!(succession.is_none());
        }
    }
}
