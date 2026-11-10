// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Coordinate Run, Memory and MCP quiescence before checkpoint capture.
//!
//! Memory background work, MCP generations, committed delegated Runs and Agent
//! publication selection belong to their existing owners. Checkpoint and source
//! release effects execute on RealizationExecutor after these participants drain.

use crate::session_execution_error::to_run_error;
use awaken_runtime_contract::runs::RunError;
use awaken_session_configuration_contract::mcp as session_mcp;
use std::time::Duration;

impl crate::RunAssembly {
    pub(crate) async fn quiesce_environment_continuation(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_contract::SandboxGeneration,
        expected_mcp_generations: &[session_mcp::McpGenerationRef],
    ) -> Result<awaken_session_contract::QuiescenceReceipt, RunError> {
        self.realization_executor
            .continuation_effect_fence(thread, operation)?;
        let realization = self
            .installed_sessions
            .acquire_realization_with(thread, |_| ())
            .await
            .map_err(|error| {
                awaken_runtime_contract::runs::RunError::from(
                    crate::realization_error::RealizationError::from(error),
                )
            })?
            .0;
        quiesce_environment_continuation_with_permit(
            self,
            thread,
            operation,
            source_effect_id,
            source_binding,
            generation,
            expected_mcp_generations,
            &realization,
        )
        .await
    }
}

pub(crate) fn borrowed_permit_is_current(
    sessions: &crate::installed_session::InstalledSessions,
    session: &str,
    permit: &awaken_session_execution_contract::SessionRealizationPermit,
) -> Result<bool, crate::realization_error::RealizationLock> {
    if permit.session_id().as_str() != session {
        return Ok(false);
    }
    Ok(sessions
        .modify(session, |installed| {
            permit.registered_identity_matches::<Option<std::sync::Arc<tokio::sync::Mutex<()>>>>(
                |identity| {
                    identity.as_ref().is_some_and(|identity| {
                        std::sync::Arc::ptr_eq(&installed.realization.execution().phase, identity)
                    })
                },
            )
        })?
        .unwrap_or(false))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn quiesce_environment_continuation_with_permit(
    host: &crate::RunAssembly,
    thread: &str,
    operation: &awaken_session_contract::SessionEnvironmentOperation,
    source_effect_id: &str,
    source_binding: &str,
    generation: &awaken_session_contract::SandboxGeneration,
    expected_mcp_generations: &[session_mcp::McpGenerationRef],
    permit: &awaken_session_execution_contract::SessionRealizationPermit,
) -> Result<awaken_session_contract::QuiescenceReceipt, RunError> {
    let sessions = &host.installed_sessions;
    if !borrowed_permit_is_current(sessions, thread, permit)
        .map_err(crate::realization_error::RealizationError::from)?
    {
        return Err(RunError::unavailable_classified(
            "session_realization_permit_stale",
            "quiescence requires this installed Session's held phase permit",
        ));
    }
    let realization_view = sessions.realizations();
    host.realization_executor
        .continuation_effect_fence(thread, operation)?;
    let lifecycle = realization_view.lock_lifecycle(thread).await?;
    let environment = host
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
                })?
                .unwrap_or(false))
        },
        host.commit_source
            .delegated_runs_under_lifecycle(&host.thread_reads, lifecycle.held()),
    )
    .await
    .map_err(to_run_error)?
    {
        return Err(RunError::unavailable_classified(
            "session_environment_not_quiescent",
            "Primary or delegated work still owns the Session environment",
        ));
    }
    host.realization_executor
        .continuation_effect_fence(thread, operation)?;
    if !host
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
    host.realization_executor
        .continuation_effect_fence(thread, operation)?;
    let mcp = host
        .drain_mcp_projections(thread, expected_mcp_generations)
        .await
        .map_err(to_run_error)?;
    host.realization_executor
        .continuation_effect_fence(thread, operation)?;
    environment.lifecycle().quiesce().await.map_err(|error| {
        RunError::unavailable_classified(
            "session_environment_hand_not_quiescent",
            error.to_string(),
        )
    })?;
    host.realization_executor
        .continuation_effect_fence(thread, operation)?;
    sessions
        .modify_run(thread, |run| run.context = None)
        .map_err(crate::installed_session::registry_run_error)?;
    Ok(awaken_session_contract::QuiescenceReceipt {
        effect_id: operation.effect_id().to_owned(),
        generation_id: generation.id().to_owned(),
        activity_epoch: operation.activity_epoch(),
        live_environment_effects: 0,
        mcp_generations: mcp.generations,
    })
}
