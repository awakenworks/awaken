// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Policy-free Windows NTFS handles pin the local directory chain and
// @formal reason: implement the existing Sandbox lock, no-follow, publication, read,
// @formal reason: shred, and exact-delete contracts. Providers retain authorization;
// @formal reason: executable tests cover effects but this is not a direct proof claim.
//! Windows NTFS physical identity and the existing sidecar lock boundary.
//! Every directory component is opened relative to a retained handle without
//! following reparse points. The 64-bit persisted inode format is qualified
//! only on NTFS; ReFS and remote filesystems require a different identity
//! format and fail closed here.

use super::{
    DirectoryIdentity, ExclusiveFileLock, PathEntry, ReadBudget, TreeDirectory, TreeFile,
    TreeReadLimits, TreeSnapshot,
};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::ops::Deref;
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Component, Path, PathBuf, Prefix};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_ATTRIBUTES,
};

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn leaf(path: &Path) -> io::Result<&OsStr> {
    let leaf = super::validate_sibling_leaf(path)?;
    if leaf.to_str().is_some_and(super::is_portable_component) {
        Ok(leaf)
    } else {
        Err(invalid("sandbox path is not one portable Windows leaf"))
    }
}

fn file_information(file: &File) -> io::Result<awaken_fs_win32::NtfsFileInfo> {
    awaken_fs_win32::ntfs_file_information(file)
}

fn identity(file: &File) -> io::Result<DirectoryIdentity> {
    let info = file_information(file)?;
    Ok(DirectoryIdentity {
        device: info.volume_serial,
        inode: info.file_index,
    })
}

fn require_ntfs(file: &File) -> io::Result<()> {
    file_information(file).map(|_| ())
}

fn require_plain_directory(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        Ok(())
    } else {
        Err(invalid(
            "sandbox directory is not one plain no-follow directory",
        ))
    }
}

fn open_drive_root(root: &Path) -> io::Result<File> {
    awaken_fs_win32::require_local_fixed_drive(root)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let directory = options.open(root)?;
    require_plain_directory(&directory)?;
    require_ntfs(&directory)?;
    Ok(directory)
}

struct PinnedDirectory {
    file: File,
    ancestors: Vec<File>,
}

impl Deref for PinnedDirectory {
    type Target = File;

    fn deref(&self) -> &File {
        &self.file
    }
}

impl PinnedDirectory {
    fn descend(mut self, child: File) -> Self {
        self.ancestors.push(self.file);
        Self {
            file: child,
            ancestors: self.ancestors,
        }
    }
}

fn open_directory_nofollow(path: &Path) -> io::Result<PinnedDirectory> {
    let mut components = path.components();
    let prefix = match components.next() {
        Some(Component::Prefix(prefix)) => prefix,
        _ => {
            return Err(invalid(
                "sandbox directory must have an absolute local drive path",
            ));
        }
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(invalid(
            "sandbox directory must have an absolute local drive path",
        ));
    }
    let root = match prefix.kind() {
        Prefix::Disk(drive) => PathBuf::from(format!("{}:\\", char::from(drive))),
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("\\\\?\\{}:\\", char::from(drive))),
        _ => return Err(invalid("sandbox directory must be on a local NTFS drive")),
    };
    let mut directory = PinnedDirectory {
        file: open_drive_root(&root)?,
        ancestors: Vec::new(),
    };
    for component in components {
        let Component::Normal(name) = component else {
            return Err(invalid("sandbox directory has a noncanonical component"));
        };
        let child = open_child_directory(&directory, leaf(Path::new(name))?)?;
        directory = directory.descend(child);
        require_plain_directory(&directory)?;
    }
    Ok(directory)
}

fn open_child_directory(parent: &File, name: &OsStr) -> io::Result<File> {
    open_child_directory_with(
        parent,
        name,
        GENERIC_READ,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )
}

fn open_child_directory_with(
    parent: &File,
    name: &OsStr,
    access: u32,
    share: u32,
) -> io::Result<File> {
    use cap_primitives::fs::{FollowSymlinks, OpenOptions as CapOpenOptions, OpenOptionsExt as _};

    let mut options = CapOpenOptions::new();
    options.read(true).access_mode(access).share_mode(share);
    options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    options._cap_fs_ext_follow(FollowSymlinks::No);
    let child = cap_primitives::fs::open(parent, Path::new(name), &options)?;
    require_plain_directory(&child)?;
    require_ntfs(&child)?;
    Ok(child)
}

