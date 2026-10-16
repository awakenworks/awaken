// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Runtime-side mechanics for Session Environment checkpoint continuation.
//!
//! The Session application remains the durable lifecycle owner. These helpers
//! execute checkpoint capture, physical source disposal, restore, and checkpoint
//! deletion from aggregate-owned requests
//! through the realization executor and its provider owners. Host-level Memory,
//! MCP, delegated-Run and publication coordination stays in the Host adapter.

use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_realization_contract::ResourceOperationFence;

use awaken_session_types::RunError;

use crate::host_error::to_run_error;
use crate::realization_executor::RealizationExecutor;

/// The Runtime Host is the sole adapter from aggregate checkpoint intent to
/// provider byte-custody input. Ordinary suspension and terminal takeover both
/// call this projection so metadata, expiry, and effect identity cannot drift.
/// Session activity/source/lease coordinates stay on the aggregate-side request;
/// this capture input neither grants authority nor replaces the separate fence.
pub fn provisioning_checkpoint_request(
    request: &awaken_session_state_contract::SandboxCheckpointRequest,
) -> awaken_provisioning_contract::SandboxCheckpointCaptureRequest {
    awaken_provisioning_contract::SandboxCheckpointCaptureRequest {
        workspace_id: request.workspace_id.clone(),
        session_id: request.session_id.to_string(),
        generation_id: request.generation.id().to_owned(),
        environment_fingerprint: request.generation.environment_fingerprint().to_owned(),
        base_image_fingerprint: request.generation.base_image_fingerprint().to_owned(),
        effect_id: request.operation.effect_id.clone(),
        format: request.format.clone(),
        created_at_unix_ms: request.created_at_unix_ms,
        expires_at_unix_ms: request.expires_at_unix_ms,
        max_bytes: request.max_bytes,
    }
}

