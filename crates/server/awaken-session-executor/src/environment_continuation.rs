//! Runtime-side mechanics for Session Environment checkpoint continuation.
//!
//! The Session application remains the durable lifecycle owner. These helpers
//! only execute one already-fenced quiesce, checkpoint, dispose, restore, or
//! checkpoint-delete effect through the existing Host and provider owners.

use std::sync::Arc;
use std::time::Duration;

use awaken_session_contract::{McpAttachmentRealizer as _, RunError};

use crate::{ManagedHost, to_run_error};

impl ManagedHost {
    pub(super) async fn quiesce_environment_continuation(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
    ) -> Result<awaken_session_contract::QuiescenceReceipt, RunError> {
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
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
            .delegated_runs(thread)
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
        let environment = self
            .host
            .session_slots
            .read(thread, |slot| slot.environment.clone())
            .flatten()
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_environment_missing",
                    "Resident Session environment is unavailable for checkpoint",
                )
            })?;
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
        let environment = self
            .host
            .session_slots
            .read(thread, |slot| slot.environment.clone())
            .flatten()
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_environment_missing",
                    "Resident Session environment is unavailable for checkpoint",
                )
            })?;
        environment
            .checkpoint(&request, store.as_ref())
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))
    }

    pub(super) async fn dispose_environment_continuation_source(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<awaken_session_contract::SourceDisposedReceipt, RunError> {
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        let environment = self
            .host
            .session_slots
            .read(thread, |slot| slot.environment.clone())
            .flatten();
        if let Some(environment) = environment {
            let binding = serde_json::to_string(&environment.handle())
                .map_err(|error| RunError::internal(error.to_string()))?;
            if binding != source_binding {
                return Err(RunError::classified(
                    "session_environment_stale_source",
                    "Checkpoint disposal was fenced by another environment binding",
                ));
            }
            self.host.session_slots.modify(thread, |slot| {
                slot.runtime = None;
                slot.environment = None;
                slot.expected_environment_binding = None;
            });
            environment
                .dispose()
                .await
                .map_err(|error| RunError::unavailable(error.to_string()))?;
            if environment
                .status()
                .await
                .map_err(|error| RunError::unavailable(error.to_string()))?
                != awaken_provisioning_contract::SandboxStatus::Terminated
            {
                return Err(RunError::unavailable(
                    "Checkpoint source still reports a live Sandbox",
                ));
            }
        }
        Ok(awaken_session_contract::SourceDisposedReceipt {
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id.clone(),
            source_binding: source_binding.to_string(),
            terminated: true,
        })
    }

    pub(super) async fn restore_environment_continuation(
        &self,
        agent: &str,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
        checkpoint: &awaken_session_contract::SandboxCheckpointRef,
    ) -> Result<awaken_session_contract::RestoreReceipt, RunError> {
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let lifecycle_guard = lifecycle.lock().await;
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
        let (_, _, publication) = self
            .host
            .resolve_session_publication(thread, Some(agent), None)
            .map_err(to_run_error)?;
        let provisioning = publication
            .as_ref()
            .map(|snapshot| &snapshot.resolved_spec.model_binding.provisioning)
            .unwrap_or(&awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor);
        let provider = self
            .host
            .session_environment_provider(provisioning)
            .map_err(to_run_error)?;
        let environment = Arc::new(
            provider
                .restore(&self.host.sandbox_spec(thread), checkpoint, store.as_ref())
                .await
                .map_err(|error| RunError::unavailable(error.to_string()))?,
        );
        let binding = serde_json::to_string(&environment.handle())
            .map_err(|error| RunError::internal(error.to_string()))?;
        let requests = self.host.session_slots.update(thread, |slot| {
            let requests = slot
                .mcp
                .iter()
                .map(|projection| projection.request.clone())
                .collect::<Vec<_>>();
            slot.mcp.clear();
            slot.runtime = None;
            slot.environment = Some(environment);
            slot.expected_environment_binding = Some(binding.clone());
            requests
        });
        drop(lifecycle_guard);
        for request in requests {
            if let Err(error) = self.stage_mcp_attachment(request).await {
                let failed = self.host.session_slots.modify(thread, |slot| {
                    slot.runtime = None;
                    slot.expected_environment_binding = None;
                    slot.environment.take()
                });
                if let Some(Some(environment)) = failed {
                    let _ = environment.dispose().await;
                }
                return Err(error);
            }
        }
        Ok(awaken_session_contract::RestoreReceipt {
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id.clone(),
            checkpoint_id: checkpoint.id.clone(),
            binding,
        })
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
