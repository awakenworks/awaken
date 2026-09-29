//! A Runtime fixture role for tests whose Runtime has committed no Thread.
//!
//! Admission reads the root Thread's commit cursor before opening a Session
//! interval (the interval's span starts after it). The neutral role reports
//! that read as unsupported; a fixture that only records activations instead
//! answers what its Runtime holds: no committed Run, so the cursor is 0.

/// Implement `ThreadReadRuntime` for a fixture Runtime that has committed no
/// Thread: every read answers "nothing committed", archival is unsupported.
macro_rules! no_committed_threads {
    ($ty:ty) => {
        #[async_trait::async_trait]
        impl awaken_session_execution_contract::ThreadReadRuntime for $ty {
            async fn owns_thread(&self, _thread: &str) -> Result<bool, RunError> {
                Ok(false)
            }

            async fn committed_messages(
                &self,
                _thread: &str,
            ) -> Result<Vec<awaken_agent_contract::agent::message::Message>, RunError> {
                Ok(Vec::new())
            }

            async fn session_thread_recovery_snapshot(
                &self,
                _session_id: &awaken_session_types::SessionId,
                _thread_id: &str,
            ) -> Result<
                Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
                RunError,
            > {
                Ok(None)
            }

            async fn session_thread_run_recovery_snapshot(
                &self,
                _session_id: &awaken_session_types::SessionId,
                _thread_id: &str,
                _run_id: &awaken_agent_contract::agent::run::Id,
            ) -> Result<
                Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
                RunError,
            > {
                Ok(None)
            }

            async fn session_thread_disposition(
                &self,
                _session_id: &awaken_session_types::SessionId,
                _thread_id: &str,
            ) -> Result<awaken_agent_contract::ThreadDisposition, RunError> {
                Ok(awaken_agent_contract::ThreadDisposition::Active)
            }

            async fn archive_session_thread(
                &self,
                _session_id: &awaken_session_types::SessionId,
                _thread_id: &str,
            ) -> Result<(), RunError> {
                Err(RunError::unavailable(
                    "durable coordinated Thread archive is unsupported",
                ))
            }

            async fn committed_outcome_projection(
                &self,
                _thread: &str,
                _outcome_id: &str,
            ) -> Result<Option<awaken_session_execution_contract::CommittedOutcomeProjection>, RunError>
            {
                Ok(None)
            }
        }
    };
}
