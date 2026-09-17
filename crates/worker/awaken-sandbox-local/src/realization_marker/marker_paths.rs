use super::*;

pub(super) fn private_stage_leaf(
    root: &Path,
    incarnation: &str,
) -> Result<String, pc::SandboxError> {
    let name = root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| pc::SandboxError::invalid("sandbox root has no portable final component"))?;
    Ok(format!(".{name}.awaken-realization-stage-{incarnation}"))
}

pub(super) fn stage_path(root: &Path, record: &RootRecord) -> Result<PathBuf, pc::SandboxError> {
    let leaf = stage_leaf(record)?;
    let parent = root
        .parent()
        .ok_or_else(|| pc::SandboxError::invalid("sandbox root has no provider-owned parent"))?;
    Ok(parent.join(leaf))
}

pub(super) fn validate_stage_leaf(record: &RootRecord) -> Result<(), pc::SandboxError> {
    stage_leaf(record).map(|_| ())
}

pub(super) fn stage_leaf(record: &RootRecord) -> Result<&Path, pc::SandboxError> {
    let leaf = Path::new(&record.private_stage_leaf);
    if leaf.components().count() == 1
        && matches!(leaf.components().next(), Some(Component::Normal(_)))
    {
        Ok(leaf)
    } else {
        Err(pc::SandboxError::invalid(
            "sandbox realization marker has an invalid private stage leaf",
        ))
    }
}

pub(super) fn root_leaf(root: &Path) -> Result<&Path, pc::SandboxError> {
    let leaf = root
        .file_name()
        .map(Path::new)
        .ok_or_else(|| pc::SandboxError::invalid("sandbox root has no final component"))?;
    if leaf.components().count() == 1
        && matches!(leaf.components().next(), Some(Component::Normal(_)))
    {
        Ok(leaf)
    } else {
        Err(pc::SandboxError::invalid(
            "sandbox root has an invalid final component",
        ))
    }
}

pub(super) fn marker_leaf(root: &Path) -> Result<PathBuf, pc::SandboxError> {
    let name = root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| pc::SandboxError::invalid("sandbox root has no portable final component"))?;
    Ok(PathBuf::from(format!(".{name}.awaken-realization.json")))
}

pub(crate) fn marker_path(root: &Path) -> Result<PathBuf, pc::SandboxError> {
    let parent = root
        .parent()
        .ok_or_else(|| pc::SandboxError::invalid("sandbox root has no provider-owned parent"))?;
    Ok(parent.join(marker_leaf(root)?))
}

pub(super) fn lock_path(root: &Path) -> Result<PathBuf, pc::SandboxError> {
    let name = root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| pc::SandboxError::invalid("sandbox root has no portable final component"))?;
    let parent = root
        .parent()
        .ok_or_else(|| pc::SandboxError::invalid("sandbox root has no provider-owned parent"))?;
    Ok(parent.join(format!(".{name}.awaken-realization.lock")))
}

pub(super) fn acquire(
    root: &Path,
) -> Result<awaken_sandbox_fs::ExclusiveFileLock, pc::SandboxError> {
    let lock = lock_path(root)?;
    let parent = lock
        .parent()
        .ok_or_else(|| pc::SandboxError::invalid("sandbox realization lock has no parent"))?;
    std::fs::create_dir_all(parent).map_err(|error| pc::SandboxError::backend("acquire", error))?;
    awaken_sandbox_fs::try_lock_exclusive(&lock).map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            pc::SandboxError::not_ready(format!(
                "sandbox realization `{}` is already being mutated",
                root.display()
            ))
        } else {
            pc::SandboxError::backend(
                "acquire sandbox realization lock",
                format!("`{}`: {error}", lock.display()),
            )
        }
    })
}
