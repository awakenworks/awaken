//! Recovery driving for Thread-owned Outcome aggregates.
//!
//! The Session root supplies only the durable candidate selector. Active state,
//! transitions, Run identities, and terminal truth remain exclusively in the
//! existing Outcome aggregate and its Thread commits.

use awaken_session_contract::OutcomeDrive;

use super::{SessionApplication, mutation::repository_failure};

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
    pub(crate) async fn reconcile_outcome_continuations(&self) -> OutcomeReconciliation {
        let mut report = OutcomeReconciliation::default();
        let scan = match self.session_repository().reconcilable_sessions().await {
            Ok(scan) => scan,
            Err(error) => {
                report
                    .failures
                    .push(("<repository>".into(), repository_failure(error).to_string()));
                return report;
            }
        };
        report.quarantined = scan.quarantined.len();
        for scoped in scan.sessions {
            if !scoped.session.needs_outcome_reconciliation() {
                continue;
            }
            let session_id = scoped.session.session_id;
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
