//! Policy-free filesystem primitives shared by sandbox providers.
//!
//! Repository identity, sandbox realization ownership, retry, and cleanup
//! remain in their provider adapters. This crate owns only the filesystem
//! primitives shared at those boundaries: exclusive locking of one stable
//! sidecar inode and no-replace publication of one directory.

use std::fs::File;
#[cfg(unix)]
use std::io::Read as _;
#[cfg(not(windows))]
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static PRIVATE_STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Immutable identity of one opened directory inode.
///
/// This is policy-free physical evidence. Provider adapters decide whether an
/// identity is authorized; this crate only compares it at no-follow filesystem
/// edges.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectoryIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Nofollow classification of one final path component.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathEntry {
    Absent,
    Directory(DirectoryIdentity),
    RegularFile,
    Symlink,
    Other,
}

/// One regular file captured from an exact descriptor-relative tree walk.
///
/// `relative_path` is relative to the requested tree root, never to a host
/// pathname. The bytes come from the same nofollow descriptor whose identity
/// was checked during the walk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeFile {
    pub relative_path: PathBuf,
    pub bytes: Vec<u8>,
    pub mode: u32,
}

/// One directory captured by the same exact tree walk as [`TreeFile`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeDirectory {
    pub relative_path: PathBuf,
    pub mode: u32,
}

/// One complete descriptor-relative tree snapshot. Entries are sorted by their
/// canonical relative paths and never include the requested tree root itself.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TreeSnapshot {
    pub directories: Vec<TreeDirectory>,
    pub files: Vec<TreeFile>,
}

/// A process-scoped exclusive lock on one stable, non-symlink filesystem inode.
///
/// Dropping the value explicitly releases the OS lock before closing its file.
/// The caller owns the sidecar's lifecycle and must not unlink or replace it
/// while it can be used as a lock identity.
#[derive(Debug)]
pub struct ExclusiveFileLock {
    owner_pid: u32,
    parent_path: PathBuf,
    parent_identity: DirectoryIdentity,
    parent: File,
    _file: File,
    #[cfg(windows)]
    _ancestors: Vec<File>,
}

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

fn validate_sibling_leaf(path: &Path) -> std::io::Result<&std::ffi::OsStr> {
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(leaf)), None) => Ok(leaf),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "sibling path `{}` is not one canonical leaf",
                path.display()
            ),
        )),
    }
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
        let destination_leaf = validate_sibling_leaf(destination_leaf)?;
        self.validate_parent_path()?;
        let descriptor = rustix::fs::openat(
            &self.parent,
            destination_leaf,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(io_error)?;
        let opened = rustix::fs::fstat(&descriptor).map_err(io_error)?;
        if rustix::fs::FileType::from_raw_mode(opened.st_mode) != rustix::fs::FileType::RegularFile
            || opened.st_nlink != 1
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "replacement sibling is not one unaliased regular file",
            ));
        }
        let stage = self.write_sibling_file_stage(destination_leaf, contents)?;
        let result = (|| {
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

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[path = "locked_parent_fallback.rs"]
mod locked_parent_fallback;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub use locked_parent_fallback::try_lock_exclusive;
/// Inspect the final path component without following a symlink.
#[cfg(not(windows))]
pub fn classify_nofollow(path: &Path) -> std::io::Result<PathEntry> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PathEntry::Absent);
        }
        Err(error) => return Err(error),
    };
    let kind = metadata.file_type();
    if kind.is_symlink() {
        return Ok(PathEntry::Symlink);
    }
    if kind.is_dir() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            return Ok(PathEntry::Directory(DirectoryIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }));
        }
        #[cfg(not(unix))]
        {
            return Ok(PathEntry::Directory(locked_parent_fallback::identity(
                path,
            )?));
        }
    }
    Ok(if kind.is_file() {
        PathEntry::RegularFile
    } else {
        PathEntry::Other
    })
}