fn open_regular(parent: &File, name: &OsStr) -> io::Result<File> {
    use cap_primitives::fs::{FollowSymlinks, OpenOptions as CapOpenOptions, OpenOptionsExt as _};

    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    options._cap_fs_ext_follow(FollowSymlinks::No);
    let file = cap_primitives::fs::open(parent, Path::new(name), &options)?;
    let metadata = file.metadata()?;
    let info = file_information(&file)?;
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || info.number_of_links != 1
    {
        return Err(invalid("sandbox file is not one unaliased regular file"));
    }
    Ok(file)
}

pub fn directory_identity_nofollow(path: &Path) -> io::Result<DirectoryIdentity> {
    let directory = open_directory_nofollow(path)?;
    identity(&directory)
}

pub(super) fn classify_nofollow(path: &Path) -> io::Result<PathEntry> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("sandbox path has no parent"))?;
    let _pinned_parent = match open_directory_nofollow(parent) {
        Ok(parent) => parent,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(PathEntry::Absent),
        Err(error) => return Err(error),
    };
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(PathEntry::Absent),
        Err(error) => return Err(error),
    };
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(PathEntry::Symlink);
    }
    if metadata.is_dir() {
        return directory_identity_nofollow(path).map(PathEntry::Directory);
    }
    Ok(if metadata.is_file() {
        PathEntry::RegularFile
    } else {
        PathEntry::Other
    })
}

pub(super) fn try_lock_exclusive(path: &Path) -> io::Result<ExclusiveFileLock> {
    use cap_primitives::fs::{FollowSymlinks, OpenOptions as CapOpenOptions, OpenOptionsExt as _};

    let parent_path = path
        .parent()
        .ok_or_else(|| invalid("sandbox lock has no parent"))?;
    let leaf = leaf(Path::new(
        path.file_name()
            .ok_or_else(|| invalid("sandbox lock has no leaf"))?,
    ))?;
    let PinnedDirectory {
        file: parent,
        ancestors,
    } = open_directory_nofollow(parent_path)?;
    let parent_identity = identity(&parent)?;
    let mut options = CapOpenOptions::new();
    options.read(true).write(true).create(true);
    options._cap_fs_ext_follow(FollowSymlinks::No);
    options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    let file = cap_primitives::fs::open(&parent, Path::new(leaf), &options)?;
    let metadata = file.metadata()?;
    let info = file_information(&file)?;
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || info.number_of_links != 1
    {
        return Err(invalid("sandbox lock is not one unaliased regular file"));
    }
    file.try_lock()?;
    let lock = ExclusiveFileLock {
        owner_pid: std::process::id(),
        parent_path: parent_path.to_owned(),
        parent_identity,
        parent,
        _ancestor_handles: ancestors,
        _file: file,
    };
    validate_parent_path(&lock)?;
    Ok(lock)
}

pub(super) fn classify_sibling(lock: &ExclusiveFileLock, path: &Path) -> io::Result<PathEntry> {
    let name = leaf(path)?;
    validate_parent_path(lock)?;
    // The parent and every ancestor deny DELETE sharing while the guard is
    // alive. A final component may still be replaced by another process, so
    // classification is observational and grants no later mutation authority.
    let path = lock.parent_path.join(name);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(PathEntry::Absent),
        Err(error) => return Err(error),
    };
    let entry = if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        PathEntry::Symlink
    } else if metadata.is_dir() {
        PathEntry::Directory(identity(&open_child_directory(&lock.parent, name)?)?)
    } else if metadata.is_file() {
        PathEntry::RegularFile
    } else {
        PathEntry::Other
    };
    validate_parent_path(lock)?;
    Ok(entry)
}

pub(super) fn sibling_names(lock: &ExclusiveFileLock) -> io::Result<Vec<std::ffi::OsString>> {
    validate_parent_path(lock)?;
    let names = directory_names(&lock.parent)?;
    validate_parent_path(lock)?;
    Ok(names)
}

