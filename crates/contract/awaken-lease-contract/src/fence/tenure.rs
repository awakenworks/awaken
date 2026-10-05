// @formal evidence sandbox.effect_fence_succession_is_renewal_or_epoch_advance
//! Complete lease identity (tenure) and the time-bounded lease over it.

use std::fmt;
use std::hash::{Hash, Hasher};

use super::{Epoch, FenceSuccession, fence_succession};
use awaken_time::{Expiry, UtcTime};

/// The coordinates one family of leases is scoped by.
///
/// `Subject` is *what* is held (a Session's MCP generation, a unit of Work, a
/// background task); `Holder` is *who* holds it (a runtime incarnation, a
/// worker). A lease comparison that omits either is the bug class this kernel
/// exists to end: without the subject, another resource's lease at a higher
/// epoch reads as a reassignment of this one; without the holder, a foreign
/// process at the same epoch reads as the owner.
///
/// Implementors are zero-sized markers owned by the contract that names the
/// family; they carry no data.
pub trait TenureScope {
    type Subject: Eq + Clone + fmt::Debug;
    type Holder: Eq + Clone + fmt::Debug;
}

/// The complete identity of one grant: subject, holder and epoch.
///
/// Equality is over all three, so "the same tenure" can only mean the same
/// grant of the same thing to the same holder. The fields are private; a tenure
/// is minted by [`Tenure::granted`] or advanced by [`Tenure::reassigned`].
pub struct Tenure<S: TenureScope> {
    subject: S::Subject,
    holder: S::Holder,
    epoch: Epoch,
}

impl<S: TenureScope> Tenure<S> {
    /// The tenure of one grant, as the granting authority minted it.
    #[must_use]
    pub const fn granted(subject: S::Subject, holder: S::Holder, epoch: Epoch) -> Self {
        Self {
            subject,
            holder,
            epoch,
        }
    }

    #[must_use]
    pub const fn subject(&self) -> &S::Subject {
        &self.subject
    }

    #[must_use]
    pub const fn holder(&self) -> &S::Holder {
        &self.holder
    }

    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The same subject granted to `holder` at the next epoch, or `None` once
    /// the epoch is exhausted. A reassignment to the current holder is still a
    /// new grant: it advances the epoch, so every fence minted under this one
    /// is superseded.
    #[must_use]
    pub fn reassigned(&self, holder: S::Holder) -> Option<Self> {
        Some(Self {
            subject: self.subject.clone(),
            holder,
            epoch: self.epoch.next()?,
        })
    }
}

impl<S: TenureScope> Clone for Tenure<S> {
    fn clone(&self) -> Self {
        Self {
            subject: self.subject.clone(),
            holder: self.holder.clone(),
            epoch: self.epoch,
        }
    }
}

impl<S: TenureScope> fmt::Debug for Tenure<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Tenure")
            .field("subject", &self.subject)
            .field("holder", &self.holder)
            .field("epoch", &self.epoch)
            .finish()
    }
}

impl<S: TenureScope> PartialEq for Tenure<S> {
    fn eq(&self, other: &Self) -> bool {
        self.subject == other.subject && self.holder == other.holder && self.epoch == other.epoch
    }
}

impl<S: TenureScope> Eq for Tenure<S> {}

impl<S: TenureScope> Hash for Tenure<S>
where
    S::Subject: Hash,
    S::Holder: Hash,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.subject.hash(state);
        self.holder.hash(state);
        self.epoch.hash(state);
    }
}

/// One tenure held until an expiry.
///
/// Equality is over the tenure *and* the expiry: two values of one grant at
/// different expiries are different leases. A comparison that deliberately
/// ignores expiry goes through [`Lease::tenure`] (the same grant) or
/// [`Lease::succession`] (a permitted successor); there is no third way, so no
/// caller can clone a lease, overwrite its expiry and compare the result.
pub struct Lease<S: TenureScope> {
    tenure: Tenure<S>,
    expires: Expiry,
}

impl<S: TenureScope> Lease<S> {
    #[must_use]
    pub const fn new(tenure: Tenure<S>, expires: Expiry) -> Self {
        Self { tenure, expires }
    }

    #[must_use]
    pub const fn tenure(&self) -> &Tenure<S> {
        &self.tenure
    }

    #[must_use]
    pub const fn expires(&self) -> Expiry {
        self.expires
    }

    /// How `successor` takes over from this lease, or `None` when it may not.
    ///
    /// A successor for another subject is never a succession, whatever its
    /// epoch. For the same subject a strictly higher epoch reassigns, and the
    /// exact tenure renews only without shortening expiry.
    #[must_use]
    pub fn succession(&self, successor: &Self) -> Option<FenceSuccession> {
        fence_succession(
            self.tenure.subject == successor.tenure.subject,
            self.tenure.epoch.get(),
            self.expires,
            successor.tenure.epoch.get(),
            successor.expires,
            self.tenure == successor.tenure,
        )
    }

    /// Whether `successor` is a renewal or a reassignment of this lease.
    #[must_use]
    pub fn authorizes_successor(&self, successor: &Self) -> bool {
        self.succession(successor).is_some()
    }

    /// The same tenure held until `expires`, or `None` when that would shorten
    /// the lease: a renewal never gives authority back to a predecessor's
    /// deadline.
    #[must_use]
    pub fn renewed_to(&self, expires: Expiry) -> Option<Self> {
        (expires >= self.expires).then(|| Self {
            tenure: self.tenure.clone(),
            expires,
        })
    }

    /// Whether the lease still holds at `now` under the one expiry law.
    #[must_use]
    pub fn is_live_at(&self, now: UtcTime) -> bool {
        self.expires.is_live_at(now)
    }
}

