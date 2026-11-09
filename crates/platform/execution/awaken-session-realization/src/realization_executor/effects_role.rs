// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the realization effects role the realization executor serves itself. Each
// @formal reason: method performs one aggregate-authored realization effect (layout check, continuation
// @formal reason: checkpoint/restore, Resource inputs, terminal Repository publication and disposal); the
// @formal reason: decisions stay in the Session aggregate.
//! The [`RealizationEffects`](awaken_session_execution_contract::RealizationEffects)
//! role, served by the realization executor: the effects a Session application
//! asks for that need no Session context, Memory, MCP or delegated-Run fact.

use awaken_runtime_contract::runs::RunError;

use super::RealizationExecutor;

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationEffects for RealizationExecutor {
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), RunError> {
        self.validate_prospective_session_layout(thread, layout)
    }

    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_contract::SandboxCheckpointRequest,
    ) -> Result<awaken_session_contract::CheckpointReceipt, RunError> {
        self.checkpoint_environment_continuation(thread, request)
            .await
    }

    async fn prepare_checkpoint_source_disposal(
        &self,
        thread: &str,
        preparation: &awaken_session_contract::SourceReleasePreparationEffect,
        generation: &awaken_session_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<awaken_session_contract::SourceReleasePreparedReceipt, RunError> {
        self.prepare_environment_continuation_source_release(
            thread,
            preparation,
            generation,
            source_binding,
        )
        .await
    }

    async fn dispose_prepared_checkpoint_source(
        &self,
        thread: &str,
        disposal: &awaken_session_contract::SourceReleaseDisposal,
    ) -> Result<awaken_session_contract::SourceDisposedReceipt, RunError> {
        self.dispose_prepared_environment_continuation_source(thread, disposal)
            .await
    }

    async fn restore_checkpointed_session_environment(
        &self,
        request: awaken_provisioning_contract::SandboxRestoreRequest,
        realization: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<awaken_session_contract::RestoreReceipt, RunError> {
        self.restore_environment_continuation(request, realization)
            .await
    }

    async fn delete_session_checkpoint(
        &self,
        _thread: &str,
        checkpoint: &awaken_provisioning_contract::SandboxCheckpointRef,
    ) -> Result<(), RunError> {
        self.delete_environment_continuation_checkpoint(checkpoint)
            .await
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        transition: &awaken_session_configuration_contract::resource::SessionResourceTransition,
    ) -> Result<(), RunError> {
        self.apply_session_inputs_with_context(thread, transition, None, None)
            .await
    }

    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: awaken_session_contract::SessionRepositoryPublicationCommand,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<awaken_session_contract::SessionRepositoryPublicationEffect, RunError> {
        self.effects()
            .publish_terminal_repository(self.peers(), command, lease)
            .await
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_contract::SessionCleanupDisposalReceipt, RunError> {
        self.effects()
            .dispose_terminal_cleanup_effect(self.peers(), effect)
            .await
            .map_err(RunError::from)
    }
}
