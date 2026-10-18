// @formal surface requirement=runtime.outcomes.outcome_lifecycle
// @formal reason: Static owner: the Outcome component. It drives the extension-owned Outcome controller over the
// @formal reason: Session's Runs and Grader Runs and projects its results; the Outcome transitions stay in
// @formal reason: awaken-ext-goal. This is a product-requirement classification, not a proof claim.
//! [`Outcomes`] — drives a Session's Outcomes.
//!
//! It prepares and continues the extension-owned Outcome aggregate, running
//! Worker Runs through [`crate::SessionRuns`] and Grader Runs through the same
//! Run boundary, and serves the Session application's `OutcomeRuntime` role.

use std::sync::Arc;

use awaken_session_execution_contract::OutcomeDrive;
use awaken_session_types::RunError;

/// Outcome orchestration for one process. Consumer: the Session application's
/// `OutcomeRuntime` role.
pub struct Outcomes {
    pub(crate) session_runs: Arc<crate::SessionRuns>,
    pub(crate) installed_sessions: Arc<crate::InstalledSessions>,
}

impl Outcomes {
    pub(crate) fn new(session_runs: Arc<crate::SessionRuns>) -> Self {
        Self {
            installed_sessions: session_runs.installed_sessions.clone(),
            session_runs,
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for Outcomes {
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, RunError> {
        crate::managed_outcome::prepare(
            self,
            thread,
            outcome_id,
            description,
            rubric,
            max_iterations,
        )
        .await
    }

    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeDrive, RunError> {
        crate::managed_outcome::define(self, thread, description, rubric, max_iterations).await
    }

    async fn continue_outcome(&self, thread: &str) -> Result<Option<OutcomeDrive>, RunError> {
        crate::managed_outcome::resume(self, thread).await
    }
}
