// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Coordinate Run, Memory and MCP quiescence before checkpoint capture.
//!
//! Memory background work, MCP generations, committed delegated Runs and Agent
//! publication selection belong to their existing owners. Checkpoint and source
//! release effects execute on RealizationExecutor after these participants drain.

use crate::host_error::to_run_error;
use awaken_session_configuration_contract::mcp as session_mcp;
use awaken_session_types::RunError;
use std::time::Duration;

impl crate::RunAssembly {
    pub(crate) async fn quiesce_environment_continuation(
        &self,
        thread: &str,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_state_contract::SandboxGeneration,
        expected_mcp_generations: &[session_mcp::McpGenerationRef],
    ) -> Result<awaken_session_state_contract::QuiescenceReceipt, RunError> {
        let sessions = &self.installed_sessions;
        let realization_view = sessions.realizations();
        self.realization_executor
            .continuation_effect_fence(thread, operation)?;
        let _realization = sessions.acquire_realization_with(thread, |_| ()).await.0;
        let _lifecycle = realization_view.lock_lifecycle(thread).await?;
        let environment = self
            .realization_executor
            .environments()
            .resident_checkpoint_source_environment(
                thread,
                operation,
                source_effect_id,
                source_binding,
                generation,
            )
            .map_err(RunError::from)?;
        realization_view
            .close_mcp_realization_admission(
                thread,
                awaken_session_mcp::McpQuiescenceAdmissionFence::new(
                    operation,
                    source_effect_id,
                    source_binding,
                    generation,
                ),
            )
            .map_err(RunError::from)?;
        if crate::session_realizations::primary_or_delegated_run_active(
            thread,
            |session| {
                Ok(sessions
                    .read_run(session, |run| {
                        run.context
                            .as_ref()
                            .is_some_and(|runtime| runtime.has_active_run())
                    })
                    .unwrap_or(false))
            },
            self.thread_reads.delegated_runs_under_lifecycle(thread),
        )
        .await
        .map_err(to_run_error)?
        {
            return Err(RunError::unavailable_classified(
                "session_environment_not_quiescent",
                "Primary or delegated work still owns the Session environment",
            ));
        }
        self.realization_executor
            .continuation_effect_fence(thread, operation)?;
        if !self
            .resources
            .memory
            .background()
            .quiesce_shared_environment(thread, generation.id(), Duration::from_secs(30))
            .await
        {
            return Err(RunError::unavailable_classified(
                "session_environment_background_not_quiescent",
                "Shared-Environment background work did not reach a durable boundary",
            ));
        }
        self.realization_executor
            .continuation_effect_fence(thread, operation)?;
        let mcp = self
            .drain_mcp_projections(thread, expected_mcp_generations)
            .await
            .map_err(to_run_error)?;
        self.realization_executor
            .continuation_effect_fence(thread, operation)?;
        environment.lifecycle().quiesce().await.map_err(|error| {
            RunError::unavailable_classified(
                "session_environment_hand_not_quiescent",
                error.to_string(),
            )
        })?;
        self.realization_executor
            .continuation_effect_fence(thread, operation)?;
        sessions.modify_run(thread, |run| run.context = None);
        Ok(awaken_session_state_contract::QuiescenceReceipt {
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id().to_owned(),
            activity_epoch: operation.activity_epoch,
            live_environment_effects: 0,
            mcp_generations: mcp.generations,
        })
    }
}
