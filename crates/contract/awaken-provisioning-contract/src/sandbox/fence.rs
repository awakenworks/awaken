//! The realization-lease fence algebra shared by physical Sandbox effects.
//!
//! [`FenceToken`] names exactly the coordinates and the successor law that the
//! aggregate's realization lease and every provider effect fence share: owner,
//! runtime incarnation, monotonic epoch and renewable expiry. Operation
//! identity is deliberately outside the trait; the typed effect that owns a
//! transition compares it. Dispatch claims, attempt fences, task leases and
//! credential receipts have different successor rules and do not implement it.

use serde::{Deserialize, Serialize};

use super::foundation::SandboxError;

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
}

/// Provider-neutral authorization fence for one physical Sandbox effect.
///
/// The operation identity and lease fields are projected from an existing
/// aggregate-owned effect and realization lease. Providers may persist this
/// value as backend evidence, but must never mint or reinterpret it as a second
/// ownership authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxEffectFence {
    pub operation_id: String,
    pub owner: String,
    pub runtime_incarnation: String,
    pub epoch: u64,
    pub expires_at_unix_ms: u64,
}

impl FenceToken for SandboxEffectFence {
    fn owner(&self) -> &str {
        &self.owner
    }

    fn runtime_incarnation(&self) -> &str {
        &self.runtime_incarnation
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }
}

impl SandboxEffectFence {
    pub fn new(
        operation_id: impl Into<String>,
        owner: impl Into<String>,
        runtime_incarnation: impl Into<String>,
        epoch: u64,
        expires_at_unix_ms: u64,
    ) -> Result<Self, SandboxError> {
        let fence = Self {
            operation_id: operation_id.into(),
            owner: owner.into(),
            runtime_incarnation: runtime_incarnation.into(),
            epoch,
            expires_at_unix_ms,
        };
        fence.validate_identity()?;
        Ok(fence)
    }

    /// Validate the complete aggregate effect identity carried across provider
    /// boundaries. Deserialized and structurally constructed values use the
    /// same rule as [`Self::new`].
    pub fn validate_identity(&self) -> Result<(), SandboxError> {
        for (field, value) in [
            ("operation id", self.operation_id.as_str()),
            ("owner", self.owner.as_str()),
            ("runtime incarnation", self.runtime_incarnation.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(SandboxError::invalid(format!(
                    "Sandbox effect fence requires a non-empty {field}"
                )));
            }
        }
        Ok(())
    }

    /// Validate identity and expiry against a caller-observed clock instant.
    /// Clock acquisition remains adapter-owned; the decision rule does not.
    pub fn validate_live_at(&self, now_unix_ms: u64) -> Result<(), SandboxError> {
        self.validate_identity()?;
        if self.expired_at(now_unix_ms) {
            return Err(SandboxError::fence_rejected(
                "Sandbox effect fence expired before the provider boundary",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn expired_at(&self, now_unix_ms: u64) -> bool {
        now_unix_ms >= self.expires_at_unix_ms
    }

    /// Compare the immutable aggregate effect identity while deliberately
    /// ignoring its renewable expiry boundary.
    #[must_use]
    pub fn same_effect_identity(&self, other: &Self) -> bool {
        self.operation_id == other.operation_id && self.same_realization_lease(other)
    }

    /// Whether this fence authorizes a successor for the same aggregate
    /// operation. This is the canonical provider-preparation comparison:
    /// callers must not combine lease monotonicity with a separately guessed
    /// operation identity.
    #[must_use]
    pub fn authorizes_effect_successor(&self, successor: &Self) -> bool {
        self.operation_id == successor.operation_id && self.authorizes_successor(successor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn succession_classifies_every_successor_law_row() {
        // Cause/effect decision table over the lease coordinates alone
        // (operation identity is not a FenceToken coordinate):
        // C1 successor epoch lower/equal/higher; C2 lease exact/foreign;
        // C3 successor expiry shorter/equal/longer.
        // | Rule | C1     | C2      | C3             | Effect        |
        // | S1   | higher | any     | any            | Reassignment  |
        // | S2   | equal  | exact   | equal/longer   | Renewal       |
        // | S3   | equal  | exact   | shorter        | reject        |
        // | S4   | equal  | foreign | any            | reject        |
        // | S5   | lower  | any     | any            | reject        |
        let fence = SandboxEffectFence::new("op", "owner", "boot", 7, 100).unwrap();
        let with = |owner: &str, epoch, expiry| {
            SandboxEffectFence::new("other-op", owner, "boot", epoch, expiry).unwrap()
        };
        for (rule, successor, expected) in [
            (
                "S1 foreign",
                with("foreign", 8, 1),
                Some(FenceSuccession::Reassignment),
            ),
            (
                "S1 exact",
                with("owner", 8, 200),
                Some(FenceSuccession::Reassignment),
            ),
            (
                "S2 equal",
                with("owner", 7, 100),
                Some(FenceSuccession::Renewal),
            ),
            (
                "S2 longer",
                with("owner", 7, 101),
                Some(FenceSuccession::Renewal),
            ),
            ("S3 shorter", with("owner", 7, 99), None),
            ("S4 foreign", with("foreign", 7, 200), None),
            ("S5 lower", with("owner", 6, 200), None),
        ] {
            assert_eq!(fence.succession(&successor), expected, "{rule}");
            assert_eq!(
                fence.authorizes_successor(&successor),
                expected.is_some(),
                "{rule}"
            );
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn fence_succession_is_exactly_renewal_or_epoch_advance() {
        let predecessor_epoch: u64 = kani::any();
        let predecessor_expiry: u64 = kani::any();
        let successor_epoch: u64 = kani::any();
        let successor_expiry: u64 = kani::any();
        let same_lease: bool = kani::any();
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
                // A renewal never regresses expiry, never changes the lease,
                // and can only arise when the epoch did not advance.
                assert!(same_lease);
                assert!(successor_expiry >= predecessor_expiry);
                assert!(successor_epoch <= predecessor_epoch);
            }
            None => {
                // Rejection is total: no epoch advance and no exact renewal.
                assert!(successor_epoch <= predecessor_epoch);
                assert!(!same_lease || successor_expiry < predecessor_expiry);
            }
        }
        // An older epoch can never take over, whatever else is presented.
        if successor_epoch < predecessor_epoch && !same_lease {
            assert!(succession.is_none());
        }
    }
}
