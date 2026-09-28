// @formal entrypoint sandbox.effect_fence_succession_is_renewal_or_epoch_advance
//! The one ownership-fence kernel.
//!
//! A lease is a [`Tenure`] — the complete identity of one grant: *what* is held
//! (the subject), *who* holds it (the holder) and *which* grant it is (the
//! [`Epoch`]) — held until an [`Expiry`](crate::Expiry). Every successor
//! judgement goes through one law, [`fence_succession`]: another subject is
//! never a successor; a strictly higher epoch reassigns; only the exact tenure
//! renews, and only without shortening expiry. [`EffectFence`] projects a lease
//! onto one exact operation. [`SlotToken`] is the identity of one occupancy
//! of a process-local single-holder slot.
//!
//! [`FenceToken`] is the same law over the persisted coordinates of the
//! realization leases that predate [`Lease`] (the Session realization lease and
//! the Sandbox effect fence carried in provider evidence). Dispatch claims,
//! attempt fences and credential receipts have different successor rules and
//! do not implement it.
//!
//! The kernel lives beside the clock and the expiry law so that every contract
//! that fences an effect takes both from one depth-0 leaf (see the crate
//! documentation for why the leaf is shared).

mod effect;
mod epoch;
mod slot;
mod tenure;

pub use effect::EffectFence;
pub use epoch::{Epoch, ZeroEpoch};
pub use slot::SlotToken;
pub use tenure::{Lease, Tenure, TenureScope};

/// How a successor fence relates to the fence it takes over from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceSuccession {
    /// The exact owner/incarnation/epoch lease, with an equal or later expiry.
    Renewal,
    /// The aggregate advanced its monotonic epoch and reassigned the lease.
    Reassignment,
}

/// The one successor law. It is a representation-free kernel so the bounded
/// proof exercises the same code [`Lease::succession`] and the provided
/// [`FenceToken`] methods run: a successor for another subject is never a
/// succession; for the same subject a strictly higher epoch reassigns;
/// otherwise only the exact tenure may renew, and only without shortening
/// expiry.
#[must_use]
pub fn fence_succession<E: PartialOrd>(
    same_subject: bool,
    predecessor_epoch: u64,
    predecessor_expiry: E,
    successor_epoch: u64,
    successor_expiry: E,
    same_tenure: bool,
) -> Option<FenceSuccession> {
    if !same_subject {
        None
    } else if successor_epoch > predecessor_epoch {
        Some(FenceSuccession::Reassignment)
    } else if same_tenure && successor_expiry >= predecessor_expiry {
        Some(FenceSuccession::Renewal)
    } else {
        None
    }
}

/// Coordinates of one aggregate-owned realization lease as projected onto a
/// physical effect. Implementors expose the coordinates only; the successor law
/// is provided once here and must not be restated by callers.
///
/// A token carries no subject coordinate because its persisted wire has none:
/// the subject is the record the token lives in (one Session's realization
/// slot, one Sandbox's marker, labels or handle), and every comparison reads
/// both tokens from that one record. A family whose values can meet across
/// subjects must use [`Lease`], whose succession refuses a foreign subject.
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
        // Single-subject by construction (see the trait documentation).
        fence_succession(
            true,
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
    use crate::Expiry;

    struct Probe;

    impl TenureScope for Probe {
        type Subject = u8;
        type Holder = u8;
    }

    fn any_lease() -> (u8, u8, u64, u64, Lease<Probe>) {
        let subject: u8 = kani::any();
        let holder: u8 = kani::any();
        let epoch: u64 = kani::any();
        kani::assume(epoch != 0);
        let expiry: u64 = kani::any();
        let lease = Lease::new(
            Tenure::granted(subject, holder, Epoch::from_persisted(epoch).unwrap()),
            Expiry::from_unix_ms(expiry),
        );
        (subject, holder, epoch, expiry, lease)
    }

    /// The succession law as production derives its inputs: `Lease::succession`
    /// compares the subjects, the complete tenures and the expiries itself, so
    /// the harness feeds it symbolic coordinates, never a free "same" bit.
    /// @obligation sandbox.effect_fence_succession_is_renewal_or_epoch_advance
    #[kani::proof]
    fn fence_succession_is_exactly_renewal_or_epoch_advance() {
        let (subject, holder, epoch, expiry, predecessor) = any_lease();
        let (next_subject, next_holder, next_epoch, next_expiry, successor) = any_lease();
        let same_subject = subject == next_subject;
        let same_tenure = same_subject && holder == next_holder && epoch == next_epoch;
        let succession = predecessor.succession(&successor);
        match succession {
            Some(FenceSuccession::Reassignment) => {
                assert!(same_subject);
                assert!(next_epoch > epoch);
            }
            Some(FenceSuccession::Renewal) => {
                assert!(same_tenure);
                assert!(next_expiry >= expiry);
            }
            None => {
                assert!(!same_subject || next_epoch <= epoch);
                assert!(!same_tenure || next_expiry < expiry);
            }
        }
        if !same_subject || next_epoch < epoch {
            assert!(succession.is_none());
        }
    }
}
