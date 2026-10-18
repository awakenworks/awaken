// @formal entrypoint runtime.settled_step_projection_is_closed_exact_and_non_widening
//! The protocol-neutral Run application behind AI SDK, AG-UI and A2A, served
//! by [`SessionRuns`]: every protocol drives the same Session Runs.

use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_session_application_contract::{
    Pending, RunApplication, RunApplicationError, RunResume, StepOutcome,
};

use super::SessionRuns;
use crate::HostResume;
use crate::session_execution_error::to_run_error;

#[async_trait::async_trait]
impl RunApplication for SessionRuns {
    async fn run(
        &self,
        _operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let result = self
            .run(agent.as_deref(), thread, messages)
            .await
            .map_err(to_run_error)?;
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
            .run_streaming(agent.as_deref(), thread, messages, sink)
            .await
            .map_err(to_run_error)?;
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
            .resume(thread, tool_use_id, resume)
            .await
            .map_err(to_run_error)?;
        crate::step_projection::settled_step(result)
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunApplicationError> {
        self.interrupt(thread).await.map_err(to_run_error)
    }

    async fn pending(&self, thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        self.thread_reads
            .pending_tool(thread)
            .await
            .map_err(to_run_error)
    }

    async fn history(&self, thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        self.thread_reads
            .committed_messages(thread)
            .await
            .map_err(to_run_error)
    }

    fn model(&self) -> String {
        self.capabilities.model()
    }

    async fn usage(&self, thread: &str) -> Result<(u64, u64), RunApplicationError> {
        let usage = self
            .thread_reads
            .session_thread_usage(thread, &ThreadId(thread.to_string()))
            .await
            .map_err(to_run_error)?;
        Ok((usage.input_tokens, usage.output_tokens))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionExecutionError;
    use awaken_session_types::RunErrorKind;

    #[test]
    fn host_failures_keep_their_fault_for_every_run_protocol() {
        // Decision table: cause = the Host fault; effect = the Run
        // application error AI SDK, AG-UI and A2A project. A state conflict
        // stays a conflict (409); the other faults keep their kind and code.
        // | Rule | Host fault | Run kind | code |
        // | A1 | BadRequest | BadRequest | invalid_request |
        // | A2 | Conflict | Conflict | conflict |
        // | A3 | Internal | Internal | internal |
        // | A4 | Unavailable (classified) | Unavailable | its own code |
        // | A5 | Internal (classified) | Internal | its own code |
        // | A6 | BadRequest (classified) | BadRequest | its own code, including not_found |
        // | A7 | Conflict (classified) | Conflict | its own code |
        // | A8 | Unavailable (default) | Unavailable | unavailable |
        let rows = [
            (
                "A1",
                SessionExecutionError::bad_request("m"),
                RunErrorKind::BadRequest,
                "invalid_request",
            ),
            (
                "A2",
                SessionExecutionError::conflict("m"),
                RunErrorKind::Conflict,
                "conflict",
            ),
            (
                "A3",
                SessionExecutionError::internal("m"),
                RunErrorKind::Internal,
                "internal",
            ),
            (
                "A4",
                SessionExecutionError::unavailable_classified("dependency", "m"),
                RunErrorKind::Unavailable,
                "dependency",
            ),
            (
                "A5",
                SessionExecutionError::classified("classified_internal", "m"),
                RunErrorKind::Internal,
                "classified_internal",
            ),
            (
                "A6",
                crate::session_execution_error::from_run_error(
                    awaken_session_types::RunError::bad_request_classified("not_found", "m"),
                ),
                RunErrorKind::BadRequest,
                "not_found",
            ),
            (
                "A7",
                crate::session_execution_error::from_run_error(
                    awaken_session_types::RunError::conflict_classified("projection_changed", "m"),
                ),
                RunErrorKind::Conflict,
                "projection_changed",
            ),
            (
                "A8",
                SessionExecutionError::unavailable("m"),
                RunErrorKind::Unavailable,
                "unavailable",
            ),
        ];
        for (rule, error, kind, code) in rows {
            let projected = to_run_error(error);
            assert_eq!(projected.kind, kind, "{rule}");
            assert_eq!(projected.code, code, "{rule}");
            assert_eq!(projected.message, "m", "{rule}");
        }
    }
}