fn directory_names(parent: &File) -> io::Result<Vec<OsString>> {
    let directory = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    let mut names = directory
        .entries()?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

fn windows_mode(file: &File, directory: bool) -> io::Result<u32> {
    let readonly = file.metadata()?.permissions().readonly();
    Ok(match (directory, readonly) {
        (true, true) => 0o500,
        (true, false) => 0o700,
        (false, true) => 0o400,
        (false, false) => 0o600,
    })
}

fn read_tree_from(
    directory: &File,
    relative_parent: &Path,
    excluded: &[PathBuf],
    snapshot: &mut TreeSnapshot,
    budget: &mut ReadBudget,
) -> io::Result<()> {
    for raw_name in directory_names(directory)? {
        let name = leaf(Path::new(&raw_name))?;
        let relative = relative_parent.join(name);
        if super::tree_path_is_excluded(&relative, excluded) {
            continue;
        }
        budget.entry(&relative)?;
        match open_child_directory(directory, name) {
            Ok(child) => {
                snapshot.directories.push(TreeDirectory {
                    relative_path: relative.clone(),
                    mode: windows_mode(&child, true)?,
                });
                read_tree_from(&child, &relative, excluded, snapshot, budget)?;
            }
            Err(_) => {
                let mut file = open_regular(directory, name)?;
                let opened = file_information(&file)?;
                let bytes = super::read_tree_file_bounded(
                    &mut file,
                    budget.limits.file_bytes,
                    &mut budget.bytes,
                )?;
                if file_information(&file)? != opened {
                    return Err(invalid("Windows tree file changed identity during read"));
                }
                snapshot.files.push(TreeFile {
                    relative_path: relative,
                    bytes,
                    mode: windows_mode(&file, false)?,
                });
            }
        }
    }
    Ok(())
}

pub(super) fn read_tree_nofollow_limited(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative_tree: &Path,
    excluded: &[PathBuf],
    limits: TreeReadLimits,
) -> io::Result<TreeSnapshot> {
    super::validate_relative_tree_path(relative_tree, true)?;
    for path in excluded {
        super::validate_relative_tree_path(path, false)?;
    }
    let mut directory = open_exact_directory(root, expected_root)?;
    for component in relative_tree.components() {
        let Component::Normal(name) = component else {
            unreachable!("validated relative tree component")
        };
        directory = match open_child_directory(&directory, leaf(Path::new(name))?) {
            Ok(child) => directory.descend(child),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(TreeSnapshot::default());
            }
            Err(error) => return Err(error),
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
    if directory_identity_nofollow(root)? != expected_root {
        return Err(invalid("Windows tree root changed identity during read"));
    }
    Ok(snapshot)
}

fn read_file_in(parent: &File, name: &OsStr) -> io::Result<Vec<u8>> {
    use std::io::Read as _;

    let mut file = open_regular(parent, name)?;
    let opened = file_information(&file)?;
    let named = open_regular(parent, name)?;
    if file_information(&named)? != opened {
        return Err(invalid("sandbox file changed identity while being opened"));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if file_information(&open_regular(parent, name)?)? != opened {
        return Err(invalid("sandbox file changed identity during read"));
    }
    Ok(bytes)
}

pub(super) fn read_sibling_regular_file(
    lock: &ExclusiveFileLock,
    path: &Path,
) -> io::Result<Vec<u8>> {
    let name = leaf(path)?;
    validate_parent_path(lock)?;
    let bytes = read_file_in(&lock.parent, name)?;
    validate_parent_path(lock)?;
    Ok(bytes)
}

pub(super) fn read_regular_file_nofollow(path: &Path) -> io::Result<Vec<u8>> {
    let parent_path = path
        .parent()
        .ok_or_else(|| invalid("Windows file has no parent"))?;
    let name = leaf(Path::new(
        path.file_name()
            .ok_or_else(|| invalid("Windows file has no leaf"))?,
    ))?;
    let parent = open_directory_nofollow(parent_path)?;
    let expected = identity(&parent)?;
    let bytes = read_file_in(&parent, name)?;
    if directory_identity_nofollow(parent_path)? != expected {
        return Err(invalid("Windows read parent changed identity"));
    }
    Ok(bytes)
}

fn open_regular_for_delete(parent: &File, name: &OsStr) -> io::Result<File> {
    use cap_primitives::fs::{FollowSymlinks, OpenOptions as CapOpenOptions, OpenOptionsExt as _};

    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES | DELETE);
    options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options._cap_fs_ext_follow(FollowSymlinks::No);
    let file = cap_primitives::fs::open(parent, Path::new(name), &options)?;
    let metadata = file.metadata()?;
    let info = file_information(&file)?;
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || info.number_of_links != 1
    {
        return Err(invalid(
            "Windows removal target is not one unaliased regular file",
        ));
    }
    Ok(file)
}

fn remove_regular_file_in(parent_path: &Path, parent: &File, name: &OsStr) -> io::Result<()> {
    let file = open_regular_for_delete(parent, name)?;
    awaken_fs_win32::delete_open(&file)?;
    drop(file);
    crate::sync_directory(parent_path)
}

pub(super) fn remove_sibling_regular_file_nofollow(
    lock: &ExclusiveFileLock,
    path: &Path,
) -> io::Result<()> {
    let name = leaf(path)?;
    validate_parent_path(lock)?;
    remove_regular_file_in(&lock.parent_path, &lock.parent, name)?;
    validate_parent_path(lock)
}

pub(super) fn remove_regular_file_nofollow(path: &Path) -> io::Result<()> {
    let parent_path = path
        .parent()
        .ok_or_else(|| invalid("Windows file has no parent"))?;
    let name = leaf(Path::new(
        path.file_name()
            .ok_or_else(|| invalid("Windows file has no leaf"))?,
    ))?;
    let parent = open_directory_nofollow(parent_path)?;
    let expected = identity(&parent)?;
    remove_regular_file_in(parent_path, &parent, name)?;
    if directory_identity_nofollow(parent_path)? != expected {
        return Err(invalid("Windows removal parent changed identity"));
    }
    Ok(())
}

fn create_sibling_file_stage(parent: &File, destination: &OsStr) -> io::Result<(OsString, File)> {
    use cap_primitives::fs::{FollowSymlinks, OpenOptions as CapOpenOptions, OpenOptionsExt as _};

    for _ in 0..32 {
        let sequence =
            super::PRIVATE_STAGE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = OsString::from(format!(
            ".{}.awaken-file-stage-{}-{sequence}",
            destination.to_string_lossy(),
            std::process::id()
        ));
        let mut options = CapOpenOptions::new();
        options.write(true).create_new(true);
        options.access_mode(GENERIC_WRITE | DELETE);
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        options._cap_fs_ext_follow(FollowSymlinks::No);
        match cap_primitives::fs::open(parent, Path::new(&name), &options) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate Windows sibling stage",
    ))
}

fn publish_file_in(
    parent_path: &Path,
    parent: &File,
    destination: &OsStr,
    contents: &[u8],
    mode: Option<u32>,
) -> io::Result<()> {
    let (stage_name, mut stage) = create_sibling_file_stage(parent, destination)?;
    let stage_path = parent_path.join(&stage_name);
    let result = (|| {
        stage.write_all(contents)?;
        if let Some(mode) = mode {
            apply_windows_mode(&stage, mode)?;
        }
        stage.sync_all()?;
        awaken_fs_win32::rename_open_noreplace(&stage, &parent_path.join(destination))?;
        crate::sync_directory(parent_path)
    })();
    if result.is_err() {
        drop(stage);
        let _ = std::fs::remove_file(stage_path);
    }
    result
}

fn transform_file_in<F>(
    parent_path: &Path,
    parent: &File,
    destination: &OsStr,
    mode: Option<u32>,
    transform: F,
) -> io::Result<()>
where
    F: FnOnce(&mut File, &mut File) -> io::Result<()>,
{
    let mut source = open_regular(parent, destination)?;
    let original = file_information(&source)?;
    let (stage_name, mut stage) = create_sibling_file_stage(parent, destination)?;
    let stage_path = parent_path.join(&stage_name);
    let result = (|| {
        transform(&mut source, &mut stage)?;
        if let Some(mode) = mode {
            apply_windows_mode(&stage, mode)?;
        }
        stage.sync_all()?;
        if file_information(&open_regular(parent, destination)?)? != original {
            return Err(invalid("Windows replacement file changed identity"));
        }
        let stage_identity = file_information(&stage)?;
        drop(source);
        awaken_fs_win32::rename_open_replace(&stage, &parent_path.join(destination))?;
        drop(stage);
        crate::sync_directory(parent_path)?;
        if file_information(&open_regular(parent, destination)?)? != stage_identity {
            return Err(invalid("Windows replacement publication changed identity"));
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(stage_path);
    }
    result
}

pub(super) fn transform_sibling_regular_file_atomic<F>(
    lock: &ExclusiveFileLock,
    destination: &Path,
    transform: F,
) -> io::Result<()>
where
    F: FnOnce(&mut File, &mut File) -> io::Result<()>,
{
    let destination = leaf(destination)?;
    validate_parent_path(lock)?;
    transform_file_in(
        &lock.parent_path,
        &lock.parent,
        destination,
        None,
        transform,
    )?;
    validate_parent_path(lock)
}

pub(super) fn replace_regular_file_atomic(destination: &Path, contents: &[u8]) -> io::Result<()> {
    let parent_path = destination
        .parent()
        .ok_or_else(|| invalid("Windows file has no parent"))?;
    let name = leaf(Path::new(
        destination
            .file_name()
            .ok_or_else(|| invalid("Windows file has no leaf"))?,
    ))?;
    let parent = open_directory_nofollow(parent_path)?;
    let expected = identity(&parent)?;
    transform_file_in(parent_path, &parent, name, None, |_, stage| {
        stage.write_all(contents)
    })?;
    if directory_identity_nofollow(parent_path)? != expected {
        return Err(invalid("Windows replacement parent changed identity"));
    }
    Ok(())
}

pub(super) fn publish_sibling_file_noreplace(
    lock: &ExclusiveFileLock,
    destination: &Path,
    contents: &[u8],
) -> io::Result<()> {
    let destination = leaf(destination)?;
    validate_parent_path(lock)?;
    publish_file_in(&lock.parent_path, &lock.parent, destination, contents, None)?;
    validate_parent_path(lock)
}

pub(super) fn publish_file_noreplace(destination: &Path, contents: &[u8]) -> io::Result<()> {
    let parent_path = destination
        .parent()
        .ok_or_else(|| invalid("Windows file has no parent"))?;
    let name = leaf(Path::new(
        destination
            .file_name()
            .ok_or_else(|| invalid("Windows file has no leaf"))?,
    ))?;
    let parent = open_directory_nofollow(parent_path)?;
    let expected = identity(&parent)?;
    publish_file_in(parent_path, &parent, name, contents, None)?;
    if directory_identity_nofollow(parent_path)? != expected {
        return Err(invalid("Windows publication parent changed identity"));
    }
    Ok(())
}

fn open_directory_for_rename(parent: &File, name: &OsStr) -> io::Result<File> {
    open_child_directory_with(
        parent,
        name,
        FILE_READ_ATTRIBUTES | DELETE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )
}

fn create_directory_in(
    parent_path: &Path,
    parent: &File,
    name: &OsStr,
) -> io::Result<DirectoryIdentity> {
    let directory_path = parent_path.join(name);
    std::fs::create_dir(&directory_path)?;
    let directory = open_child_directory(parent, name)?;
    let created = identity(&directory)?;
    if directory_identity_nofollow(&directory_path)? != created {
        return Err(invalid("created Windows directory changed identity"));
    }
    crate::sync_directory(parent_path)?;
    Ok(created)
}

pub(super) fn create_sibling_directory_noreplace(
    lock: &ExclusiveFileLock,
    path: &Path,
) -> io::Result<DirectoryIdentity> {
    let name = leaf(path)?;
    validate_parent_path(lock)?;
    let created = create_directory_in(&lock.parent_path, &lock.parent, name)?;
    validate_parent_path(lock)?;
    Ok(created)
}

pub(super) fn create_directory_noreplace(path: &Path) -> io::Result<DirectoryIdentity> {
    let parent_path = path
        .parent()
        .ok_or_else(|| invalid("Windows directory has no parent"))?;
    let name = leaf(Path::new(
        path.file_name()
            .ok_or_else(|| invalid("Windows directory has no leaf"))?,
    ))?;
    let parent = open_directory_nofollow(parent_path)?;
    let expected = identity(&parent)?;
    let created = create_directory_in(parent_path, &parent, name)?;
    if directory_identity_nofollow(parent_path)? != expected {
        return Err(invalid("Windows directory parent changed identity"));
    }
    Ok(created)
}

fn publish_directory_in(
    parent_path: &Path,
    parent: &File,
    stage_name: &OsStr,
    destination_name: &OsStr,
) -> io::Result<()> {
    let source = open_directory_for_rename(parent, stage_name)?;
    let source_identity = identity(&source)?;
    awaken_fs_win32::rename_open_noreplace(&source, &parent_path.join(destination_name))?;
    drop(source);
    if directory_identity_nofollow(&parent_path.join(destination_name))? != source_identity {
        return Err(invalid("published Windows directory changed identity"));
    }
    crate::sync_directory(parent_path)?;
    Ok(())
}

pub(super) fn publish_sibling_directory_noreplace(
    lock: &ExclusiveFileLock,
    stage: &Path,
    destination: &Path,
) -> io::Result<()> {
    let stage_name = leaf(stage)?;
    let destination_name = leaf(destination)?;
    validate_parent_path(lock)?;
    publish_directory_in(
        &lock.parent_path,
        &lock.parent,
        stage_name,
        destination_name,
    )?;
    validate_parent_path(lock)
}

pub(super) fn publish_directory_noreplace(stage: &Path, destination: &Path) -> io::Result<()> {
    if !stage.is_absolute() || stage.parent().is_none() || stage.parent() != destination.parent() {
        return Err(invalid(
            "Windows stage and destination must be absolute siblings",
        ));
    }
    let parent_path = stage.parent().expect("checked parent");
    let stage_name = leaf(Path::new(
        stage
            .file_name()
            .ok_or_else(|| invalid("Windows stage has no leaf"))?,
    ))?;
    let destination_name = leaf(Path::new(
        destination
            .file_name()
            .ok_or_else(|| invalid("Windows destination has no leaf"))?,
    ))?;
    let parent = open_directory_nofollow(parent_path)?;
    let expected = identity(&parent)?;
    publish_directory_in(parent_path, &parent, stage_name, destination_name)?;
    if directory_identity_nofollow(parent_path)? != expected {
        return Err(invalid("Windows directory parent changed identity"));
    }
    Ok(())
}

fn open_directory_for_delete(parent: &File, name: &OsStr) -> io::Result<File> {
    open_child_directory_with(
        parent,
        name,
        GENERIC_READ | DELETE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )
}

fn remove_non_directory_entry(parent: &File, name: &OsStr) -> io::Result<()> {
    use cap_primitives::fs::{FollowSymlinks, OpenOptions as CapOpenOptions, OpenOptionsExt as _};

    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES | DELETE);
    options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    options._cap_fs_ext_follow(FollowSymlinks::No);
    let file = cap_primitives::fs::open(parent, Path::new(name), &options)?;
    let metadata = file.metadata()?;
    if metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return Err(invalid("Windows cleanup cannot skip an ordinary directory"));
    }
    awaken_fs_win32::delete_open(&file)?;
    drop(file);
    Ok(())
}

fn remove_directory_contents(directory: &File) -> io::Result<()> {
    for name in directory_names(directory)? {
        let name = leaf(Path::new(&name))?;
        match open_directory_for_delete(directory, name) {
            Ok(child) => {
                remove_directory_contents(&child)?;
                awaken_fs_win32::delete_open(&child)?;
                drop(child);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => remove_non_directory_entry(directory, name)?,
        }
    }
    Ok(())
}

fn open_exact_directory(path: &Path, expected: DirectoryIdentity) -> io::Result<PinnedDirectory> {
    let directory = open_directory_nofollow(path)?;
    if identity(&directory)? != expected {
        return Err(invalid("Windows directory has a foreign identity"));
    }
    Ok(directory)
}

pub(super) fn clear_directory_contents_exact(
    path: &Path,
    expected: DirectoryIdentity,
) -> io::Result<()> {
    let directory = open_exact_directory(path, expected)?;
    remove_directory_contents(&directory)?;
    if directory_identity_nofollow(path)? != expected {
        return Err(invalid("Windows directory changed identity during cleanup"));
    }
    crate::sync_directory(path)
}

fn remove_directory_tree_in(
    parent_path: &Path,
    parent: &File,
    name: &OsStr,
    expected: DirectoryIdentity,
) -> io::Result<()> {
    let directory = open_directory_for_delete(parent, name)?;
    if identity(&directory)? != expected {
        return Err(invalid("Windows directory has a foreign identity"));
    }
    remove_directory_contents(&directory)?;
    awaken_fs_win32::delete_open(&directory)?;
    drop(directory);
    crate::sync_directory(parent_path)
}

pub(super) fn remove_directory_tree_exact(
    path: &Path,
    expected: DirectoryIdentity,
) -> io::Result<()> {
    let parent_path = path
        .parent()
        .ok_or_else(|| invalid("Windows directory has no parent"))?;
    let name = leaf(Path::new(
        path.file_name()
            .ok_or_else(|| invalid("Windows directory has no leaf"))?,
    ))?;
    let parent = open_directory_nofollow(parent_path)?;
    let parent_identity = identity(&parent)?;
    remove_directory_tree_in(parent_path, &parent, name, expected)?;
    if directory_identity_nofollow(parent_path)? != parent_identity {
        return Err(invalid("Windows cleanup parent changed identity"));
    }
    Ok(())
}

pub(super) fn clear_sibling_directory_contents_exact(
    lock: &ExclusiveFileLock,
    path: &Path,
    expected: DirectoryIdentity,
) -> io::Result<()> {
    let name = leaf(path)?;
    validate_parent_path(lock)?;
    let directory = open_directory_for_delete(&lock.parent, name)?;
    if identity(&directory)? != expected {
        return Err(invalid("Windows sibling directory has a foreign identity"));
    }
    remove_directory_contents(&directory)?;
    if identity(&directory)? != expected {
        return Err(invalid(
            "Windows sibling directory changed identity during cleanup",
        ));
    }
    crate::sync_directory(&lock.parent_path.join(name))?;
    validate_parent_path(lock)
}

pub(super) fn remove_sibling_directory_tree_exact(
    lock: &ExclusiveFileLock,
    path: &Path,
    expected: DirectoryIdentity,
) -> io::Result<()> {
    let name = leaf(path)?;
    validate_parent_path(lock)?;
    remove_directory_tree_in(&lock.parent_path, &lock.parent, name, expected)?;
    validate_parent_path(lock)
}

fn open_relative_parent(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
    create: bool,
) -> io::Result<Option<(PinnedDirectory, PathBuf, OsString)>> {
    super::validate_relative_tree_path(relative, false)?;
    let names = relative
        .components()
        .map(|component| {
            let Component::Normal(name) = component else {
                unreachable!("validated relative component")
            };
            leaf(Path::new(name)).map(OsStr::to_owned)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let (last, parents) = names
        .split_last()
        .ok_or_else(|| invalid("relative path is empty"))?;
    let mut directory = open_exact_directory(root, expected_root)?;
    let mut parent_path = root.to_owned();
    for name in parents {
        let child = match open_child_directory(&directory, name) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                create_directory_in(&parent_path, &directory, name)?;
                open_child_directory(&directory, name)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        directory = directory.descend(child);
        parent_path.push(name);
    }
    Ok(Some((directory, parent_path, last.clone())))
}

pub(super) fn create_relative_directory_all(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
) -> io::Result<()> {
    super::validate_relative_tree_path(relative, true)?;
    let mut directory = open_exact_directory(root, expected_root)?;
    let mut parent_path = root.to_owned();
    for component in relative.components() {
        let Component::Normal(raw_name) = component else {
            unreachable!("validated relative component")
        };
        let name = leaf(Path::new(raw_name))?;
        let child = match open_child_directory(&directory, name) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_directory_in(&parent_path, &directory, name)?;
                open_child_directory(&directory, name)?
            }
            Err(error) => return Err(error),
        };
        directory = directory.descend(child);
        parent_path.push(name);
    }
    if directory_identity_nofollow(root)? != expected_root {
        return Err(invalid("Windows relative directory root changed identity"));
    }
    Ok(())
}

fn apply_windows_mode(file: &File, mode: u32) -> io::Result<()> {
    if mode & !0o777 != 0 {
        return Err(invalid("Windows sandbox file mode has non-permission bits"));
    }
    // Workdir does not provide process isolation. NTFS ACLs remain inherited
    // from its provider-owned root; Windows has no POSIX execute bit. Preserve
    // the one native mode distinction Windows exposes on std::fs files.
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    file.set_permissions(permissions)
}

pub(super) fn set_relative_directory_mode(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
    mode: u32,
) -> io::Result<()> {
    let Some((parent, _, name)) = open_relative_parent(root, expected_root, relative, false)?
    else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Windows relative directory is absent",
        ));
    };
    let directory = open_child_directory_with(
        &parent,
        &name,
        GENERIC_READ | FILE_WRITE_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )?;
    apply_windows_mode(&directory, mode)?;
    if directory_identity_nofollow(root)? != expected_root {
        return Err(invalid("Windows directory-mode root changed identity"));
    }
    Ok(())
}

