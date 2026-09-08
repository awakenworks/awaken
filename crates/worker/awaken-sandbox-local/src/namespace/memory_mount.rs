use std::path::{Path, PathBuf};
use std::sync::Arc;

use awaken_provisioning_contract as pc;

pub(crate) async fn prepare_memory_before_root(
    owner: &crate::MemoryMountOwner,
    guard: &crate::realization_marker::ProviderCreationGuard<'_>,
) -> Result<(), pc::SandboxError> {
    let mut state = owner.lock().await;
    if guard.root_identity()?.is_some() {
        state.release(|| guard.validate_before_mutation()).await
    } else {
        // Legacy one-shot creation has not prepared a root yet. It permits an
        // empty owner, not adopting or clearing a retained prior participant.
        state.require_empty()
    }
}

/// Mount one MemoryStore requirement through the crate's single provider-neutral
/// adapter. The guard is retained before any mode/evidence validation so every
/// caller can compensate a partially successful external mount.
pub(crate) async fn mount_memory_requirement(
    memory_mounter: &Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
    requirement: &pc::MountRequirement,
    host_path: &Path,
    retained_mounts: &mut crate::MemoryMountState,
) -> Result<Option<(pc::RealizedMount, Option<pc::MemoryMaterializationEvidence>)>, pc::SandboxError>
{
    let pc::MountSource::MemoryStore {
        store_id,
        materialization_reference,
        write_consistency,
    } = &requirement.source
    else {
        return Ok(None);
    };
    let Some(mounter) = memory_mounter
        .read()
        .expect("memory mounter lock poisoned")
        .clone()
    else {
        return Err(pc::SandboxError::new(format!(
            "mount {:?}: memory_store is not realizable on this provider (no memory mounter wired)",
            requirement.mount_id
        )));
    };
    let index = retained_mounts
        .acquire(
            mounter,
            requirement.clone(),
            materialization_reference
                .as_deref()
                .unwrap_or(store_id)
                .to_owned(),
            host_path.to_owned(),
            requirement.access,
        )
        .await?;
    let guard = retained_mounts
        .get(index)
        .expect("the acquired Memory guard was retained");
    let realization = guard.realization();
    let materialization = match (realization, guard.materialization_heads()) {
        (pc::Realization::Copy, Some(heads)) => Some(
            pc::MemoryMaterializationEvidence::new(
                store_id.clone(),
                requirement.mount_path.clone(),
                heads,
            )
            .map_err(|cause| {
                pc::SandboxError::new(format!(
                    "mount {:?}: invalid memory materialization evidence: {cause}",
                    requirement.mount_id
                ))
            })?,
        ),
        (pc::Realization::Copy, None) => {
            return Err(pc::SandboxError::new(format!(
                "mount {:?}: copy-backed memory_store returned no durable heads",
                requirement.mount_id
            )));
        }
        (_, Some(_)) => {
            return Err(pc::SandboxError::new(format!(
                "mount {:?}: non-copy memory_store returned copy materialization heads",
                requirement.mount_id
            )));
        }
        (_, None) => None,
    };
    if *write_consistency == pc::MemoryWriteConsistency::WriteThroughRequired
        && realization != pc::Realization::Fuse
    {
        return Err(pc::SandboxError::new(format!(
            "mount {:?}: memory_store requires write-through FUSE realization",
            requirement.mount_id
        )));
    }
    Ok(Some((
        pc::RealizedMount {
            mount_id: requirement.mount_id.clone(),
            mount_path: requirement.mount_path.clone(),
            access: requirement.access,
            realization,
            content_hash: None,
        },
        materialization,
    )))
}

/// Validate that a current handle carries exactly the Copy subset named by an
/// exact Ready receipt. Fuse participants have no durable head evidence and are
/// deliberately excluded before invoking the unchanged strict Copy validator.
pub(crate) fn validate_replay_memory_handle(
    spec: &pc::SandboxSpec,
    realized: &[pc::RealizedMount],
    receipt_materializations: &[pc::MemoryMaterializationEvidence],
    handle: Option<&pc::SandboxHandle>,
) -> Result<(), pc::SandboxError> {
    if realized.len() != spec.mounts.len() {
        return Err(pc::SandboxError::new(
            "Memory handle validation has no exact Ready realization for the frozen mount set",
        ));
    }
    let mut copy_spec = spec.clone();
    copy_spec.mounts = spec
        .mounts
        .iter()
        .zip(realized)
        .filter(|(required, realized)| {
            matches!(&required.source, pc::MountSource::MemoryStore { .. })
                && realized.realization == pc::Realization::Copy
        })
        .map(|(required, _)| required.clone())
        .collect();
    let handle_materializations = super::terminal_copy_materializations(&copy_spec, handle)?;
    if receipt_materializations != handle_materializations {
        return Err(pc::SandboxError::new(
            "sandbox handle Memory evidence differs from the exact Ready receipt",
        ));
    }
    Ok(())
}

