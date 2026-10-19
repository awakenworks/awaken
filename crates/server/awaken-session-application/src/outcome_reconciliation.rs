// @formal surface requirement=runtime.outcomes.outcome_lifecycle
// @formal reason: Static owner: reconciliation between committed Run terminal facts and managed outcomes.
// @formal reason: Dynamic path: terminal execution evidence advances the existing outcome lifecycle
// @formal reason: idempotently after commit or restart. Evidence: runtime.outcomes.outcome_lifecycle and
// @formal reason: outcome reconciliation tests; this is a product-requirement classification, not a direct
// @formal reason: proof claim.
//! Recovery driving for Thread-owned Outcome aggregates.
//!
//! The Session root supplies only the durable candidate selector. Active state,
//! transitions, Run identities, and terminal truth remain exclusively in the
//! existing Outcome aggregate and its Thread commits.

use awaken_session_execution_contract::OutcomeDrive;

#[cfg(test)]
use super::mutation::repository_failure;
use super::{SessionApplication, SessionRecoveryCandidates};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OutcomeReconciliation {
    pub settled: usize,
    pub pending: usize,
    pub failures: Vec<(String, String)>,
    pub quarantined: usize,
}

impl SessionApplication {
    /// Continue every possible Outcome through the sole lifecycle supervisor.
    /// Candidate selection is deliberately conservative because copying an
    /// active/terminal marker into the Session root would create a second truth.
    #[cfg(test)]
    pub(crate) async fn reconcile_outcome_continuations(&self) -> OutcomeReconciliation {
        let candidates = match super::recovery_scan::recovery_candidates(self, true).await {
            Ok(candidates) => candidates,
            Err(error) => {
                let mut report = OutcomeReconciliation::default();
                report
                    .failures
                    .push(("<repository>".into(), repository_failure(error).to_string()));
                return report;
            }
        };
        self.reconcile_outcome_continuations_from(&candidates).await
    }

    pub(super) async fn reconcile_outcome_continuations_from(
        &self,
        candidates: &SessionRecoveryCandidates,
    ) -> OutcomeReconciliation {
        let outcome_runtime = self.runtime().outcomes();

        let mut report = OutcomeReconciliation {
            quarantined: candidates.quarantined.len(),
            ..Default::default()
        };
        for candidate in &candidates.sessions {
            let session = match self.session_repository().get(&candidate.session_id).await {
                Ok(session) => session,
                Err(awaken_session_state_contract::SessionRepositoryError::NotFound) => continue,
                Err(error) => {
                    report
                        .failures
                        .push((candidate.session_id.to_string(), error.to_string()));
                    continue;
                }
            };
            if !session.needs_outcome_reconciliation() {
                continue;
            }
            let session_id = session.session_id.clone();
            // A replacement process knows this Session durably but has
            // installed none of its frozen facts. Continuing the Outcome
            // against that blank projection would let Session admission supply
            // the flat-protocol `assistant` default, and the Outcome would pin
            // and grade an Agent the operator never published. Restore the
            // exact dispatch projection first, and leave the aggregate
            // untouched when restoration is impossible.
            let owner = match self.mutations.owner(&session_id).await {
                Ok(owner) => owner,
                Err(error) => {
                    report
                        .failures
                        .push((String::from(session_id), error.to_string()));
                    continue;
                }
            };
            if let Err(error) = self.install_dispatch_projection(&owner, &session).await {
                report
                    .failures
                    .push((String::from(session_id), error.to_string()));
                continue;
            }
            match outcome_runtime.continue_outcome(session_id.as_str()).await {
                Ok(Some(OutcomeDrive::Completed(_))) => report.settled += 1,
                Ok(Some(OutcomeDrive::Awaiting)) => report.pending += 1,
                Ok(None) => {}
                Err(error) => report
                    .failures
                    .push((String::from(session_id), error.to_string())),
            }
        }
        report
    }
}
