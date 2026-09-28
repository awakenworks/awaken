// @formal evidence sandbox.effect_fence_succession_is_renewal_or_epoch_advance
//! The provider-neutral Sandbox effect fence.
//!
//! [`SandboxEffectFence`] projects the aggregate's realization lease onto one
//! physical operation. It is the kernel's [`EffectFence`] over the
//! [`SandboxRealization`] lease family, so its successor law is the shared
//! fence kernel in `awaken_lease_contract::fence`; dispatch claims, attempt
//! fences, task leases and credential receipts have different successor rules
//! and do not implement [`FenceToken`].

use awaken_lease_contract::Expiry;
use awaken_lease_contract::fence::{
    EffectFence, Epoch, FenceSuccession, FenceToken, Lease, Tenure, TenureScope,
};
use serde::{Deserialize, Serialize};

use super::foundation::SandboxError;

/// The lease family a Sandbox effect fence is projected from: the
/// aggregate's realization lease, held by one owner's runtime incarnation.
///
/// The subject is unit because the fence's persisted wire carries none: a
/// fence is only ever compared with another fence read from the same Sandbox
/// (its realization marker, labels or handle), which is the subject by
/// construction. Consumer: [`SandboxEffectFence`].
#[derive(Debug)]
pub struct SandboxRealization;

impl TenureScope for SandboxRealization {
    type Subject = ();
    type Holder = RealizationHolder;
}

/// Who holds a realization lease: the owner and its runtime incarnation.
/// Consumer: the [`SandboxRealization`] tenure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RealizationHolder {
    owner: String,
    runtime_incarnation: String,
}

/// Provider-neutral authorization fence for one physical Sandbox effect.
///
/// The operation identity and lease fields are projected from an existing
/// aggregate-owned effect and realization lease. Providers may persist this
/// value as backend evidence, but must never mint or reinterpret it as a second
/// ownership authority.
///
/// Every value passed [`SandboxEffectFence::new`]: construction and decoding
/// share that one admission (non-blank operation, owner and incarnation; a
/// nonzero epoch), so a fence that exists is a complete identity. The wire
/// keeps the flat shape it had while the coordinates were public fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SandboxEffectFenceWire", into = "SandboxEffectFenceWire")]
pub struct SandboxEffectFence(EffectFence<SandboxRealization, String>);

/// The persisted coordinates of [`SandboxEffectFence`]. Converting back into
/// the fence runs [`SandboxEffectFence::new`], the same admission as decoding.
/// Consumers: the fence's serde codec, and provider decision tables that name
/// the one persisted coordinate they tamper with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxEffectFenceWire {
    pub operation_id: String,
    pub owner: String,
    pub runtime_incarnation: String,
    pub epoch: u64,
    pub expires_at_unix_ms: u64,
}

impl TryFrom<SandboxEffectFenceWire> for SandboxEffectFence {
    type Error = SandboxError;

    fn try_from(wire: SandboxEffectFenceWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.operation_id,
            wire.owner,
            wire.runtime_incarnation,
            wire.epoch,
            wire.expires_at_unix_ms,
        )
    }
}

impl From<SandboxEffectFence> for SandboxEffectFenceWire {
    fn from(fence: SandboxEffectFence) -> Self {
        Self {
            operation_id: fence.operation_id().to_owned(),
            owner: fence.owner().to_owned(),
            runtime_incarnation: fence.runtime_incarnation().to_owned(),
            epoch: fence.epoch(),
            expires_at_unix_ms: fence.expires_at_unix_ms(),
        }
    }
}

impl FenceToken for SandboxEffectFence {
    fn owner(&self) -> &str {
        SandboxEffectFence::owner(self)
    }

    fn runtime_incarnation(&self) -> &str {
        SandboxEffectFence::runtime_incarnation(self)
    }

    fn epoch(&self) -> u64 {
        SandboxEffectFence::epoch(self)
    }

    fn expires_at_unix_ms(&self) -> u64 {
        SandboxEffectFence::expires_at_unix_ms(self)
    }
}

