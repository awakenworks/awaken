//! Managed adapter trigger for the canonical Session WorkQueue reconciler.

use super::ManagedState;

impl ManagedState {
    /// Trigger the application-owned durable Session-to-WorkQueue projection.
    pub async fn reconcile_work_dispatches(&self) -> usize {
        let report = self.application.reconcile_work_dispatches().await;
        for failure in report.failures {
            tracing::warn!(
                session = %failure.session_id,
                environment = %failure.environment_id,
                error = %failure.message,
                "Session WorkQueue dispatch remains pending"
            );
        }
        report.settled
    }
}
