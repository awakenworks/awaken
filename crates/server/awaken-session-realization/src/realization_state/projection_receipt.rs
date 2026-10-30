// @formal surface obligation=session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
// @formal reason: The installed realization admits each frozen root projection through the contract's
// @formal reason: FrozenProjectionReceipt::admit law and only maps its refusal to RealizationError.
//! Admission of complete aggregate snapshots at the existing installed Session
//! boundary, through the contract's one ordering law.
use super::SessionRealization;
use crate::realization_error::{AuthorityLoss, RealizationError};
use awaken_session_realization_contract::{FrozenProjectionReceipt, ProjectionReceiptRefusal};

impl SessionRealization {
    pub fn validate_projection_receipt(
        &self,
        receipt: &FrozenProjectionReceipt,
    ) -> Result<bool, RealizationError> {
        FrozenProjectionReceipt::admit(self.execution.frozen_receipt.as_ref(), receipt).map_err(
            |refusal| match refusal {
                ProjectionReceiptRefusal::Stale => RealizationError::authority_lost(
                    AuthorityLoss::ProjectionStale,
                    "stale frozen Session root projection; refresh from Control",
                ),
                ProjectionReceiptRefusal::Conflict => RealizationError::invariant(
                    "conflicting frozen Session root projection at the same revision",
                ),
            },
        )
    }

    /// Whether a frozen projection newer than `revision` is already held under
    /// the live generation of `lease`: the Worker installed it from a newer
    /// Control directive, so the older directive is superseded, not stale.
    pub fn holds_newer_projection_under(
        &self,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        revision: awaken_session_types::SessionRevision,
    ) -> bool {
        self.execution.authority.lease().is_some_and(|current| {
            awaken_session_realization_contract::realization_lease_generation_authorizes(
                current, lease,
            )
        }) && self
            .execution
            .frozen_receipt
            .as_ref()
            .is_some_and(|held| held.revision.0 > revision.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{test_observed_lease, wall_clock_now_ms};
    use awaken_session_configuration_contract::resource::SessionRealizationLease;
    use awaken_session_types::SessionRevision;

    fn lease(generation: u64) -> SessionRealizationLease {
        SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker/1".into(),
            epoch: generation,
            expires_at_unix_ms: wall_clock_now_ms() + 60_000,
        }
    }

    fn receipt(revision: u64) -> FrozenProjectionReceipt {
        FrozenProjectionReceipt {
            revision: SessionRevision(revision),
            fingerprint: "a".into(),
        }
    }

    #[test]
    fn a_projection_is_superseded_only_by_a_newer_one_under_the_same_live_generation() {
        // Decision table N1-N5. C1 a receipt is held and orders after/equal to
        // the refused directive; C2 the live lease is the refused directive's
        // generation or another one.
        // N1 newer + same generation => superseded; N2 equal revision => not;
        // N3 newer but another epoch (a retiring owner's) => not; N4 nothing
        // held => not; N5 no live lease => not.
        let mut realization = SessionRealization::default();
        let directive_lease = lease(1);
        assert!(
            !realization.holds_newer_projection_under(&directive_lease, SessionRevision(3)),
            "N5 no live lease"
        );
        realization.install_lease(test_observed_lease(directive_lease.clone()));
        assert!(
            !realization.holds_newer_projection_under(&directive_lease, SessionRevision(3)),
            "N4 nothing held"
        );
        realization.execution.frozen_receipt = Some(receipt(7));
        assert!(
            realization.holds_newer_projection_under(&directive_lease, SessionRevision(3)),
            "N1"
        );
        assert!(
            !realization.holds_newer_projection_under(&directive_lease, SessionRevision(7)),
            "N2"
        );
        assert!(
            !realization.holds_newer_projection_under(&lease(2), SessionRevision(3)),
            "N3"
        );
    }
}
