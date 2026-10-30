// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: Static owner: the Session application's Run roles served by SessionRuns. Each method revalidates
// @formal reason: the Session's live Resource bindings, delivers through SessionRuns and projects the
// @formal reason: committed step once; it chooses no lifecycle transition.
//! The Session application's `SessionRuns` and `StreamingRuns` roles,
//! served by [`SessionRuns`].

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_application_contract::{Pending, StepOutcome};
use awaken_session_execution_contract::ToolPermissionDecision;
use awaken_session_types::RunError;

use super::SessionRuns;
use crate::HostResume;
use crate::managed_input_projection::user_message;
use crate::session_execution_error::to_run_error;

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionRuns for SessionRuns {
    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        self.run_assembly
            .realization_executor
            .validate_thread_resource_bindings(thread)
            .await?;
        let result = self
            .run(Some(agent), thread, vec![user_message(content)])
            .await;
        crate::step_projection::finish_managed_step(result)
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        self.run_assembly
            .realization_executor
            .validate_thread_resource_bindings(thread)
            .await?;
        let result = self
            .resume(thread, tool_use_id, HostResume::Permission(decision))
            .await;
        crate::step_projection::finish_managed_step(result)
    }

    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        self.run_assembly
            .realization_executor
            .validate_thread_resource_bindings(thread)
            .await?;
        let result = self
            .resume(
                thread,
                tool_use_id,
                HostResume::ClientResult { content, is_error },
            )
            .await;
        crate::step_projection::finish_managed_step(result)
    }

    fn model(&self) -> String {
        self.capabilities.model()
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::StreamingRuns for SessionRuns {
    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
    ) -> Result<StepOutcome, RunError> {
        self.run_assembly
            .realization_executor
            .validate_thread_resource_bindings(thread)
            .await?;
        let result = self
            .run_attributed(
                Some(agent),
                thread,
                vec![user_message(content)],
                data_subject_id.map(awaken_runtime_contract::DataSubjectId),
            )
            .await;
        crate::step_projection::finish_managed_step(result)
    }

    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.run_assembly
            .realization_executor
            .validate_thread_resource_bindings(thread)
            .await?;
        // Same committed Run as `run`; `sink` mirrors in-flight `stream::Kind` so
        // the Managed adapter can project live `agent.message` previews.
        let result = self
            .run_streaming(Some(agent), thread, vec![user_message(content)], sink)
            .await;
        crate::step_projection::finish_managed_step(result)
    }

    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.run_assembly
            .realization_executor
            .validate_thread_resource_bindings(thread)
            .await?;
        let result = self
            .run_streaming_attributed(
                Some(agent),
                thread,
                vec![user_message(content)],
                sink,
                data_subject_id.map(awaken_runtime_contract::DataSubjectId),
            )
            .await;
        crate::step_projection::finish_managed_step(result)
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.interrupt(thread).await.map_err(to_run_error)
    }

    /// Replace only the already-selected model projection for one Session and
    /// force its next context construction to consume that exact selection.
    async fn rebind_model(&self, thread: &str, model: &str) -> Result<(), RunError> {
        // R5: re-stage the thread's model and evict its cached context so the next
        // Run rebuilds with the newly resolved executor (native switch is O(1); an
        // ACP thread's cached context relaunches its CLI on rebuild).
        self.capabilities
            .register_thread_model(thread, model)
            .map_err(crate::installed_session::registry_run_error)?;
        self.run_assembly
            .realization_executor
            .environments()
            .evict_session_for_rebuild(thread)
            .await?;
        Ok(())
    }

    async fn supports_mid_conversation_system(&self, thread: &str) -> Result<bool, RunError> {
        let model = self.capabilities.model_for_thread(thread)?;
        Ok(awaken_session_run_delivery::supports_mid_conversation_system(&model))
    }

    async fn pending_tool(&self, thread: &str) -> Result<Option<Pending>, RunError> {
        self.thread_reads.pending_tool(thread).await
    }
}
