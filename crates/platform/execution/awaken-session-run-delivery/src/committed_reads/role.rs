// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: the ThreadReads role served by CommittedReads. Each method reads committed
// @formal reason: Thread truth through CommittedReads and projects its error once; it holds no state and no write
// @formal reason: path. This is a product-requirement classification, not a direct proof claim.
//! The Session application's `ThreadReads` role, served by [`CommittedReads`].

use awaken_runtime_contract::runs::RunError;

use super::CommittedReads;

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReads for CommittedReads {
    async fn transcript_page(
        &self,
        session: &awaken_session_types::SessionId,
        request: &awaken_agent_contract::thread::read::transcript::TranscriptPageRequest,
    ) -> Result<awaken_agent_contract::thread::read::transcript::TranscriptPage, RunError> {
        self.transcript_page(session.as_str(), request).await
    }

    async fn owns_thread(&self, thread: &str) -> Result<bool, RunError> {
        self.has_durable_thread(thread).await
    }

    /// Committed transcript from durable truth, so the adapter can rehydrate a
    /// session lost to a process restart and resume its awaiting run (ADR-0039).
    async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_agent_contract::Message>, RunError> {
        self.committed_messages(thread).await
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
            .await?;
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
    }

    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<awaken_session_execution_contract::CommittedOutcomeProjection>, RunError>
    {
        self.committed_outcome(thread, outcome_id)
            .await
            .map(|terminal| terminal.map(crate::committed_outcome_projection))
    }
}
