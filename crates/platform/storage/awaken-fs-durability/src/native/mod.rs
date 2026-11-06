// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Static owner: shared policy-free native filesystem primitives. Dynamic path: callers retain
// @formal reason: resource and realization authority while this crate performs stable-inode locking,
// @formal reason: descriptor-relative no-follow reads, writes, shredding, exact removal, and atomic
// @formal reason: no-replace publication; external filesystem durability remains the declared Resource
// @formal reason: boundary and this is not a direct formal-proof claim.
//! Policy-free native filesystem primitives shared by stores and providers.
//!
//! Repository identity, sandbox realization ownership, retry, and cleanup
//! remain in their provider adapters. This crate owns only the filesystem
//! primitives shared at those boundaries: exclusive locking of one stable
//! sidecar inode and no-replace publication of one directory.

use std::fs::File;
use std::io::Read as _;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::atomic::Ordering;

static PRIVATE_STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

mod portable_name;
pub use portable_name::{is_portable_component, is_reserved_component};
mod projection_root;
pub use projection_root::ProjectionRoot;
#[cfg(target_os = "linux")]
mod unix_socket_path;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
pub use unix_socket_path::PinnedUnixSocketPath;
mod tree_limits;
use tree_limits::ReadBudget;
pub use tree_limits::TreeReadLimits;

/// Immutable identity of one opened physical directory.
///
/// This is policy-free physical evidence. Provider adapters decide whether an
/// identity is authorized; this crate only compares it at no-follow filesystem
/// edges. Windows v1 uses local fixed NTFS volume/file IDs; other Windows
/// filesystems are rejected before their identifiers can enter this shape.
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

/// One regular file captured by a tree reader.
///
/// `relative_path` is relative to the requested tree root, never to a host
/// pathname. Descriptor identity guarantees belong to the reader used:
/// exact-dirfd APIs check the opened file; the bounded convenience API documents
/// its platform-specific quiescence requirement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeFile {
    pub relative_path: PathBuf,
    pub bytes: Vec<u8>,
    pub mode: u32,
}

/// One directory captured by the same tree walk as [`TreeFile`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeDirectory {
    pub relative_path: PathBuf,
    pub mode: u32,
}

/// One complete tree snapshot. Entries are sorted by their
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
    #[cfg(windows)]
    _ancestor_handles: Vec<File>,
    _file: File,
}

// All held-parent effects stay in this private component family. The same
// physical guard type remains defined above for Windows/fallback and tests.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod locked_parent;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use locked_parent::try_lock_exclusive;

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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[path = "locked_parent_fallback.rs"]
mod locked_parent_fallback;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub use locked_parent_fallback::try_lock_exclusive;
/// Inspect the final path component without following a symlink.
#[cfg(windows)]
pub fn classify_nofollow(path: &Path) -> std::io::Result<PathEntry> {
    windows::classify_nofollow(path)
}

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
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "nofollow directory identity is unsupported on this platform",
            ));
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

#[cfg(windows)]
pub use windows::directory_identity_nofollow;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn directory_identity_nofollow(path: &Path) -> std::io::Result<DirectoryIdentity> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "nofollow directory identity is unsupported for `{}` on this platform",
            path.display()
        ),
    ))
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

#[cfg(windows)]
pub fn remove_regular_file_nofollow(path: &Path) -> std::io::Result<()> {
    windows::remove_regular_file_nofollow(path)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn remove_regular_file_nofollow(path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "nofollow regular-file removal is unsupported for `{}` on this platform",
            path.display()
        ),
    ))
}

#[cfg(windows)]
pub fn read_regular_file_nofollow(path: &Path) -> std::io::Result<Vec<u8>> {
    windows::read_regular_file_nofollow(path)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn read_regular_file_nofollow(path: &Path) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "nofollow regular-file reads are unsupported for `{}` on this platform",
            path.display()
        ),
    ))
}

mod publication;
pub use publication::{
    create_directory_noreplace, create_private_directory_stage, publish_directory_noreplace,
    publish_file_noreplace, replace_regular_file_atomic,
};

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

