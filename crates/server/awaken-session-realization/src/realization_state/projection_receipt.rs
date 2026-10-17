// @formal entrypoint session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
//! Admission of complete aggregate snapshots at the existing slot boundary.
use super::SessionRealization;
use crate::realization_error::{AuthorityLoss, RealizationError};
use awaken_session_realization_contract::FrozenSessionProjection;
use awaken_session_types::SessionRevision;

#[derive(Clone, Debug)]
pub struct FrozenProjectionReceipt {
    pub revision: SessionRevision,
    pub fingerprint: String,
}

impl FrozenProjectionReceipt {
    pub fn from_projection(p: &FrozenSessionProjection) -> Result<Self, RealizationError> {
        // Only aggregate-owned coordinates enter equality. Executable publication,
        // materialized context and decoded Resource receipts are derived by adapters;
        // their existing exact publication/manifest checks remain authoritative.
        let fingerprint = awaken_agent_contract::canonical_json_sha256(
            "session-root-projection-v1",
            &(
                &p.workspace_id,
                &p.environment,
                &p.environment_materialization,
                p.resource_revision,
                &p.tools,
                &p.mcp,
            ),
        )
        .map_err(RealizationError::from)?;
        Ok(Self {
            revision: p.revision,
            fingerprint,
        })
    }
}

// This production kernel is also the bounded proof boundary. Neither a lease
// renewal nor a Resource revision is an aggregate ordering coordinate. The
// fingerprint is aggregate identity, so divergence at one revision conflicts.
fn projection_revision_admission(current: Option<u64>, incoming: u64, exact: bool) -> Option<bool> {
    use awaken_session_types::{MonotoneAdmission, SourceOrder, monotone_admission};
    match monotone_admission(SourceOrder::of_revision(current, incoming), exact) {
        MonotoneAdmission::Advance => Some(true),
        MonotoneAdmission::Replay => Some(false),
        MonotoneAdmission::Divergent | MonotoneAdmission::Reject => None,
    }
}

impl SessionRealization {
    pub fn validate_projection_receipt(
        &self,
        receipt: &FrozenProjectionReceipt,
    ) -> Result<bool, RealizationError> {
        projection_revision_admission(
            self.execution.frozen_receipt.as_ref().map(|r| r.revision.0),
            receipt.revision.0,
            self.execution
                .frozen_receipt
                .as_ref()
                .is_none_or(|r| r.fingerprint == receipt.fingerprint),
        )
        .ok_or_else(|| {
            if self
                .execution
                .frozen_receipt
                .as_ref()
                .is_some_and(|current| receipt.revision.0 < current.revision.0)
            {
                // A delayed Control read is retryable; never terminally fail a Run
                // merely because another request installed a newer snapshot.
                RealizationError::authority_lost(
                    AuthorityLoss::ProjectionStale,
                    "stale frozen Session root projection; refresh from Control",
                )
            } else {
                RealizationError::invariant(
                    "conflicting frozen Session root projection at the same revision",
                )
            }
        })
    }
}

#[cfg(kani)]
/// @obligation session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
/// @proof-input-debt free_symbol projection_revision_admission exact
#[kani::proof]
fn projection_revision_admission_is_monotonic_and_exact() {
    // C1 prior receipt exists; C2 incoming root revision orders before/equal/after;
    // C3 aggregate coordinates agree. R1 first/newer admit replacement, R2 equal
    // and exact is replay, R3 older or equal-conflicting reject. Full u64 domain,
    // including zero/MAX, without arithmetic or ordering effect/generation strings.
    let current: Option<u64> = kani::any();
    let incoming: u64 = kani::any();
    let exact: bool = kani::any();
    let decision = projection_revision_admission(current, incoming, exact);
    assert_eq!(decision == Some(true), current.is_none_or(|r| incoming > r));
    assert_eq!(
        decision == Some(false),
        current.is_some_and(|r| incoming == r && exact)
    );
    assert_eq!(
        decision.is_none(),
        current.is_some_and(|r| incoming < r || incoming == r && !exact)
    );
}
