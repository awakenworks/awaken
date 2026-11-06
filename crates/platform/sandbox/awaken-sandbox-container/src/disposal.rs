// @formal surface obligation=acp.process_cleanup_requires_observed_exit_and_retained_owner
// @formal reason: Static owner: what cleanup takes back out of an environment and what it does when it
// @formal reason: cannot. Dynamic path: the seal is a parameter of every in-container read, so the order
// @formal reason: seal-reap-read is stated by the signatures rather than by line order; a container that
// @formal reason: cannot be quiesced has its in-container reads skipped and the skip reported, while
// @formal reason: removal proceeds, because a lost Memory round and a permanently leaked container are not
// @formal reason: the same price. A rotated durable credential is excluded from that trade and still fails
// @formal reason: closed, since the copy in the container may be newer than any the host holds.
//! Taking what an environment owes back out of it, and what happens when it
//! cannot be taken.
//!
//! # Why this is one module
//!
//! Cleanup reads a live filesystem. Every such read is only sound once the
//! container has been quiesced, and the order that makes it so — seal, reap,
//! then read — used to be a sequence of lines someone had to keep in the right
//! order. It is a signature now: the reads take the seal. What also lives here
//! is the decision the seal forces when the container cannot be quiesced at
//! all, because that decision is about these reads specifically and about
//! nothing else in disposal.

use awaken_provisioning_contract as pc;

use crate::{
    ContainerRuntime, ContainerSandbox, MAX_MEMORY_SNAPSHOT_BYTES, MAX_MEMORY_SNAPSHOT_FILES, files,
};
use awaken_provisioning_contract::SandboxFile;

/// What cleanup managed to take out of an environment before removing it.
///
/// Reaping is what makes a workspace read sound, so when the container cannot
/// be quiesced the reads that run inside it are skipped. The removal is not
/// skipped with them: a lost harvest costs one round of bytes, while a
/// container left on the host costs it permanently, and a single `?` used to
/// charge the second price for the first.
pub(crate) enum DisposalHarvest {
    /// Everything cleanup owed was taken.
    Complete,
    /// The container could not be made quiet, so its in-container reads were
    /// skipped. The environment is still gone.
    SkippedInContainerReads(String),
}

impl DisposalHarvest {
    /// Report an incomplete harvest to the caller — after the removal it did
    /// not prevent.
    pub(crate) fn into_result(self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Complete => Ok(()),
            Self::SkippedInContainerReads(why) => Err(pc::SandboxError::backend(
                "dispose",
                format!(
                    "removed the environment but skipped its in-container harvest, \
                     because the container could not be quiesced: {why}"
                ),
            )),
        }
    }
}

fn replace_memory_snapshot(
    root: &std::path::Path,
    files: &[SandboxFile],
) -> Result<(), pc::SandboxError> {
    if root.exists() {
        std::fs::remove_dir_all(root)
            .map_err(|error| pc::SandboxError::backend("replace_memory_snapshot", error))?;
    }
    std::fs::create_dir_all(root)
        .map_err(|error| pc::SandboxError::backend("replace_memory_snapshot", error))?;
    let mut total = 0_u64;
    for file in files {
        let relative = std::path::Path::new(&file.path);
        if relative.is_absolute()
            || file.path.is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(pc::SandboxError::invalid(
                "unsafe path in harvested Memory snapshot",
            ));
        }
        total = total.saturating_add(file.bytes.len() as u64);
        if total > MAX_MEMORY_SNAPSHOT_BYTES || files.len() > MAX_MEMORY_SNAPSHOT_FILES {
            return Err(pc::SandboxError::invalid(
                "harvested Memory snapshot exceeds the projection limit",
            ));
        }
        let destination = root.join(relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| pc::SandboxError::backend("replace_memory_snapshot", error))?;
        }
        std::fs::write(destination, &file.bytes)
            .map_err(|error| pc::SandboxError::backend("replace_memory_snapshot", error))?;
    }
    Ok(())
}

impl<R: ContainerRuntime + 'static> ContainerSandbox<R> {
    pub(crate) async fn prepare_disposal(
        &self,
        effect_fence: Option<&pc::SandboxEffectFence>,
        authorize: impl Fn() -> Result<(), pc::SandboxError> + Sync,
    ) -> Result<DisposalHarvest, pc::SandboxError> {
        authorize()?;
        // Before anything reads the workspace. An exec whose caller was
        // cancelled is still running and still writing, so Secret write-back
        // and the Memory harvest below would otherwise capture bytes a live
        // orphan is mutating. This is the single gate both disposal edges pass
        // through: `dispose` reaches `dispose_once` through it, and
        // `prepare_disposal_for_effect` reaches `dispose_for_effect` through it.
        let seal = self.reap_exec_orphans().await;
        authorize()?;
        self.control_publication.close_for_dispose().await;
        authorize()?;
        // Writable durable credentials belong to the Session environment and
        // are harvested when that environment terminates. Managed cleanup
        // carries the root operation into the credential authority, whose
        // WAL/CAS makes response-loss replay idempotent; the local flag is only
        // an in-process fast path.
        let handle = effect_fence.map(|_| pc::Sandbox::handle(self));
        self.lifecycle
            .write_back_secrets(
                self.runtime.as_ref(),
                &self.container_id,
                effect_fence.zip(handle.as_ref()),
                &authorize,
                &seal,
            )
            .await?;
        // Explicit effectless Sandbox disposal still owns its live RunV1 Memory
        // claim. Managed terminal/continuation cleanup carries an effect
        // fence and must use Host terminal-v2 reconciliation plus the explicit
        // acknowledgement port instead of replaying this stale claim.
        if effect_fence.is_none() {
            self.harvest_memory_snapshots(&seal).await?;
        }
        Ok(match seal.unquiet() {
            None => DisposalHarvest::Complete,
            Some(why) => DisposalHarvest::SkippedInContainerReads(why.to_string()),
        })
    }

    /// Copy each writable native Memory mount back out of the container.
    ///
    /// Takes the seal because this is the read the seal exists for: a live
    /// Session process would otherwise still be mutating these paths while
    /// they are archived.
    async fn harvest_memory_snapshots(
        &self,
        // Taken for its existence, not its contents: holding one is the proof
        // that nothing new can be writing to what this reads.
        _seal: &crate::exec_attempts::DisposalSeal,
    ) -> Result<(), pc::SandboxError> {
        let mut memory = self.lifecycle.memory.lock().await;
        let Some(memory) = memory.as_mut() else {
            return Ok(());
        };
        for mount in memory.iter_mut().filter(|mount| {
            mount.native_runtime_volume
                && mount.access == pc::MountAccess::ReadWrite
                && !mount.writeback_prepared
        }) {
            let files = files::read_files(self, &mount.mount_path).await?;
            replace_memory_snapshot(&mount.host_path, &files)?;
            mount.writeback_prepared = true;
        }
        Ok(())
    }
}