/// A V2 Ready receipt may name a process-owned FUSE participant. Unfenced
/// public adoption has no frozen MemoryStore source/reference from which to
/// reacquire that participant, so it must fail instead of returning a handle
/// that merely claims the FUSE realization is live. Copy receipts remain
/// durable and continue through the caller's exact handle-evidence check.
fn validate_adoption_memory_replay_spec(
    spec: Option<&pc::SandboxSpec>,
    realized: &[pc::RealizedMount],
) -> Result<(), pc::SandboxError> {
    if spec.is_none()
        && realized
            .iter()
            .any(|mount| mount.realization == pc::Realization::Fuse)
    {
        return Err(pc::SandboxError::new(
            "FUSE Memory replay requires the frozen Sandbox spec",
        ));
    }
    Ok(())
}

/// Provider-neutral Ready-adoption projection. Marker evidence and the
/// ReadyOperationGuard remain the lifecycle authority; this carrier returns
/// only their exact receipt projection and the shared physical Memory owner.
pub(crate) struct AdoptionMemoryReplay {
    pub(crate) realized: Vec<pc::RealizedMount>,
    pub(crate) materializations: Vec<pc::MemoryMaterializationEvidence>,
    pub(crate) memory_mounts: crate::MemoryMountOwner,
}

/// Reproject one exact Ready receipt for either Local or Namespace adoption.
/// Copy remains durable and mount-free. FUSE requires the frozen spec plus an
/// exact Ready operation guard held across every external mount and its
/// post-mount validation; spec-less public adoption therefore fails closed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_adoption_memory_replay(
    memory_mounter: &Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
    memory_mounts: crate::MemoryMountOwner,
    root: &Path,
    current_evidence: Option<&super::realization_marker::RealizationEvidence>,
    effect_fence: Option<&pc::SandboxEffectFence>,
    authority: Option<&dyn pc::CurrentExecutionAuthority>,
    spec: Option<&pc::SandboxSpec>,
    receipt: &super::realization_marker::RealizationCompletionReceipt,
    handle: &pc::SandboxHandle,
    ordinary_realization: pc::Realization,
    host_paths: Option<&[PathBuf]>,
) -> Result<AdoptionMemoryReplay, pc::SandboxError> {
    let admit = || match effect_fence {
        Some(fence) => pc::with_current_sandbox_effect(
            fence,
            authority.ok_or_else(|| pc::SandboxError::new("adoption has no current authority"))?,
            || Ok(()),
        ),
        None => Ok(()),
    };
    admit()?;
    let (realized, materializations) = match spec {
        Some(spec) => super::replay_completion_receipt(spec, receipt, ordinary_realization)?,
        None => (receipt.mounts(), receipt.memory_materializations().to_vec()),
    };
    validate_adoption_memory_replay_spec(spec, &realized)?;
    if let Some(spec) = spec {
        validate_replay_memory_handle(spec, &realized, &materializations, Some(handle))?;
    } else if handle.memory_materializations()?.unwrap_or_default() != materializations.as_slice() {
        return Err(pc::SandboxError::new(
            "sandbox handle Memory evidence differs from the Ready receipt",
        ));
    }

    let memory_mounts = if let Some(spec) = spec {
        let ready_operation = match (current_evidence, effect_fence) {
            (Some(evidence), Some(effect_fence)) => Some(
                super::realization_marker::begin_ready_operation(root, evidence, effect_fence)?,
            ),
            _ if realized
                .iter()
                .any(|mount| mount.realization == pc::Realization::Fuse) =>
            {
                return Err(pc::SandboxError::new(
                    "FUSE replay requires exact Ready evidence and its adoption effect fence",
                ));
            }
            _ => None,
        };
        let host_paths = host_paths.ok_or_else(|| {
            pc::SandboxError::new("Memory replay has no provider-projected host paths")
        })?;
        if let Some(operation) = ready_operation.as_ref() {
            replay_memory_mount_guards(
                memory_mounter,
                memory_mounts,
                spec,
                &realized,
                host_paths,
                || {
                    admit()?;
                    operation.validate_receipt_before_effect(receipt)
                },
            )
            .await?
        } else {
            // Unfenced Copy projection stays read-only. It cannot opportunistically
            // resume a cancelled mount or harvest a retained unpublished copy.
            memory_mounts.lock().await.require_no_pending()?;
            memory_mounts
        }
    } else {
        memory_mounts.lock().await.require_no_pending()?;
        memory_mounts
    };
    admit()?;
    Ok(AdoptionMemoryReplay {
        realized,
        materializations,
        memory_mounts,
    })
}