/// Open and identify one exact directory without following its final component.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn directory_identity_nofollow(path: &Path) -> std::io::Result<DirectoryIdentity> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    let opened = rustix::fs::fstat(&descriptor)
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    let identity = DirectoryIdentity {
        device: opened.st_dev as u64,
        inode: opened.st_ino as u64,
    };
    match classify_nofollow(path)? {
        PathEntry::Directory(named) if named == identity => Ok(identity),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "directory `{}` changed identity while being opened",
                path.display()
            ),
        )),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn directory_identity_nofollow(path: &Path) -> std::io::Result<DirectoryIdentity> {
    locked_parent_fallback::identity(path)
}

/// Read one unaliased regular file without following its final component.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn read_regular_file_nofollow(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::MetadataExt as _;

    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    let mut file = File::from(descriptor);
    let opened = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    if !opened.is_file()
        || opened.nlink() != 1
        || !named.is_file()
        || named.nlink() != 1
        || opened.dev() != named.dev()
        || opened.ino() != named.ino()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "regular file `{}` is aliased or changed identity while being opened",
                path.display()
            ),
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Remove one unaliased regular-file final component without following it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn remove_regular_file_nofollow(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("file `{}` has no parent", path.display()),
        )
    })?;
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("file `{}` has no file name", path.display()),
        )
    })?;
    let parent = rustix::fs::open(
        parent,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    let descriptor = rustix::fs::openat(
        &parent,
        name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    let opened = rustix::fs::fstat(&descriptor)
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    let named = rustix::fs::statat(&parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    if rustix::fs::FileType::from_raw_mode(opened.st_mode) != rustix::fs::FileType::RegularFile
        || opened.st_nlink != 1
        || opened.st_dev != named.st_dev
        || opened.st_ino != named.st_ino
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("file `{}` is aliased or changed identity", path.display()),
        ));
    }
    rustix::fs::unlinkat(&parent, name, rustix::fs::AtFlags::empty())
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn remove_regular_file_nofollow(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("file `{}` is not a regular file", path.display()),
        ));
    }
    std::fs::remove_file(path)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn read_regular_file_nofollow(path: &Path) -> std::io::Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("file `{}` is not a regular file", path.display()),
        ));
    }
    std::fs::read(path)
}

/// Allocate one private empty directory beside `destination` and return its
/// nofollow inode identity. The caller owns the returned stage and decides when
/// to publish or remove it.
#[cfg(not(windows))]
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
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(&stage) {
            Ok(()) => return Ok((stage.clone(), directory_identity_nofollow(&stage)?)),
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

/// Create one absent directory final component with owner-only permissions and
/// return the exact nofollow inode identity. No parent is created and an
/// occupied name is never reused or removed.
#[cfg(not(windows))]
pub fn create_directory_noreplace(path: &Path) -> std::io::Result<DirectoryIdentity> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)?;
    directory_identity_nofollow(path)
}

/// Atomically rename `stage` to an absent sibling `destination` without ever
/// replacing an existing name.
///
/// The caller must prove ownership of `stage`; this primitive validates only
/// its structural preconditions and never deletes either path.
#[cfg(not(windows))]
pub fn publish_directory_noreplace(stage: &Path, destination: &Path) -> std::io::Result<()> {
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
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        if destination.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("destination `{}` already exists", destination.display()),
            ));
        }
        std::fs::rename(stage, destination)
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
#[cfg(not(windows))]
pub fn publish_file_noreplace(destination: &Path, contents: &[u8]) -> std::io::Result<()> {
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
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "atomic no-replace file publication is unsupported on this platform",
            ));
        }

        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&stage_path);
    }
    result
}

