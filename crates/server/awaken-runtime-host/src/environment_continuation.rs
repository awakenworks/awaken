//! Runtime-side mechanics for Session Environment checkpoint continuation.
//!
//! The Session application remains the durable lifecycle owner. These helpers
//! only execute one already-fenced quiesce, checkpoint, dispose, restore, or
//! checkpoint-delete effect through the existing Host and provider owners.

use std::time::Duration;

use awaken_session_contract::RunError;

use crate::{ManagedHost, to_run_error};

impl ManagedHost {
    fn restoration_provider_and_spec(
        &self,
        request: &awaken_session_contract::SandboxRestoreRequest,
    ) -> Result<
        (
            &crate::session_environment::SessionEnvironmentProvider,
            awaken_provisioning_contract::SandboxSpec,
        ),
        RunError,
    > {
        request
            .validate()
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        let (workspace, _, publication) = self
            .host
            .resolve_session_publication(&request.session_id, None, None)
            .map_err(to_run_error)?;
        if workspace != request.workspace_id {
            return Err(RunError::unavailable(
                "restore request Workspace does not match the Session projection",
            ));
        }
        let provisioning = publication
            .as_ref()
            .map(|snapshot| snapshot.resolved_spec.model_binding.provisioning())
            .unwrap_or(&awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor);
        let provider = self
            .host
            .session_environment_provider(provisioning)
            .map_err(to_run_error)?;
        Ok((provider, self.host.sandbox_spec(&request.session_id)))
    }

    pub(super) async fn quiesce_environment_continuation(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_contract::SandboxGeneration,
    ) -> Result<awaken_session_contract::QuiescenceReceipt, RunError> {
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        let environment = self
            .host
            .resident_checkpoint_source_environment(
                thread,
                operation,
                source_effect_id,
                source_binding,
                generation,
            )
            .map_err(to_run_error)?;
        let primary_active = self
            .host
            .session_slots
            .read(thread, |slot| {
                slot.runtime.as_ref().is_some_and(|runtime| {
                    runtime
                        .active_run
                        .lock()
                        .expect("active run mutex poisoned")
                        .is_some()
                })
            })
            .unwrap_or(false);
        let delegated = self
            .host
            .delegated_runs_under_lifecycle(thread)
            .await
            .map_err(to_run_error)?;
        let delegated_active = delegated.iter().any(|run| {
            run.status.occupies_parallel_slot()
                && self
                    .host
                    .session_slots
                    .read(&run.run_id.0, |slot| {
                        slot.runtime.as_ref().is_some_and(|runtime| {
                            runtime
                                .active_run
                                .lock()
                                .expect("active run mutex poisoned")
                                .is_some()
                        })
                    })
                    .unwrap_or(false)
        });
        if primary_active || delegated_active {
            return Err(RunError::unavailable_classified(
                "session_environment_not_quiescent",
                "Primary or delegated work still owns the Session environment",
            ));
        }
        if !self
            .host
            .memory
            .background()
            .quiesce_shared_environment(thread, &generation.id, Duration::from_secs(30))
            .await
        {
            return Err(RunError::unavailable_classified(
                "session_environment_background_not_quiescent",
                "Shared-Environment background work did not reach a durable boundary",
            ));
        }
        self.host.stop_session_mcp_processes(thread).await;
        environment.quiesce().await;
        self.host
            .session_slots
            .modify(thread, |slot| slot.runtime = None);
        Ok(awaken_session_contract::QuiescenceReceipt {
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id.clone(),
            activity_epoch: operation.activity_epoch,
            live_environment_effects: 0,
        })
    }

