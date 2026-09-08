use std::path::PathBuf;
use std::sync::Arc;

use awaken_provisioning_contract as pc;

use super::{BindPlan, ContainerPlan, SecretWriteback, StagingGuard};

pub(super) const MEMORY_PRE_ACTIVATION_RECEIPT: &str = ".awaken-memory-pre-activation.json";

#[derive(Default)]
pub(super) struct StagedMounts {
    pub(super) guard: Option<StagingGuard>,
    pub(super) secret_writebacks: Vec<SecretWriteback>,
    pub(super) memory: Vec<StagedMemoryMount>,
    retained_memory: Vec<RetainedMemoryResponsibility>,
}

enum RetainedMemoryResponsibility {
    Mounted(Box<dyn pc::MemoryMount>),
    Unknown {
        mounter: Arc<dyn pc::MemoryMounter>,
        store_reference: String,
        host_path: PathBuf,
        access: pc::MountAccess,
    },
}

pub(super) struct StagedMemoryMount {
    pub(super) handle: Box<dyn pc::MemoryMount>,
    pub(super) host_path: PathBuf,
    pub(super) mount_path: String,
    pub(super) access: pc::MountAccess,
    pub(super) materialization: Option<pc::MemoryMaterializationEvidence>,
    pub(super) native_runtime_volume: bool,
    pub(super) writeback_prepared: bool,
}

async fn reject_unaccepted_memory_mount(
    staged: &mut StagedMounts,
    handle: Box<dyn pc::MemoryMount>,
    cause: pc::SandboxError,
    admit: &(impl Fn() -> Result<(), pc::SandboxError> + Sync),
) -> pc::SandboxError {
    if let Err(authority_error) = admit() {
        staged.retain_mounted_memory(handle);
        return pc::SandboxError::new(format!(
            "{cause}; Memory participant cleanup was not authorized: {authority_error}"
        ));
    }
    let cleanup = handle.teardown().await;
    let current = admit();
    match (cleanup, current) {
        (Ok(()), Ok(())) => cause,
        (Ok(()), Err(authority_error)) => pc::SandboxError::new(format!(
            "{cause}; Memory participant cleanup completed after authority changed: {authority_error}"
        )),
        (Err(cleanup_error), current) => {
            staged.retain_mounted_memory(handle);
            pc::SandboxError::new(format!(
                "{cause}; Memory participant cleanup failed: {cleanup_error}{}",
                current
                    .err()
                    .map(|error| format!("; authority changed: {error}"))
                    .unwrap_or_default()
            ))
        }
    }
}

