//! Physical Memory participant ownership within a live Provider. Marker and
//! current-effect admission remain the authority; this index keeps the original
//! mount future and guards reachable across caller cancellation and wrapper Drop.

use std::collections::HashMap;
use std::future::Future;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use awaken_provisioning_contract as pc;
use awaken_sandbox_fs::DirectoryIdentity;

#[derive(PartialEq)]
struct MountKey {
    requirement: pc::MountRequirement,
    path: PathBuf,
}

struct PendingMount {
    key: MountKey,
    reference: String,
    access: pc::MountAccess,
    mounter: Arc<dyn pc::MemoryMounter>,
    phase: PendingMountPhase,
}

enum PendingMountPhase {
    Running(MountFuture),
    RecoveryRequired { detail: String },
}

type RawMountFuture = Pin<Box<dyn Future<Output = pc::MemoryMountOutcome> + Send>>;

/// Retained future wrapper that classifies an unwind as an unknown physical
/// result. Polling remains caller-driven, so cancellation leaves this exact
/// future in the long-lived owner instead of detaching work into the runtime.
struct MountFuture {
    inner: RawMountFuture,
}

impl Future for MountFuture {
    type Output = pc::MemoryMountOutcome;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.get_mut().inner;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            inner.as_mut().poll(context)
        })) {
            Ok(outcome) => outcome,
            Err(_) => Poll::Ready(pc::MemoryMountOutcome::RecoveryRequired(
                pc::SandboxError::new(
                    "Memory mount operation panicked before classifying its effect",
                ),
            )),
        }
    }
}

enum PendingMountCompletion {
    Mounted,
    NoEffect(pc::SandboxError),
    RecoveryRequired(pc::SandboxError),
}

pub(crate) struct MemoryMountParticipant {
    key: Option<MountKey>,
    confirmed: bool,
    guard: Box<dyn pc::MemoryMount>,
}

impl Deref for MemoryMountParticipant {
    type Target = dyn pc::MemoryMount;
    fn deref(&self) -> &Self::Target {
        self.guard.as_ref()
    }
}
pub(crate) type MemoryMountOwner = Arc<tokio::sync::Mutex<MemoryMountState>>;

#[derive(Clone, Default)]
pub(crate) struct MemoryMountRegistry {
    participants: Arc<Mutex<HashMap<PathBuf, MemoryMountOwner>>>,
}

impl MemoryMountRegistry {
    pub(crate) fn owner(&self, root: &Path) -> MemoryMountOwner {
        let mut participants = self
            .participants
            .lock()
            .expect("Memory participants lock poisoned");
        // Only reclaim empty owners with no wrapper/waiter. A pending mount or
        // failed teardown is never evicted, even if its original caller vanished.
        participants.retain(|_, owner| {
            Arc::strong_count(owner) != 1
                || owner.try_lock().map_or(true, |state| {
                    state.pending.is_some() || !state.mounts.is_empty()
                })
        });
        participants
            .entry(root.to_owned())
            .or_insert_with(|| {
                Arc::new(tokio::sync::Mutex::new(MemoryMountState {
                    root: root.to_owned(),
                    ..MemoryMountState::default()
                }))
            })
            .clone()
    }
}

#[derive(Default)]
pub(crate) struct MemoryMountState {
    root: PathBuf,
    identity: Option<DirectoryIdentity>,
    mounts: Vec<MemoryMountParticipant>,
    pending: Option<PendingMount>,
}

impl MemoryMountState {
    fn validate_root(
        root: &Path,
        identity: Option<DirectoryIdentity>,
    ) -> Result<(), pc::SandboxError> {
        if let Some(expected) = identity {
            let observed = awaken_sandbox_fs::directory_identity_nofollow(root)
                .map_err(|error| pc::SandboxError::new(error.to_string()))?;
            if observed != expected {
                return Err(pc::SandboxError::new(
                    "retained Memory participant root identity changed",
                ));
            }
        }
        Ok(())
    }

