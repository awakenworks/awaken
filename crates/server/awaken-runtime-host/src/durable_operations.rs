//! Adapter exposing Host run/session orchestration through the neutral Run Ingress port.

use awaken_agent_contract::agent::message::Message;
use awaken_run_ingress::{
    ApplicationError, DurableDispatchStatus, DurableRunOperations, DurableSupersedeResult,
};

use crate::{HostError, HostErrorKind, SharedHost};

fn map_error(error: HostError) -> ApplicationError {
    match error.kind {
        HostErrorKind::BadRequest => ApplicationError::invalid(error.message),
        HostErrorKind::Conflict => ApplicationError::conflict(error.message),
        HostErrorKind::Internal => ApplicationError::internal(error.message),
    }
}

#[async_trait::async_trait]
impl DurableRunOperations for SharedHost {
    async fn submit_background(
        &self,
        agent: Option<&str>,
        thread: &str,
        messages: Vec<Message>,
    ) -> Result<String, ApplicationError> {
        self.submit_background_async(agent, thread, messages)
            .await
            .map_err(map_error)
    }

    async fn cancel(&self, thread: &str, run_id: &str) -> Result<(), ApplicationError> {
        self.cancel_durable(thread, run_id).await.map_err(map_error)
    }

    async fn pause(&self, thread: &str, run_id: Option<&str>) -> Result<String, ApplicationError> {
        self.pause_durable(thread, run_id).await.map_err(map_error)
    }

    async fn resume(&self, thread: &str, text: String) -> Result<String, ApplicationError> {
        self.stage_manual_resume(thread, text)
            .await
            .map_err(map_error)
    }

    async fn wake(&self, thread: &str, run_id: &str) -> Result<(), ApplicationError> {
        self.wake_durable(thread, run_id).await.map_err(map_error)
    }

    async fn deliver(&self, thread: &str, allow: bool) -> Result<String, ApplicationError> {
        self.stage_decision(thread, allow).await.map_err(map_error)
    }

    async fn supersede(
        &self,
        agent: Option<&str>,
        thread: &str,
        messages: Vec<Message>,
    ) -> Result<DurableSupersedeResult, ApplicationError> {
        let turn = self
            .supersede_run(agent, thread, messages)
            .await
            .map_err(map_error)?;
        let superseded = self.superseded(thread).await.map_err(map_error)?;
        Ok(DurableSupersedeResult {
            state: format!("{:?}", turn.state),
            superseded,
        })
    }

    async fn messages(&self, thread: &str) -> Result<Vec<Message>, ApplicationError> {
        Ok(self.committed_messages(thread).await)
    }

    async fn superseded(&self, thread: &str) -> Result<Vec<String>, ApplicationError> {
        self.superseded(thread).await.map_err(map_error)
    }

    async fn dispatches(
        &self,
        thread: &str,
    ) -> Result<Vec<DurableDispatchStatus>, ApplicationError> {
        self.list_dispatches(thread)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(
                        |(run_id, status, attempts, sandbox_bound)| DurableDispatchStatus {
                            run_id,
                            status,
                            attempts,
                            sandbox_bound,
                        },
                    )
                    .collect()
            })
            .map_err(map_error)
    }

    async fn reconcile(&self, thread: &str) -> Result<Vec<String>, ApplicationError> {
        self.reconcile(thread).await.map_err(map_error)
    }

    async fn reap(
        &self,
        thread: &str,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, ApplicationError> {
        self.reap(thread, max_attempts, now_ms)
            .await
            .map_err(map_error)
    }

    async fn dead_letters(&self, thread: &str) -> Result<Vec<String>, ApplicationError> {
        self.dead_letters(thread).await.map_err(map_error)
    }

    async fn requeue_dead_letter(
        &self,
        thread: &str,
        run_id: &str,
    ) -> Result<bool, ApplicationError> {
        self.requeue_dead_letter(thread, run_id)
            .await
            .map_err(map_error)
    }

    async fn purge_dead_letters(&self, thread: &str) -> Result<usize, ApplicationError> {
        self.purge_dead_letters(thread).await.map_err(map_error)
    }
}