/// Atomically replace one existing regular-file name with complete contents.
///
/// The caller owns replacement policy and serialization. This leaf never reads
/// or interprets the destination bytes; it only rejects a missing, non-regular,
/// symlinked, or hard-linked destination and performs one same-directory rename.
#[cfg(not(windows))]
pub fn replace_regular_file_atomic(destination: &Path, contents: &[u8]) -> std::io::Result<()> {
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
    let parent = destination.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("destination `{}` has no parent", destination.display()),
        )
    })?;
    let stage = write_private_file_stage(destination, contents)?;
    #[cfg(unix)]
    let result = std::fs::rename(&stage, destination).and_then(|()| File::open(parent)?.sync_all());
    #[cfg(not(any(unix, windows)))]
    let result = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic regular-file replacement is unsupported on this platform",
    ));
    if result.is_err() {
        let _ = std::fs::remove_file(stage);
    }
    result
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn io_error(error: rustix::io::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn descriptor_identity<Fd: std::os::fd::AsFd>(
    descriptor: Fd,
) -> std::io::Result<DirectoryIdentity> {
    let stat = rustix::fs::fstat(descriptor).map_err(io_error)?;
    Ok(DirectoryIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_exact_root(
    root: &Path,
    expected: DirectoryIdentity,
) -> std::io::Result<std::os::fd::OwnedFd> {
    let directory = rustix::fs::open(
        root,
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
            format!(
                "filesystem root `{}` has a foreign identity",
                root.display()
            ),
        ));
    }
    Ok(directory)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_child_directory(
    directory: &std::os::fd::OwnedFd,
    component: &std::ffi::OsStr,
    create: bool,
) -> std::io::Result<Option<std::os::fd::OwnedFd>> {
    match rustix::fs::openat(
        directory,
        component,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    ) {
        Ok(child) => Ok(Some(child)),
        Err(rustix::io::Errno::NOENT) if !create => Ok(None),
        Err(rustix::io::Errno::NOENT) => {
            rustix::fs::mkdirat(
                directory,
                component,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
            )
            .map_err(io_error)?;
            rustix::fs::openat(
                directory,
                component,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .map(Some)
            .map_err(io_error)
        }
        Err(error) => Err(io_error(error)),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_relative_parent(
    mut directory: std::os::fd::OwnedFd,
    relative: &Path,
    create_parents: bool,
) -> std::io::Result<Option<(std::os::fd::OwnedFd, std::ffi::OsString)>> {
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(component) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "path `{}` is not canonical and relative",
                    relative.display()
                ),
            ));
        };
        if components.peek().is_none() {
            return Ok(Some((directory, component.to_owned())));
        }
        let Some(child) = open_child_directory(&directory, component, create_parents)? else {
            return Ok(None);
        };
        directory = child;
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "relative file path is empty",
    ))
}

/// Overwrite one canonical relative regular file with zero bytes while bound to
/// an exact root dirfd. Missing parents/files are idempotent; every component is
/// opened nofollow and a symlink, hard link, special file, or identity race is
/// rejected before the provider may remove its root.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn zero_relative_regular_file_nofollow(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
) -> std::io::Result<()> {
    let root = open_exact_root(root, expected_root)?;
    let Some((parent, name)) = open_relative_parent(root, relative, false)? else {
        return Ok(());
    };
    let descriptor = match rustix::fs::openat(
        &parent,
        &name,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(io_error(error)),
    };
    let opened = rustix::fs::fstat(&descriptor).map_err(io_error)?;
    let named = rustix::fs::statat(&parent, &name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io_error)?;
    if rustix::fs::FileType::from_raw_mode(opened.st_mode) != rustix::fs::FileType::RegularFile
        || opened.st_nlink != 1
        || opened.st_dev != named.st_dev
        || opened.st_ino != named.st_ino
        || rustix::fs::FileType::from_raw_mode(named.st_mode) != rustix::fs::FileType::RegularFile
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "secret shred target is not one stable unaliased regular file",
        ));
    }

    let mut file = File::from(descriptor);
    let mut remaining: u64 = opened
        .st_size
        .try_into()
        .map_err(|_| std::io::Error::other("secret shred target has a negative size"))?;
    let zeros = [0_u8; 8192];
    while remaining != 0 {
        let length =
            usize::try_from(remaining.min(zeros.len() as u64)).expect("bounded shred chunk length");
        file.write_all(&zeros[..length])?;
        remaining -= length as u64;
    }
    file.sync_all()?;

    let named_after = rustix::fs::statat(&parent, &name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io_error)?;
    if opened.st_dev != named_after.st_dev
        || opened.st_ino != named_after.st_ino
        || named_after.st_nlink != 1
        || rustix::fs::FileType::from_raw_mode(named_after.st_mode)
            != rustix::fs::FileType::RegularFile
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "secret shred target changed identity during overwrite",
        ));
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn zero_relative_regular_file_nofollow(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
) -> std::io::Result<()> {
    if directory_identity_nofollow(root)? != expected_root {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sandbox root identity changed",
        ));
    }
    validate_relative_tree_path(relative, false)?;
    let path = root.join(relative);
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "shred target is not a regular file",
        ));
    }
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    let mut remaining = metadata.len();
    let zeros = [0_u8; 8192];
    while remaining > 0 {
        let length = usize::try_from(remaining.min(zeros.len() as u64)).expect("bounded length");
        file.write_all(&zeros[..length])?;
        remaining -= length as u64;
    }
    file.sync_all()
}

