use std::path::PathBuf;
use std::sync::Arc;

use awaken_provisioning_contract as pc;

use super::{BindPlan, ContainerPlan, SecretWriteback, StagingGuard};

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
    pub(super) attempt: pc::MemoryMountAttempt,
    pub(super) handle: Box<dyn pc::MemoryMount>,
    pub(super) host_path: PathBuf,
    pub(super) mount_path: String,
    pub(super) access: pc::MountAccess,
    pub(super) materialization: Option<pc::MemoryMaterializationEvidence>,
    pub(super) native_runtime_volume: bool,
    pub(super) writeback_prepared: bool,
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

    pub(super) fn publish_memory(&mut self) -> Result<(), pc::SandboxError> {
        for mount in &mut self.memory {
            mount
                .attempt
                .apply(pc::MemoryMountAttemptTransition::Published)?;
        }
        Ok(())
    }

    /// Recover every Memory participant while the backend authority proves its
    /// exact reservation is still not active. One failure must not prevent the
    /// remaining independent responsibilities from being tried.
    pub(super) async fn recover_memory_before_activation(
        &mut self,
    ) -> Result<(), pc::SandboxError> {
        let mut first_error = None;
        for mount in &mut self.memory {
            match mount.handle.teardown().await {
                Ok(()) => mount
                    .attempt
                    .apply(pc::MemoryMountAttemptTransition::Compensated)?,
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        for responsibility in &self.retained_memory {
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
