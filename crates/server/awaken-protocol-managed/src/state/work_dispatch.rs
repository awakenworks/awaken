// @formal surface exclude=application_adapter
// @formal owner crates/server/awaken-session-application/src/realization.rs
// @formal reason: Triggers the application-owned WorkQueue reconciler and logs its report; it makes no
// @formal reason: dispatch state or settlement decision.
//! Managed adapter trigger for the canonical Session WorkQueue reconciler.

use super::ManagedState;

impl ManagedState {
    /// Fixture driver for the application-owned durable Session-to-WorkQueue
    /// projection. Product composition runs it in the lifecycle supervisor.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn reconcile_work_dispatches(&self) -> usize {
        let report = self.application.work_assignments().reconcile().await;
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
