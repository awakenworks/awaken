//! Claim-fenced bridge from Worker execution to the Session application.
//!
//! The bridge owns only the dispatch claim envelope. The Coordinator transport
//! validates that envelope before invoking the claim-free Session application
//! APIs, so `RunClaim` never enters the durable Session aggregate.

use crate::RunClaim;
use awaken_session_contract::{
    ApplicationSessionContribution, ApplicationSessionContributionReceipt,
    SessionRealizationControl, SessionRealizationDirective,
};

/// Failure crossing the authenticated claim-fenced Session-control boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("claimed Session control failed: {0}")]
pub struct ClaimedSessionControlError(String);

impl ClaimedSessionControlError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Atomic result of contribution plus initial realization assignment.
#[derive(Clone)]
pub struct ClaimedSessionContributionReceipt {
    pub contribution: ApplicationSessionContributionReceipt,
    pub realization: SessionRealizationDirective,
}

/// Worker-side outbound port over the authenticated Coordinator transport.
///
/// Extending `SessionRealizationControl` forces contribution and realization to
/// use one client/authority instead of permitting two independently wired paths.
#[async_trait::async_trait]
pub trait ClaimedSessionControl: SessionRealizationControl + Send + Sync {
    async fn resume_frozen(
        &self,
        claim: &RunClaim,
        session_id: &str,
    ) -> Result<Option<SessionRealizationDirective>, ClaimedSessionControlError>;

    async fn contribute(
        &self,
        claim: &RunClaim,
        contribution: ApplicationSessionContribution,
    ) -> Result<ClaimedSessionContributionReceipt, ClaimedSessionControlError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cause-effect graph: C1 contribution receipt and C2 realization directive
    /// are produced by one claim-fenced command; E1 the bridge returns both as
    /// one inseparable value. Constraint: neither effect is optional.
    ///
    /// | Rule | C1 | C2 | E1 |
    /// |---|---|---|---|
    /// | R1 | yes | yes | one combined receipt |
    #[test]
    fn claimed_contribution_receipt_keeps_both_authoritative_effects() {
        fn assert_required_fields(value: &ClaimedSessionContributionReceipt) {
            let _ = &value.contribution;
            let _ = &value.realization;
        }
        let _ = assert_required_fields;
    }
}
