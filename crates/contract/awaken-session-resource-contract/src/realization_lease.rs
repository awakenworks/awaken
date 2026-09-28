// @formal entrypoint session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
//! Session-owned realization fence and its laws. Persistence stores this value;
//! it does not own a second lease vocabulary or authorization algorithm.

/// Durable owner fence for all process-local Session projections. Runtime and
/// Worker identities are opaque to the Session domain.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationLease {
    pub owner: String,
    pub runtime_incarnation: String,
    pub epoch: u64,
    pub expires_at_unix_ms: u64,
}

/// The aggregate's realization lease carries exactly the coordinates of the
/// neutral fence algebra, so it states its succession through the one law
/// instead of restating the field comparisons.
impl awaken_lease_contract::fence::FenceToken for SessionRealizationLease {
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

impl SessionRealizationLease {
    /// Project the aggregate-owned realization generation into the one neutral
    /// provider fence. Domain callers still decide whether this lease is the
    /// current/live authority; this method only prevents adapters from growing
    /// duplicate field mappings or operation-identity algorithms.
    pub fn sandbox_effect_fence(
        &self,
        operation_id: impl Into<String>,
    ) -> Result<
        awaken_provisioning_contract::SandboxEffectFence,
        awaken_provisioning_contract::SandboxError,
    > {
        awaken_provisioning_contract::SandboxEffectFence::for_operation(operation_id, self)
    }
}

/// Canonical Session-realization lease boundary. A lease is half-open: it is
/// live strictly before its expiry and stale at the exact expiry millisecond.
#[must_use]
pub const fn realization_lease_is_live_at(expires_at_unix_ms: u64, now_unix_ms: u64) -> bool {
    awaken_lease_contract::fence::fence_is_live_at(expires_at_unix_ms, now_unix_ms)
}

#[must_use]
fn realization_lease_facts_authorize(
    same_owner: bool,
    same_runtime_incarnation: bool,
    same_epoch: bool,
    current_expires_at_unix_ms: u64,
    asserted_expires_at_unix_ms: u64,
    now_unix_ms: u64,
) -> bool {
    // The current lease must be a *renewal* of the asserted one under the one
    // fence succession law (same lease, expiry never shortened) and still live.
    same_owner
        && same_runtime_incarnation
        && same_epoch
        && matches!(
            awaken_lease_contract::fence::fence_succession(
                true,
                0,
                asserted_expires_at_unix_ms,
                0,
                current_expires_at_unix_ms,
                true,
            ),
            Some(awaken_lease_contract::fence::FenceSuccession::Renewal)
        )
        && realization_lease_is_live_at(current_expires_at_unix_ms, now_unix_ms)
}

#[must_use]
pub fn realization_lease_authorizes(
    current: &SessionRealizationLease,
    asserted: &SessionRealizationLease,
    now_unix_ms: u64,
) -> bool {
    realization_lease_facts_authorize(
        current.owner == asserted.owner,
        current.runtime_incarnation == asserted.runtime_incarnation,
        current.epoch == asserted.epoch,
        current.expires_at_unix_ms,
        asserted.expires_at_unix_ms,
        now_unix_ms,
    )
}

/// Whether an asserted terminal generation is still the aggregate's exact
/// owner/epoch. Terminal cleanup may intentionally outlive the ordinary lease
/// wall-clock expiry after Work retirement; authenticated topology and the
/// aggregate's monotonic generation, rather than a second timer rule, fence it.
#[must_use]
pub fn realization_lease_generation_authorizes(
    current: &SessionRealizationLease,
    asserted: &SessionRealizationLease,
) -> bool {
    use awaken_lease_contract::fence::{FenceSuccession, FenceToken};
    // The current generation must be a renewal of the asserted one: the exact
    // lease, expiry never shortened. A reassignment (higher epoch) is a
    // different owner generation and does not authorize the assertion.
    asserted.succession(current) == Some(FenceSuccession::Renewal)
}

#[cfg(kani)]
/// @obligation session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
/// @proof-input-debt free_symbol realization_lease_facts_authorize same_epoch
/// @proof-input-debt free_symbol realization_lease_facts_authorize same_owner
/// @proof-input-debt free_symbol realization_lease_facts_authorize same_runtime_incarnation
#[kani::proof]
fn realization_renewal_never_widens_owner_epoch_or_expiry_authority() {
    let same_owner = kani::any();
    let same_runtime_incarnation = kani::any();
    let same_epoch = kani::any();
    let current_expires_at_unix_ms = kani::any();
    let asserted_expires_at_unix_ms = kani::any();
    let now_unix_ms = kani::any();
    let authorized = realization_lease_facts_authorize(
        same_owner,
        same_runtime_incarnation,
        same_epoch,
        current_expires_at_unix_ms,
        asserted_expires_at_unix_ms,
        now_unix_ms,
    );
    assert_eq!(
        authorized,
        same_owner
            && same_runtime_incarnation
            && same_epoch
            && current_expires_at_unix_ms >= asserted_expires_at_unix_ms
            && current_expires_at_unix_ms > now_unix_ms
    );
}
