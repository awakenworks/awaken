// @formal entrypoint sandbox.effect_fence_succession_is_renewal_or_epoch_advance
//! The one ownership-fence kernel.
//!
//! A lease is a [`Tenure`] — the complete identity of one grant: *what* is held
//! (the subject), *who* holds it (the holder) and *which* grant it is (the
//! [`Epoch`]) — held until an [`Expiry`](awaken_time::Expiry). Every successor
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
    awaken_time::Expiry::from_epoch_ms(expires_at_unix_ms)
        .is_live_at(awaken_time::UtcTime::from_epoch_ms(now_unix_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_kernel_is_strictly_before_expiry() {
        // Causes: C1 unsigned expiry is zero, ordinary or beyond the signed
        // SQL domain; C2 now is before/at/after it. R1 zero is always expired;
        // R2 ordinary expiry is live only before; R3 the u64 wire domain must
        // not clamp through SQL milliseconds. All rows use the canonical
        // Expiry law, so token and lease judgments have no separate boundary.
        for (expiry, now, live) in [
            (0, 0, false),
            (100, 0, true),
            (100, 99, true),
            (100, 100, false),
            (100, 101, false),
            (100, u64::MAX, false),
            (u64::MAX, u64::MAX - 1, true),
            (u64::MAX, u64::MAX, false),
        ] {
            assert_eq!(
                fence_is_live_at(expiry, now),
                live,
                "expiry={expiry} now={now}"
            );
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;
    use awaken_time::{Expiry, UtcTime};

    struct Probe;

    impl TenureScope for Probe {
        type Subject = u8;
        type Holder = u8;
    }

    /// Expiries the succession harnesses draw: the bottom of the `u64` wire
    /// domain, its successor and its top. Sufficiency rests on
    /// [`expiry_from_epoch_ms_is_strictly_monotone`]; see
    /// [`fence_succession_is_exactly_renewal_or_epoch_advance`].
    const EXPIRY_REPRESENTATIVES: [u64; 3] = [0, 1, u64::MAX];

    fn any_expiry_representative() -> u64 {
        let index: usize = kani::any();
        kani::assume(index < EXPIRY_REPRESENTATIVES.len());
        EXPIRY_REPRESENTATIVES[index]
    }

    fn any_lease() -> (u8, u8, u64, u64, Lease<Probe>) {
        let subject: u8 = kani::any();
        let holder: u8 = kani::any();
        let epoch: u64 = kani::any();
        kani::assume(epoch != 0);
        let expiry = any_expiry_representative();
        let lease = Lease::new(
            Tenure::granted(subject, holder, Epoch::from_persisted(epoch).unwrap()),
            Expiry::from_epoch_ms(expiry),
        );
        (subject, holder, epoch, expiry, lease)
    }

    /// The Expiry wire-order bridge, checked in this same lemma by two
    /// independent duties: the complete millisecond wrapper value, and actual
    /// Expiry Ord over every canonical Duration pair (full u64 seconds).
    ///
    /// UtcTime/Expiry are direct single-field wrappers. Duration::new retains
    /// (seconds, nanos) when nanos < 1_000_000_000; these inputs cover every
    /// valid Duration, and as_nanos observes seconds*1_000_000_000+nanos.
    /// On canonical pairs that positional value has exactly lexicographic
    /// order: a one-second advance exceeds every subsecond difference.
    ///
    /// The independently checked full-u64 image in
    /// [`unix_nanos_of_epoch_ms_is_strictly_monotone`] is wire*1_000_000.
    /// Combining that image with the actual Ord duty below gives
    /// from(x).cmp(&from(y)) == x.cmp(&y) for every u64 pair, including MAX;
    /// hence every adjacent x < MAX is strictly below x+1. Image equality
    /// alone is not used as a substitute for the separate Ord assertion.
    /// Only the persisted SQL projection clamps; this path never reads it.
    /// @obligation sandbox.effect_fence_succession_is_renewal_or_epoch_advance
    /// @obligation session_realization.generation_authority_requires_every_immutable_axis
    /// @kani profile=solver-z3-v1
    #[kani::proof]
    fn expiry_from_epoch_ms_is_strictly_monotone() {
        // C/E table: C1 any wire, including zero/MAX; C2 independent full-u64
        // seconds; C3 independent canonical nanos. R1 checks the complete
        // wire wrapper. R2 checks all three Ord results against input tuple
        // order, including equal seconds and opposing subsecond orders.
        // R3 retains valid successor carry/noncarry classes; MAX has no
        // successor but remains in R1. R4 witnesses both full Duration edges.
        let wire: u64 = kani::any();
        assert_eq!(
            Expiry::from_epoch_ms(wire),
            Expiry::at(UtcTime::from_epoch_ms(wire))
        );

        let seconds_a: u64 = kani::any();
        let seconds_b: u64 = kani::any();
        // These independent nanos inputs cover every canonical Duration
        // representation. The two bounds exclude no valid Duration and do
        // not constrain wire/seconds or assume the target Ord relationship.
        let nanos_a: u32 = kani::any();
        let nanos_b: u32 = kani::any();
        kani::assume(nanos_a < 1_000_000_000);
        kani::assume(nanos_b < 1_000_000_000);
        let a = Expiry::at(UtcTime::from_duration_since_epoch(
            std::time::Duration::new(seconds_a, nanos_a),
        ));
        let b = Expiry::at(UtcTime::from_duration_since_epoch(
            std::time::Duration::new(seconds_b, nanos_b),
        ));
        assert_eq!(a.cmp(&b), (seconds_a, nanos_a).cmp(&(seconds_b, nanos_b)));

        kani::cover!(
            wire < u64::MAX && wire % 1000 == 999,
            "the successor carries into the next second"
        );
        kani::cover!(
            wire < u64::MAX && wire % 1000 != 999,
            "the successor stays within the second"
        );
        kani::cover!(wire == 0, "the epoch keeps its complete wrapper value");
        kani::cover!(
            wire == u64::MAX,
            "the maximum wire keeps its complete wrapper value"
        );
        kani::cover!(
            seconds_a == seconds_b && nanos_a == nanos_b,
            "equal canonical coordinates compare equal"
        );
        kani::cover!(
            seconds_a == seconds_b && nanos_a < nanos_b,
            "within a second a smaller nanos coordinate is earlier"
        );
        kani::cover!(
            seconds_a == seconds_b && nanos_a > nanos_b,
            "within a second a larger nanos coordinate is later"
        );
        kani::cover!(
            seconds_a < seconds_b && nanos_a > nanos_b,
            "a lower second is earlier despite a larger nanos coordinate"
        );
        kani::cover!(
            seconds_a > seconds_b && nanos_a < nanos_b,
            "a higher second is later despite a smaller nanos coordinate"
        );
        kani::cover!(
            seconds_a == 0 && nanos_a == 0 && seconds_b == u64::MAX && nanos_b == 999_999_999,
            "both complete canonical Duration endpoints are represented"
        );
    }

    /// Lemma for every liveness harness that draws times from
    /// representatives: the Unix-nanosecond image that [`fence_is_live_at`]
    /// (and `Expiry::is_live_at` / `is_expired_at` on epoch milliseconds)
    /// compares has the exact image `wire * 1_000_000` for every `u64`,
    /// including its maximum (one symbolic value and one production conversion).
    ///
    /// The multiplication first promotes to `u128`; even the largest image is
    /// below 2^84, so it cannot wrap. This stronger equality implies strict
    /// monotonicity without constructing both adjacent production readings.
    /// Consequence: the image is an order embedding of `u64`,
    /// so `fence_is_live_at(expiry, now) == (now < expiry)` for every pair, and
    /// a liveness predicate depends on its times only through their order. The
    /// realization-lease and dispatch-deadline harnesses rely on this to draw
    /// times from {0, 1, u64::MAX} instead of from all of `u64`: the image
    /// goes through a `u64` division and a `u128` multiplication, which made
    /// two free full-width times expensive enough that those harnesses were
    /// narrowed to `u16` without a stated reason.
    /// @obligation sandbox.effect_fence_succession_is_renewal_or_epoch_advance
    /// @obligation session_realization.generation_authority_requires_every_immutable_axis
    /// @obligation run_ingress.a_deadline_passes_once_it_is_reached
    /// @obligation session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
    /// @obligation background_task.stale_fence_never_finishes_or_heartbeats
    /// @kani profile=solver-z3-v1
    #[kani::proof]
    fn unix_nanos_of_epoch_ms_is_strictly_monotone() {
        // C/E table: C1 every unsigned wire, including zero/MAX; C2 an ordinary
        // second carry or noncarry. R1 the actual unsigned production image is
        // exactly wire*1_000_000, with no signed SQL clamp. R2 applying R1 to
        // each of w and w+1 proves the original strict order for every w<MAX.
        // The image assertion has no domain assumption. Successor covers keep
        // w<MAX because MAX has no successor; separate zero/MAX covers witness
        // both endpoints. Expiry's derived Ord keeps its independent lemma.
        // Normalize only the unit oracle: for q=w/1000 and r=w%1000,
        // w=1000*q+r, hence q*1_000_000_000+r*1_000_000=w*1_000_000.
        // All terms are promoted before multiplication and fit u128. The
        // oracle reads only w; native E4 retains the independent linear form.
        let wire: u64 = kani::any();
        let image = UtcTime::from_epoch_ms(wire).as_unix_nanos();
        let expected =
            u128::from(wire / 1_000) * 1_000_000_000 + u128::from(wire % 1_000) * 1_000_000;
        assert_eq!(image, expected);
        kani::cover!(
            wire < u64::MAX && wire % 1000 == 999,
            "the successor carries into the next second"
        );
        kani::cover!(
            wire < u64::MAX && wire % 1000 != 999,
            "the successor stays within the second"
        );
        kani::cover!(wire == 0, "the epoch has its exact image");
        kani::cover!(wire == u64::MAX, "the maximum wire has its exact image");
    }

    /// The succession law as production derives its inputs: `Lease::succession`
    /// compares the subjects, the complete tenures and the expiries itself, so
    /// the harness feeds it symbolic coordinates, never a free "same" bit.
    ///
    /// Expiries come from [`EXPIRY_REPRESENTATIVES`], not from all of `u64`.
    /// Why that loses nothing: `fence_succession` reads the two expiries only
    /// through `successor >= predecessor`, and the oracle below reads the raw
    /// wire values only through `>=` and `<`. By the lemma
    /// [`expiry_from_epoch_ms_is_strictly_monotone`] the `Expiry` order equals
    /// the wire order, so both sides depend on an expiry pair only through
    /// which of `<`, `=`, `>` relates it, and the other coordinates are drawn
    /// independently of it. The representatives realise every one of the three
    /// relations ((0, 1), (0, 0), (1, 0)), and also at the top of the domain
    /// ((1, MAX), (MAX, MAX), (MAX, 1)). So every `u64` pair behaves exactly
    /// as some representative pair, which this harness checks. Two full-width
    /// expiries carried a `u64` division by 1,000 each through every branch
    /// and did not finish within the batch deadline.
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
        kani::cover!(
            succession == Some(FenceSuccession::Reassignment),
            "reassignment"
        );
        kani::cover!(
            succession == Some(FenceSuccession::Renewal) && next_expiry > expiry,
            "renewal that extends the expiry"
        );
        kani::cover!(
            succession == Some(FenceSuccession::Renewal) && next_expiry == expiry,
            "renewal at an equal expiry"
        );
        kani::cover!(
            succession.is_none() && same_tenure,
            "the exact tenure refused for shortening its expiry"
        );
        kani::cover!(
            succession.is_none() && !same_subject && next_epoch > epoch,
            "a foreign subject at a higher epoch refused"
        );
        kani::cover!(
            succession.is_none() && same_subject && !same_tenure && next_epoch == epoch,
            "another holder at the same epoch refused"
        );
    }
}