/// Provider-neutral terminal reconstruction result. The marker guard remains
/// the lifecycle owner; this carrier only returns the exact receipt projection
/// and any process-owned FUSE guards to the provider-specific Sandbox wrapper.
pub(crate) struct TerminalMemoryReplay {
    pub(crate) realization: super::realization_marker::RealizationEvidence,
    pub(crate) removal: super::realization_marker::RemovalGuard,
    pub(crate) owned_root_present: bool,
    pub(crate) realized: Vec<pc::RealizedMount>,
    pub(crate) materializations: Vec<pc::MemoryMaterializationEvidence>,
    pub(crate) memory_mounts: crate::MemoryMountOwner,
}

/// Prepare the one Local/Namespace terminal Memory participant. The typed
/// marker observation is checked again by `begin_terminal_takeover` under the
/// mutation lock; an exact receipt is then projected once, Copy evidence is
/// checked without remounting, and only FUSE guards are reacquired under the
/// retained removal validator.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_terminal_memory_replay(
    memory_mounter: &Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
    memory_mounts: crate::MemoryMountOwner,
    root: &Path,
    fingerprint: &pc::SandboxRealizationFingerprint,
    spec: &pc::SandboxSpec,
    handle: Option<&pc::SandboxHandle>,
    expected_effect_fence: Option<&pc::SandboxEffectFence>,
    terminal_effect_fence: &pc::SandboxEffectFence,
    authority: &dyn pc::CurrentExecutionAuthority,
    ordinary_realization: pc::Realization,
    host_paths: &[PathBuf],
) -> Result<Option<TerminalMemoryReplay>, pc::SandboxError> {
    pc::with_current_sandbox_effect(terminal_effect_fence, authority, || Ok(()))?;
    let rebuild_source = handle
        .map(super::realization_marker::rebuild_source)
        .transpose()?;
    let terminal_observation = super::realization_marker::observe_terminal_receipt(
        root,
        fingerprint,
        rebuild_source.as_ref(),
        expected_effect_fence,
        terminal_effect_fence,
    )?;
    let (ready_projection, incomplete) = match &terminal_observation {
        super::realization_marker::TerminalReceiptObservation::Receipt(receipt) => {
            let (realized, materializations) =
                super::replay_completion_receipt(spec, receipt, ordinary_realization)?;
            if handle.is_some() {
                validate_replay_memory_handle(spec, &realized, &materializations, handle)?;
            }
            (Some((receipt.clone(), realized, materializations)), false)
        }
        super::realization_marker::TerminalReceiptObservation::Incomplete => (None, true),
        super::realization_marker::TerminalReceiptObservation::Closed => (None, false),
    };
    if incomplete && handle.is_some() {
        return Err(pc::SandboxError::new(
            "terminal handle targets a realization without a Ready receipt",
        ));
    }

    // An Incomplete marker has no Ready receipt. Therefore the provider never
    // returned a Sandbox to Runtime and no Agent could have observed or
    // mutated a writable Memory mount. The terminal fence may dispose that
    // exact session-scoped partial realization with zero materializations.
    // A durable handle above remains contradictory and is rejected.

    let terminal = pc::with_current_sandbox_effect(terminal_effect_fence, authority, || {
        super::realization_marker::begin_terminal_takeover(
            root,
            fingerprint,
            rebuild_source,
            expected_effect_fence,
            terminal_effect_fence,
            Some(&terminal_observation),
        )
    })?;
    let Some((realization, removal)) = terminal else {
        let state = memory_mounts.lock().await;
        pc::with_current_sandbox_effect(terminal_effect_fence, authority, || {
            state.require_empty()
        })?;
        return Ok(None);
    };
    let completed_receipt = if incomplete {
        None
    } else {
        removal.completed_receipt()?
    };
    let (realized, materializations) = match (ready_projection, completed_receipt) {
        (Some((ready_receipt, realized, materializations)), Some(receipt))
            if receipt == &ready_receipt =>
        {
            (realized, materializations)
        }
        (None, None) => (Vec::new(), Vec::new()),
        _ => {
            return Err(pc::SandboxError::new(
                "terminal takeover changed the preflight phase or exact Ready receipt",
            ));
        }
    };
    let owned_root_present = removal.owned_root()?.is_some();
    let memory_mounts = if realized.is_empty() || !owned_root_present {
        memory_mounts
            .lock()
            .await
            .release_unconfirmed(|| {
                pc::with_current_sandbox_effect(terminal_effect_fence, authority, || {
                    removal.validate_before_reconstruction(terminal_effect_fence)
                })
            })
            .await?;
        pc::with_current_sandbox_effect(terminal_effect_fence, authority, || Ok(memory_mounts))?
    } else {
        replay_memory_mount_guards(
            memory_mounter,
            memory_mounts,
            spec,
            &realized,
            host_paths,
            || {
                pc::with_current_sandbox_effect(terminal_effect_fence, authority, || {
                    removal.validate_before_reconstruction(terminal_effect_fence)
                })
            },
        )
        .await?
    };
    Ok(Some(TerminalMemoryReplay {
        realization,
        removal,
        owned_root_present,
        realized,
        materializations,
        memory_mounts,
    }))
}

