//! Protocol-neutral Run application adapter over the shared runtime host.

use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_session_contract::{
    Pending, RunApplication, RunApplicationError, RunResume, StepOutcome,
};

use crate::host::{HostError, HostErrorKind};
use crate::{HostResume, SharedHost};

fn to_application_error(error: HostError) -> RunApplicationError {
    match error.kind {
        HostErrorKind::BadRequest | HostErrorKind::Conflict => {
            RunApplicationError::bad_request(error.message)
        }
        HostErrorKind::Internal => RunApplicationError::internal(error.message),
        HostErrorKind::Unavailable if error.code == "unavailable" => {
            RunApplicationError::unavailable(error.message)
        }
        HostErrorKind::Unavailable => {
            RunApplicationError::unavailable_classified(error.code, error.message)
        }
    }
}

/// The one [`RunApplication`] adapter wired behind AI SDK, AG-UI, and A2A.
/// Every protocol shares the same [`SharedHost`] and therefore the same thread.
pub struct RunApplicationHost {
    host: Arc<SharedHost>,
}

impl RunApplicationHost {
    #[must_use]
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self { host }
    }
}

#[async_trait::async_trait]
impl RunApplication for RunApplicationHost {
    async fn run(
        &self,
        _operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let result = self
            .host
            .run(agent.as_deref(), thread, messages)
            .await
            .map_err(to_application_error)?;
        crate::step_projection::settled_step(result)
    }

    async fn run_streaming(
        &self,
        _operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let result = self
            .host
            .run_streaming(agent.as_deref(), thread, messages, sink)
            .await
            .map_err(to_application_error)?;
        crate::step_projection::settled_step(result)
    }

    async fn resume(
        &self,
        _operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        let resume = match resume {
            RunResume::Permission(decision) => HostResume::Permission(decision),
            RunResume::ClientResult { content, is_error } => {
                HostResume::ClientResult { content, is_error }
            }
        };
        let result = self
            .host
            .resume(thread, tool_use_id, resume)
            .await
            .map_err(to_application_error)?;
        crate::step_projection::settled_step(result)
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunApplicationError> {
        self.host
            .interrupt(thread)
            .await
            .map_err(to_application_error)
    }

    async fn pending(&self, thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        self.host
            .pending_tool(thread)
            .await
            .map_err(to_application_error)
    }

    async fn history(&self, thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        self.host
            .committed_messages(thread)
            .await
            .map_err(to_application_error)
    }

    fn model(&self) -> String {
        self.host.model()
    }

    async fn usage(&self, thread: &str) -> Result<(u64, u64), RunApplicationError> {
        let usage = self
            .host
            .session_thread_usage(thread, &ThreadId(thread.to_string()))
            .await
            .map_err(to_application_error)?;
        Ok((usage.input_tokens, usage.output_tokens))
    }
}
