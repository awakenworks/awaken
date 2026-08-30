use super::*;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn directory_entry_names<Fd: std::os::fd::AsFd>(
    directory: Fd,
) -> std::io::Result<Vec<std::ffi::CString>> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;

    let mut buffer = [MaybeUninit::<u8>::uninit(); 8192];
    let mut entries = Vec::<CString>::new();
    let mut iterator = rustix::fs::RawDir::new(directory.as_fd(), &mut buffer);
    while let Some(entry) = iterator.next() {
        let entry = entry.map_err(io_error)?;
        if entry.file_name().to_bytes() == b"." || entry.file_name().to_bytes() == b".." {
            continue;
        }
        entries.push(entry.file_name().to_owned());
    }
    Ok(entries)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn remove_directory_contents<Fd: std::os::fd::AsFd>(
    directory: Fd,
) -> std::io::Result<()> {
    // The exact provider-owned tree may legitimately contain restored or
    // materialized read-only directories. Deletion authority comes from the
    // retained descriptor identity, so restore owner traversal/write bits on
    // that descriptor before unlinking children; no pathname permission bypass
    // or foreign inode is introduced.
    rustix::fs::fchmod(
        directory.as_fd(),
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
    )
    .map_err(io_error)?;
    let entries = directory_entry_names(directory.as_fd())?;

    for name in entries {
        match rustix::fs::openat(
            directory.as_fd(),
            &name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        ) {
            Ok(child) => {
                let expected = descriptor_identity(&child)?;
                remove_directory_contents(&child)?;
                let named = rustix::fs::statat(
                    directory.as_fd(),
                    &name,
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                )
                .map_err(io_error)?;
                if named.st_dev as u64 != expected.device || named.st_ino as u64 != expected.inode {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "directory entry changed identity during exact cleanup",
                    ));
                }
                rustix::fs::unlinkat(directory.as_fd(), &name, rustix::fs::AtFlags::REMOVEDIR)
                    .map_err(io_error)?;
            }
            Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
                rustix::fs::unlinkat(directory.as_fd(), &name, rustix::fs::AtFlags::empty())
                    .map_err(io_error)?;
            }
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(())
}

/// Remove one relative file, symlink name, special-file name, or directory tree
/// beneath an exact root dirfd. Missing parents/final names are idempotent; no
/// symlink is followed and the root itself can never be selected.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn remove_relative_entry_exact(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
) -> std::io::Result<()> {
    let root = open_exact_root(root, expected_root)?;
    let Some((parent, name)) = open_relative_parent(root, relative, false)? else {
        return Ok(());
    };
    match rustix::fs::openat(
        &parent,
        &name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    ) {
        Ok(directory) => {
            let identity = descriptor_identity(&directory)?;
            remove_directory_contents(&directory)?;
            let named = rustix::fs::statat(&parent, &name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io_error)?;
            if named.st_dev as u64 != identity.device || named.st_ino as u64 != identity.inode {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "relative directory changed identity during exact removal",
                ));
            }
            rustix::fs::unlinkat(&parent, &name, rustix::fs::AtFlags::REMOVEDIR).map_err(io_error)
        }
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
            match rustix::fs::unlinkat(&parent, &name, rustix::fs::AtFlags::empty()) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
                Err(error) => Err(io_error(error)),
            }
        }
        Err(error) => Err(io_error(error)),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn remove_relative_entry_exact(
    root: &Path,
    _expected_root: DirectoryIdentity,
    _relative: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "descriptor-relative removal is unsupported for `{}` on this platform",
            root.display()
        ),
    ))
}
