// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: The container adapter has one static owner for exact restore metadata, stable target
// @formal reason: identity, and the retained host-staging namespace; physical lifecycle remains in
// @formal reason: provider_realization and durable-handle decoding remains in recovery, so this is a
// @formal reason: product-requirement classification rather than a parallel state machine or direct
// @formal reason: formal-proof claim.
//! Exact restoration-target projection for the container provider.
//!
//! This module owns restore metadata labels, stable target identity, and the
//! retained host-staging namespace. Physical recover/create/dispose sequencing
//! remains solely in `provider_realization`; durable-handle decoding remains in
//! `recovery`.

use super::{ContainerPlan, RootfsPlan, StagingGuard, pc};
use awaken_container_contract::{LIVE_INPUTS_ROOT, RuntimeError};

pub(crate) const RESTORE_EFFECT_LABEL: &str = "awaken.sandbox.restore.effect";
pub(crate) const RESTORE_GENERATION_LABEL: &str = "awaken.sandbox.restore.generation";
pub(crate) const RESTORE_CHECKPOINT_LABEL: &str = "awaken.sandbox.restore.checkpoint";
pub(crate) const RESTORE_CHECKPOINT_DIGEST_LABEL: &str = "awaken.sandbox.restore.checkpoint-digest";
pub(crate) const RESTORE_SPEC_LABEL: &str = "awaken.sandbox.restore.spec";
pub(crate) const RESTORE_EXCLUSIONS_LABEL: &str = "awaken.sandbox.restore.exclusions";
pub const RESTORE_PLAN_LABEL: &str = "awaken.sandbox.restore.plan";

pub fn restoration_metadata(
    evidence: &pc::SandboxRestorationEvidence,
) -> [(&'static str, &str); 6] {
    [
        (RESTORE_EFFECT_LABEL, evidence.effect_id()),
        (RESTORE_GENERATION_LABEL, evidence.generation_id()),
        (RESTORE_CHECKPOINT_LABEL, evidence.checkpoint_id()),
        (
            RESTORE_CHECKPOINT_DIGEST_LABEL,
            evidence.checkpoint_digest(),
        ),
        (RESTORE_SPEC_LABEL, evidence.sandbox_spec_fingerprint()),
        (
            RESTORE_EXCLUSIONS_LABEL,
            evidence.checkpoint_exclusions_fingerprint(),
        ),
    ]
}

pub fn restoration_plan_fingerprint(plan: &ContainerPlan) -> String {
    let mut canonical = plan.clone();
    canonical
        .binds
        .retain(|bind| bind.mount_path != LIVE_INPUTS_ROOT);
    if !canonical.packages.is_empty() {
        canonical.image = "<awaken-package-derived-image>".into();
        if matches!(canonical.rootfs, RootfsPlan::Image(_)) {
            canonical.rootfs = RootfsPlan::Image("<awaken-package-derived-image>".into());
        }
    }
    blake3::hash(format!("container-restore-plan-v1\0{canonical:?}").as_bytes())
        .to_hex()
        .to_string()
}

pub fn restoration_evidence_from_metadata(
    mut value: impl FnMut(&str) -> Option<String>,
    substrate: &str,
) -> Result<Option<pc::SandboxRestorationEvidence>, RuntimeError> {
    let effect_id = value(RESTORE_EFFECT_LABEL);
    let generation_id = value(RESTORE_GENERATION_LABEL);
    let checkpoint_id = value(RESTORE_CHECKPOINT_LABEL);
    let checkpoint_digest = value(RESTORE_CHECKPOINT_DIGEST_LABEL);
    let sandbox_spec_fingerprint = value(RESTORE_SPEC_LABEL);
    let checkpoint_exclusions_fingerprint = value(RESTORE_EXCLUSIONS_LABEL);
    match (
        effect_id,
        generation_id,
        checkpoint_id,
        checkpoint_digest,
        sandbox_spec_fingerprint,
        checkpoint_exclusions_fingerprint,
    ) {
        (None, None, None, None, None, None) => Ok(None),
        (
            Some(effect_id),
            Some(generation_id),
            Some(checkpoint_id),
            Some(checkpoint_digest),
            Some(sandbox_spec_fingerprint),
            Some(checkpoint_exclusions_fingerprint),
        ) => pc::SandboxRestorationEvidence::from_exact_parts(
            effect_id,
            generation_id,
            checkpoint_id,
            checkpoint_digest,
            sandbox_spec_fingerprint,
            checkpoint_exclusions_fingerprint,
        )
        .map(Some)
        .map_err(|error| RuntimeError::Backend(error.to_string())),
        _ => Err(RuntimeError::Backend(format!(
            "{substrate} has incomplete restore evidence"
        ))),
    }
}

pub(super) fn retained_host_staging(
    handle: Option<&crate::ContainerContinuationHandle>,
) -> Result<Option<StagingGuard>, RuntimeError> {
    let Some(crate::ContainerContinuationHandle::HostBindRestoration(locator)) = handle else {
        return Ok(None);
    };
    retained_host_staging_path(std::path::PathBuf::from(locator.staging_root())).map(Some)
}

pub(crate) fn retained_host_staging_path(
    path: std::path::PathBuf,
) -> Result<StagingGuard, RuntimeError> {
    validate_host_staging_path(&path)?;
    let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RuntimeError::Backend("retained host staging path is missing".into())
        } else {
            RuntimeError::Backend(error.to_string())
        }
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(RuntimeError::Backend(
            "restored host-bind staging locator is not a physical directory".into(),
        ));
    }
    Ok(StagingGuard::retained(path))
}

