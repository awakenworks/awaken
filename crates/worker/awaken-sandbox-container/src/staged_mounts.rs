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
        return cause.map_detail(|detail| {
            format!("{detail}; Memory participant cleanup was not authorized: {authority_error}")
        });
    }
    let cleanup = handle.teardown().await;
    let current = admit();
    match (cleanup, current) {
        (Ok(()), Ok(())) => cause,
        (Ok(()), Err(authority_error)) => cause.map_detail(|detail| format!("{detail}; Memory participant cleanup completed after authority changed: {authority_error}"
        )),
        (Err(cleanup_error), current) => {
            staged.retain_mounted_memory(handle);
            cause.map_detail(|detail| format!("{detail}; Memory participant cleanup failed: {cleanup_error}{}",
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
    let mounter = mounter.ok_or_else(|| {
        pc::SandboxError::unsupported("container MemoryStore mount has no MemoryMounter")
    })?;
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
        // Claim the responsibility immediately before the call that creates the
        // host mount, not after it. This await is a cancellation point: a
        // dropped caller never reaches the match arms, so a record taken
        // afterwards cannot exist while the mount already does. Claiming first
        // leaves only "no record and no mount" or "record and maybe a mount" —
        // never a mount nobody owns. Every exit from the provisional state
        // below is synchronous, so no await can interleave.
        //
        // The recovery call above is deliberately outside the claim: it acts on
        // the previous attempt's persisted claim, and claiming again would make
        // each failed recovery schedule an extra one.
        staged.retain_unknown_memory(
            Arc::clone(&mounter),
            store_reference.clone(),
            host_path.clone(),
            access,
        );
        let handle = match mounter.mount(&store_reference, &host_path, access).await {
            pc::MemoryMountOutcome::Mounted(handle) => {
                // The claim is discharged and the exact handle takes its place
                // in the same synchronous step, so the mount is owned once.
                staged.discharge_unknown_memory(&host_path);
                if let Err(error) = admit() {
                    staged.retain_mounted_memory(handle);
                    return Err(error);
                }
                handle
            }
            pc::MemoryMountOutcome::NoEffect(error) => {
                // The mounter proved no host effect, so the claim is void.
                staged.discharge_unknown_memory(&host_path);
                admit()?;
                return Err(error);
            }
            pc::MemoryMountOutcome::RecoveryRequired(error) => {
                // Indeterminate: the claim taken before the call is exactly the
                // record this outcome needs, so it stands rather than repeating.
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
                        pc::SandboxError::invalid(
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
                    pc::SandboxError::invalid(
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
                    pc::SandboxError::invalid(
                        "MemoryStore mount returned an unsupported realization",
                    ),
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
                .ok_or_else(|| pc::SandboxError::not_found("Memory mount disappeared from plan"));
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

/// A cancelled caller drops the staging it was building. Memory claims are the
/// one part of it that names a *host* effect, so letting the staging root be
/// removed underneath one turns a recoverable orphan into an unrecoverable
/// mess: `StagingGuard`'s own drop would `remove_dir_all` the directory a live
/// FUSE mount still sits on.
///
/// `Drop` cannot await, so it does not unmount. It does the one thing a
/// synchronous context can that the existing recovery path already consumes:
/// retain the staging root, so restoration finds the directory instead of a
/// hole. Teardown proper stays in [`StagedMounts::teardown_memory`], on the
/// paths that can await. With a claim discharged synchronously the moment its
/// outcome is known, a successful build leaves nothing retained and this is a
/// no-op.
impl Drop for StagedMounts {
    fn drop(&mut self) {
        if self.retained_memory.is_empty() {
            return;
        }
        if let Some(guard) = self.guard.as_mut() {
            guard.retain_for_restoration();
        }
    }
}

/// Every part of a completed staging, handed to a longer-lived owner in one
/// move. It exists because [`StagedMounts`] owns a drop guard and so cannot be
/// destructured field by field — which is the point: the only way past the
/// guard is [`StagedMounts::into_parts`], and that refuses while a claim is
/// unsettled.
pub(super) struct StagedMountsParts {
    pub(super) guard: Option<StagingGuard>,
    pub(super) secret_writebacks: Vec<SecretWriteback>,
    pub(super) memory: Vec<StagedMemoryMount>,
}

impl StagedMounts {
    /// Hand every part on to an owner that outlives this build.
    ///
    /// This is the one discharge that is not a cancellation, so it is the one
    /// place that may legitimately empty the staging — and it refuses unless
    /// every provisional Memory claim has already been settled. A handoff that
    /// silently carried an unsettled claim would recreate exactly the hole the
    /// claim exists to close: a host mount whose owner has gone away.
    pub(super) fn into_parts(mut self) -> Result<StagedMountsParts, pc::SandboxError> {
        if !self.retained_memory.is_empty() {
            return Err(pc::SandboxError::not_ready(
                "container staging cannot be handed on while a Memory claim is unsettled",
            ));
        }
        Ok(StagedMountsParts {
            guard: self.guard.take(),
            secret_writebacks: std::mem::take(&mut self.secret_writebacks),
            memory: std::mem::take(&mut self.memory),
        })
    }

    pub(super) fn new(
        guard: Option<StagingGuard>,
        secret_writebacks: Vec<SecretWriteback>,
    ) -> Self {
        Self {
            guard,
            secret_writebacks,
            memory: Vec::new(),
            retained_memory: Vec::new(),
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

    /// Drop the provisional claim for `host_path` once its outcome is known.
    /// Only a definitive answer discharges a claim: a cancelled caller never
    /// runs this, which is the point — the claim outlives it.
    pub(super) fn discharge_unknown_memory(&mut self, host_path: &std::path::Path) {
        self.retained_memory.retain(|responsibility| {
            !matches!(
                responsibility,
                RetainedMemoryResponsibility::Unknown { host_path: claimed, .. }
                    if claimed == host_path
            )
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
                Err(pc::SandboxError::invalid(
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
        let bytes = serde_json::to_vec(&evidence).map_err(|error| {
            pc::SandboxError::backend("write_memory_pre_activation_receipt", error)
        })?;
        let pending = root.join(format!("{MEMORY_PRE_ACTIVATION_RECEIPT}.pending"));
        std::fs::write(&pending, bytes).map_err(|error| {
            pc::SandboxError::backend("write_memory_pre_activation_receipt", error)
        })?;
        std::fs::rename(&pending, root.join(MEMORY_PRE_ACTIVATION_RECEIPT)).map_err(|error| {
            pc::SandboxError::backend("write_memory_pre_activation_receipt", error)
        })
    }

    pub(super) fn read_memory_pre_activation_receipt(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Vec<pc::MemoryMaterializationEvidence>, pc::SandboxError> {
        let root = self.guard.as_ref().map(StagingGuard::path).ok_or_else(|| {
            pc::SandboxError::invalid("active container has no Memory staging root")
        })?;
        let bytes = std::fs::read(root.join(MEMORY_PRE_ACTIVATION_RECEIPT)).map_err(|error| {
            pc::SandboxError::backend("read_memory_pre_activation_receipt", error)
        })?;
        let evidence: Vec<pc::MemoryMaterializationEvidence> = serde_json::from_slice(&bytes)
            .map_err(|error| {
                pc::SandboxError::backend("read_memory_pre_activation_receipt", error)
            })?;
        pc::MemoryMaterializationEvidence::validate_all(&evidence)?;
        let projections = super::memory_projections(spec);
        if evidence.len() != projections.len()
            || projections.iter().any(|projection| {
                !evidence.iter().any(|item| {
                    item.store_id == projection.store_id && item.mount_path == projection.mount_path
                })
            })
        {
            return Err(pc::SandboxError::conflict(
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

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use std::future::Future;
    use std::pin::pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    /// A mounter whose physical call never resolves, so the only way out of the
    /// await is the caller being dropped — the exact shape of a cancellation.
    struct NeverResolvingMounter {
        entered: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl pc::MemoryMounter for NeverResolvingMounter {
        async fn mount(
            &self,
            _: &str,
            _: &std::path::Path,
            _: pc::MountAccess,
        ) -> pc::MemoryMountOutcome {
            self.entered.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }

        async fn recover_unknown_mount(
            &self,
            _: &str,
            _: &std::path::Path,
            _: pc::MountAccess,
        ) -> Result<(), pc::SandboxError> {
            Ok(())
        }

        async fn reconcile_recovered_copy(
            &self,
            reference: &str,
            evidence: &awaken_provisioning_contract::MemoryMaterializationEvidence,
            files: &[(String, Vec<u8>)],
            access: awaken_provisioning_contract::MountAccess,
        ) -> Result<(), awaken_provisioning_contract::SandboxError> {
            let _ = (reference, evidence, files, access);
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "recovered Memory copy reconciliation is outside this fixture",
            ))
        }
    }

    fn poll_once<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
        let waker = std::task::Waker::noop();
        future.poll(&mut Context::from_waker(waker))
    }

    #[test]
    fn a_cancelled_mount_leaves_its_claim_and_blocks_the_handoff() {
        // C1 the caller is dropped while the physical mount is in flight.
        // E1 the claim taken before the call survives in the staging;
        // E2 `into_parts` refuses, so the unsettled claim cannot be handed to a
        //    longer-lived owner as if the build had succeeded;
        // E3 dropping the staging retains the staging root instead of removing
        //    a directory a live host mount may sit on.
        // R1 = C1 => E1 & E2 & E3. Before the claim moved ahead of the await,
        // C1 produced no record at all: the match arms that used to take it are
        // unreachable from a dropped future.
        let root = tempfile::tempdir().expect("staging root");
        let entered = Arc::new(AtomicUsize::new(0));
        let mounter: Arc<dyn pc::MemoryMounter> = Arc::new(NeverResolvingMounter {
            entered: Arc::clone(&entered),
        });
        let host_path = root.path().join("memory-one");

        let mut staged = StagedMounts::new(None, Vec::new());
        staged.retain_unknown_memory(
            Arc::clone(&mounter),
            "store://one".to_owned(),
            host_path.clone(),
            pc::MountAccess::ReadWrite,
        );

        {
            let mut call =
                pin!(mounter.mount("store://one", &host_path, pc::MountAccess::ReadWrite));
            assert!(poll_once(call.as_mut()).is_pending(), "C1 is in flight");
            assert_eq!(entered.load(Ordering::SeqCst), 1, "C1 reached the backend");
        } // the caller's future is dropped here, mid-effect.

        assert_eq!(staged.retained_memory.len(), 1, "R1/E1");
        let refusal = match staged.into_parts() {
            Err(error) => error,
            Ok(_) => panic!("R1/E2: an unsettled claim must not be handed on"),
        };
        assert!(
            matches!(refusal, pc::SandboxError::NotReady(_)),
            "R1/E2 an unsettled claim is a transitional refusal: {refusal}"
        );
    }

    /// Drive the real production path, not a hand-built `StagedMounts`.
    ///
    /// The isolated tests above prove the invariants of the claim; this one
    /// proves `materialize_memory_binds` actually takes the claim *before* the
    /// physical call. Reverting the claim to its old post-await position leaves
    /// the isolated tests green and fails only this one, which is the whole
    /// reason it exists.
    #[test]
    fn the_production_path_claims_before_it_mounts() {
        // C3 the caller is dropped while `materialize_memory_binds` is inside
        // the mount call. E5 the staging it was filling already holds the
        // claim. R3 = C3 => E5.
        let root = tempfile::tempdir().expect("staging root");
        let entered = Arc::new(AtomicUsize::new(0));
        let mounter: Arc<dyn pc::MemoryMounter> = Arc::new(NeverResolvingMounter {
            entered: Arc::clone(&entered),
        });

        let mut spec = crate::tests::spec("cancelled-memory-stage");
        spec.mounts.push(pc::MountRequirement {
            mount_id: "memory".into(),
            source: pc::MountSource::MemoryStore {
                store_id: "cancelled-store".into(),
                materialization_reference: None,
                write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
            },
            mount_path: "/mnt/memory/cancelled".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Session,
            required: true,
        });

        let command = vec!["sleep".to_owned(), "infinity".to_owned()];
        let mut plan =
            crate::container_plan(&spec, "ghcr.io/awaken/sandbox:latest", &command, None)
                .expect("container plan");
        let mut staged = StagedMounts::new(
            Some(StagingGuard::retained(root.path().to_path_buf())),
            Vec::new(),
        );

        {
            let mut call = pin!(materialize_memory_binds(
                &spec,
                &mut plan,
                &mut staged,
                Some(Arc::clone(&mounter)),
                false,
                false,
                &(|| Ok(())),
            ));
            assert!(poll_once(call.as_mut()).is_pending(), "C3 is in flight");
            assert_eq!(entered.load(Ordering::SeqCst), 1, "C3 reached the backend");
        } // the caller is dropped mid-mount.

        assert_eq!(staged.retained_memory.len(), 1, "R3/E5");
    }

    #[test]
    fn a_settled_claim_lets_the_staging_be_handed_on() {
        // C2 the outcome is known and the claim discharged synchronously.
        // E4 the handoff is admitted. R2 = C2 => E4, so the guard above costs a
        // successful build nothing.
        let root = tempfile::tempdir().expect("staging root");
        let mounter: Arc<dyn pc::MemoryMounter> = Arc::new(NeverResolvingMounter {
            entered: Arc::new(AtomicUsize::new(0)),
        });
        let host_path = root.path().join("memory-one");

        let mut staged = StagedMounts::new(None, Vec::new());
        staged.retain_unknown_memory(
            Arc::clone(&mounter),
            "store://one".to_owned(),
            host_path.clone(),
            pc::MountAccess::ReadWrite,
        );
        staged.discharge_unknown_memory(&host_path);
        assert!(staged.into_parts().is_ok(), "R2/E4");
    }
}
