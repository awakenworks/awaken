// @formal surface obligation=resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal reason: Existing terminal takeover and handle reconstruction share the same provider authority,
// @formal reason: participant verification and UID disposal. Derived empty membership is not a cleanup receipt.
//! Durable container-handle validation and physical locator decoding.

use crate::ContainerHandleCodec as _;
use crate::runtime::container_realization_fingerprint;
use crate::{
    ContainerCleanupState, ContainerProvider, ContainerRuntime, ContainerSandbox,
    MissingHandleTerminalEvidence,
};
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::{SandboxAdoption, SandboxObservationPurpose};

pub(crate) type ContainerHandleV1 = crate::ContainerSandboxHandleV1;

pub(crate) fn decode_handle(
    handle: &pc::SandboxHandle,
) -> Result<ContainerHandleV1, pc::SandboxError> {
    let payload = handle.container_payload()?;
    if payload
        .runtime_handle
        .as_ref()
        .is_some_and(crate::ContainerContinuationHandle::is_host_bind_restoration)
    {
        return Err(pc::SandboxError::unsupported(
            "host-bind restoration requires the gated restoring provider",
        ));
    }
    Ok(payload)
}

/// Lower the one verified original realization into its existing terminal
/// handle. Control identity is a Pod fact, never permission to bind a channel.
pub fn terminal_handle(
    spec: &pc::SandboxSpec,
    container_id: String,
    runtime_handle: crate::ContainerContinuationHandle,
    adoption_fingerprint: pc::SandboxRealizationFingerprint,
    realization_fingerprint: pc::SandboxRealizationFingerprint,
    live_input_projection: bool,
) -> Result<pc::SandboxHandle, pc::SandboxError> {
    let sandbox_control_incarnation = if spec.control_services.is_empty() {
        None
    } else {
        match &runtime_handle {
            crate::ContainerContinuationHandle::KubernetesContinuationV3 {
                pod_uid,
                participants,
                ..
            } if !participants.terminal_only() && participants.restoration().is_none() => Some(
                crate::SandboxControlIncarnation::kubernetes_pod(pod_uid.clone())?,
            ),
            _ => {
                return Err(pc::SandboxError::invalid(
                    "terminal Control reconstruction lacks its complete original Pod realization",
                ));
            }
        }
    };
    let owned_paths = source_mount_paths(spec);
    Ok(pc::SandboxHandle::container_v2(
        &spec.scope,
        crate::ContainerSandboxHandleV2 {
            previous: crate::ContainerSandboxHandleV1 {
                container_id,
                outputs_path: spec.outputs_path.clone(),
                base_env: spec.env.clone(),
                live_input_projection,
                continuation_excluded_paths: owned_paths.clone(),
                runtime_handle: Some(runtime_handle),
                sandbox_control_incarnation,
                control_services: spec.control_services.clone(),
            },
            adoption_fingerprint,
            realization_fingerprint,
            owned_paths,
        },
    ))
}

pub fn source_mount_paths(spec: &pc::SandboxSpec) -> Vec<String> {
    spec.mounts.iter().map(|m| m.mount_path.clone()).collect()
}

struct ReconstructedTerminalHandle {
    handle: pc::SandboxHandle,
    guarded_claim: bool,
}