pub(super) fn write_relative_file_atomic(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
    contents: &[u8],
    mode: u32,
) -> io::Result<()> {
    if mode & !0o777 != 0 {
        return Err(invalid("Windows sandbox file mode has non-permission bits"));
    }
    let (parent, parent_path, name) = open_relative_parent(root, expected_root, relative, true)?
        .ok_or_else(|| io::Error::other("Windows materialization parent unexpectedly absent"))?;
    match open_regular(&parent, &name) {
        Ok(file) => {
            drop(file);
            transform_file_in(&parent_path, &parent, &name, Some(mode), |_, stage| {
                stage.write_all(contents)
            })?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            publish_file_in(&parent_path, &parent, &name, contents, Some(mode))?;
        }
        Err(error) => return Err(error),
    }
    if directory_identity_nofollow(root)? != expected_root {
        return Err(invalid("Windows materialization root changed identity"));
    }
    Ok(())
}

fn open_regular_for_write(parent: &File, name: &OsStr) -> io::Result<File> {
    use cap_primitives::fs::{FollowSymlinks, OpenOptions as CapOpenOptions, OpenOptionsExt as _};

    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    options._cap_fs_ext_follow(FollowSymlinks::No);
    let file = cap_primitives::fs::open(parent, Path::new(name), &options)?;
    let metadata = file.metadata()?;
    let info = file_information(&file)?;
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || info.number_of_links != 1
    {
        return Err(invalid(
            "Windows write target is not one unaliased regular file",
        ));
    }
    Ok(file)
}