impl<S: TenureScope> Clone for Lease<S> {
    fn clone(&self) -> Self {
        Self {
            tenure: self.tenure.clone(),
            expires: self.expires,
        }
    }
}

impl<S: TenureScope> fmt::Debug for Lease<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Lease")
            .field("tenure", &self.tenure)
            .field("expires", &self.expires)
            .finish()
    }
}

impl<S: TenureScope> PartialEq for Lease<S> {
    fn eq(&self, other: &Self) -> bool {
        self.tenure == other.tenure && self.expires == other.expires
    }
}

impl<S: TenureScope> Eq for Lease<S> {}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe;

    impl TenureScope for Probe {
        type Subject = &'static str;
        type Holder = &'static str;
    }

    fn lease(
        subject: &'static str,
        holder: &'static str,
        epoch: u64,
        expires: u64,
    ) -> Lease<Probe> {
        Lease::new(
            Tenure::granted(subject, holder, Epoch::from_persisted(epoch).unwrap()),
            Expiry::at(UtcTime::from_epoch_ms(expires)),
        )
    }

    #[test]
    fn succession_requires_the_same_subject_and_classifies_every_row() {
        // Predecessor: subject s, holder h, epoch 7, expiry 100.
        // | Rule | subject | holder  | epoch  | expiry  | effect       |
        // | L1   | other   | any     | higher | any     | reject       |
        // | L2   | other   | same    | same   | same    | reject       |
        // | L3   | same    | foreign | higher | shorter | Reassignment |
        // | L4   | same    | same    | higher | longer  | Reassignment |
        // | L5   | same    | same    | same   | equal   | Renewal      |
        // | L6   | same    | same    | same   | longer  | Renewal      |
        // | L7   | same    | same    | same   | shorter | reject       |
        // | L8   | same    | foreign | same   | longer  | reject       |
        // | L9   | same    | same    | lower  | longer  | reject       |
        let held = lease("s", "h", 7, 100);
        for (rule, successor, expected) in [
            ("L1", lease("t", "g", 8, 200), None),
            ("L2", lease("t", "h", 7, 100), None),
            (
                "L3",
                lease("s", "g", 8, 1),
                Some(FenceSuccession::Reassignment),
            ),
            (
                "L4",
                lease("s", "h", 8, 200),
                Some(FenceSuccession::Reassignment),
            ),
            (
                "L5",
                lease("s", "h", 7, 100),
                Some(FenceSuccession::Renewal),
            ),
            (
                "L6",
                lease("s", "h", 7, 101),
                Some(FenceSuccession::Renewal),
            ),
            ("L7", lease("s", "h", 7, 99), None),
            ("L8", lease("s", "g", 7, 200), None),
            ("L9", lease("s", "h", 6, 200), None),
        ] {
            assert_eq!(held.succession(&successor), expected, "{rule}");
            assert_eq!(
                held.authorizes_successor(&successor),
                expected.is_some(),
                "{rule}"
            );
        }
    }

    #[test]
    fn reused_holder_names_cannot_recover_a_predecessor_tenure() {
        // C1 subject fixed; C2 holder goes A -> B -> A; C3 each reassignment
        // advances Epoch. E1 current A remains a distinct tenure from old A;
        // E2 the current lease rejects both predecessors despite renewed
        // expiries; E3 a foreign subject never succeeds even at a larger epoch.
        // This three-transition sequence is the smallest ABA state coverage;
        // the independent coordinate decision table above owns other rows.
        let old_a = lease("s", "A", 7, 100);
        let b = Lease::new(
            old_a.tenure().reassigned("B").unwrap(),
            Expiry::from_epoch_ms(200),
        );
        let current_a = Lease::new(
            b.tenure().reassigned("A").unwrap(),
            Expiry::from_epoch_ms(300),
        );
        assert_ne!(current_a.tenure(), old_a.tenure(), "E1");
        for predecessor in [old_a, b] {
            let renewed = predecessor
                .renewed_to(Expiry::from_epoch_ms(u64::MAX))
                .unwrap();
            assert!(!current_a.authorizes_successor(&renewed), "E2");
        }
        assert!(
            !current_a.authorizes_successor(&lease("foreign", "A", 10, 400)),
            "E3"
        );
    }

    #[test]
    fn identity_renewal_reassignment_and_liveness() {
        // R1 equality is over the tenure and the expiry; R2 the tenure alone
        // ignores expiry; R3 renewal never shortens; R4 reassignment advances
        // the epoch and keeps the subject; R5 an exhausted epoch cannot be
        // reassigned; R6 liveness is the one expiry law.
        let held = lease("s", "h", 7, 100);
        let renewed = held
            .renewed_to(Expiry::at(UtcTime::from_epoch_ms(150)))
            .expect("R3 longer");
        assert_ne!(held, renewed, "R1");
        assert_eq!(held.tenure(), renewed.tenure(), "R2");
        assert_eq!(
            held.renewed_to(Expiry::at(UtcTime::from_epoch_ms(99))),
            None,
            "R3"
        );
        let moved = held.tenure().reassigned("g").expect("R4");
        assert_eq!(
            (moved.subject(), moved.holder(), moved.epoch().get()),
            (&"s", &"g", 8),
            "R4"
        );
        let exhausted =
            Tenure::<Probe>::granted("s", "h", Epoch::from_persisted(u64::MAX).unwrap());
        assert!(exhausted.reassigned("g").is_none(), "R5");
        for (now, live) in [(99, true), (100, false), (101, false)] {
            assert_eq!(
                held.is_live_at(UtcTime::from_epoch_ms(now)),
                live,
                "R6 now={now}"
            );
        }
    }
}