/// Create (or verify) one canonical relative directory chain beneath an exact
/// root dirfd without following any component.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn create_relative_directory_all(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
) -> std::io::Result<()> {
    let mut directory = open_exact_root(root, expected_root)?;
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "path `{}` is not canonical and relative",
                    relative.display()
                ),
            ));
        };
        directory = open_child_directory(&directory, component, true)?
            .ok_or_else(|| std::io::Error::other("created directory is unexpectedly absent"))?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn create_relative_directory_all(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
) -> std::io::Result<()> {
    if directory_identity_nofollow(root)? != expected_root {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sandbox root identity changed",
        ));
    }
    validate_relative_tree_path(relative, true)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path is not canonical and relative",
            ));
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "relative component is not a directory",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Apply mode bits to one existing canonical relative directory beneath an
/// exact root dirfd. Every component is opened nofollow; the root itself cannot
/// be selected.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn set_relative_directory_mode(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
    mode: u32,
) -> std::io::Result<()> {
    if relative.as_os_str().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "relative directory path is empty",
        ));
    }
    let mut directory = open_exact_root(root, expected_root)?;
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "path `{}` is not canonical and relative",
                    relative.display()
                ),
            ));
        };
        directory = open_child_directory(&directory, component, false)?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("directory `{}` is absent", relative.display()),
            )
        })?;
    }
    rustix::fs::fchmod(&directory, portable_mode(mode)?).map_err(io_error)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn set_relative_directory_mode(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
    _mode: u32,
) -> std::io::Result<()> {
    if directory_identity_nofollow(root)? != expected_root {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sandbox root identity changed",
        ));
    }
    validate_relative_tree_path(relative, false)?;
    let metadata = std::fs::symlink_metadata(root.join(relative))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "relative path is not a directory",
        ));
    }
    Ok(())
}

