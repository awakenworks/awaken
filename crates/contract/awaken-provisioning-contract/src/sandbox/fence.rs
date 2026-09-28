// @formal evidence sandbox.effect_fence_succession_is_renewal_or_epoch_advance
//! The provider-neutral Sandbox effect fence.
//!
//! [`SandboxEffectFence`] projects the aggregate's realization lease onto one
//! physical operation. The successor law it obeys is the shared fence kernel
//! in `awaken_lease_contract::fence`; dispatch claims, attempt fences, task
//! leases and credential receipts have different successor rules and do not
//! implement [`FenceToken`].

use awaken_lease_contract::fence::FenceToken;
use serde::{Deserialize, Serialize};

use super::foundation::SandboxError;

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
        assert_eq!(fence.operation_id, "target-op", "R1 operation");
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
        let blank_owner = SandboxEffectFence {
            operation_id: "x".into(),
            owner: " ".into(),
            runtime_incarnation: "runtime-1".into(),
            epoch: 7,
            expires_at_unix_ms: 100,
        };
        assert!(
            SandboxEffectFence::for_operation("target-op", &blank_owner).is_err(),
            "R2 blank owner"
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
            assert_eq!(token.is_live_at(now), live, "trait now={now}");
            assert_eq!(fence_is_live_at(100, now), live, "kernel now={now}");
        }
    }
}