#[cfg(windows)]
pub fn zero_relative_regular_file_nofollow(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
) -> std::io::Result<()> {
    windows::zero_relative_regular_file_nofollow(root, expected_root, relative)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn zero_relative_regular_file_nofollow(
    root: &Path,
    _expected_root: DirectoryIdentity,
    _relative: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "descriptor-relative secret shredding is unsupported for `{}` on this platform",
            root.display()
        ),
    ))
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

#[cfg(windows)]
pub fn create_relative_directory_all(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
) -> std::io::Result<()> {
    windows::create_relative_directory_all(root, expected_root, relative)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn create_relative_directory_all(
    root: &Path,
    _expected_root: DirectoryIdentity,
    _relative: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "descriptor-relative directory creation is unsupported for `{}` on this platform",
            root.display()
        ),
    ))
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

#[cfg(windows)]
pub fn set_relative_directory_mode(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
    mode: u32,
) -> std::io::Result<()> {
    windows::set_relative_directory_mode(root, expected_root, relative, mode)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn set_relative_directory_mode(
    root: &Path,
    _expected_root: DirectoryIdentity,
    _relative: &Path,
    _mode: u32,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "descriptor-relative directory metadata is unsupported for `{}` on this platform",
            root.display()
        ),
    ))
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
#[allow(
    clippy::useless_conversion,
    reason = "Linux mode_t is u32; macOS has a narrower mode_t and requires checked conversion"
)]
fn portable_mode(mode: u32) -> std::io::Result<rustix::fs::Mode> {
    if mode & !0o7777 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("file mode {mode:#o} contains non-permission bits"),
        ));
    }
    // `RawMode` is u32 on Linux and u16 on macOS. The explicit 12-bit
    // permission bound above makes this conversion lossless on both targets.
    Ok(rustix::fs::Mode::from_raw_mode(mode as rustix::fs::RawMode))
}

#[cfg(windows)]
pub fn write_relative_file_atomic(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
    contents: &[u8],
    mode: u32,
) -> std::io::Result<()> {
    windows::write_relative_file_atomic(root, expected_root, relative, contents, mode)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn write_relative_file_atomic(
    root: &Path,
    _expected_root: DirectoryIdentity,
    _relative: &Path,
    _contents: &[u8],
    _mode: u32,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "descriptor-relative materialization is unsupported for `{}` on this platform",
            root.display()
        ),
    ))
}

#[path = "directory_tree.rs"]
mod directory_tree;
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
    budget: &mut ReadBudget,
) -> std::io::Result<()> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    for name in directory_tree::directory_entry_names_bounded(directory.as_fd(), budget.entries)? {
        let relative_path = relative_parent.join(OsString::from_vec(name.to_bytes().to_vec()));
        if tree_path_is_excluded(&relative_path, excluded) {
            continue;
        }
        budget.entry(&relative_path)?;
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
                read_tree_from(&child, &relative_path, excluded, snapshot, budget)?;
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
                let bytes = read_tree_file_bounded(
                    File::from(descriptor),
                    budget.limits.file_bytes,
                    &mut budget.bytes,
                )?;
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
    read_tree_nofollow_limited(
        root,
        expected_root,
        relative_tree,
        excluded,
        TreeReadLimits {
            file_bytes: u64::MAX,
            total_bytes: u64::MAX,
            entries: usize::MAX,
            depth: usize::MAX,
        },
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_tree_nofollow_limited(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative_tree: &Path,
    excluded: &[PathBuf],
    limits: TreeReadLimits,
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
    read_tree_from(
        &directory,
        Path::new(""),
        excluded,
        &mut snapshot,
        &mut ReadBudget::new(limits),
    )?;
    snapshot
        .directories
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    snapshot
        .files
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(snapshot)
}

/// Read a required plain tree with caller-owned per-file and total byte limits. Missing
/// roots and incomplete reads fail; callers never receive a partial snapshot.
/// Linux/macOS and Windows reuse exact handle-bound walkers, capturing the root
/// identity at entry. Other platforms use the quiescent-path adapter below,
/// which rejects static links but does not claim handle identity fencing.
/// This convenience API grants neither publication nor deletion authority.
pub fn read_tree_nofollow_bounded(
    root: &Path,
    max_file_bytes: u64,
    max_total_bytes: u64,
) -> std::io::Result<TreeSnapshot> {
    read_tree_nofollow_with_limits(
        root,
        TreeReadLimits {
            file_bytes: max_file_bytes,
            total_bytes: max_total_bytes,
            entries: usize::MAX,
            depth: usize::MAX,
        },
    )
}

/// Required-tree reader with caller-owned metadata and content budgets.
pub fn read_tree_nofollow_with_limits(
    root: &Path,
    limits: TreeReadLimits,
) -> std::io::Result<TreeSnapshot> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        read_tree_nofollow_limited(
            root,
            directory_identity_nofollow(root)?,
            Path::new(""),
            &[],
            limits,
        )
    }
    #[cfg(windows)]
    {
        windows::read_tree_nofollow_limited(
            root,
            directory_identity_nofollow(root)?,
            Path::new(""),
            &[],
            limits,
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        directory_tree::read_plain_tree_bounded(root, limits)
    }
}

fn read_tree_file_bounded(
    file: impl std::io::Read,
    max_file_bytes: u64,
    remaining_bytes: &mut u64,
) -> std::io::Result<Vec<u8>> {
    let limit = max_file_bytes.min(*remaining_bytes);
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "tree exceeds the caller's per-file or total byte limit",
        ));
    }
    *remaining_bytes -= bytes.len() as u64;
    Ok(bytes)
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