impl<R: ContainerRuntime + 'static> ContainerProvider<R> {
    pub(super) fn normalized_terminal_handle(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
    ) -> Result<Option<pc::SandboxHandle>, pc::SandboxError> {
        let Some(handle) = handle else {
            return Ok(None);
        };
        let (plan, adoption) = self.adoption_plan(spec)?;
        let mut decoded = crate::DecodedContainerHandle::decode(handle)?;
        let Some(runtime_handle) = self
            .runtime
            .terminal_continuation(&plan, &adoption, &decoded)
            .map_err(pc::SandboxError::from)?
        else {
            return Ok(None);
        };
        decoded.payload.runtime_handle = Some(runtime_handle);
        Ok(Some(pc::SandboxHandle::container_v2(
            &handle.sandbox_id,
            crate::ContainerSandboxHandleV2 {
                previous: decoded.payload,
                adoption_fingerprint: decoded.adoption_fingerprint.ok_or_else(|| {
                    pc::SandboxError::invalid("terminal normalization lost adoption evidence")
                })?,
                realization_fingerprint: decoded.realization_fingerprint.ok_or_else(|| {
                    pc::SandboxError::invalid("terminal normalization lost realization evidence")
                })?,
                owned_paths: handle
                    .owned_paths()
                    .ok_or_else(|| {
                        pc::SandboxError::invalid("terminal normalization lost owned-path evidence")
                    })?
                    .to_vec(),
            },
        )))
    }

    async fn reconstruct_missing_terminal_handle(
        &self,
        spec: &pc::SandboxSpec,
        expected_effect_fence: Option<&pc::SandboxEffectFence>,
        admit: &(impl Fn() -> Result<(), pc::SandboxError> + Sync),
    ) -> Result<Option<ReconstructedTerminalHandle>, pc::SandboxError> {
        let (plan, adoption_fingerprint) = self.adoption_plan(spec)?;
        let evidence = self
            .runtime
            .missing_handle_terminal_evidence(&spec.scope, &plan, expected_effect_fence)
            .await
            .map_err(pc::SandboxError::from)?;
        admit()?;
        let (container_id, runtime_handle) = match evidence {
            MissingHandleTerminalEvidence::Unmaterialized
            | MissingHandleTerminalEvidence::Isolated => return Ok(None),
            MissingHandleTerminalEvidence::Unknown => {
                return Err(pc::SandboxError::invalid(
                    "container provider cannot dispose an in-flight restore without a durable handle",
                ));
            }
            MissingHandleTerminalEvidence::ExactIncarnation {
                container_id,
                runtime_handle,
            } => (container_id, runtime_handle),
        };
        // Only self-contained immutable inputs can be reprojected from the
        // frozen spec. Memory, broker refs/writeback and package materialization
        // still need their original durability evidence; Control stays within
        // the exact original Pod and never reopens a live binding.
        if !plan.packages.is_empty()
            || !plan.memory_mounts.is_empty()
            || spec.mounts.iter().any(|mount| {
                mount.access != pc::MountAccess::ReadOnly
                    || !matches!(
                        &mount.source,
                        pc::MountSource::Inline { .. } | pc::MountSource::InlineBytes { .. }
                    )
            })
        {
            return Err(pc::SandboxError::invalid(
                "materialized container has unretained terminal participants",
            ));
        }
        let guarded_claim = matches!(
            runtime_handle,
            crate::ContainerContinuationHandle::KubernetesGuardedClaim(_)
        );
        let handle = terminal_handle(
            spec,
            container_id,
            runtime_handle,
            adoption_fingerprint.clone(),
            container_realization_fingerprint(spec, &adoption_fingerprint, &plan.image),
            self.runtime.supports_live_input_projection(),
        )?;
        Ok(Some(ReconstructedTerminalHandle {
            handle,
            guarded_claim,
        }))
    }

    pub(super) async fn prepare_terminal_container_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<ContainerSandbox<R>>, pc::SandboxError> {
        let admit =
            || pc::with_current_sandbox_effect(terminal_effect_fence, authority, |_| Ok(()));
        admit()?;
        let reconstructed = handle.is_none();
        let normalized = self.normalized_terminal_handle(spec, handle)?;
        let handle = normalized.as_ref().or(handle);
        let recovered_handle = if reconstructed {
            self.reconstruct_missing_terminal_handle(spec, expected_effect_fence, &admit)
                .await?
        } else {
            None
        };
        let guarded_claim = recovered_handle
            .as_ref()
            .is_some_and(|recovered| recovered.guarded_claim);
        let handle = match (handle, recovered_handle.as_ref()) {
            (Some(handle), _) => handle,
            (None, Some(recovered)) => &recovered.handle,
            (None, None) => return Ok(None),
        };
        let joined_empty = handle
            .container_payload()?
            .runtime_handle
            .as_ref()
            .and_then(crate::ContainerContinuationHandle::projected_participants)
            .is_some_and(|participants| {
                matches!(
                    participants.origin,
                    crate::kubernetes_handle::KubernetesProjectedOrigin::JoinedEmpty { .. }
                )
            });
        if guarded_claim || joined_empty {
            // The original Pod never became the Session environment. The
            // provider already re-observed the exact immutable reservation and
            // PVC above; preparation has no user bytes to harvest. The same
            // ContainerSandbox disposal edge re-observes both under the later
            // aggregate authorization before deleting the original claim.
            admit()?;
            return self
                .adopt_observed_container_with_lifecycle(
                    handle,
                    Some(spec),
                    ContainerCleanupState::physical_cleanup_only(),
                    SandboxObservationPurpose::TerminalCleanup,
                    &admit,
                )
                .await
                .map(Some);
        }
        if handle.restoration().is_some() {
            return Err(pc::SandboxError::invalid(
                "restored container terminal cleanup requires its exact SandboxRestoreRequest",
            ));
        }
        if self.runtime.uses_host_bind_materialization()
            && !self.runtime.uses_host_live_input_bind()
        {
            return Err(pc::SandboxError::unsupported(
                "host-bind container terminal participants cannot be reconstructed from a prior process-local realization attempt",
            ));
        }
        // Host-bind recovery uses the same exact staging-root and frozen
        // participant reconstruction as ordinary adoption below. Capability
        // alone is not evidence: missing roots/brokers still fail closed there.
        // Built-in container runtimes do not implement checkpoint restore, so
        // the optional prior fence carries no reconstructible participant when
        // a durable handle is present. Root authorization remains the sole
        // policy owner; the runtime re-observes with the terminal fence below.
        let adoption = SandboxAdoption::new(spec, handle);
        let observed = self
            .observe_container_adoption(
                adoption,
                Some(terminal_effect_fence),
                SandboxObservationPurpose::TerminalCleanup,
            )
            .await?;
        // Observation is not a grant for staging/control reconstruction.
        admit()?;
        if reconstructed
            && !spec.control_services.is_empty()
            && matches!(
                observed,
                pc::SandboxObservation::Ready | pc::SandboxObservation::Provisioning { .. }
            )
        {
            return Err(pc::SandboxError::not_ready(
                "cold terminal Control reconstruction requires a closed original source",
            ));
        }
        match observed {
            pc::SandboxObservation::RetainedOriginal { .. } => Err(pc::SandboxError::conflict(
                "original source retention is not terminal preparation authority",
            )),
            pc::SandboxObservation::Ready => self
                .adopt_observed_container_with_spec(
                    adoption.handle,
                    adoption.spec,
                    // Ready is physical source availability, not permission to
                    // restore live Control during an authorized terminal effect.
                    // Reuse the terminal role's closed publication slot while
                    // retaining the same source participants for harvesting.
                    SandboxObservationPurpose::TerminalCleanup,
                    &admit,
                )
                .await
                .map(Some),
            pc::SandboxObservation::Terminal { .. } => self
                .adopt_observed_container_with_spec(
                    adoption.handle,
                    adoption.spec,
                    SandboxObservationPurpose::TerminalCleanup,
                    &admit,
                )
                .await
                .map(Some),
            pc::SandboxObservation::Disposing { .. } => self
                .adopt_observed_container_with_lifecycle(
                    adoption.handle,
                    None,
                    ContainerCleanupState::physical_cleanup_only(),
                    SandboxObservationPurpose::TerminalCleanup,
                    &admit,
                )
                .await
                .map(Some),
            pc::SandboxObservation::Provisioning { .. } if reconstructed => self
                .adopt_observed_container_with_spec(
                    adoption.handle,
                    adoption.spec,
                    SandboxObservationPurpose::TerminalCleanup,
                    &admit,
                )
                .await
                .map(Some),
            pc::SandboxObservation::Provisioning { .. } => Err(pc::SandboxError::not_ready(
                "terminal preparation observed a provisioning container realization",
            )),
            pc::SandboxObservation::DefinitivelyUnavailable { .. } => Ok(None),
            pc::SandboxObservation::Incompatible { reason } => {
                Err(pc::SandboxError::conflict(reason))
            }
        }
    }
}