/// Atomically materialize one regular file relative to an exact directory
/// descriptor. Every parent is opened with `O_NOFOLLOW`; absent parents are
/// created one component at a time. The final rename can replace only a name
/// that was observed as one unaliased regular file, and never follows it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn write_relative_file_atomic(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
    contents: &[u8],
    mode: u32,
) -> std::io::Result<()> {
    use std::os::fd::AsFd as _;

    let root = open_exact_root(root, expected_root)?;
    let (parent, destination) = open_relative_parent(root, relative, true)?
        .ok_or_else(|| std::io::Error::other("materialization parent unexpectedly absent"))?;
    let sequence = PRIVATE_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stage = format!(".awaken-write-{}-{sequence}", std::process::id());
    let descriptor = rustix::fs::openat(
        &parent,
        stage.as_str(),
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .map_err(io_error)?;
    let mut stage_file = File::from(descriptor);
    let result = (|| {
        stage_file.write_all(contents)?;
        rustix::fs::fchmod(stage_file.as_fd(), portable_mode(mode)?).map_err(io_error)?;
        stage_file.sync_all()?;

        let flags = match rustix::fs::statat(
            &parent,
            &destination,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Err(rustix::io::Errno::NOENT) => rustix::fs::RenameFlags::NOREPLACE,
            Ok(named)
                if rustix::fs::FileType::from_raw_mode(named.st_mode)
                    == rustix::fs::FileType::RegularFile
                    && named.st_nlink == 1 =>
            {
                rustix::fs::RenameFlags::empty()
            }
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "materialization destination is not one unaliased regular file",
                ));
            }
            Err(error) => return Err(io_error(error)),
        };
        rustix::fs::renameat_with(&parent, stage.as_str(), &parent, &destination, flags)
            .map_err(io_error)?;
        rustix::fs::fsync(&parent).map_err(io_error)
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&parent, stage.as_str(), rustix::fs::AtFlags::empty());
    }
    result
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn portable_mode(mode: u32) -> std::io::Result<rustix::fs::Mode> {
    let raw = mode.try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("file mode {mode:#o} is outside the platform range"),
        )
    })?;
    Ok(rustix::fs::Mode::from_raw_mode(raw))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn write_relative_file_atomic(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
    contents: &[u8],
    _mode: u32,
) -> std::io::Result<()> {
    if directory_identity_nofollow(root)? != expected_root {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sandbox root identity changed",
        ));
    }
    validate_relative_tree_path(relative, false)?;
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    create_relative_directory_all(root, expected_root, parent_relative)?;
    let destination = root.join(relative);
    if let Ok(metadata) = std::fs::symlink_metadata(&destination) {
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "materialization destination is not a regular file",
            ));
        }
    }
    let stage = write_private_file_stage(&destination, contents)?;
    if destination.exists() {
        std::fs::remove_file(&destination)?;
    }
    let result = std::fs::rename(&stage, &destination);
    if result.is_err() {
        let _ = std::fs::remove_file(stage);
    }
    result
}

#[path = "directory_tree.rs"]
#[cfg(not(windows))]
mod directory_tree;
#[cfg(not(windows))]
pub use directory_tree::remove_relative_entry_exact;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use directory_tree::{directory_entry_names, remove_directory_contents};

fn validate_relative_tree_path(path: &Path, empty_allowed: bool) -> std::io::Result<()> {
    if (!empty_allowed && path.as_os_str().is_empty())
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "tree path `{}` is not one canonical relative path",
                path.display()
            ),
        ))
    } else {
        Ok(())
    }
}

fn tree_path_is_excluded(path: &Path, excluded: &[PathBuf]) -> bool {
    excluded
        .iter()
        .any(|prefix| path == prefix || path.starts_with(prefix))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_tree_from<Fd: std::os::fd::AsFd>(
    directory: Fd,
    relative_parent: &Path,
    excluded: &[PathBuf],
    snapshot: &mut TreeSnapshot,
) -> std::io::Result<()> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    for name in directory_entry_names(directory.as_fd())? {
        let relative_path = relative_parent.join(OsString::from_vec(name.to_bytes().to_vec()));
        if tree_path_is_excluded(&relative_path, excluded) {
            continue;
        }
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
                let opened = rustix::fs::fstat(&child).map_err(io_error)?;
                let identity = DirectoryIdentity {
                    device: opened.st_dev as u64,
                    inode: opened.st_ino as u64,
                };
                snapshot.directories.push(TreeDirectory {
                    relative_path: relative_path.clone(),
                    mode: opened.st_mode as u32 & 0o777,
                });
                read_tree_from(&child, &relative_path, excluded, snapshot)?;
                let named = rustix::fs::statat(
                    directory.as_fd(),
                    &name,
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                )
                .map_err(io_error)?;
                if named.st_dev as u64 != identity.device
                    || named.st_ino as u64 != identity.inode
                    || rustix::fs::FileType::from_raw_mode(named.st_mode)
                        != rustix::fs::FileType::Directory
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "directory entry changed identity during exact tree read",
                    ));
                }
            }
            Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
                let descriptor = rustix::fs::openat(
                    directory.as_fd(),
                    &name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::CLOEXEC
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::NONBLOCK,
                    rustix::fs::Mode::empty(),
                )
                .map_err(io_error)?;
                let opened = rustix::fs::fstat(&descriptor).map_err(io_error)?;
                let named = rustix::fs::statat(
                    directory.as_fd(),
                    &name,
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                )
                .map_err(io_error)?;
                if rustix::fs::FileType::from_raw_mode(opened.st_mode)
                    != rustix::fs::FileType::RegularFile
                    || opened.st_nlink != 1
                    || opened.st_dev != named.st_dev
                    || opened.st_ino != named.st_ino
                    || rustix::fs::FileType::from_raw_mode(named.st_mode)
                        != rustix::fs::FileType::RegularFile
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "tree entry is not one stable unaliased regular file",
                    ));
                }
                let mut file = File::from(descriptor);
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                let named_after = rustix::fs::statat(
                    directory.as_fd(),
                    &name,
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                )
                .map_err(io_error)?;
                if opened.st_dev != named_after.st_dev
                    || opened.st_ino != named_after.st_ino
                    || named_after.st_nlink != 1
                    || rustix::fs::FileType::from_raw_mode(named_after.st_mode)
                        != rustix::fs::FileType::RegularFile
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "regular-file entry changed identity during exact tree read",
                    ));
                }
                snapshot.files.push(TreeFile {
                    relative_path,
                    bytes,
                    mode: opened.st_mode as u32 & 0o777,
                });
            }
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(())
}

