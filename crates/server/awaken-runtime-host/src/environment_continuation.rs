//! Runtime-side mechanics for Session Environment checkpoint continuation.
//!
//! The Session application remains the durable lifecycle owner. These helpers
//! only execute one already-fenced quiesce, checkpoint, source-release
//! preparation, physical disposal, restore, or checkpoint-delete effect
//! through the existing Host and provider owners.

use std::time::Duration;

use awaken_session_contract::RunError;

use crate::{ManagedHost, to_run_error};

/// The Runtime Host is the sole adapter from aggregate checkpoint intent to
/// provider byte-custody input. Ordinary suspension and terminal takeover both
/// call this projection so metadata, expiry, and effect identity cannot drift.
pub(super) fn provisioning_checkpoint_request(
    request: &awaken_session_contract::SandboxCheckpointRequest,
) -> awaken_provisioning_contract::SandboxCheckpointRequest {
    awaken_provisioning_contract::SandboxCheckpointRequest {
        workspace_id: request.workspace_id.clone(),
        session_id: request.session_id.clone(),
        generation_id: request.generation.id.clone(),
        environment_fingerprint: request.generation.environment_fingerprint.clone(),
        base_image_fingerprint: request.generation.base_image_fingerprint.clone(),
        effect_id: request.operation.effect_id.clone(),
        format: request.format.clone(),
        created_at_unix_ms: request.created_at_unix_ms,
        expires_at_unix_ms: request.expires_at_unix_ms,
        max_bytes: request.max_bytes,
    }
}

