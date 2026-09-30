// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: The existing policy-free Sandbox filesystem owner allocates private stages,
// @formal reason: publishes absent names, and replaces one validated regular file; providers
// @formal reason: retain authorization and lifecycle authority. This is not a direct proof claim.
//! Shared file and directory publication entrypoints.

use super::*;

/// Allocate one private empty directory beside `destination` and return its
/// nofollow inode identity. The caller owns the returned stage and decides when
/// to publish or remove it.
pub fn create_private_directory_stage(
    destination: &Path,
) -> std::io::Result<(PathBuf, DirectoryIdentity)> {
    let parent = destination.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("destination `{}` has no parent", destination.display()),
        )
    })?;
    let name = destination.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("destination `{}` has no file name", destination.display()),
        )
    })?;
    for _ in 0..32 {
        let sequence = PRIVATE_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let stage = parent.join(format!(
            ".{}.awaken-directory-stage-{}-{sequence}",
            name.to_string_lossy(),
            std::process::id()
        ));
        match create_directory_noreplace(&stage) {
            Ok(identity) => return Ok((stage, identity)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "could not allocate a private directory stage beside `{}`",
            destination.display()
        ),
    ))
}

/// Create one absent directory final component with owner-only Unix permissions
/// or the parent directory's inherited Windows NTFS ACL, and
/// return the exact nofollow inode identity. No parent is created and an
/// occupied name is never reused or removed.
pub fn create_directory_noreplace(path: &Path) -> std::io::Result<DirectoryIdentity> {
    #[cfg(windows)]
    return windows::create_directory_noreplace(path);
    #[cfg(not(windows))]
    {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(path)?;
        directory_identity_nofollow(path)
    }
}

/// Atomically rename `stage` to an absent sibling `destination` without ever
/// replacing an existing name.
///
/// The caller must prove ownership of `stage`; this primitive validates only
/// its structural preconditions and never deletes either path.
pub fn publish_directory_noreplace(stage: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    return windows::publish_directory_noreplace(stage, destination);
    #[cfg(not(windows))]
    {
        let stage_metadata = std::fs::symlink_metadata(stage)?;
        if stage_metadata.file_type().is_symlink() || !stage_metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("stage `{}` is not a directory", stage.display()),
            ));
        }
        if !stage.is_absolute()
            || !destination.is_absolute()
            || stage.parent().is_none()
            || stage.parent() != destination.parent()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "stage and destination must be absolute siblings",
            ));
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            rustix::fs::renameat_with(
                rustix::fs::CWD,
                stage,
                rustix::fs::CWD,
                destination,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (stage, destination);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "atomic no-replace directory publication is unsupported on this platform",
            ))
        }
    }
}

#[cfg(not(windows))]
fn create_private_file_stage(destination: &Path) -> std::io::Result<(PathBuf, File)> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("destination `{}` has no parent", destination.display()),
            )
        })?;
    let file_name = destination.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("destination `{}` has no file name", destination.display()),
        )
    })?;

    for _ in 0..32 {
        let sequence = PRIVATE_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{}.awaken-file-stage-{}-{sequence}",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "could not allocate a private publication stage beside `{}`",
            destination.display()
        ),
    ))
}

#[cfg(not(windows))]
fn write_private_file_stage(destination: &Path, contents: &[u8]) -> std::io::Result<PathBuf> {
    let (stage_path, mut stage_file) = create_private_file_stage(destination)?;
    let result = (|| {
        stage_file.write_all(contents)?;
        stage_file.sync_all()?;
        Ok(stage_path.clone())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&stage_path);
    }
    result
}

/// Atomically publish immutable file contents at an absent path.
///
/// The function owns one randomly named sibling stage created with `create_new`,
/// flushes its bytes before a no-replace rename, and removes only that stage on
/// failure. It never opens, truncates, removes, or replaces `destination`.
pub fn publish_file_noreplace(destination: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(windows)]
    return windows::publish_file_noreplace(destination, contents);
    #[cfg(not(windows))]
    {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("destination `{}` has no parent", destination.display()),
                )
            })?;
        let stage_path = write_private_file_stage(destination, contents)?;

        let result = (|| {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                rustix::fs::renameat_with(
                    rustix::fs::CWD,
                    &stage_path,
                    rustix::fs::CWD,
                    destination,
                    rustix::fs::RenameFlags::NOREPLACE,
                )
                .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "atomic no-replace file publication is unsupported on this platform",
                ));
            }

            awaken_fs_durability::sync_directory(parent)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&stage_path);
        }
        result
    }
}

/// Atomically replace one existing regular-file name with complete contents.
///
/// The caller owns replacement policy and serialization. This leaf never reads
/// or interprets the destination bytes; it only rejects a missing, non-regular,
/// symlinked, or hard-linked destination and performs one same-directory rename.
pub fn replace_regular_file_atomic(destination: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(windows)]
    return windows::replace_regular_file_atomic(destination, contents);
    #[cfg(not(windows))]
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = std::fs::symlink_metadata(destination)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.nlink() != 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "replacement destination `{}` is not one unaliased regular file",
                        destination.display()
                    ),
                ));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (destination, contents);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "atomic regular-file replacement is unsupported on this platform",
            ));
        }

        let parent = destination.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("destination `{}` has no parent", destination.display()),
            )
        })?;
        let stage = write_private_file_stage(destination, contents)?;
        let result = std::fs::rename(&stage, destination)
            .and_then(|()| awaken_fs_durability::sync_directory(parent));
        if result.is_err() {
            let _ = std::fs::remove_file(stage);
        }
        result
    }
}
