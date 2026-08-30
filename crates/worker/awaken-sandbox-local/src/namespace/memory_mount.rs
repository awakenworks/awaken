use std::path::{Path, PathBuf};
use std::sync::Arc;

use awaken_provisioning_contract as pc;

/// Mount one MemoryStore requirement through the crate's single provider-neutral
/// adapter. The guard is retained before any mode/evidence validation so every
/// caller can compensate a partially successful external mount.
pub(crate) async fn mount_memory_requirement(
    memory_mounter: &Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
    requirement: &pc::MountRequirement,
    host_path: &Path,
    retained_mounts: &mut Vec<Box<dyn pc::MemoryMount>>,
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
    let guard = mounter
        .mount(
            materialization_reference.as_deref().unwrap_or(store_id),
            host_path,
            requirement.access,
        )
        .await?;
    retained_mounts.push(guard);
    let guard = retained_mounts
        .last()
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
/// only their exact receipt projection and reacquired process-owned guards.
pub(crate) struct AdoptionMemoryReplay {
    pub(crate) realized: Vec<pc::RealizedMount>,
    pub(crate) materializations: Vec<pc::MemoryMaterializationEvidence>,
    pub(crate) memory_mounts: Vec<Box<dyn pc::MemoryMount>>,
}

/// Reproject one exact Ready receipt for either Local or Namespace adoption.
/// Copy remains durable and mount-free. FUSE requires the frozen spec plus an
/// exact Ready operation guard held across every external mount and its
/// post-mount validation; spec-less public adoption therefore fails closed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_adoption_memory_replay(
    memory_mounter: &Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
    root: &Path,
    current_evidence: Option<&super::realization_marker::RealizationEvidence>,
    effect_fence: Option<&pc::SandboxEffectFence>,
    spec: Option<&pc::SandboxSpec>,
    receipt: &super::realization_marker::RealizationCompletionReceipt,
    handle: &pc::SandboxHandle,
    ordinary_realization: pc::Realization,
    host_paths: Option<&[PathBuf]>,
) -> Result<AdoptionMemoryReplay, pc::SandboxError> {
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
        let ready_operation = if realized
            .iter()
            .any(|mount| mount.realization == pc::Realization::Fuse)
        {
            let evidence = current_evidence.ok_or_else(|| {
                pc::SandboxError::new("FUSE replay has no exact Ready realization evidence")
            })?;
            let effect_fence = effect_fence.ok_or_else(|| {
                pc::SandboxError::new("FUSE replay requires its exact adoption effect fence")
            })?;
            Some(super::realization_marker::begin_ready_operation(
                root,
                evidence,
                effect_fence,
            )?)
        } else {
            None
        };
        let host_paths = host_paths.ok_or_else(|| {
            pc::SandboxError::new("Memory replay has no provider-projected host paths")
        })?;
        replay_memory_mount_guards(memory_mounter, spec, &realized, host_paths, || {
            match ready_operation.as_ref() {
                Some(operation) => operation.validate_receipt_before_effect(receipt),
                None => Ok(()),
            }
        })
        .await?
    } else {
        Vec::new()
    };
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
    pub(crate) memory_mounts: Vec<Box<dyn pc::MemoryMount>>,
}

/// Prepare the one Local/Namespace terminal Memory participant. The typed
/// marker observation is checked again by `begin_terminal_takeover` under the
/// mutation lock; an exact receipt is then projected once, Copy evidence is
/// checked without remounting, and only FUSE guards are reacquired under the
/// retained removal validator.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_terminal_memory_replay(
    memory_mounter: &Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
    root: &Path,
    fingerprint: &pc::SandboxRealizationFingerprint,
    spec: &pc::SandboxSpec,
    handle: Option<&pc::SandboxHandle>,
    expected_effect_fence: Option<&pc::SandboxEffectFence>,
    terminal_effect_fence: &pc::SandboxEffectFence,
    ordinary_realization: pc::Realization,
    host_paths: &[PathBuf],
) -> Result<Option<TerminalMemoryReplay>, pc::SandboxError> {
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
    if incomplete {
        super::terminal_copy_materializations(spec, None)?;
        if handle.is_some() {
            return Err(pc::SandboxError::new(
                "terminal handle targets a realization without a Ready receipt",
            ));
        }
    }

    let terminal = super::realization_marker::begin_terminal_takeover(
        root,
        fingerprint,
        rebuild_source,
        expected_effect_fence,
        terminal_effect_fence,
        Some(&terminal_observation),
    )?;
    let Some((realization, removal)) = terminal else {
        return Ok(None);
    };
    let (realized, materializations) = match (ready_projection, removal.completed_receipt()?) {
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
        Vec::new()
    } else {
        replay_memory_mount_guards(memory_mounter, spec, &realized, host_paths, || {
            removal.validate_before_reconstruction(terminal_effect_fence)
        })
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

/// Reacquire only process-owned FUSE guards named by an exact Ready receipt.
/// Copy bytes and heads remain durable projections and are never remounted.
/// Any mismatch tears down every guard acquired by this replay; teardown faults
/// are preserved alongside the original cause.
pub(crate) async fn replay_memory_mount_guards<F>(
    memory_mounter: &Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
    spec: &pc::SandboxSpec,
    realized: &[pc::RealizedMount],
    host_paths: &[PathBuf],
    mut validate_before_mount: F,
) -> Result<Vec<Box<dyn pc::MemoryMount>>, pc::SandboxError>
where
    F: FnMut() -> Result<(), pc::SandboxError>,
{
    if realized.len() != spec.mounts.len() || host_paths.len() != spec.mounts.len() {
        return Err(pc::SandboxError::new(
            "Memory replay has no exact Ready realization or host path for the frozen mount set",
        ));
    }
    let mut retained_mounts = Vec::new();
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
        }
        Ok(())
    }
    .await;
    match replay {
        Ok(()) => Ok(retained_mounts),
        Err(cause) => match compensate_memory_mounts(retained_mounts).await {
            Ok(()) => Err(cause),
            Err(teardown) => Err(pc::SandboxError::new(format!(
                "Memory replay failed: {cause}; Memory replay teardown failed: {teardown}"
            ))),
        },
    }
}

/// Consume a temporary all-or-retain participant set. An all-Ok teardown makes
/// normal drop safe; any failure deliberately retains the complete set for the
/// process lifetime because no Sandbox owner exists to retry it. This is the
/// same fail-closed ownership rule as container staging compensation.
pub(crate) async fn compensate_memory_mounts(
    mounts: Vec<Box<dyn pc::MemoryMount>>,
) -> Result<(), pc::SandboxError> {
    match super::teardown_memory_mounts(&mounts).await {
        Ok(()) => Ok(()),
        Err(error) => {
            std::mem::forget(mounts);
            Err(error)
        }
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