impl SandboxEffectFence {
    /// The one admission of a Sandbox effect fence, shared by the decoder:
    /// the operation, owner and runtime incarnation are non-blank and the
    /// epoch is nonzero (no realization lease grants zero).
    pub fn new(
        operation_id: impl Into<String>,
        owner: impl Into<String>,
        runtime_incarnation: impl Into<String>,
        epoch: u64,
        expires_at_unix_ms: u64,
    ) -> Result<Self, SandboxError> {
        let (operation_id, owner, runtime_incarnation) = (
            operation_id.into(),
            owner.into(),
            runtime_incarnation.into(),
        );
        for (field, value) in [
            ("operation id", operation_id.as_str()),
            ("owner", owner.as_str()),
            ("runtime incarnation", runtime_incarnation.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(SandboxError::invalid(format!(
                    "Sandbox effect fence requires a non-empty {field}"
                )));
            }
        }
        let epoch = Epoch::from_persisted(epoch).ok_or_else(|| {
            SandboxError::invalid("Sandbox effect fence requires a nonzero lease epoch")
        })?;
        Ok(Self(EffectFence::new(
            operation_id,
            Lease::new(
                Tenure::granted(
                    (),
                    RealizationHolder {
                        owner,
                        runtime_incarnation,
                    },
                    epoch,
                ),
                Expiry::from_unix_ms(expires_at_unix_ms),
            ),
        )))
    }

    /// Project any realization-lease token into the fence for one exact
    /// physical operation. This is the only place the lease→fence field
    /// mapping is written; domain leases delegate here instead of repeating it.
    pub fn for_operation(
        operation_id: impl Into<String>,
        token: &impl FenceToken,
    ) -> Result<Self, SandboxError> {
        Self::new(
            operation_id,
            token.owner(),
            token.runtime_incarnation(),
            token.epoch(),
            token.expires_at_unix_ms(),
        )
    }

    /// The persisted coordinates of this fence.
    #[must_use]
    pub fn to_wire(&self) -> SandboxEffectFenceWire {
        self.clone().into()
    }

    /// This fence with persisted coordinates edited, re-admitted through
    /// [`SandboxEffectFence::new`].
    pub fn with_persisted(
        &self,
        edit: impl FnOnce(&mut SandboxEffectFenceWire),
    ) -> Result<Self, SandboxError> {
        let mut wire = self.to_wire();
        edit(&mut wire);
        Self::try_from(wire)
    }

    #[must_use]
    pub fn operation_id(&self) -> &str {
        self.0.operation()
    }

    #[must_use]
    pub fn owner(&self) -> &str {
        &self.0.lease().tenure().holder().owner
    }

    #[must_use]
    pub fn runtime_incarnation(&self) -> &str {
        &self.0.lease().tenure().holder().runtime_incarnation
    }

