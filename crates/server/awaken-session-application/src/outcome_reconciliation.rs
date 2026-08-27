//! Recovery driving for Thread-owned Outcome aggregates.
//!
//! The Session root supplies only the durable candidate selector. Active state,
//! transitions, Run identities, and terminal truth remain exclusively in the
//! existing Outcome aggregate and its Thread commits.

use awaken_session_contract::OutcomeDrive;

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
        let candidates = match self.session_repository().reconcilable_sessions().await {
            Ok(scan) => SessionRecoveryCandidates::from(scan),
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
        let mut report = OutcomeReconciliation {
            quarantined: candidates.quarantined.len(),
            ..Default::default()
        };
        for candidate in &candidates.sessions {
            let session = match self.session_repository().get(&candidate.session_id).await {
                Ok(session) => session,
                Err(awaken_session_contract::SessionRepositoryError::NotFound) => continue,
                Err(error) => {
                    report
                        .failures
                        .push((candidate.session_id.clone(), error.to_string()));
                    continue;
                }
            };
            if !session.needs_outcome_reconciliation() {
                continue;
            }
            let session_id = session.session_id;
            match self.runtime().continue_outcome(&session_id).await {
                Ok(Some(OutcomeDrive::Completed(_))) => report.settled += 1,
                Ok(Some(OutcomeDrive::Awaiting)) => report.pending += 1,
                Ok(None) => {}
                Err(error) => report.failures.push((session_id, error.to_string())),
            }
        }
        report
    }
}