/// Capture one exact directory/file tree while mechanically omitting canonical
/// relative prefixes selected by the caller.
///
/// `expected_root` binds the initial dirfd to provider-owned physical evidence;
/// an absent sandbox root is an error. An absent requested subtree is empty.
/// Exclusions are relative to that requested subtree. Substituted roots,
/// symlinks, special files, permission faults, and races are surfaced.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn read_tree_nofollow(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative_tree: &Path,
) -> std::io::Result<TreeSnapshot> {
    read_tree_nofollow_excluding(root, expected_root, relative_tree, &[])
}

/// The exclusion-capable form of [`read_tree_nofollow`]. Exclusion is a
/// structural walker option only; providers remain the authority that decides
/// which independently governed paths must be omitted.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn read_tree_nofollow_excluding(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative_tree: &Path,
    excluded: &[PathBuf],
) -> std::io::Result<TreeSnapshot> {
    validate_relative_tree_path(relative_tree, true)?;
    for path in excluded {
        validate_relative_tree_path(path, false)?;
    }
    let mut directory = open_exact_root(root, expected_root)?;

    for component in relative_tree.components() {
        let std::path::Component::Normal(component) = component else {
            unreachable!("relative tree path was validated")
        };
        directory = match rustix::fs::openat(
            &directory,
            component,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        ) {
            Ok(directory) => directory,
            Err(rustix::io::Errno::NOENT) => return Ok(TreeSnapshot::default()),
            Err(error) => return Err(io_error(error)),
        };
    }

    let mut snapshot = TreeSnapshot::default();
    read_tree_from(&directory, Path::new(""), excluded, &mut snapshot)?;
    snapshot
        .directories
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    snapshot
        .files
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(snapshot)
}

