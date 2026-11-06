// @formal surface requirement=protocol.adapters.adapter_projection
// @formal reason: Cross-owner read composition; Session authorizes original membership and Dispatch projects physical evidence. No second authority or transition.
//! Scope one existing Session/Run before its original physical Dispatch read.
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_dispatch_contract::{DispatchExecutionReads, DispatchOperationalFeed};
use awaken_runtime_contract::runs::RunError;
use awaken_session_application::{
    ProfiledSessionCommands, SessionApplication, SessionProjectionRecoveryError,
};
use awaken_session_application_contract::SessionRunDispatchObservation;
use awaken_session_application_contract::SessionRunDispatchQueries;
use awaken_session_types::SessionId;
use std::sync::Arc;
#[derive(Clone)]
pub(super) struct SessionRunDispatchReads {
    admissions: Arc<dyn ProfiledSessionCommands>,
    dispatch: Arc<dyn DispatchExecutionReads>,
}
impl SessionRunDispatchReads {
    pub(super) fn new(
        sessions: &SessionApplication,
        dispatch: Arc<dyn DispatchExecutionReads>,
    ) -> Result<Self, &'static str> {
        let feed: Arc<dyn DispatchOperationalFeed> = dispatch.clone();
        sessions.execution_admission().verify_dispatch(&feed)?;
        Ok(Self {
            admissions: sessions.run_admissions(),
            dispatch,
        })
    }
}
#[async_trait::async_trait]
impl SessionRunDispatchQueries for SessionRunDispatchReads {
    async fn observe_for_owner(
        &self,
        owner: &str,
        session: &SessionId,
        run: &RunId,
    ) -> Result<SessionRunDispatchObservation, RunError> {
        self.admissions
            .session_run_events_after(
                owner,
                session,
                run,
                Default::default(),
                Default::default(),
                0,
            )
            .await
            .map_err(|error| match error {
                SessionProjectionRecoveryError::NotFound => {
                    RunError::bad_request_classified("not_found", "Session was not found")
                }
                SessionProjectionRecoveryError::Rejected(error) => error,
                SessionProjectionRecoveryError::Unavailable(message) => {
                    RunError::unavailable(message)
                }
            })?;
        let phase = self
            .dispatch
            .observe_execution(run, &awaken_agent_contract::ThreadId(session.to_string()))
            .await
            .map_err(|error| match error {
                awaken_dispatch_contract::DispatchError::Rejected(_) => {
                    RunError::bad_request_classified("not_found", "Run was not found")
                }
                error => RunError::unavailable(error.to_string()),
            })?;
        Ok(SessionRunDispatchObservation {
            session_id: session.to_string(),
            run_id: run.clone(),
            phase,
        })
    }
}
#[cfg(test)]
#[path = "session_dispatch_reads_tests.rs"]
mod tests;