pub(super) async fn materialize_memory_binds(
    spec: &pc::SandboxSpec,
    plan: &mut ContainerPlan,
    staged: &mut StagedMounts,
    mounter: Option<Arc<dyn pc::MemoryMounter>>,
    native_runtime_volume: bool,
    recovering_reservation: bool,
    admit: &(impl Fn() -> Result<(), pc::SandboxError> + Sync),
) -> Result<(), pc::SandboxError> {
    let memory = super::memory_projections(spec);
    if memory.is_empty() {
        return Ok(());
    }
    let mounter = mounter
        .ok_or_else(|| pc::SandboxError::new("container MemoryStore mount has no MemoryMounter"))?;
    let root = super::staging_dir(&mut staged.guard, &spec.scope)?;
    for projection in memory {
        admit()?;
        let host_path = root.join(format!(
            "memory-{}",
            super::stage_name(&projection.mount_path)
        ));
        let store_reference = projection.store_reference.clone();
        let access = projection.access;
        if recovering_reservation {
            let recovery = mounter
                .recover_unknown_mount(&store_reference, &host_path, access)
                .await;
            admit()?;
            recovery?;
        }
        admit()?;
        let handle = match mounter.mount(&store_reference, &host_path, access).await {
            pc::MemoryMountOutcome::Mounted(handle) => {
                if let Err(error) = admit() {
                    staged.retain_mounted_memory(handle);
                    return Err(error);
                }
                handle
            }
            pc::MemoryMountOutcome::NoEffect(error) => {
                admit()?;
                return Err(error);
            }
            pc::MemoryMountOutcome::RecoveryRequired(error) => {
                staged.retain_unknown_memory(
                    Arc::clone(&mounter),
                    store_reference,
                    host_path,
                    access,
                );
                admit()?;
                return Err(error);
            }
        };
        let realization = handle.realization();
        let materialization = match realization {
            pc::Realization::Copy => {
                let Some(heads) = handle.materialization_heads() else {
                    return Err(reject_unaccepted_memory_mount(
                        staged,
                        handle,
                        pc::SandboxError::new(
                            "copy-backed Memory mount omitted its original durable heads",
                        ),
                        admit,
                    )
                    .await);
                };
                match pc::MemoryMaterializationEvidence::new(
                    projection.store_id.clone(),
                    projection.mount_path.clone(),
                    heads,
                ) {
                    Ok(evidence) => Some(evidence),
                    Err(error) => {
                        return Err(
                            reject_unaccepted_memory_mount(staged, handle, error, admit).await
                        );
                    }
                }
            }
            pc::Realization::Fuse => {
                return Err(reject_unaccepted_memory_mount(
                    staged,
                    handle,
                    pc::SandboxError::new(
                        "container Memory requires a restart-recoverable copy realization",
                    ),
                    admit,
                )
                .await);
            }
            _ => {
                return Err(reject_unaccepted_memory_mount(
                    staged,
                    handle,
                    pc::SandboxError::new("MemoryStore mount returned an unsupported realization"),
                    admit,
                )
                .await);
            }
        };
        #[cfg(unix)]
        if let Err(error) = super::make_memory_tree_accessible(&host_path, projection.access) {
            return Err(reject_unaccepted_memory_mount(staged, handle, error, admit).await);
        }
        if native_runtime_volume {
            let snapshot_tar = match super::memory_snapshot_tar(&host_path) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return Err(reject_unaccepted_memory_mount(staged, handle, error, admit).await);
                }
            };
            let planned = plan
                .memory_mounts
                .iter_mut()
                .find(|planned| planned.mount_path == projection.mount_path)
                .ok_or_else(|| pc::SandboxError::new("Memory mount disappeared from plan"));
            let planned = match planned {
                Ok(planned) => planned,
                Err(error) => {
                    return Err(reject_unaccepted_memory_mount(staged, handle, error, admit).await);
                }
            };
            planned.snapshot_tar = snapshot_tar;
            planned.access = projection.access;
        }
        staged.memory.push(StagedMemoryMount {
            handle,
            host_path,
            mount_path: projection.mount_path,
            access: projection.access,
            materialization,
            native_runtime_volume,
            writeback_prepared: false,
        });
        admit()?;
    }
    Ok(())
}

impl StagedMounts {
    pub(super) fn new(
        guard: Option<StagingGuard>,
        secret_writebacks: Vec<SecretWriteback>,
    ) -> Self {
        Self {
            guard,
            secret_writebacks,
            ..Self::default()
        }
    }

    pub(super) fn retain_unknown_memory(
        &mut self,
        mounter: Arc<dyn pc::MemoryMounter>,
        store_reference: String,
        host_path: PathBuf,
        access: pc::MountAccess,
    ) {
        self.retained_memory
            .push(RetainedMemoryResponsibility::Unknown {
                mounter,
                store_reference,
                host_path,
                access,
            });
    }

    pub(super) fn retain_mounted_memory(&mut self, mount: Box<dyn pc::MemoryMount>) {
        self.retained_memory
            .push(RetainedMemoryResponsibility::Mounted(mount));
    }

