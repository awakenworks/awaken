//! Shared Local/Namespace terminal disposal, under the original marker participant.
//! Session owns durable authorization; this adapter serializes physical effects.

use crate::{IsolatedRoot, realization_marker, release_memory_mounts, shred_secret_paths_at};
use std::path::{Path, PathBuf};

/// Admit or refresh one exact filesystem removal participant. Both Local and
/// Namespace use this sole provider authorization path before Copy guards are
/// retired or disposal preparation is reported. Same-operation renewals retain
/// the first durable predecessor; another operation cannot reuse it.
pub(crate) fn authorize_terminal_disposal(
    root: &Path,
    realization: &realization_marker::LiveRealization,
    terminal_removal: &tokio::sync::Mutex<Option<realization_marker::RemovalGuard>>,
    effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
) -> Result<
    awaken_provisioning_contract::SandboxEffectFence,
    awaken_provisioning_contract::SandboxError,
> {
    use awaken_provisioning_contract as pc;

    let evidence = realization.current().ok_or_else(|| {
        pc::SandboxError::unsupported(
            "legacy sandbox handle cannot authorize fenced Memory reconciliation",
        )
    })?;
    let mut slot = terminal_removal
        .try_lock()
        .map_err(|_| pc::SandboxError::not_ready("terminal removal participant is busy"))?;
    if slot.is_none() {
        *slot = realization_marker::begin_terminal_takeover(
            root,
            evidence.fingerprint(),
            Some(realization_marker::RebuildSource {
                fingerprint: evidence.fingerprint(),
                effect_fence: evidence.effect_fence(),
                physical_incarnation: evidence.physical_incarnation().to_owned(),
            }),
            None,
            effect_fence,
            None,
        )?
        .map(|(_, removal)| removal);
    }
    let Some(removal) = slot.as_mut() else {
        return Err(pc::SandboxError::gone(
            "Memory reconciliation lost its exact physical Sandbox participant",
        ));
    };
    removal.prepare_disposal_effect(effect_fence)
}

/// Validate the provider-neutral Memory acknowledgement and bind the exact
/// filesystem removal participant without deleting or shredding any bytes.
/// The aggregate persists successful preparation before invoking physical
/// authorization; this process-local guard is only its provider projection.
pub(crate) fn prepare_terminal_disposal(
    acknowledgement: &awaken_provisioning_contract::MemoryReconciliationAck,
    expected_materializations: &[awaken_provisioning_contract::MemoryMaterializationEvidence],
    root: &Path,
    realization: &realization_marker::LiveRealization,
    terminal_removal: &tokio::sync::Mutex<Option<realization_marker::RemovalGuard>>,
    effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
    authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
) -> Result<
    awaken_provisioning_contract::SandboxEffectFence,
    awaken_provisioning_contract::SandboxError,
> {
    acknowledgement.require_for_disposal(expected_materializations, effect_fence, authority, || {
        authorize_terminal_disposal(root, realization, terminal_removal, effect_fence)
    })
}

/// Borrow the original prepared participant through every await. Cancellation
/// releases only the async mutex, never the retained filesystem guard. Admit
/// authorization before quiescing provider services, then release mounts, shred
/// credentials, remove the exact root and publish Removed. Only success forgets
/// the participant; no take/restore compensation path owns a second lifecycle.
pub(crate) async fn dispose_terminal_realization(
    memory_mounts: &tokio::sync::Mutex<crate::MemoryMountState>,
    sandbox_root: &Path,
    secret_paths: &[PathBuf],
    terminal_removal: &tokio::sync::Mutex<Option<realization_marker::RemovalGuard>>,
    authorization: &awaken_provisioning_contract::SandboxDisposalAuthorization,
    authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    quiesce: impl std::future::Future<Output = ()>,
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    use awaken_provisioning_contract as pc;

    let mut slot = terminal_removal.lock().await;
    let removal = slot.as_mut().ok_or_else(|| {
        pc::SandboxError::invalid(
            "physical disposal requires an exact prepared filesystem participant",
        )
    })?;
    pc::with_current_sandbox_effect(authorization.effect_fence(), authority, || {
        removal.authorize_disposal(authorization)
    })?;
    quiesce.await;
    release_memory_mounts(memory_mounts, || {
        pc::with_current_sandbox_effect(authorization.effect_fence(), authority, || {
            removal.authorize_disposal(authorization)
        })
    })
    .await?;
    pc::with_current_sandbox_effect(authorization.effect_fence(), authority, || {
        removal.authorize_disposal(authorization)?;
        shred_secret_paths_for_removal(removal, sandbox_root, secret_paths)
    })?;
    pc::with_current_sandbox_effect(authorization.effect_fence(), authority, || {
        removal.remove_root()
    })?;
    pc::with_current_sandbox_effect(authorization.effect_fence(), authority, || removal.finish())?;
    pc::with_current_sandbox_effect(authorization.effect_fence(), authority, || {
        slot.take();
        Ok(())
    })?;
    Ok(())
}

fn shred_secret_paths_for_removal(
    removal: &realization_marker::RemovalGuard,
    sandbox_root: &Path,
    secret_paths: &[PathBuf],
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    let Some((owned_root, identity)) = removal.owned_root()? else {
        return Ok(());
    };
    let owned = IsolatedRoot::new(owned_root);
    let paths = secret_paths
        .iter()
        .map(|path| {
            path.strip_prefix(sandbox_root)
                .map(|relative| owned.root().join(relative))
                .map_err(|_| {
                    awaken_provisioning_contract::SandboxError::invalid(format!(
                        "secret path `{}` is outside sandbox root `{}`",
                        path.display(),
                        sandbox_root.display()
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    shred_secret_paths_at(&owned, Some(identity), &paths)
}

#[cfg(test)]
mod tests;
