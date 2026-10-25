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
}