    /// Persist the Copy heads needed to adopt an activation whose response is
    /// lost. This receipt belongs to the same retained staging lifecycle as the
    /// mounted bytes; it is not an independent realization authority.
    pub(super) fn write_memory_pre_activation_receipt(&self) -> Result<(), pc::SandboxError> {
        let Some(root) = self.guard.as_ref().map(StagingGuard::path) else {
            return if self.memory.is_empty() {
                Ok(())
            } else {
                Err(pc::SandboxError::new(
                    "container Memory materialization has no backend-bound staging root",
                ))
            };
        };
        let mut evidence = self
            .memory
            .iter()
            .filter_map(|mount| mount.materialization.clone())
            .collect::<Vec<_>>();
        pc::MemoryMaterializationEvidence::canonicalize_all(&mut evidence)?;
        let bytes = serde_json::to_vec(&evidence)
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        let pending = root.join(format!("{MEMORY_PRE_ACTIVATION_RECEIPT}.pending"));
        std::fs::write(&pending, bytes)
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        std::fs::rename(&pending, root.join(MEMORY_PRE_ACTIVATION_RECEIPT))
            .map_err(|error| pc::SandboxError::new(error.to_string()))
    }

    pub(super) fn read_memory_pre_activation_receipt(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Vec<pc::MemoryMaterializationEvidence>, pc::SandboxError> {
        let root =
            self.guard.as_ref().map(StagingGuard::path).ok_or_else(|| {
                pc::SandboxError::new("active container has no Memory staging root")
            })?;
        let bytes = std::fs::read(root.join(MEMORY_PRE_ACTIVATION_RECEIPT))
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        let evidence: Vec<pc::MemoryMaterializationEvidence> = serde_json::from_slice(&bytes)
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        pc::MemoryMaterializationEvidence::validate_all(&evidence)?;
        let projections = super::memory_projections(spec);
        if evidence.len() != projections.len()
            || projections.iter().any(|projection| {
                !evidence.iter().any(|item| {
                    item.store_id == projection.store_id && item.mount_path == projection.mount_path
                })
            })
        {
            return Err(pc::SandboxError::new(
                "container Memory pre-activation receipt differs from the frozen specification",
            ));
        }
        Ok(evidence)
    }

    /// Recover every Memory participant while the backend authority proves its
    /// exact reservation is still not active. One failure must not prevent the
    /// remaining independent responsibilities from being tried.
    pub(super) async fn recover_memory_before_activation(
        &mut self,
        admit: &(impl Fn() -> Result<(), pc::SandboxError> + Sync),
    ) -> Result<(), pc::SandboxError> {
        let mut first_error = None;
        for mount in &self.memory {
            admit()?;
            let result = mount.handle.teardown().await;
            admit()?;
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        for responsibility in &self.retained_memory {
            admit()?;
            let result = match responsibility {
                RetainedMemoryResponsibility::Mounted(mount) => mount.teardown().await,
                RetainedMemoryResponsibility::Unknown {
                    mounter,
                    store_reference,
                    host_path,
                    access,
                } => {
                    mounter
                        .recover_unknown_mount(store_reference, host_path, *access)
                        .await
                }
            };
            admit()?;
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

pub(super) fn reserve_memory_binds(
    spec: &pc::SandboxSpec,
    plan: &mut ContainerPlan,
    staged: &mut StagedMounts,
    native_runtime_volume: bool,
) -> Result<(), pc::SandboxError> {
    let memory = super::memory_projections(spec);
    if memory.is_empty() || native_runtime_volume {
        return Ok(());
    }
    let root = super::staging_dir(&mut staged.guard, &spec.scope)?;
    for projection in memory {
        let host_path = root.join(format!(
            "memory-{}",
            super::stage_name(&projection.mount_path)
        ));
        plan.binds.push(BindPlan {
            source_ref: host_path.to_string_lossy().into_owned(),
            mount_path: projection.mount_path,
            read_only: projection.access == pc::MountAccess::ReadOnly,
            content: None,
            content_bytes: None,
            secret_content: None,
            secret_writeback: false,
            credential_file_path: None,
        });
    }
    Ok(())
}

impl std::fmt::Debug for StagedMounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedMounts")
            .field("guard", &self.guard)
            .field("secret_writebacks", &self.secret_writebacks)
            .field("memory_mounts", &self.memory.len())
            .field("retained_memory", &self.retained_memory.len())
            .finish()
    }
}
