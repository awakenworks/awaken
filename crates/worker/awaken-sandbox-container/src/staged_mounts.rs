use std::path::PathBuf;
use std::sync::Arc;

use awaken_provisioning_contract as pc;

use super::{SecretWriteback, StagingGuard};

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

    /// Recover every effect-scoped Memory participant after the runtime has
    /// proved that no backend object was committed. One failure must not prevent
    /// the remaining independent responsibilities from being tried.
    pub(super) async fn recover_memory_after_definite_no_backend_effect(
        &self,
    ) -> Result<(), pc::SandboxError> {
        let mut first_error = None;
        for mount in &self.memory {
            if let Err(error) = mount.handle.teardown().await
                && first_error.is_none()
            {
                first_error = Some(error);
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
