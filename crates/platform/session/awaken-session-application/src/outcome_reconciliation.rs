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
use std::sync::Arc;

use super::SessionRecoveryCandidates;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OutcomeReconciliation {
    pub settled: usize,
    pub pending: usize,
    pub failures: Vec<(String, String)>,
    pub quarantined: usize,
}

/// Recovery of committed Thread Outcomes whose continuation has not yet been
/// admitted: re-install the Session projection and continue the Outcome.
pub struct SessionOutcomeReconciliation {
    mutations: Arc<crate::SessionMutations>,
    realizations: Arc<crate::SessionRealizations>,
    outcomes: Arc<dyn awaken_session_execution_contract::Outcomes>,
    run_admissions: Arc<crate::SessionRunAdmissions>,
}

#[cfg(test)]
impl crate::SessionApplication {
    /// Continue every possible Outcome through the sole lifecycle supervisor.
    /// Candidate selection is deliberately conservative because copying an
    /// active/terminal marker into the Session root would create a second truth.
    pub(crate) async fn reconcile_outcome_continuations(&self) -> OutcomeReconciliation {
        let candidates = match super::recovery_scan::recovery_candidates(
            self.mutations.repository().as_ref(),
            self.clock.as_ref(),
            true,
        )
        .await
        {
            Ok(candidates) => candidates,
            Err(error) => {
                let mut report = OutcomeReconciliation::default();
                report
                    .failures
                    .push(("<repository>".into(), repository_failure(error).to_string()));
                return report;
            }
        };
        self.outcome_reconciliation.continue_from(&candidates).await
    }
}

impl SessionOutcomeReconciliation {
    pub(crate) fn new(
        mutations: Arc<crate::SessionMutations>,
        realizations: Arc<crate::SessionRealizations>,
        run_admissions: Arc<crate::SessionRunAdmissions>,
        runtime: &crate::SessionExecutionRoles,
    ) -> Self {
        Self {
            mutations,
            realizations,
            run_admissions,
            outcomes: runtime.outcomes.clone(),
        }
    }

    pub(crate) async fn continue_from(
        &self,
        candidates: &SessionRecoveryCandidates,
    ) -> OutcomeReconciliation {
        let outcome_runtime = self.outcomes.as_ref();

        let mut report = OutcomeReconciliation {
            quarantined: candidates.quarantined.len(),
            ..Default::default()
        };
        for candidate in &candidates.sessions {
            let session = match self.mutations.repository().get(&candidate.session_id).await {
                Ok(session) => session,
                Err(awaken_session_contract::SessionRepositoryError::NotFound) => continue,
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
            if let Err(error) = self
                .realizations
                .install_dispatch_projection(&owner, &session)
                .await
            {
                report
                    .failures
                    .push((String::from(session_id), error.to_string()));
                continue;
            }
            let admission = self
                .run_admissions
                .outcome_run_admission(session_id.as_str());
            match outcome_runtime
                .continue_outcome(session_id.as_str(), &admission)
                .await
            {
                Ok(Some(OutcomeDrive::Completed(_))) => report.settled += 1,
                Ok(Some(OutcomeDrive::Awaiting)) => report.pending += 1,
                Ok(None) => {}
                Err(error) => report
                    .failures
                    .push((session_id.to_string(), error.to_string())),
            }
        }
        report
    }
}