fn validate_host_staging_path(path: &std::path::Path) -> Result<(), RuntimeError> {
    let provider_temp = std::env::temp_dir();
    let trusted_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("awaken-acp-stage-"));
    if path.parent() != Some(provider_temp.as_path()) || !trusted_name {
        return Err(RuntimeError::Backend(
            "restored host-bind staging locator is outside the provider staging namespace".into(),
        ));
    }
    Ok(())
}

#[cfg(any(test, feature = "docker", feature = "podman"))]
pub(super) fn remove_host_staging_path(path: &std::path::Path) -> Result<(), RuntimeError> {
    validate_host_staging_path(path)?;
    remove_staging_tree(path).map_err(|error| RuntimeError::Backend(error.to_string()))
}

/// Remove one host staging tree, whatever its projections left it as.
///
/// This is the one removal every cleanup of a staging tree goes through: the
/// guard's compensation and explicit release, terminal removal of a restored
/// host-bind root, and replacement of a harvested Memory snapshot. A read-only
/// Memory copy is closed to `0o555` for the container
/// (`make_memory_tree_accessible`), and a directory without owner write cannot
/// be emptied, so owner access is restored first.
///
/// Restoration never follows a symbolic link. A symlink or other non-directory
/// entry is only ever unlinked, which needs its parent's write bit and not its
/// own mode. A directory's mode changes only through a handle whose device and
/// inode equal the no-follow `lstat` of that same entry, so an entry replaced by
/// a link after it was classified is refused rather than chmodded through.
/// Nothing outside the root can therefore have its mode changed by cleanup.
/// Removal is `std::fs::remove_dir_all`, which does not follow links either.
///
/// Every entry is attempted. When the whole tree cannot go, each of the root's
/// children is still removed on its own, so one unrestorable entry leaves only
/// itself behind. Success means the tree is gone and every entry was restored;
/// anything that could not be restored is reported together with the removal's
/// own outcome.
pub(crate) fn remove_staging_tree(root: &std::path::Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return std::fs::remove_file(root);
    }
    let unrestored = restore_owner_access(root, metadata);
    let removed = std::fs::remove_dir_all(root).map_err(|error| {
        let remaining = remove_children(root);
        if remaining.is_empty() {
            error
        } else {
            std::io::Error::new(
                error.kind(),
                format!("{error}; could not remove: {}", remaining.join("; ")),
            )
        }
    });
    match (removed, unrestored.is_empty()) {
        (Ok(()), true) => Ok(()),
        (Ok(()), false) => Err(std::io::Error::other(format!(
            "removed staging tree {} but could not restore owner access on: {}",
            root.display(),
            unrestored.join("; ")
        ))),
        (Err(error), true) => Err(error),
        (Err(error), false) => Err(std::io::Error::new(
            error.kind(),
            format!(
                "remove staging tree {}: {error}; could not restore owner access on: {}",
                root.display(),
                unrestored.join("; ")
            ),
        )),
    }
}

/// Remove each child of a staging root independently after the whole-tree
/// removal failed, returning one description per child that could not go.
fn remove_children(root: &std::path::Path) -> Vec<String> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => return vec![format!("{}: {error}", root.display())],
    };
    let mut remaining = Vec::new();
    for entry in entries {
        let removed = entry.and_then(|entry| {
            let path = entry.path();
            if std::fs::symlink_metadata(&path)?.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            }
            .map_err(|error| {
                std::io::Error::new(error.kind(), format!("{}: {error}", path.display()))
            })
        });
        if let Err(error) = removed {
            remaining.push(error.to_string());
        }
    }
    remaining
}

/// Give the owner `rwx` on every physical directory under `root`, returning
/// one description per entry it could not.
#[cfg(unix)]
fn restore_owner_access(root: &std::path::Path, root_metadata: std::fs::Metadata) -> Vec<String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    const OWNER_ACCESS: u32 = 0o700;
    let mut unrestored = Vec::new();
    let mut pending = vec![(root.to_path_buf(), root_metadata)];
    while let Some((directory, classified)) = pending.pop() {
        if classified.mode() & OWNER_ACCESS != OWNER_ACCESS {
            let restored = std::fs::File::open(&directory).and_then(|handle| {
                let opened = handle.metadata()?;
                if !opened.is_dir()
                    || (opened.dev(), opened.ino()) != (classified.dev(), classified.ino())
                {
                    return Err(std::io::Error::other(
                        "entry changed identity after its no-follow classification",
                    ));
                }
                handle.set_permissions(std::fs::Permissions::from_mode(
                    (opened.mode() & 0o7777) | OWNER_ACCESS,
                ))
            });
            if let Err(error) = restored {
                unrestored.push(format!("{}: {error}", directory.display()));
                continue;
            }
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                unrestored.push(format!("{}: {error}", directory.display()));
                continue;
            }
        };
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(error) => {
                    unrestored.push(format!("{}: {error}", directory.display()));
                    continue;
                }
            };
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() => pending.push((path, metadata)),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => unrestored.push(format!("{}: {error}", path.display())),
            }
        }
    }
    unrestored
}

/// Without Unix modes nothing in a staging tree was closed to its owner.
#[cfg(not(unix))]
fn restore_owner_access(_root: &std::path::Path, _root_metadata: std::fs::Metadata) -> Vec<String> {
    Vec::new()
}

pub(super) fn restoration_runtime_scope(
    evidence: &pc::SandboxRestorationEvidence,
) -> Result<String, pc::SandboxError> {
    evidence.physical_target_key()
}

#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) fn restore_container_name(scope: &str) -> String {
    let identity = blake3::hash(scope.as_bytes()).to_hex();
    format!("awaken-restore-{identity}")
}