    async fn finish_mount(&mut self) -> Option<PendingMountCompletion> {
        let outcome = match &mut self.pending.as_mut()?.phase {
            // Await a borrowed JoinHandle: cancellation cannot consume the
            // operation, while task panic becomes an explicit unknown result.
            PendingMountPhase::Running(operation) => operation.await,
            PendingMountPhase::RecoveryRequired { detail } => {
                return Some(PendingMountCompletion::RecoveryRequired(
                    pc::SandboxError::new(detail.clone()),
                ));
            }
        };
        match outcome {
            pc::MemoryMountOutcome::Mounted(guard) => {
                let pending = self.pending.take().expect("completed pending Memory mount");
                self.mounts.push(MemoryMountParticipant {
                    key: Some(pending.key),
                    confirmed: false,
                    guard,
                });
                Some(PendingMountCompletion::Mounted)
            }
            pc::MemoryMountOutcome::NoEffect(error) => {
                self.pending.take();
                if self.mounts.is_empty() {
                    self.identity = None;
                }
                Some(PendingMountCompletion::NoEffect(error))
            }
            pc::MemoryMountOutcome::RecoveryRequired(error) => {
                let detail = error.to_string();
                self.pending
                    .as_mut()
                    .expect("unknown Memory operation remains retained")
                    .phase = PendingMountPhase::RecoveryRequired {
                    detail: detail.clone(),
                };
                Some(PendingMountCompletion::RecoveryRequired(
                    pc::SandboxError::new(detail),
                ))
            }
        }
    }

    async fn recover_unknown_mount(&mut self) -> Result<(), pc::SandboxError> {
        let pending = self
            .pending
            .as_ref()
            .expect("unknown Memory recovery requires a retained operation");
        if !matches!(pending.phase, PendingMountPhase::RecoveryRequired { .. }) {
            return Err(pc::SandboxError::new(
                "Memory mount has not reached an unknown result",
            ));
        }
        let mounter = pending.mounter.clone();
        let reference = pending.reference.clone();
        let path = pending.key.path.clone();
        let access = pending.access;
        mounter
            .recover_unknown_mount(&reference, &path, access)
            .await?;
        self.pending.take();
        if self.mounts.is_empty() {
            self.identity = None;
        }
        Ok(())
    }

    pub(crate) async fn acquire(
        &mut self,
        mounter: Arc<dyn pc::MemoryMounter>,
        requirement: pc::MountRequirement,
        reference: String,
        path: PathBuf,
        access: pc::MountAccess,
    ) -> Result<usize, pc::SandboxError> {
        if self.pending.is_some() {
            return Err(pc::SandboxError::new(
                "unfinished Memory mount requires admitted cleanup before retry",
            ));
        }
        if self.mounts.is_empty() {
            self.identity = Some(
                awaken_sandbox_fs::directory_identity_nofollow(&self.root)
                    .map_err(|error| pc::SandboxError::new(error.to_string()))?,
            );
        }
        Self::validate_root(&self.root, self.identity)?;
        let key = MountKey {
            requirement,
            path: path.clone(),
        };
        if let Some(index) = self
            .mounts
            .iter()
            .position(|mount| mount.confirmed && mount.key.as_ref() == Some(&key))
        {
            return Ok(index);
        }
        let operation_mounter = mounter.clone();
        let operation_reference = reference.clone();
        let operation_path = path.clone();
        self.pending = Some(PendingMount {
            key,
            reference,
            access,
            mounter,
            phase: PendingMountPhase::Running(MountFuture {
                inner: Box::pin(async move {
                    operation_mounter
                        .mount(&operation_reference, &operation_path, access)
                        .await
                }),
            }),
        });
        match self.finish_mount().await.expect("new mount is pending") {
            PendingMountCompletion::Mounted => Ok(self.mounts.len() - 1),
            PendingMountCompletion::NoEffect(error)
            | PendingMountCompletion::RecoveryRequired(error) => Err(error),
        }
    }

    pub(crate) fn confirm_all(&mut self) {
        for mount in &mut self.mounts {
            mount.confirmed = true;
        }
    }

    pub(crate) fn confirm(&mut self, requirement: &pc::MountRequirement, path: &Path) {
        for mount in &mut self.mounts {
            if mount
                .key
                .as_ref()
                .is_some_and(|key| key.requirement == *requirement && key.path == path)
            {
                mount.confirmed = true;
            }
        }
    }

    pub(crate) async fn prepare_replay(
        &mut self,
        spec: &pc::SandboxSpec,
        paths: &[PathBuf],
        authorize: impl FnMut() -> Result<(), pc::SandboxError>,
    ) -> Result<(), pc::SandboxError> {
        // Confirmed Copy participants keep the original Agent edits and CAS
        // bases until the Host's existing reconciliation acknowledgement. Only
        // unpublished attempts or FUSE projections with changed references are
        // compensated here. Exact confirmed FUSE guards are reused, not remounted.
        self.release_where(authorize, |mount| {
            !mount.confirmed
                || (mount.realization() == pc::Realization::Fuse
                    && !spec.mounts.iter().zip(paths).any(|(requirement, path)| {
                        mount
                            .key
                            .as_ref()
                            .is_some_and(|key| key.requirement == *requirement && key.path == *path)
                    }))
        })
        .await
    }