    pub(super) async fn checkpoint_environment_continuation(
        &self,
        thread: &str,
        request: awaken_session_contract::SandboxCheckpointRequest,
    ) -> Result<awaken_session_contract::CheckpointReceipt, RunError> {
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        if request.session_id != thread {
            return Err(RunError::internal(
                "checkpoint request does not belong to its Session",
            ));
        }
        let environment = self
            .host
            .resident_checkpoint_source_environment(
                thread,
                &request.operation,
                &request.source_effect_id,
                &request.source_binding,
                &request.generation,
            )
            .map_err(to_run_error)?;
        let store = self
            .host
            .environment_checkpoint_store
            .as_ref()
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_environment_checkpoint_store_unavailable",
                    "No Session environment checkpoint store is installed",
                )
            })?;
        let provider_request = awaken_provisioning_contract::SandboxCheckpointRequest {
            workspace_id: request.workspace_id,
            session_id: request.session_id,
            generation_id: request.generation.id.clone(),
            environment_fingerprint: request.generation.environment_fingerprint.clone(),
            base_image_fingerprint: request.generation.base_image_fingerprint.clone(),
            effect_id: request.operation.effect_id.clone(),
            format: request.format,
            created_at_unix_ms: request.created_at_unix_ms,
            expires_at_unix_ms: request.expires_at_unix_ms,
            max_bytes: request.max_bytes,
        };
        let checkpoint = environment
            .checkpoint(&provider_request, store.as_ref())
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        Ok(awaken_session_contract::CheckpointReceipt {
            effect_id: provider_request.effect_id,
            generation_id: provider_request.generation_id,
            checkpoint,
        })
    }

    pub(super) async fn dispose_environment_continuation_source(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        generation: &awaken_session_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<awaken_session_contract::SourceDisposedReceipt, RunError> {
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        self.host
            .dispose_checkpoint_source_environment(
                thread,
                operation,
                source_effect_id,
                generation,
                source_binding,
            )
            .await
            .map_err(to_run_error)?;
        Ok(awaken_session_contract::SourceDisposedReceipt {
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id.clone(),
            source_binding: source_binding.to_string(),
            terminated: true,
        })
    }

    pub(super) async fn restore_environment_continuation(
        &self,
        request: awaken_session_contract::SandboxRestoreRequest,
    ) -> Result<awaken_session_contract::RestoreReceipt, RunError> {
        let thread = request.session_id.clone();
        let lifecycle = self
            .host
            .session_slots
            .update(&thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        self.host
            .retry_unpublished_session_environment_cleanup(&thread)
            .await
            .map_err(to_run_error)?;
        self.host
            .begin_session_environment_restore(&thread, &request)
            .map_err(to_run_error)?;
        let store = self
            .host
            .environment_checkpoint_store
            .as_ref()
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_environment_checkpoint_store_unavailable",
                    "No Session environment checkpoint store is installed",
                )
            })?;
        let (provider, spec) = self.restoration_provider_and_spec(&request)?;
        let handle = provider
            .restore(&spec, &request, store.as_ref())
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        let binding = serde_json::to_string(&handle)
            .map_err(|error| RunError::internal(error.to_string()))?;
        Ok(awaken_session_contract::RestoreReceipt {
            effect_id: request.effect_id,
            generation_id: request.generation_id,
            checkpoint_id: request.checkpoint.id,
            binding,
        })
    }

    /// Dispose only the unpublished physical target named by the durable
    /// `Restoring` tuple. The Session cleanup operation remains the sole retry
    /// and completion authority; this adapter owns no side journal or slot.
    pub(super) async fn dispose_restoring_environment_continuation(
        &self,
        request: &awaken_session_contract::SandboxRestoreRequest,
    ) -> Result<(), RunError> {
        // The terminal cleanup caller owns the Session lifecycle guard across
        // exact target disposal and generic owner removal.
        self.host
            .begin_session_environment_restore(&request.session_id, request)
            .map_err(to_run_error)?;
        let (provider, spec) = self.restoration_provider_and_spec(request)?;
        provider
            .dispose_restored(&spec, request)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        self.host
            .complete_session_environment_restore_target_disposal(&request.session_id, request)
            .map_err(to_run_error)
    }

    pub(super) async fn delete_environment_continuation_checkpoint(
        &self,
        checkpoint: &awaken_session_contract::SandboxCheckpointRef,
    ) -> Result<(), RunError> {
        let store = self
            .host
            .environment_checkpoint_store
            .as_ref()
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_environment_checkpoint_store_unavailable",
                    "No Session environment checkpoint store is installed",
                )
            })?;
        store
            .delete(&checkpoint.id)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))
    }
}