impl ManagedHost {
    /// Revalidate the continuation operation against the process projection's
    /// current live realization and project the one neutral provider fence. A
    /// same-epoch renewal may extend the physical deadline; owner,
    /// incarnation, or epoch replacement fails before provider I/O.
    fn continuation_effect_fence(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
    ) -> Result<awaken_provisioning_contract::SandboxEffectFence, RunError> {
        let asserted = operation.realization.as_ref().ok_or_else(|| {
            RunError::unavailable_classified(
                "session_environment_continuation_unfenced",
                "Session Environment continuation has no realization fence",
            )
        })?;
        let current = self
            .host
            .session_slots
            .read(thread, |slot| slot.realization_lease.clone())
            .flatten()
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_environment_continuation_projection_missing",
                    "Session Environment continuation has no installed realization generation",
                )
            })?;
        let now_unix_ms = crate::terminal_repository_publication::runtime_unix_now_ms();
        if !awaken_session_contract::realization_lease_authorizes(&current, asserted, now_unix_ms) {
            return Err(RunError::unavailable_classified(
                "session_environment_continuation_generation_stale",
                "Session Environment continuation realization was replaced or expired",
            ));
        }
        current
            .sandbox_effect_fence(&operation.effect_id)
            .map_err(|error| RunError::internal(error.to_string()))
    }

    /// Revalidate the exact aggregate-owned preparation effect against the
    /// process projection, then lower that effect once. A later local renewal
    /// may authorize delivery but cannot silently replace the fence persisted
    /// by the preparation receipt.
    fn continuation_preparation_effect_fence(
        &self,
        thread: &str,
        preparation: &awaken_session_contract::SourceReleasePreparationEffect,
    ) -> Result<awaken_provisioning_contract::SandboxEffectFence, RunError> {
        let current = self
            .host
            .session_slots
            .read(thread, |slot| slot.realization_lease.clone())
            .flatten()
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_environment_continuation_projection_missing",
                    "Session Environment preparation has no installed realization generation",
                )
            })?;
        let now_unix_ms = crate::terminal_repository_publication::runtime_unix_now_ms();
        if !awaken_session_contract::realization_lease_authorizes(
            &current,
            preparation.lease(),
            now_unix_ms,
        ) {
            return Err(RunError::unavailable_classified(
                "session_environment_continuation_preparation_generation_stale",
                "Session Environment preparation is not authorized by the current realization",
            ));
        }
        let effect_fence = preparation
            .sandbox_effect_fence()
            .map_err(|error| RunError::internal(error.to_string()))?;
        effect_fence
            .validate_live_at(now_unix_ms)
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        Ok(effect_fence)
    }

    /// Revalidate the aggregate-current successor carried by a durable
    /// Disposing projection, then lower it through the one neutral destructive
    /// authority. Unlike live preparation, this deliberately does not require
    /// the original realization to remain current after a higher-epoch failover.
    fn continuation_source_disposal_authorization(
        &self,
        thread: &str,
        disposal: &awaken_session_contract::SourceReleaseDisposal,
    ) -> Result<awaken_provisioning_contract::SandboxDisposalAuthorization, RunError> {
        let current = self
            .host
            .session_slots
            .read(thread, |slot| slot.realization_lease.clone())
            .flatten()
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_environment_continuation_projection_missing",
                    "Session Environment disposal has no installed realization generation",
                )
            })?;
        let now_unix_ms = crate::terminal_repository_publication::runtime_unix_now_ms();
        if !awaken_session_contract::realization_lease_authorizes(
            &current,
            disposal.current_realization(),
            now_unix_ms,
        ) {
            return Err(RunError::unavailable_classified(
                "session_environment_continuation_disposal_generation_stale",
                "Session Environment disposal is not authorized by the current realization",
            ));
        }
        let provider_authorization = disposal
            .sandbox_disposal_authorization()
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        provider_authorization
            .effect_fence()
            .validate_live_at(now_unix_ms)
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        Ok(provider_authorization)
    }

    pub(super) async fn quiesce_environment_continuation(
        &self,
        thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_contract::SandboxGeneration,
        expected_mcp_generations: &[awaken_session_contract::McpGenerationRef],
    ) -> Result<awaken_session_contract::QuiescenceReceipt, RunError> {
        self.continuation_effect_fence(thread, operation)?;
        let realization = self.host.session_slots.realization_lock(thread);
        let _realization = realization.lock().await;
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
        self.host
            .session_slots
            .close_mcp_realization_admission(
                thread,
                crate::session_slot::McpQuiescenceAdmissionFence::new(
                    operation,
                    source_effect_id,
                    source_binding,
                    generation,
                ),
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
        self.continuation_effect_fence(thread, operation)?;
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
        self.continuation_effect_fence(thread, operation)?;
        let mcp = self
            .host
            .drain_mcp_projections(thread, expected_mcp_generations)
            .await
            .map_err(to_run_error)?;
        self.continuation_effect_fence(thread, operation)?;
        environment.quiesce().await.map_err(|error| {
            RunError::unavailable_classified(
                "session_environment_hand_not_quiescent",
                error.to_string(),
            )
        })?;
        self.continuation_effect_fence(thread, operation)?;
        self.host
            .session_slots
            .modify(thread, |slot| slot.runtime = None);
        Ok(awaken_session_contract::QuiescenceReceipt {
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id.clone(),
            activity_epoch: operation.activity_epoch,
            live_environment_effects: 0,
            mcp_generations: mcp.generations,
        })
    }

    pub(super) async fn checkpoint_environment_continuation(
        &self,
        thread: &str,
        request: awaken_session_contract::SandboxCheckpointRequest,
    ) -> Result<awaken_session_contract::CheckpointReceipt, RunError> {
        if request.session_id != thread {
            return Err(RunError::internal(
                "checkpoint request does not belong to its Session",
            ));
        }
        let effect_fence = self.continuation_effect_fence(thread, &request.operation)?;
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
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
        let provider_request = provisioning_checkpoint_request(&request);
        self.continuation_effect_fence(thread, &request.operation)?;
        let checkpoint = environment
            .checkpoint_for_effect(&provider_request, store.as_ref(), &effect_fence)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        self.continuation_effect_fence(thread, &request.operation)?;
        Ok(awaken_session_contract::CheckpointReceipt {
            effect_id: provider_request.effect_id,
            generation_id: provider_request.generation_id,
            checkpoint,
        })
    }

    pub(super) async fn prepare_environment_continuation_source_release(
        &self,
        thread: &str,
        preparation: &awaken_session_contract::SourceReleasePreparationEffect,
        generation: &awaken_session_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<awaken_session_contract::SourceReleasePreparedReceipt, RunError> {
        self.continuation_preparation_effect_fence(thread, preparation)?;
        let resources = self.host.thread_resource_manifest(thread).ok_or_else(|| {
            RunError::unavailable_classified(
                "session_environment_continuation_resources_missing",
                "Checkpoint source preparation has no frozen active Resource manifest",
            )
        })?;
        let handle =
            serde_json::from_str::<awaken_provisioning_contract::SandboxHandle>(source_binding)
                .map_err(|error| {
                    RunError::unavailable_classified(
                        "session_environment_continuation_binding_invalid",
                        error.to_string(),
                    )
                })?;
        if handle.sandbox_id != thread {
            return Err(RunError::classified(
                "session_environment_continuation_binding_scope_mismatch",
                "Checkpoint source binding belongs to another Session",
            ));
        }
        let materializations = handle.memory_materializations().map_err(|error| {
            RunError::unavailable_classified(
                "session_environment_continuation_memory_invalid",
                error.to_string(),
            )
        })?;
        // Source-release preparation decision table:
        // | Rule | frozen input / handle evidence | provider mode | Effect |
        // | P1 | legacy None + any RW, RW Copy, foreign/noncanonical | any | reject with Artifact/ack/provider-prep/physical-delete all zero |
        // | P2 | exact Some (including empty WTR/FUSE) | Live | publish unscoped CheckpointRelease, ack exact slice, provider prepare, no delete |
        // | P3 | exact Some | RecoverOnly | skip live Artifact, restore exact ack, provider prepare, no delete |
        // | P4 | accepted legacy None (RO-only/no Memory) | Live/RecoverOnly | no ack; otherwise P2/P3 |
        // | P5 | exact physical absence before aggregate preparation | any | fail closed without a preparation receipt |
        // The contract-owned Option join is the only None/Some authority. This
        // pure step precedes provider observation and every externally visible
        // source-dependent effect.
        let acknowledged_materializations =
            awaken_session_contract::validate_continuation_memory_materializations(
                resources.resources.inputs(),
                materializations,
            )
            .map_err(|error| {
                RunError::unavailable_classified(
                    "session_environment_continuation_memory_unreconciled",
                    error.to_string(),
                )
            })?;
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        self.continuation_preparation_effect_fence(thread, preparation)?;
        let effect_fence = self.continuation_preparation_effect_fence(thread, preparation)?;
        let prepared = self
            .host
            .prepare_bound_environment_for_effect_under_lifecycle(
                thread,
                source_binding,
                &effect_fence,
                Some(&resources.resources),
                crate::host::BoundEnvironmentPreparationMode::LiveSource,
            )
            .await
            .map_err(to_run_error)?;
        let Some((environment, _)) = prepared.environment.as_ref() else {
            return Err(RunError::unavailable_classified(
                "session_environment_continuation_source_absent_before_preparation",
                "Checkpoint source is absent without an aggregate-owned preparation fact",
            ));
        };
        if prepared.permits_live_io() {
            self.continuation_preparation_effect_fence(thread, preparation)?;
            self.host
                .artifact_harvester()
                .harvest_with_environment_and_fence(
                    thread,
                    Some(
                        awaken_run_ingress::ArtifactPublicationFence::CheckpointRelease(
                            preparation.operation().clone(),
                        ),
                    ),
                    environment,
                )
                .await
                .map_err(|error| {
                    RunError::unavailable_classified(
                        "session_environment_continuation_artifact_publication_failed",
                        error.to_string(),
                    )
                })?;
        }
        if let Some(materializations) = acknowledged_materializations {
            let effect_fence = self.continuation_preparation_effect_fence(thread, preparation)?;
            environment
                .acknowledge_memory_reconciliation(&effect_fence, materializations)
                .await
                .map_err(|error| {
                    RunError::unavailable_classified(
                        "session_environment_continuation_memory_ack_failed",
                        error.to_string(),
                    )
                })?;
        }
        let effect_fence = self.continuation_preparation_effect_fence(thread, preparation)?;
        let provider_prepared_effect_fence = environment
            .prepare_disposal_for_effect(&effect_fence)
            .await
            .map_err(|error| {
                RunError::unavailable_classified(
                    "session_environment_continuation_source_preparation_failed",
                    error.to_string(),
                )
            })?;
        self.continuation_preparation_effect_fence(thread, preparation)?;
        let receipt = awaken_session_contract::SourceReleasePreparedReceipt::try_new(
            preparation.clone(),
            provider_prepared_effect_fence,
            generation,
            source_binding,
        )
        .map_err(|error| {
            RunError::unavailable_classified(
                "session_environment_continuation_provider_preparation_invalid",
                error.to_string(),
            )
        })?;
        self.host
            .retain_checkpoint_source_environment_for_disposal(
                thread,
                preparation.operation(),
                generation,
                source_binding,
                environment,
            )
            .map_err(to_run_error)?;
        Ok(receipt)
    }

    pub(super) async fn dispose_prepared_environment_continuation_source(
        &self,
        thread: &str,
        disposal: &awaken_session_contract::SourceReleaseDisposal,
    ) -> Result<awaken_session_contract::SourceDisposedReceipt, RunError> {
        let operation = disposal.operation();
        let generation = disposal.generation();
        let source_binding = disposal.source_binding();
        self.continuation_source_disposal_authorization(thread, disposal)?;
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        let provider_authorization =
            self.continuation_source_disposal_authorization(thread, disposal)?;
        self.host
            .dispose_prepared_checkpoint_source_environment(
                thread,
                operation,
                generation,
                source_binding,
                &provider_authorization,
            )
            .await
            .map_err(to_run_error)?;
        self.continuation_source_disposal_authorization(thread, disposal)?;
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
        request
            .validate()
            .map_err(|error| RunError::unavailable(error.to_string()))?;
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
        let (workspace, _, publication, _) = self
            .host
            .resolve_session_publication(&thread, None, None)
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
        let spec = self.host.sandbox_spec_for_provider(&thread, provider);
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
