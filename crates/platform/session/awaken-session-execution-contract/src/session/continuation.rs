// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Stateless dispatch of an aggregate-authored continuation instruction through the
// @formal reason: existing effect ports. It keeps no phase, lease, object or receipt state.
//! Shared provider effects for the root's continuation instructions.
//! Host quiescence keeps its Memory/MCP/Run coordination at the existing port.

use crate::{RealizationEffects, SessionContinuationReceipt as R};
use awaken_runtime_contract::runs::RunError;
use awaken_session_configuration_contract::resource::SessionRealizationLease;
use awaken_session_contract::{SessionEnvironmentInstruction as I, SessionEnvironmentState};

/// Execute one instruction from immutable aggregate data. This function owns
/// neither the next phase nor receipt application and keeps no retry state.
pub async fn execute_environment_provider_continuation(
    workspace: &str,
    session: &awaken_session_types::SessionId,
    environment: &SessionEnvironmentState,
    policy: &awaken_provisioning_contract::SandboxIdleRetentionPolicy,
    delivery: Option<&SessionRealizationLease>,
    instruction: I,
    effects: &dyn RealizationEffects,
) -> Result<R, RunError> {
    let moved = || RunError::unavailable("continuation no longer names the decided effect");
    let thread = session.as_str();
    match instruction {
        I::Checkpoint => {
            let request = environment
                .checkpoint_request(workspace, session, policy)
                .map_err(|e| RunError::unavailable(e.to_string()))?
                .ok_or_else(moved)?;
            tokio::time::timeout(
                std::time::Duration::from_secs(policy.max_checkpoint_duration_secs),
                effects.checkpoint_session_environment(thread, request),
            )
            .await
            .map_err(|_| {
                RunError::unavailable_classified(
                    "session_environment_checkpoint_timeout",
                    "Session environment checkpoint exceeded its frozen duration bound",
                )
            })?
            .map(R::Checkpointed)
        }
        I::PrepareSourceRelease => {
            let (_, _, binding, generation) = environment.suspend_source().ok_or_else(moved)?;
            let preparation = environment
                .source_release_preparation_effect(delivery.ok_or_else(moved)?)
                .map_err(|e| RunError::unavailable(e.to_string()))?;
            effects
                .prepare_checkpoint_source_disposal(thread, &preparation, generation, binding)
                .await
                .map(R::SourceReleasePrepared)
        }
        I::DisposeSource => {
            let disposal = environment
                .source_release_disposal(delivery.ok_or_else(moved)?)
                .map_err(|e| RunError::unavailable(e.to_string()))?;
            effects
                .dispose_prepared_checkpoint_source(thread, &disposal)
                .await
                .map(R::SourceDisposed)
        }
        I::Restore => effects
            .restore_checkpointed_session_environment(
                environment
                    .restoring_request(workspace, session)
                    .ok_or_else(moved)?,
                delivery.ok_or_else(moved)?,
            )
            .await
            .map(R::Restored),
        I::DeleteExpiredCheckpoint => {
            let checkpoint = environment.checkpoint().ok_or_else(moved)?;
            effects
                .delete_session_checkpoint(thread, checkpoint)
                .await?;
            Ok(R::ExpiredCheckpointDeleted {
                checkpoint_id: checkpoint.id.clone(),
            })
        }
        I::Quiesce | I::Materialize(_) | I::Retire => Err(RunError::bad_request(
            "instruction is not a provider continuation effect",
        )),
    }
}