#[cfg(windows)]
pub fn read_tree_nofollow(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative_tree: &Path,
) -> std::io::Result<TreeSnapshot> {
    read_tree_nofollow_excluding(root, expected_root, relative_tree, &[])
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn read_tree_nofollow(
    root: &Path,
    _expected_root: DirectoryIdentity,
    _relative_tree: &Path,
) -> std::io::Result<TreeSnapshot> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "descriptor-relative nofollow tree reads are unsupported for `{}` on this platform",
            root.display()
        ),
    ))
}

#[cfg(windows)]
pub fn read_tree_nofollow_excluding(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative_tree: &Path,
    excluded: &[PathBuf],
) -> std::io::Result<TreeSnapshot> {
    windows::read_tree_nofollow_limited(
        root,
        expected_root,
        relative_tree,
        excluded,
        TreeReadLimits {
            file_bytes: u64::MAX,
            total_bytes: u64::MAX,
            entries: usize::MAX,
            depth: usize::MAX,
        },
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn read_tree_nofollow_excluding(
    root: &Path,
    _expected_root: DirectoryIdentity,
    _relative_tree: &Path,
    _excluded: &[PathBuf],
) -> std::io::Result<TreeSnapshot> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "descriptor-relative nofollow tree reads are unsupported for `{}` on this platform",
            root.display()
        ),
    ))
}

#[cfg(windows)]
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
pub fn read_regular_tree_nofollow(
    root: &Path,
    _expected_root: DirectoryIdentity,
    _relative_tree: &Path,
) -> std::io::Result<Vec<TreeFile>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "descriptor-relative nofollow tree reads are unsupported for `{}` on this platform",
            root.display()
        ),
    ))
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

#[cfg(windows)]
pub fn clear_directory_contents_exact(
    path: &Path,
    expected: DirectoryIdentity,
) -> std::io::Result<()> {
    windows::clear_directory_contents_exact(path, expected)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn clear_directory_contents_exact(
    path: &Path,
    _expected: DirectoryIdentity,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "descriptor-relative exact cleanup is unsupported for `{}` on this platform",
            path.display()
        ),
    ))
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

#[cfg(windows)]
pub fn remove_directory_tree_exact(
    path: &Path,
    expected: DirectoryIdentity,
) -> std::io::Result<()> {
    windows::remove_directory_tree_exact(path, expected)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn remove_directory_tree_exact(
    path: &Path,
    _expected: DirectoryIdentity,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "descriptor-relative exact cleanup is unsupported for `{}` on this platform",
            path.display()
        ),
    ))
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