    /// The persisted lease epoch (never zero).
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.0.lease().tenure().epoch().get()
    }

    /// The persisted expiry millisecond.
    #[must_use]
    pub fn expires_at_unix_ms(&self) -> u64 {
        self.0.lease().expires().to_unix_ms()
    }

    /// Validate expiry against a caller-observed clock instant. Identity is
    /// already complete by construction. Clock acquisition remains
    /// adapter-owned; the decision rule does not.
    pub fn validate_live_at(&self, now_unix_ms: u64) -> Result<(), SandboxError> {
        if self.expired_at(now_unix_ms) {
            return Err(SandboxError::fence_rejected(
                "Sandbox effect fence expired before the provider boundary",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn expired_at(&self, now_unix_ms: u64) -> bool {
        !self.is_live_at(now_unix_ms)
    }

    /// Whether this fence still authorizes anything at `now_unix_ms` under
    /// the one expiry law: live strictly before expiry.
    #[must_use]
    pub fn is_live_at(&self, now_unix_ms: u64) -> bool {
        self.0
            .lease()
            .is_live_at(awaken_lease_contract::UtcTime::from_epoch_ms(now_unix_ms))
    }

    /// Compare the realization lease independently of any operation: the
    /// same owner, runtime incarnation and epoch.
    #[must_use]
    pub fn same_realization_lease(&self, other: &Self) -> bool {
        self.0.lease().tenure() == other.0.lease().tenure()
    }

    /// Classify how `successor` takes over this fence's lease under the one
    /// fence law.
    #[must_use]
    pub fn succession(&self, successor: &Self) -> Option<FenceSuccession> {
        self.0.lease().succession(successor.0.lease())
    }

    /// Whether an aggregate-authorized successor may take over a participant
    /// created under this fence. Operation identity is checked separately by
    /// [`SandboxEffectFence::authorizes_effect_successor`].
    #[must_use]
    pub fn authorizes_successor(&self, successor: &Self) -> bool {
        self.succession(successor).is_some()
    }

    /// Compare the immutable aggregate effect identity while deliberately
    /// ignoring its renewable expiry boundary.
    #[must_use]
    pub fn same_effect_identity(&self, other: &Self) -> bool {
        self.0.same_effect_identity(&other.0)
    }

    /// Whether this fence authorizes a successor for the same aggregate
    /// operation. This is the canonical provider-preparation comparison:
    /// callers must not combine lease monotonicity with a separately guessed
    /// operation identity.
    #[must_use]
    pub fn authorizes_effect_successor(&self, successor: &Self) -> bool {
        self.0.authorizes_effect_successor(&successor.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_lease_contract::fence::{FenceSuccession, fence_is_live_at};

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

    #[test]
    fn for_operation_projects_every_token_field_and_rejects_blank_identity() {
        // C1 token identity nonblank/blank; C2 operation nonblank/blank.
        // R1 nonblank both => one fence carrying exactly the token's four
        // fields under the given operation; R2 any blank => rejected before
        // a fence exists, so no caller can hand a provider an unnamed fence.
        let token = SandboxEffectFence::new("source-op", "owner-1", "runtime-1", 7, 100).unwrap();
        let fence = SandboxEffectFence::for_operation("target-op", &token).expect("R1");
        assert_eq!(fence.operation_id(), "target-op", "R1 operation");
        assert_eq!(
            (
                fence.owner(),
                fence.runtime_incarnation(),
                fence.epoch(),
                fence.expires_at_unix_ms()
            ),
            (
                token.owner(),
                token.runtime_incarnation(),
                token.epoch(),
                token.expires_at_unix_ms()
            ),
            "R1 fields"
        );
        assert!(
            SandboxEffectFence::for_operation(" ", &token).is_err(),
            "R2 blank operation"
        );
        for (rule, owner, incarnation, epoch) in [
            ("R2 blank owner", " ", "runtime-1", 7),
            ("R2 blank incarnation", "owner-1", "", 7),
            ("R2 zero epoch", "owner-1", "runtime-1", 0),
        ] {
            assert!(
                SandboxEffectFence::new("x", owner, incarnation, epoch, 100).is_err(),
                "{rule}"
            );
            // The decoder is the same door: a persisted fence with the same
            // defect never becomes a value.
            let wire = serde_json::json!({
                "operation_id": "x",
                "owner": owner,
                "runtime_incarnation": incarnation,
                "epoch": epoch,
                "expires_at_unix_ms": 100,
            });
            assert!(
                serde_json::from_value::<SandboxEffectFence>(wire).is_err(),
                "{rule} decode"
            );
        }
        let wire = serde_json::to_value(&token).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "operation_id": "source-op",
                "owner": "owner-1",
                "runtime_incarnation": "runtime-1",
                "epoch": 7,
                "expires_at_unix_ms": 100,
            }),
            "R1 the persisted wire keeps its flat shape"
        );
        assert_eq!(
            serde_json::from_value::<SandboxEffectFence>(wire).unwrap(),
            token
        );
    }

    #[test]
    fn liveness_is_strictly_before_expiry_for_every_token() {
        // C1 now before/at/after expiry. R1 live only strictly before; the
        // const kernel and the trait method agree at every boundary.
        let token = SandboxEffectFence::new("op", "owner", "runtime", 1, 100).unwrap();
        for (now, live) in [
            (0, true),
            (99, true),
            (100, false),
            (101, false),
            (u64::MAX, false),
        ] {
            assert_eq!(token.is_live_at(now), live, "fence now={now}");
            assert_eq!(FenceToken::is_live_at(&token, now), live, "trait now={now}");
            assert_eq!(fence_is_live_at(100, now), live, "kernel now={now}");
        }
    }
}
