// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Existing inode lock and retained-parent sibling effects, moved
// @formal reason: intact into one private filesystem component. Callers retain
// @formal reason: authorization; native filesystem effects remain TCB, not proof.
//! The retained inode lock and its descriptor-relative sibling operations.

use super::ExclusiveFileLock;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::{
    DirectoryIdentity, PRIVATE_STAGE_SEQUENCE, PathEntry, classify_nofollow, descriptor_identity,
    directory_entry_names, io_error, remove_directory_contents, validate_sibling_leaf,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::{Read as _, Write as _};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{fs::File, path::Path, sync::atomic::Ordering};

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
impl Drop for ExclusiveFileLock {
    fn drop(&mut self) {
        // A concurrent fork inherits the locked open-file description before
        // CLOEXEC can close it in the child. Closing only the parent's fd can
        // therefore leave a short false-WouldBlock window. LOCK_UN operates on
        // that shared description, so guard lifetime remains the sole authority
        // even while an unrelated child is between fork and exec.
        // A forked child must never unlock the parent's live guard if it drops
        // inherited Rust state instead of execing. Only the acquiring process
        // can end the authoritative guard lifetime.
        if self.owner_pid == std::process::id() {
            let _ = self._file.unlock();
        }
    }
}

/// Try to acquire an exclusive lock without waiting or following a symlink.
///
/// `WouldBlock` means another process currently owns the same inode. This
/// primitive creates an absent file with owner-only permissions, rejects
/// non-regular or hard-linked aliases, and never removes or replaces the path.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn try_lock_exclusive(path: &Path) -> std::io::Result<ExclusiveFileLock> {
    use std::os::unix::fs::MetadataExt as _;

    let parent_path = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("exclusive lock `{}` has no parent", path.display()),
        )
    })?;
    let leaf = validate_sibling_leaf(path.file_name().map(Path::new).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("exclusive lock `{}` has no file name", path.display()),
        )
    })?)?;
    let parent_descriptor = rustix::fs::open(
        parent_path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(io_error)?;
    let parent_identity = descriptor_identity(&parent_descriptor)?;
    let descriptor = rustix::fs::openat(
        &parent_descriptor,
        leaf,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .map_err(io_error)?;
    let file = File::from(descriptor);
    let opened = file.metadata()?;
    if !opened.is_file() || opened.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "exclusive lock `{}` is not one unaliased regular file",
                path.display()
            ),
        ));
    }
    file.try_lock()?;

    // Prove that the name still identifies the inode we locked. The containing
    // directory is provider-owned; this closes the open-to-lock replacement
    // window without inventing a second pathname lock protocol.
    let named = rustix::fs::statat(
        &parent_descriptor,
        leaf,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(io_error)?;
    if rustix::fs::FileType::from_raw_mode(named.st_mode) != rustix::fs::FileType::RegularFile
        || named.st_dev as u64 != opened.dev()
        || named.st_ino as u64 != opened.ino()
        || named.st_nlink != 1
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "exclusive lock `{}` changed identity while being acquired",
                path.display()
            ),
        ));
    }
    let lock = ExclusiveFileLock {
        owner_pid: std::process::id(),
        parent_path: parent_path.to_owned(),
        parent_identity,
        parent: File::from(parent_descriptor),
        _file: file,
    };
    lock.validate_parent_path()?;
    Ok(lock)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn classify_sibling_at<Fd: std::os::fd::AsFd>(
    parent: Fd,
    leaf: &std::ffi::OsStr,
) -> std::io::Result<PathEntry> {
    let metadata = match rustix::fs::statat(parent, leaf, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => metadata,
        Err(rustix::io::Errno::NOENT) => return Ok(PathEntry::Absent),
        Err(error) => return Err(io_error(error)),
    };
    let kind = rustix::fs::FileType::from_raw_mode(metadata.st_mode);
    Ok(if kind == rustix::fs::FileType::Directory {
        PathEntry::Directory(DirectoryIdentity {
            device: metadata.st_dev as u64,
            inode: metadata.st_ino as u64,
        })
    } else if kind == rustix::fs::FileType::RegularFile {
        PathEntry::RegularFile
    } else if kind == rustix::fs::FileType::Symlink {
        PathEntry::Symlink
    } else {
        PathEntry::Other
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ExclusiveFileLock {
    /// Prove that the configured parent pathname still names the retained
    /// directory. All sibling effects use `parent` regardless, so a rename or
    /// symlink replacement can never redirect them into a foreign directory;
    /// this pre/post proof makes that loss fail closed to the caller as well.
    pub fn validate_parent_path(&self) -> std::io::Result<()> {
        match classify_nofollow(&self.parent_path)? {
            PathEntry::Directory(observed) if observed == self.parent_identity => Ok(()),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "locked parent `{}` changed physical identity",
                    self.parent_path.display()
                ),
            )),
        }
    }

    pub fn classify_sibling_nofollow(&self, leaf: &Path) -> std::io::Result<PathEntry> {
        let leaf = validate_sibling_leaf(leaf)?;
        self.validate_parent_path()?;
        let entry = classify_sibling_at(&self.parent, leaf)?;
        self.validate_parent_path()?;
        Ok(entry)
    }

    pub fn sibling_names(&self) -> std::io::Result<Vec<std::ffi::OsString>> {
        use std::os::unix::ffi::OsStringExt as _;

        self.validate_parent_path()?;
        let directory = rustix::fs::openat(
            &self.parent,
            ".",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(io_error)?;
        let mut names = directory_entry_names(&directory)?
            .into_iter()
            .map(|name| std::ffi::OsString::from_vec(name.into_bytes()))
            .collect::<Vec<_>>();
        names.sort();
        self.validate_parent_path()?;
        Ok(names)
    }

    pub fn read_sibling_regular_file_nofollow(&self, leaf: &Path) -> std::io::Result<Vec<u8>> {
        use std::os::unix::fs::MetadataExt as _;

        let leaf = validate_sibling_leaf(leaf)?;
        self.validate_parent_path()?;
        let descriptor = rustix::fs::openat(
            &self.parent,
            leaf,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(io_error)?;
        let mut file = File::from(descriptor);
        let opened = file.metadata()?;
        let named = rustix::fs::statat(&self.parent, leaf, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
            .map_err(io_error)?;
        if !opened.is_file()
            || opened.nlink() != 1
            || rustix::fs::FileType::from_raw_mode(named.st_mode)
                != rustix::fs::FileType::RegularFile
            || named.st_nlink != 1
            || named.st_dev as u64 != opened.dev()
            || named.st_ino as u64 != opened.ino()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "sibling regular file is aliased or changed identity",
            ));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let named_after =
            rustix::fs::statat(&self.parent, leaf, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io_error)?;
        if named_after.st_dev as u64 != opened.dev()
            || named_after.st_ino as u64 != opened.ino()
            || named_after.st_nlink != 1
            || rustix::fs::FileType::from_raw_mode(named_after.st_mode)
                != rustix::fs::FileType::RegularFile
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "sibling regular file changed identity during read",
            ));
        }
        self.validate_parent_path()?;
        Ok(bytes)
    }

    fn create_sibling_file_stage(
        &self,
        destination_leaf: &std::ffi::OsStr,
    ) -> std::io::Result<(std::ffi::OsString, File)> {
        for _ in 0..32 {
            let sequence = PRIVATE_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let candidate = std::ffi::OsString::from(format!(
                ".{}.awaken-file-stage-{}-{sequence}",
                destination_leaf.to_string_lossy(),
                std::process::id()
            ));
            match rustix::fs::openat(
                &self.parent,
                &candidate,
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            ) {
                Ok(descriptor) => return Ok((candidate, File::from(descriptor))),
                Err(rustix::io::Errno::EXIST) => continue,
                Err(error) => return Err(io_error(error)),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a private sibling file stage",
        ))
    }

    fn write_sibling_file_stage(
        &self,
        destination_leaf: &std::ffi::OsStr,
        contents: &[u8],
    ) -> std::io::Result<std::ffi::OsString> {
        let (stage, mut file) = self.create_sibling_file_stage(destination_leaf)?;
        let result = (|| {
            file.write_all(contents)?;
            file.sync_all()?;
            Ok(stage.clone())
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&self.parent, &stage, rustix::fs::AtFlags::empty());
        }
        result
    }

    pub fn publish_sibling_file_noreplace(
        &self,
        destination_leaf: &Path,
        contents: &[u8],
    ) -> std::io::Result<()> {
        let destination_leaf = validate_sibling_leaf(destination_leaf)?;
        self.validate_parent_path()?;
        let stage = self.write_sibling_file_stage(destination_leaf, contents)?;
        let result = (|| {
            rustix::fs::renameat_with(
                &self.parent,
                &stage,
                &self.parent,
                destination_leaf,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(io_error)?;
            rustix::fs::fsync(&self.parent).map_err(io_error)?;
            self.validate_parent_path()
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&self.parent, &stage, rustix::fs::AtFlags::empty());
        }
        result
    }

    pub fn replace_sibling_regular_file_atomic(
        &self,
        destination_leaf: &Path,
        contents: &[u8],
    ) -> std::io::Result<()> {
        self.transform_sibling_regular_file_atomic(destination_leaf, |_, stage| {
            stage.write_all(contents)
        })
    }

    /// Transform one regular sibling through a private, durable stage and then
    /// atomically replace it.
    ///
    /// The callback receives the opened source and stage files so callers can
    /// keep memory bounded while re-encoding large data. Publication retains the
    /// same no-follow inode checks as [`Self::replace_sibling_regular_file_atomic`].
    pub fn transform_sibling_regular_file_atomic<F>(
        &self,
        destination_leaf: &Path,
        transform: F,
    ) -> std::io::Result<()>
    where
        F: FnOnce(&mut File, &mut File) -> std::io::Result<()>,
    {
        let destination_leaf = validate_sibling_leaf(destination_leaf)?;
        self.validate_parent_path()?;
        let descriptor = rustix::fs::openat(
            &self.parent,
            destination_leaf,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(io_error)?;
        let mut source = File::from(descriptor);
        let opened = rustix::fs::fstat(&source).map_err(io_error)?;
        if rustix::fs::FileType::from_raw_mode(opened.st_mode) != rustix::fs::FileType::RegularFile
            || opened.st_nlink != 1
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "replacement sibling is not one unaliased regular file",
            ));
        }
        let (stage, mut stage_file) = self.create_sibling_file_stage(destination_leaf)?;
        let result = (|| {
            transform(&mut source, &mut stage_file)?;
            stage_file.sync_all()?;
            let named = rustix::fs::statat(
                &self.parent,
                destination_leaf,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(io_error)?;
            if named.st_dev != opened.st_dev
                || named.st_ino != opened.st_ino
                || named.st_nlink != 1
                || rustix::fs::FileType::from_raw_mode(named.st_mode)
                    != rustix::fs::FileType::RegularFile
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "replacement sibling changed identity before publication",
                ));
            }
            rustix::fs::renameat(&self.parent, &stage, &self.parent, destination_leaf)
                .map_err(io_error)?;
            rustix::fs::fsync(&self.parent).map_err(io_error)?;
            self.validate_parent_path()
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&self.parent, &stage, rustix::fs::AtFlags::empty());
        }
        result
    }

    pub fn remove_sibling_regular_file_nofollow(&self, leaf: &Path) -> std::io::Result<()> {
        let leaf = validate_sibling_leaf(leaf)?;
        self.validate_parent_path()?;
        let descriptor = rustix::fs::openat(
            &self.parent,
            leaf,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(io_error)?;
        let opened = rustix::fs::fstat(&descriptor).map_err(io_error)?;
        let named = rustix::fs::statat(&self.parent, leaf, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
            .map_err(io_error)?;
        if rustix::fs::FileType::from_raw_mode(opened.st_mode) != rustix::fs::FileType::RegularFile
            || opened.st_nlink != 1
            || opened.st_dev != named.st_dev
            || opened.st_ino != named.st_ino
            || named.st_nlink != 1
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "removed sibling is not one stable unaliased regular file",
            ));
        }
        rustix::fs::unlinkat(&self.parent, leaf, rustix::fs::AtFlags::empty()).map_err(io_error)?;
        rustix::fs::fsync(&self.parent).map_err(io_error)?;
        self.validate_parent_path()
    }

    pub fn create_sibling_directory_noreplace(
        &self,
        leaf: &Path,
    ) -> std::io::Result<DirectoryIdentity> {
        let leaf = validate_sibling_leaf(leaf)?;
        self.validate_parent_path()?;
        rustix::fs::mkdirat(
            &self.parent,
            leaf,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
        )
        .map_err(io_error)?;
        let directory = rustix::fs::openat(
            &self.parent,
            leaf,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(io_error)?;
        let identity = descriptor_identity(&directory)?;
        match classify_sibling_at(&self.parent, leaf)? {
            PathEntry::Directory(named) if named == identity => {}
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "created sibling directory changed identity",
                ));
            }
        }
        rustix::fs::fsync(&self.parent).map_err(io_error)?;
        self.validate_parent_path()?;
        Ok(identity)
    }

    pub fn publish_sibling_directory_noreplace(
        &self,
        stage_leaf: &Path,
        destination_leaf: &Path,
    ) -> std::io::Result<()> {
        let stage_leaf = validate_sibling_leaf(stage_leaf)?;
        let destination_leaf = validate_sibling_leaf(destination_leaf)?;
        self.validate_parent_path()?;
        let stage = rustix::fs::openat(
            &self.parent,
            stage_leaf,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(io_error)?;
        let identity = descriptor_identity(&stage)?;
        rustix::fs::renameat_with(
            &self.parent,
            stage_leaf,
            &self.parent,
            destination_leaf,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(io_error)?;
        match classify_sibling_at(&self.parent, destination_leaf)? {
            PathEntry::Directory(named) if named == identity => {}
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "published sibling directory changed identity",
                ));
            }
        }
        rustix::fs::fsync(&self.parent).map_err(io_error)?;
        self.validate_parent_path()
    }

    fn open_exact_sibling_directory(
        &self,
        leaf: &std::ffi::OsStr,
        expected: DirectoryIdentity,
    ) -> std::io::Result<std::os::fd::OwnedFd> {
        let directory = rustix::fs::openat(
            &self.parent,
            leaf,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(io_error)?;
        if descriptor_identity(&directory)? != expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "sibling directory has a foreign identity",
            ));
        }
        Ok(directory)
    }

    pub fn clear_sibling_directory_contents_exact(
        &self,
        leaf: &Path,
        expected: DirectoryIdentity,
    ) -> std::io::Result<()> {
        let leaf = validate_sibling_leaf(leaf)?;
        self.validate_parent_path()?;
        let directory = self.open_exact_sibling_directory(leaf, expected)?;
        remove_directory_contents(&directory)?;
        match classify_sibling_at(&self.parent, leaf)? {
            PathEntry::Directory(named) if named == expected => {}
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "cleared sibling directory changed identity",
                ));
            }
        }
        self.validate_parent_path()
    }

    pub fn remove_sibling_directory_tree_exact(
        &self,
        leaf: &Path,
        expected: DirectoryIdentity,
    ) -> std::io::Result<()> {
        let leaf = validate_sibling_leaf(leaf)?;
        self.validate_parent_path()?;
        let directory = self.open_exact_sibling_directory(leaf, expected)?;
        remove_directory_contents(&directory)?;
        match classify_sibling_at(&self.parent, leaf)? {
            PathEntry::Directory(named) if named == expected => {}
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "removed sibling directory changed identity",
                ));
            }
        }
        rustix::fs::unlinkat(&self.parent, leaf, rustix::fs::AtFlags::REMOVEDIR)
            .map_err(io_error)?;
        rustix::fs::fsync(&self.parent).map_err(io_error)?;
        self.validate_parent_path()
    }
}
