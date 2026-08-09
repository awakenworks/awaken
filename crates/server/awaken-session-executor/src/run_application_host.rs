//! Protocol-neutral Run application adapter over the shared runtime host.

use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::RunState;
use awaken_session_contract::{
    Pending, RunApplication, RunApplicationError, RunResume, StepOutcome,
};

use crate::host::{HostError, HostErrorKind, PendingTool, RunResult};
use crate::{HostResume, SharedHost};

fn to_application_error(error: HostError) -> RunApplicationError {
    match error.kind {
        HostErrorKind::BadRequest | HostErrorKind::Conflict => {
            RunApplicationError::bad_request(error.message)
        }
        HostErrorKind::Internal => RunApplicationError::internal(error.message),
    }
}

fn to_pending(pending: Option<PendingTool>) -> Option<Pending> {
    pending.map(|pending| Pending {
        tool_use_id: pending.tool_use_id,
        name: pending.name,
        input: pending.input,
        client_executed: pending.client_executed,
    })
}

fn to_step_outcome(result: RunResult) -> StepOutcome {
    let run_id = result.run_id;
    match result.state {
        RunState::Awaiting => StepOutcome::awaiting(
            result.new_messages,
            to_pending(result.pending),
            result.compacted,
            result.rescheduled,
        )
        .with_delegated_runs(result.delegated_runs)
        .with_run_id(run_id),
        RunState::Ended(cause) => StepOutcome::ended(
            result.new_messages,
            cause,
            result.compacted,
            result.rescheduled,
        )
        .with_delegated_runs(result.delegated_runs)
        .with_run_id(run_id),
        RunState::Running => unreachable!("settled host result cannot remain Running"),
    }
}

/// The one [`RunApplication`] adapter wired behind AI SDK, AG-UI, and A2A.
/// Every protocol shares the same [`SharedHost`] and therefore the same thread.
pub struct RunApplicationHost {
    host: Arc<SharedHost>,
    session_defaults: Option<Arc<dyn SessionDefaultsPreparer>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Session defaults could not be prepared: {0}")]
pub struct SessionDefaultsPreparationError(pub String);

#[async_trait::async_trait]
pub trait SessionDefaultsPreparer: Send + Sync {
    async fn prepare(
        &self,
        workspace_id: &str,
        thread_id: &str,
        agent_id: &str,
    ) -> Result<(), SessionDefaultsPreparationError>;
}

impl RunApplicationHost {
    #[must_use]
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self {
            host,
            session_defaults: None,
        }
    }

    #[must_use]
    pub fn with_session_defaults(mut self, preparer: Arc<dyn SessionDefaultsPreparer>) -> Self {
        self.session_defaults = Some(preparer);
        self
    }

    async fn prepare_defaults(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<(), RunApplicationError> {
        let Some(preparer) = &self.session_defaults else {
            return Ok(());
        };
        let projected_agent = self.host.thread_agent_projection(thread);
        preparer
            .prepare(
                &self.host.thread_workspace(thread),
                thread,
                agent.or(projected_agent.as_deref()).unwrap_or("assistant"),
            )
            .await
            .map_err(|error| RunApplicationError::bad_request(error.to_string()))
    }
}

#[async_trait::async_trait]
impl RunApplication for RunApplicationHost {
    async fn run(
        &self,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.prepare_defaults(thread, agent.as_deref()).await?;
        let result = self
            .host
            .run(agent.as_deref(), thread, messages)
            .await
            .map_err(to_application_error)?;
        Ok(to_step_outcome(result))
    }

    async fn run_streaming(
        &self,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.prepare_defaults(thread, agent.as_deref()).await?;
        let result = self
            .host
            .run_streaming(agent.as_deref(), thread, messages, sink)
            .await
            .map_err(to_application_error)?;
        Ok(to_step_outcome(result))
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        let resume = match resume {
            RunResume::Confirm { allow, note } => HostResume::ToolPermission { allow, note },
            RunResume::ClientResult { content, is_error } => {
                HostResume::ClientResult { content, is_error }
            }
        };
        let result = self
            .host
            .resume(thread, tool_use_id, resume)
            .await
            .map_err(to_application_error)?;
        Ok(to_step_outcome(result))
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunApplicationError> {
        self.host
            .interrupt(thread)
            .await
            .map_err(to_application_error)
    }

    async fn pending(&self, thread: &str) -> Option<Pending> {
        self.host
            .pending_tool(thread)
            .await
            .ok()
            .and_then(to_pending)
    }

    async fn history(&self, thread: &str) -> Vec<Message> {
        self.host.committed_messages(thread).await
    }

    fn model(&self) -> String {
        self.host.model()
    }

    async fn usage(&self, thread: &str) -> (u64, u64) {
        let usage = self.host.thread_usage(thread).await.total();
        (usage.prompt_tokens, usage.completion_tokens)
    }
}