/// Reuse exact live FUSE guards or reacquire them from an exact Ready receipt.
/// Copy bytes, original heads and confirmed guards are never remounted/harvested
/// here. An admitted retry compensates unpublished attempts; denied or failed
/// compensation remains reachable from the live Provider for another retry.
pub(crate) async fn replay_memory_mount_guards<F>(
    memory_mounter: &Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
    memory_mounts: crate::MemoryMountOwner,
    spec: &pc::SandboxSpec,
    realized: &[pc::RealizedMount],
    host_paths: &[PathBuf],
    mut validate_before_mount: F,
) -> Result<crate::MemoryMountOwner, pc::SandboxError>
where
    F: FnMut() -> Result<(), pc::SandboxError>,
{
    if realized.len() != spec.mounts.len() || host_paths.len() != spec.mounts.len() {
        return Err(pc::SandboxError::new(
            "Memory replay has no exact Ready realization or host path for the frozen mount set",
        ));
    }
    let mut retained_mounts = memory_mounts.lock().await;
    retained_mounts
        .prepare_replay(spec, host_paths, &mut validate_before_mount)
        .await?;
    let replay = async {
        for ((requirement, expected), host_path) in spec.mounts.iter().zip(realized).zip(host_paths)
        {
            if !matches!(&requirement.source, pc::MountSource::MemoryStore { .. })
                || expected.realization != pc::Realization::Fuse
            {
                continue;
            }
            validate_before_mount()?;
            let recovered = mount_memory_requirement(
                memory_mounter,
                requirement,
                host_path,
                &mut retained_mounts,
            )
            .await?;
            // The external mounter may outlive the lease check above. Recheck
            // under the same held lifecycle guard before publishing its result;
            // failure flows through the shared teardown compensation below.
            validate_before_mount()?;
            let (recovered, materialization) = recovered.ok_or_else(|| {
                pc::SandboxError::new("Memory replay did not produce the required participant")
            })?;
            if recovered != *expected || materialization.is_some() {
                return Err(pc::SandboxError::new(
                    "Memory replay changed the exact Ready realization",
                ));
            }
            retained_mounts.confirm(requirement, host_path);
        }
        // Empty/Copy-only replay also requires admission. Keep this final
        // check inside the same compensation owner as individual FUSE waits.
        validate_before_mount()
    }
    .await;
    match replay {
        Ok(()) => {
            drop(retained_mounts);
            Ok(memory_mounts)
        }
        Err(cause) => match retained_mounts
            .prepare_replay(spec, host_paths, &mut validate_before_mount)
            .await
        {
            Ok(()) => Err(cause),
            Err(teardown) => Err(pc::SandboxError::new(format!(
                "Memory replay failed: {cause}; Memory replay teardown failed: {teardown}"
            ))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfenced_adoption_memory_rule_rejects_fuse_but_allows_copy() {
        // Unfenced-adoption decision table: C1 the Ready receipt mode is
        // Copy/Fuse; C2 a frozen spec is absent. R1 C2+Copy remains an exact
        // durable projection and needs no process guard; R2 C2+Fuse rejects
        // because neither MemoryStore identity nor a host path is available to
        // reacquire the guard. The provider caller table separately proves both
        // Local and Namespace route their public adoption through this owner.
        let realized = |realization| {
            [pc::RealizedMount {
                mount_id: "memory".into(),
                mount_path: "/mnt/memory/test".into(),
                access: pc::MountAccess::ReadWrite,
                realization,
                content_hash: None,
            }]
        };
        assert!(
            validate_adoption_memory_replay_spec(None, &realized(pc::Realization::Copy),).is_ok(),
            "R1"
        );
        assert!(
            validate_adoption_memory_replay_spec(None, &realized(pc::Realization::Fuse),).is_err(),
            "R2"
        );
    }
}