pub(super) fn zero_relative_regular_file_nofollow(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
) -> io::Result<()> {
    let Some((parent, _, name)) = open_relative_parent(root, expected_root, relative, false)?
    else {
        return Ok(());
    };
    let mut file = match open_regular_for_write(&parent, &name) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let opened = file_information(&file)?;
    let mut remaining = file.metadata()?.len();
    let zeros = [0_u8; 8192];
    while remaining != 0 {
        let amount =
            usize::try_from(remaining.min(zeros.len() as u64)).expect("bounded zero chunk");
        file.write_all(&zeros[..amount])?;
        remaining -= amount as u64;
    }
    file.sync_all()?;
    if file_information(&file)? != opened || directory_identity_nofollow(root)? != expected_root {
        return Err(invalid("Windows shred target changed identity"));
    }
    Ok(())
}

pub(super) fn remove_relative_entry_exact(
    root: &Path,
    expected_root: DirectoryIdentity,
    relative: &Path,
) -> io::Result<()> {
    let Some((parent, parent_path, name)) =
        open_relative_parent(root, expected_root, relative, false)?
    else {
        return Ok(());
    };
    match open_directory_for_delete(&parent, &name) {
        Ok(directory) => {
            remove_directory_contents(&directory)?;
            awaken_fs_win32::delete_open(&directory)?;
            drop(directory);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => match remove_non_directory_entry(&parent, &name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        },
    }
    if directory_identity_nofollow(root)? != expected_root {
        return Err(invalid("Windows relative cleanup root changed identity"));
    }
    crate::sync_directory(&parent_path)
}

pub(super) fn validate_parent_path(lock: &ExclusiveFileLock) -> io::Result<()> {
    if identity(&lock.parent)? == lock.parent_identity
        && directory_identity_nofollow(&lock.parent_path)? == lock.parent_identity
    {
        Ok(())
    } else {
        Err(invalid("sandbox lock parent changed physical identity"))
    }
}