    pub(crate) async fn release(
        &mut self,
        authorize: impl FnMut() -> Result<(), pc::SandboxError>,
    ) -> Result<(), pc::SandboxError> {
        self.release_where(authorize, |_| true).await
    }

    pub(crate) async fn release_unconfirmed(
        &mut self,
        authorize: impl FnMut() -> Result<(), pc::SandboxError>,
    ) -> Result<(), pc::SandboxError> {
        self.release_where(authorize, |mount| !mount.confirmed)
            .await
    }

    pub(crate) fn require_no_pending(&self) -> Result<(), pc::SandboxError> {
        if self.pending.is_some() || self.mounts.iter().any(|mount| !mount.confirmed) {
            return Err(pc::SandboxError::new(
                "Memory acknowledgement has unfinished physical participants",
            ));
        }
        Ok(())
    }

    /// Called only inside the existing Host reconciliation ACK authority.
    /// Keep retirement encapsulated: callers cannot clear the participant Vec
    /// without the settled-operation and exact-root checks.
    pub(crate) fn acknowledge_copy_reconciliation(&mut self) -> Result<(), pc::SandboxError> {
        self.require_no_pending()?;
        Self::validate_root(&self.root, self.identity)?;
        self.mounts
            .retain(|mount| mount.realization() != pc::Realization::Copy);
        if self.mounts.is_empty() {
            self.identity = None;
        }
        Ok(())
    }

    pub(crate) fn require_empty(&self) -> Result<(), pc::SandboxError> {
        if self.pending.is_some() || !self.mounts.is_empty() {
            return Err(pc::SandboxError::new(
                "absent realization still has retained Memory participants",
            ));
        }
        Ok(())
    }

    async fn release_where(
        &mut self,
        mut authorize: impl FnMut() -> Result<(), pc::SandboxError>,
        retire: impl Fn(&MemoryMountParticipant) -> bool + Send + Sync,
    ) -> Result<(), pc::SandboxError> {
        if self.pending.is_some() {
            authorize()?;
            Self::validate_root(&self.root, self.identity)?;
            match self.finish_mount().await.expect("pending Memory operation") {
                PendingMountCompletion::Mounted | PendingMountCompletion::NoEffect(_) => {}
                PendingMountCompletion::RecoveryRequired(_) => {
                    authorize()?;
                    Self::validate_root(&self.root, self.identity)?;
                    self.recover_unknown_mount().await?;
                    authorize()?;
                    Self::validate_root(&self.root, self.identity)?;
                }
            }
        }
        let selected: Vec<&dyn pc::MemoryMount> = self
            .mounts
            .iter()
            .filter(|mount| retire(mount))
            .map(|mount| mount.guard.as_ref())
            .collect();
        crate::teardown_memory_mounts(&selected, || {
            authorize()?;
            if !self.mounts.is_empty() {
                Self::validate_root(&self.root, self.identity)?;
            }
            Ok(())
        })
        .await?;
        self.mounts.retain(|mount| !retire(mount));
        if self.mounts.is_empty() {
            self.identity = None;
        }
        Ok(())
    }
}

impl Deref for MemoryMountState {
    type Target = Vec<MemoryMountParticipant>;
    fn deref(&self) -> &Self::Target {
        &self.mounts
    }
}

#[cfg(test)]
impl From<Vec<Box<dyn pc::MemoryMount>>> for MemoryMountState {
    fn from(mounts: Vec<Box<dyn pc::MemoryMount>>) -> Self {
        Self {
            mounts: mounts
                .into_iter()
                .map(|guard| MemoryMountParticipant {
                    key: None,
                    confirmed: true,
                    guard,
                })
                .collect(),
            ..Self::default()
        }
    }
}

#[cfg(test)]
impl MemoryMountState {
    pub(crate) fn push(&mut self, guard: Box<dyn pc::MemoryMount>) {
        self.mounts.push(MemoryMountParticipant {
            key: None,
            confirmed: true,
            guard,
        });
    }
}

#[cfg(test)]
mod tests;
