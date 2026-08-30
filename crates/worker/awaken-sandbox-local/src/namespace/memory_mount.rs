use super::*;

pub(super) type RealizedMemoryMount = (
    RenderMount,
    pc::RealizedMount,
    Option<pc::MemoryMaterializationEvidence>,
);

pub(super) async fn realize_memory_mount(
    memory_mounter: &Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
    req: &pc::MountRequirement,
    host: &std::path::Path,
    retained_mounts: &mut Vec<Box<dyn pc::MemoryMount>>,
) -> Result<Option<RealizedMemoryMount>, pc::SandboxError> {
    let pc::MountSource::MemoryStore {
        store_id,
        materialization_reference,
        write_consistency,
    } = &req.source
    else {
        return Ok(None);
    };
    let Some(mounter) = memory_mounter
        .read()
        .expect("memory mounter lock poisoned")
        .clone()
    else {
        return Err(err(format!(
            "mount {:?}: memory_store is not realizable on this provider (no memory mounter wired)",
            req.mount_id
        )));
    };
    let guard = mounter
        .mount(
            materialization_reference.as_deref().unwrap_or(store_id),
            host,
            req.access,
        )
        .await?;
    let realization = guard.realization();
    let materialization = match (realization, guard.materialization_heads()) {
        (pc::Realization::Copy, Some(heads)) => {
            match pc::MemoryMaterializationEvidence::new(
                store_id.clone(),
                req.mount_path.clone(),
                heads,
            ) {
                Ok(evidence) => Some(evidence),
                Err(cause) => {
                    retained_mounts.push(guard);
                    return Err(err(format!(
                        "mount {:?}: invalid memory materialization evidence: {cause}",
                        req.mount_id
                    )));
                }
            }
        }
        (pc::Realization::Copy, None) => {
            retained_mounts.push(guard);
            return Err(err(format!(
                "mount {:?}: copy-backed memory_store returned no durable heads",
                req.mount_id
            )));
        }
        (_, Some(_)) => {
            retained_mounts.push(guard);
            return Err(err(format!(
                "mount {:?}: non-copy memory_store returned copy materialization heads",
                req.mount_id
            )));
        }
        (_, None) => None,
    };
    if *write_consistency == pc::MemoryWriteConsistency::WriteThroughRequired
        && realization != pc::Realization::Fuse
    {
        retained_mounts.push(guard);
        return Err(err(format!(
            "mount {:?}: memory_store requires write-through FUSE realization",
            req.mount_id
        )));
    }
    retained_mounts.push(guard);
    Ok(Some((
        RenderMount {
            host: host.to_path_buf(),
            dest: req.mount_path.clone(),
            read_only: req.access == pc::MountAccess::ReadOnly,
            boundary: RenderMountBoundary::ManagedMemoryStore,
        },
        pc::RealizedMount {
            mount_id: req.mount_id.clone(),
            mount_path: req.mount_path.clone(),
            access: req.access,
            realization,
            content_hash: None,
        },
        materialization,
    )))
}
