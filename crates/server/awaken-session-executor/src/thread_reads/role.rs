// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: the ThreadReadRuntime role served by ThreadReads. Each method reads committed
// @formal reason: Thread truth through ThreadReads and projects its error once; it holds no state and no write
// @formal reason: path. This is a product-requirement classification, not a direct proof claim.
//! The Session application's `ThreadReadRuntime` role, served by [`ThreadReads`].

use awaken_session_types::RunError;

use crate::managed_adapter_error::to_run_error;

use super::ThreadReads;

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for ThreadReads {
    async fn owns_thread(&self, thread: &str) -> Result<bool, RunError> {
        self.has_durable_thread(thread).await.map_err(to_run_error)
    }

    /// Committed transcript from durable truth, so the adapter can rehydrate a
    /// session lost to a process restart and resume its awaiting run (ADR-0039).
    async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_agent_contract::Message>, RunError> {
        self.committed_messages(thread).await.map_err(to_run_error)
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        self.session_thread_recovery_snapshot(
            session_id.as_str(),
            &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
        )
        .await
        .map_err(to_run_error)
    }

    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        let snapshot = self
            .session_thread_run_recovery_snapshot(
                session_id.as_str(),
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                run_id,
            )
            .await
            .map_err(to_run_error)?;
        awaken_session_execution_contract::select_recovery_snapshot_run(Some(snapshot), run_id)
    }

    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, RunError> {
        self.session_thread_disposition(
            session_id.as_str(),
            &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
        )
        .await
        .map_err(to_run_error)
    }

    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<awaken_session_execution_contract::CommittedOutcomeProjection>, RunError> {
        self.committed_outcome(thread, outcome_id)
            .await
            .map(|terminal| terminal.map(crate::outcome_report::committed_projection))
            .map_err(to_run_error)
    }
}