/// Capture every regular file below one relative subtree without following a
/// symlink at the sandbox root, subtree path, or recursive entry.
///
/// A missing sandbox root or requested subtree is explicitly empty; every
/// other failure is propagated. This convenience projection is the single
/// Artifacts/Skills/Memory reader over the same exact walker as checkpointing.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn read_regular_tree_nofollow(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative_tree: &Path,
) -> std::io::Result<Vec<TreeFile>> {
    if classify_nofollow(root)? == PathEntry::Absent {
        Ok(Vec::new())
    } else {
        read_tree_nofollow(root, expected_root, relative_tree).map(|snapshot| snapshot.files)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn read_tree_nofollow(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative_tree: &Path,
) -> std::io::Result<TreeSnapshot> {
    read_tree_nofollow_excluding(root, expected_root, relative_tree, &[])
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn read_tree_nofollow_excluding(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative_tree: &Path,
    excluded: &[PathBuf],
) -> std::io::Result<TreeSnapshot> {
    validate_relative_tree_path(relative_tree, true)?;
    for path in excluded {
        validate_relative_tree_path(path, false)?;
    }
    if directory_identity_nofollow(root)? != expected_root {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sandbox root identity changed",
        ));
    }
    let base = root.join(relative_tree);
    if !base.exists() {
        return Ok(TreeSnapshot::default());
    }
    let metadata = std::fs::symlink_metadata(&base)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "tree root is not a directory",
        ));
    }
    fn walk(
        base: &Path,
        relative: &Path,
        excluded: &[PathBuf],
        snapshot: &mut TreeSnapshot,
    ) -> std::io::Result<()> {
        let current = base.join(relative);
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;
            let child_relative = relative.join(entry.file_name());
            if tree_path_is_excluded(&child_relative, excluded) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "tree contains a symlink",
                ));
            }
            if metadata.is_dir() {
                snapshot.directories.push(TreeDirectory {
                    relative_path: child_relative.clone(),
                    mode: 0o700,
                });
                walk(base, &child_relative, excluded, snapshot)?;
            } else if metadata.is_file() {
                snapshot.files.push(TreeFile {
                    relative_path: child_relative,
                    bytes: std::fs::read(entry.path())?,
                    mode: 0o600,
                });
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "tree contains a special entry",
                ));
            }
        }
        Ok(())
    }
    let mut snapshot = TreeSnapshot::default();
    walk(&base, Path::new(""), excluded, &mut snapshot)?;
    snapshot
        .directories
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    snapshot
        .files
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(snapshot)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn read_regular_tree_nofollow(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative_tree: &Path,
) -> std::io::Result<Vec<TreeFile>> {
    read_tree_nofollow(root, expected_root, relative_tree).map(|snapshot| snapshot.files)
}

/// Remove every descendant of one exact directory while retaining that same
/// directory inode.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn clear_directory_contents_exact(
    path: &Path,
    expected: DirectoryIdentity,
) -> std::io::Result<()> {
    let directory = rustix::fs::open(
        path,
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
            format!("directory `{}` has a foreign identity", path.display()),
        ));
    }
    remove_directory_contents(&directory)?;
    if directory_identity_nofollow(path)? != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "directory `{}` changed identity during exact cleanup",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn clear_directory_contents_exact(
    path: &Path,
    expected: DirectoryIdentity,
) -> std::io::Result<()> {
    if directory_identity_nofollow(path)? != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory identity changed",
        ));
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Recursively remove exactly one directory inode without following directory
/// symlinks at any walk step.
///
/// The expected identity must come from a provider-owned durable or live guard.
/// A missing path, substituted file/symlink/directory, mount that cannot be
/// removed, or entry that changes during traversal fails without deleting the
/// substituted final name.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn remove_directory_tree_exact(
    path: &Path,
    expected: DirectoryIdentity,
) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("directory `{}` has no parent", path.display()),
        )
    })?;
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("directory `{}` has no file name", path.display()),
        )
    })?;
    let parent = rustix::fs::open(
        parent,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(io_error)?;
    let directory = rustix::fs::openat(
        &parent,
        name,
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
            format!("directory `{}` has a foreign identity", path.display()),
        ));
    }
    remove_directory_contents(&directory)?;
    let named = rustix::fs::statat(&parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io_error)?;
    if named.st_dev as u64 != expected.device || named.st_ino as u64 != expected.inode {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "directory `{}` changed identity during exact cleanup",
                path.display()
            ),
        ));
    }
    rustix::fs::unlinkat(&parent, name, rustix::fs::AtFlags::REMOVEDIR).map_err(io_error)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn remove_directory_tree_exact(
    path: &Path,
    expected: DirectoryIdentity,
) -> std::io::Result<()> {
    if directory_identity_nofollow(path)? != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory identity changed",
        ));
    }
    std::fs::remove_dir_all(path)
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[path = "lib_tests.rs"]
mod tests;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
#[cfg(all(test, windows))]
mod windows_tests;