impl RealizationExecutor {
    /// All continuation effects read the same Session slot admission decision.
    /// The returned fence remains in Control coordinates; provider validation
    /// is a separate boundary, not a reason to reinterpret expiry locally.
    fn continuation_current_lease(
        &self,
        thread: &str,
        asserted: &session_resource::SessionRealizationLease,
        stale_code: &'static str,
    ) -> Result<session_resource::SessionRealizationLease, RunError> {
        let realizations = self.sessions.clone();
        let (current, live) = realizations
            .read_realization(thread, |r| {
                r.execution.authority.lease().map(|current| {
                    (
                        current.clone(),
                        r.execution.authority.remaining_for(asserted).is_some(),
                    )
                })
            })
            .flatten()
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_environment_continuation_projection_missing",
                    "Session Environment continuation has no installed realization generation",
                )
            })?;
        if !live {
            return Err(RunError::unavailable_classified(
                stale_code,
                "Session Environment continuation realization was replaced or expired",
            ));
        }
        Ok(current)
    }

    /// Revalidate the continuation operation against the process projection's
    /// current live realization and project the one neutral provider fence. A
    /// same-epoch renewal may extend the physical deadline; owner,
    /// incarnation, or epoch replacement fails before provider I/O.
    pub fn continuation_effect_fence(
        &self,
        thread: &str,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
    ) -> Result<awaken_provisioning_contract::SandboxEffectFence, RunError> {
        let asserted = operation.realization.as_ref().ok_or_else(|| {
            RunError::unavailable_classified(
                "session_environment_continuation_unfenced",
                "Session Environment continuation has no realization fence",
            )
        })?;
        let current = self.continuation_current_lease(
            thread,
            asserted,
            "session_environment_continuation_generation_stale",
        )?;
        current
            .sandbox_effect_fence(&operation.effect_id)
            .map_err(|error| RunError::internal(error.to_string()))
    }

    /// Revalidate the exact aggregate-owned preparation effect against the
    /// process projection, then lower that effect once. A later local renewal
    /// may authorize delivery but cannot silently replace the fence persisted
    /// by the preparation receipt.
    pub fn continuation_preparation_effect_fence(
        &self,
        thread: &str,
        preparation: &awaken_session_state_contract::SourceReleasePreparationEffect,
    ) -> Result<awaken_provisioning_contract::SandboxEffectFence, RunError> {
        self.continuation_current_lease(
            thread,
            preparation.lease(),
            "session_environment_continuation_preparation_generation_stale",
        )?;
        let effect_fence = preparation
            .sandbox_effect_fence()
            .map_err(|error| RunError::internal(error.to_string()))?;
        effect_fence
            .validate_live_at(awaken_time::WallClockExt::now_ms(self.clock.as_ref()))
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
        disposal: &awaken_session_state_contract::SourceReleaseDisposal,
    ) -> Result<awaken_provisioning_contract::SandboxDisposalAuthorization, RunError> {
        self.continuation_current_lease(
            thread,
            disposal.current_realization(),
            "session_environment_continuation_disposal_generation_stale",
        )?;
        let provider_authorization = disposal
            .sandbox_disposal_authorization()
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        provider_authorization
            .effect_fence()
            .validate_live_at(awaken_time::WallClockExt::now_ms(self.clock.as_ref()))
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        Ok(provider_authorization)
    }

    pub async fn checkpoint_environment_continuation(
        &self,
        thread: &str,
        request: awaken_session_state_contract::SandboxCheckpointRequest,
    ) -> Result<awaken_session_state_contract::CheckpointReceipt, RunError> {
        let realizations = self.sessions.clone();
        if request.session_id != thread {
            return Err(RunError::internal(
                "checkpoint request does not belong to its Session",
            ));
        }
        let effect_fence = self.continuation_effect_fence(thread, &request.operation)?;
        // Capture the original Worker/Session roots before awaiting lifecycle
        // ownership; a same-byte replacement must not rebind this operation.
        let authority = self
            .execution
            .provider_effect_authority(thread, &effect_fence)
            .map_err(to_run_error)?;
        let _lifecycle = realizations.lock_lifecycle(thread).await;
        let environment = self
            .resident_checkpoint_source_environment(
                thread,
                &request.operation,
                &request.source_effect_id,
                &request.source_binding,
                &request.generation,
            )
            .map_err(to_run_error)?;
        let store = self
            .env
            .environment_checkpoint_store
            .as_ref()
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_environment_checkpoint_store_unavailable",
                    "No Session environment checkpoint store is installed",
                )
            })?;
        let provider_request = provisioning_checkpoint_request(&request);
        awaken_provisioning_contract::with_current_sandbox_effect(
            &effect_fence,
            &authority,
            |_| Ok(()),
        )
        .map_err(|error| RunError::unavailable(error.to_string()))?;
        let checkpoint = environment
            .lifecycle()
            .checkpoint_for_effect(&provider_request, store.as_ref(), &effect_fence, &authority)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        awaken_provisioning_contract::with_current_sandbox_effect(&effect_fence, &authority, |_| {
            Ok(awaken_session_state_contract::CheckpointReceipt {
                effect_id: provider_request.effect_id,
                generation_id: provider_request.generation_id,
                checkpoint,
            })
        })
        .map_err(|error| RunError::unavailable(error.to_string()))
    }

    pub async fn prepare_environment_continuation_source_release(
        &self,
        thread: &str,
        preparation: &awaken_session_state_contract::SourceReleasePreparationEffect,
        generation: &awaken_session_state_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<awaken_session_state_contract::SourceReleasePreparedReceipt, RunError> {
        let exec = &self.environments;
        self.continuation_preparation_effect_fence(thread, preparation)?;
        let Some(resources) = exec.staging.thread_resource_manifest(thread) else {
            return Err(RunError::unavailable_classified(
                "session_environment_continuation_resources_missing",
                "Checkpoint source preparation has no frozen active Resource manifest",
            ));
        };
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
            awaken_session_state_contract::validate_continuation_memory_materializations(
                resources.resources.inputs(),
                materializations,
            )
            .map_err(|error| {
                RunError::unavailable_classified(
                    "session_environment_continuation_memory_unreconciled",
                    error.to_string(),
                )
            })?;
        let _lifecycle = exec.sessions.lock_lifecycle(thread).await;
        self.continuation_preparation_effect_fence(thread, preparation)?;
        let effect_fence = self.continuation_preparation_effect_fence(thread, preparation)?;
        let prepared = self
            .prepare_bound_environment_for_effect_under_lifecycle(
                &self.cleanup,
                thread,
                source_binding,
                &effect_fence,
                Some(&resources.resources),
                crate::session_environment::BoundEnvironmentPreparationMode::LiveSource,
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
            self.cleanup
                .artifacts
                .harvest_with_environment_and_fence(
                    thread,
                    Some(ResourceOperationFence::CheckpointRelease(
                        preparation.operation().clone(),
                    )),
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
            let authority = self
                .execution
                .provider_effect_authority(thread, &effect_fence)
                .map_err(to_run_error)?;
            environment
                .lifecycle()
                .acknowledge_memory_reconciliation(&effect_fence, materializations, &authority)
                .await
                .map_err(|error| {
                    RunError::unavailable_classified(
                        "session_environment_continuation_memory_ack_failed",
                        error.to_string(),
                    )
                })?;
        }
        let effect_fence = self.continuation_preparation_effect_fence(thread, preparation)?;
        let authority = self
            .execution
            .provider_effect_authority(thread, &effect_fence)
            .map_err(to_run_error)?;
        let provider_prepared_effect_fence = environment
            .lifecycle()
            .prepare_disposal_for_effect(&effect_fence, &authority)
            .await
            .map_err(|error| {
                RunError::unavailable_classified(
                    "session_environment_continuation_source_preparation_failed",
                    error.to_string(),
                )
            })?;
        self.continuation_preparation_effect_fence(thread, preparation)?;
        let receipt = awaken_session_state_contract::SourceReleasePreparedReceipt::try_new(
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
        exec.retain_checkpoint_source_environment_for_disposal(
            thread,
            preparation.operation(),
            generation,
            source_binding,
            environment,
        )
        .map_err(to_run_error)?;
        Ok(receipt)
    }

    pub async fn dispose_prepared_environment_continuation_source(
        &self,
        thread: &str,
        disposal: &awaken_session_state_contract::SourceReleaseDisposal,
    ) -> Result<awaken_session_state_contract::SourceDisposedReceipt, RunError> {
        let realizations = self.sessions.clone();
        let operation = disposal.operation();
        let generation = disposal.generation();
        let source_binding = disposal.source_binding();
        self.continuation_source_disposal_authorization(thread, disposal)?;
        let _lifecycle = realizations.lock_lifecycle(thread).await;
        let provider_authorization =
            self.continuation_source_disposal_authorization(thread, disposal)?;
        self.dispose_prepared_checkpoint_source_environment(
            thread,
            operation,
            generation,
            source_binding,
            &provider_authorization,
        )
        .await
        .map_err(to_run_error)?;
        self.continuation_source_disposal_authorization(thread, disposal)?;
        Ok(awaken_session_state_contract::SourceDisposedReceipt {
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id().to_owned(),
            source_binding: source_binding.to_string(),
            terminated: true,
        })
    }

    pub async fn restore_environment_continuation(
        &self,
        request: awaken_session_state_contract::SandboxRestoreRequest,
    ) -> Result<awaken_session_state_contract::RestoreReceipt, RunError> {
        let realizations = self.sessions.clone();
        let exec = self;
        request
            .validate()
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        let thread = request.session_id.clone();
        let _lifecycle = realizations.lock_lifecycle(&thread).await;
        exec.retry_unpublished_session_environment_cleanup(&thread)
            .await
            .map_err(to_run_error)?;
        exec.begin_session_environment_restore(&thread, &request)
            .map_err(to_run_error)?;
        let store = exec
            .env
            .environment_checkpoint_store
            .as_ref()
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_environment_checkpoint_store_unavailable",
                    "No Session environment checkpoint store is installed",
                )
            })?;
        let (workspace, _, publication, _) = self
            .publication
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
        let provider = exec
            .session_environment_provider_for_thread(&thread, provisioning)
            .map_err(to_run_error)?;
        let spec = self.staging.sandbox_spec_for_provider(&thread, provider);
        let handle = provider
            .terminal()
            .restore(&spec, &request, store.as_ref())
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        let binding = serde_json::to_string(&handle)
            .map_err(|error| RunError::internal(error.to_string()))?;
        Ok(awaken_session_state_contract::RestoreReceipt {
            effect_id: request.effect_id,
            generation_id: request.generation_id,
            checkpoint_id: request.checkpoint.id,
            binding,
        })
    }

    pub async fn delete_environment_continuation_checkpoint(
        &self,
        checkpoint: &awaken_session_state_contract::SandboxCheckpointRef,
    ) -> Result<(), RunError> {
        let store = self
            .env
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
