//! Windows handle-bound operations. Every traversed directory stays open without
//! delete sharing. Leaf opens use OPEN_REPARSE_POINT, then validate the
//! opened object, not a preceding pathname lookup. Mutations never follow links.
//! The only pathname rename is between leaves of a retained, pinned parent.

use super::*;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt as _};
use fs_at::os::windows::{FileExt as _, OpenOptionsExt as _};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read as _, Write as _};
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn leaf(path: &Path) -> io::Result<&OsStr> {
    let name = validate_sibling_leaf(path)?;
    // Reject Win32 aliases, device names, ADS and NUL before any OS call. Rename
    // wrappers consume NUL-terminated paths, so embedded NUL must never truncate.
    let text = name.to_string_lossy();
    let stem = text.split('.').next().unwrap_or("").to_ascii_uppercase();
    if text.contains([':', '\0', '/', '\\'])
        || text.ends_with(['.', ' '])
        || matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
    {
        return Err(invalid("not one ordinary Windows filename"));
    }
    Ok(name)
}

fn reparse(file: &File) -> io::Result<bool> {
    Ok(file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

fn identity(file: &File) -> io::Result<DirectoryIdentity> {
    let info = winapi_util::file::information(file)?;
    Ok(DirectoryIdentity {
        device: info.volume_serial_number(),
        inode: info.file_index(),
    })
}

fn regular(file: &File) -> io::Result<()> {
    if reparse(file)?
        || !file.metadata()?.is_file()
        || winapi_util::file::information(file)?.number_of_links() != 1
    {
        return Err(invalid("expected one unaliased regular file"));
    }
    Ok(())
}

fn directory(file: &File) -> io::Result<()> {
    if reparse(file)? || !file.metadata()?.is_dir() {
        return Err(invalid("expected a non-reparse directory"));
    }
    Ok(())
}

fn open_at(parent: &File, name: &OsStr, access: u32, share: u32, create: bool) -> io::Result<File> {
    leaf(Path::new(name))?;
    // Establish identity through NtCreateFile(RootDirectory, leaf), without
    // following even the final reparse point. Creation happens only here.
    let named = fs_at::OpenOptions::default()
        .desired_access(FILE_READ_ATTRIBUTES)
        .create_new(create)
        .open_path_at(parent, name)?;
    #[cfg(test)]
    super::windows_tests::at_open_boundary(name)?;
    let dir = Dir::from_std_file(parent.try_clone()?);
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(access & FILE_GENERIC_WRITE == FILE_GENERIC_WRITE)
        .access_mode(access)
        .share_mode(share)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = dir.open_with(name, &options)?.into_std();
    // cap-std supplies Win32 share-mode control; the NT-relative handle supplies
    // identity. A redirected name cannot authorize content access. No truncate
    // or create occurs through the second open, and delete sharing is denied
    // before the comparison, so the verified name remains pinned afterwards.
    if identity(&file)? != identity(&named)? {
        return Err(invalid("entry changed identity while being opened"));
    }
    Ok(file)
}

/// Keep the whole absolute chain pinned, including the root's ancestors. The
/// names used by MoveFileEx cannot be renamed while these handles live.
pub(super) struct PinnedDir {
    pub(super) file: File,
    pub(super) path: PathBuf,
    pub(super) ancestors: Vec<File>,
}

impl PinnedDir {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let path = std::path::absolute(path)?;
        let mut components = path.components();
        let prefix = match components.next() {
            Some(std::path::Component::Prefix(prefix)) => {
                match prefix.kind() {
                    std::path::Prefix::Disk(_)
                    | std::path::Prefix::VerbatimDisk(_)
                    | std::path::Prefix::UNC(_, _)
                    | std::path::Prefix::VerbatimUNC(_, _) => {}
                    _ => return Err(invalid("unsupported Windows path namespace")),
                }
                prefix.as_os_str()
            }
            _ => return Err(invalid("directory has no absolute Windows prefix")),
        };
        if components.next() != Some(std::path::Component::RootDir) {
            return Err(invalid("directory is not absolute"));
        }
        let mut root = PathBuf::from(prefix);
        root.push("\\");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&root)?;
        directory(&file)?;
        let mut result = Self {
            file,
            path: root,
            ancestors: Vec::new(),
        };
        for component in components {
            let std::path::Component::Normal(name) = component else {
                return Err(invalid("noncanonical directory path"));
            };
            result = result.child(name, false)?;
        }
        Ok(result)
    }

    pub(super) fn exact(path: &Path, expected: DirectoryIdentity) -> io::Result<Self> {
        let result = Self::open(path)?;
        if identity(&result.file)? != expected {
            return Err(invalid("sandbox root identity changed"));
        }
        Ok(result)
    }

    fn child(mut self, name: &OsStr, create: bool) -> io::Result<Self> {
        leaf(Path::new(name))?;
        if create {
            match fs_at::OpenOptions::default()
                .read(true)
                .follow(false)
                .mkdir_at(&self.file, name)
            {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let file = open_at(
            &self.file,
            name,
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            false,
        )?;
        directory(&file)?;
        self.ancestors.push(self.file);
        Ok(Self {
            file,
            path: self.path.join(name),
            ancestors: self.ancestors,
        })
    }

    fn walk(mut self, relative: &Path, create: bool) -> io::Result<Self> {
        validate_relative_tree_path(relative, true)?;
        for component in relative.components() {
            self = self.child(component.as_os_str(), create)?;
        }
        Ok(self)
    }
}

fn parent(path: &Path) -> io::Result<(PinnedDir, OsString)> {
    let name = leaf(Path::new(
        path.file_name()
            .ok_or_else(|| invalid("missing filename"))?,
    ))?;
    let dir = PinnedDir::open(path.parent().ok_or_else(|| invalid("missing parent"))?)?;
    Ok((dir, name.to_owned()))
}

fn relative_parent(
    root: &Path,
    expected: DirectoryIdentity,
    relative: &Path,
    create: bool,
) -> io::Result<(PinnedDir, OsString)> {
    // The root must still be exact even when the relative entry is absent.
    let root = PinnedDir::exact(root, expected)?;
    validate_relative_tree_path(relative, false)?;
    let name = leaf(Path::new(
        relative
            .file_name()
            .ok_or_else(|| invalid("missing leaf"))?,
    ))?;
    let parent = root.walk(relative.parent().unwrap_or(Path::new("")), create)?;
    Ok((parent, name.to_owned()))
}

pub fn directory_identity_nofollow(path: &Path) -> io::Result<DirectoryIdentity> {
    identity(&PinnedDir::open(path)?.file)
}

pub(super) fn classify_at(parent: &File, name: &OsStr) -> io::Result<PathEntry> {
    let file = match open_at(parent, name, FILE_GENERIC_READ, FILE_SHARE_READ, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(PathEntry::Absent),
        Err(error) => return Err(error),
    };
    Ok(if reparse(&file)? {
        PathEntry::Symlink
    } else if file.metadata()?.is_dir() {
        PathEntry::Directory(identity(&file)?)
    } else if file.metadata()?.is_file() {
        PathEntry::RegularFile
    } else {
        PathEntry::Other
    })
}

pub fn classify_nofollow(path: &Path) -> io::Result<PathEntry> {
    let (parent, name) = match parent(path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(PathEntry::Absent),
        Err(error) => return Err(error),
    };
    classify_at(&parent.file, &name)
}

pub(super) fn read_at(parent: &File, name: &OsStr) -> io::Result<Vec<u8>> {
    let mut file = open_at(parent, name, FILE_GENERIC_READ, FILE_SHARE_READ, false)?;
    regular(&file)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn read_regular_file_nofollow(path: &Path) -> io::Result<Vec<u8>> {
    let (parent, name) = parent(path)?;
    read_at(&parent.file, &name)
}

pub(super) fn remove_file_at(parent: &File, name: &OsStr) -> io::Result<()> {
    let file = open_at(
        parent,
        name,
        FILE_GENERIC_READ | DELETE,
        FILE_SHARE_READ,
        false,
    )?;
    regular(&file)?;
    file.delete_by_handle().map_err(|(_, error)| error)
}

pub fn remove_regular_file_nofollow(path: &Path) -> io::Result<()> {
    let (parent, name) = parent(path)?;
    remove_file_at(&parent.file, &name)
}

pub(super) fn names(file: &File) -> io::Result<Vec<OsString>> {
    let mut file = file.try_clone()?;
    let mut names = fs_at::read_dir(&mut file)?
        .map(|entry| entry.map(|entry| entry.name().to_owned()))
        .collect::<io::Result<Vec<_>>>()?;
    names.retain(|name| name != "." && name != "..");
    names.sort();
    Ok(names)
}

pub fn zero_relative_regular_file_nofollow(
    root: &Path,
    expected: DirectoryIdentity,
    relative: &Path,
) -> io::Result<()> {
    // Do not turn a missing/foreign root into a successful cleanup.
    let root_guard = PinnedDir::exact(root, expected)?;
    let result = (|| {
        let (parent, name) = relative_parent(root, expected, relative, false)?;
        let mut file = open_at(
            &parent.file,
            &name,
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ,
            false,
        )?;
        regular(&file)?;
        let mut remaining = file.metadata()?.len();
        let zeros = [0_u8; 8192];
        while remaining != 0 {
            let length = usize::try_from(remaining.min(zeros.len() as u64)).expect("bounded chunk");
            file.write_all(&zeros[..length])?;
            remaining -= length as u64;
        }
        file.sync_all()
    })();
    drop(root_guard);
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

pub fn create_relative_directory_all(
    root: &Path,
    expected: DirectoryIdentity,
    relative: &Path,
) -> io::Result<()> {
    PinnedDir::exact(root, expected)?
        .walk(relative, true)
        .map(|_| ())
}

pub fn set_relative_directory_mode(
    root: &Path,
    expected: DirectoryIdentity,
    relative: &Path,
    _mode: u32,
) -> io::Result<()> {
    validate_relative_tree_path(relative, false)?;
    PinnedDir::exact(root, expected)?
        .walk(relative, false)
        .map(|_| ())
}

fn stage_name(destination: &OsStr) -> OsString {
    format!(
        ".{}.awaken-stage-{}-{}",
        destination.to_string_lossy(),
        std::process::id(),
        PRIVATE_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
    .into()
}

pub(super) fn write_at(
    parent: &File,
    parent_path: &Path,
    name: &OsStr,
    contents: &[u8],
    replace: bool,
) -> io::Result<()> {
    leaf(Path::new(name))?;
    if replace {
        let destination = open_at(parent, name, FILE_GENERIC_READ, FILE_SHARE_READ, false)?;
        regular(&destination)?;
    }
    let (stage, mut file) = loop {
        let stage = stage_name(name);
        match open_at(parent, &stage, FILE_GENERIC_WRITE, FILE_SHARE_READ, true) {
            Ok(file) => break (stage, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = file.write_all(contents).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| {
        let source = parent_path.join(&stage);
        let destination = parent_path.join(name);
        #[cfg(test)]
        super::windows_tests::at_rename_boundary(&source, &destination)?;
        // One OS rename. Never unlink the old destination. The parent chain is
        // retained by the caller, and stage is a private create_new allocation.
        if replace {
            atomicwrites::replace_atomic(&source, &destination)
        } else {
            atomicwrites::move_atomic(&source, &destination)
        }
    });
    if result.is_err() {
        let _ = remove_file_at(parent, &stage);
    }
    result
}

pub fn replace_regular_file_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let (parent, name) = parent(path)?;
    write_at(&parent.file, &parent.path, &name, contents, true)
}

pub fn publish_file_noreplace(path: &Path, contents: &[u8]) -> io::Result<()> {
    let (parent, name) = parent(path)?;
    write_at(&parent.file, &parent.path, &name, contents, false)
}

pub fn write_relative_file_atomic(
    root: &Path,
    expected: DirectoryIdentity,
    relative: &Path,
    contents: &[u8],
    _mode: u32,
) -> io::Result<()> {
    let (parent, name) = relative_parent(root, expected, relative, true)?;
    let replace = match classify_at(&parent.file, &name)? {
        PathEntry::Absent => false,
        PathEntry::RegularFile => true,
        _ => return Err(invalid("materialization destination is not a regular file")),
    };
    write_at(&parent.file, &parent.path, &name, contents, replace)
}

pub(super) fn create_dir_at(parent: &File, name: &OsStr) -> io::Result<DirectoryIdentity> {
    leaf(Path::new(name))?;
    let created = fs_at::OpenOptions::default()
        .read(true)
        .follow(false)
        .mkdir_at(parent, name)?;
    drop(created);
    let file = open_at(parent, name, FILE_GENERIC_READ, FILE_SHARE_READ, false)?;
    directory(&file)?;
    identity(&file)
}

pub fn create_directory_noreplace(path: &Path) -> io::Result<DirectoryIdentity> {
    let (parent, name) = parent(path)?;
    create_dir_at(&parent.file, &name)
}

pub fn create_private_directory_stage(path: &Path) -> io::Result<(PathBuf, DirectoryIdentity)> {
    let (parent, name) = parent(path)?;
    for _ in 0..32 {
        let stage = stage_name(&name);
        match create_dir_at(&parent.file, &stage) {
            Ok(identity) => return Ok((parent.path.join(stage), identity)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "stage allocation exhausted",
    ))
}

pub(super) fn publish_dir_at(
    parent: &File,
    path: &Path,
    stage: &OsStr,
    destination: &OsStr,
) -> io::Result<()> {
    leaf(Path::new(destination))?;
    let file = open_at(parent, stage, FILE_GENERIC_READ, FILE_SHARE_READ, false)?;
    directory(&file)?;
    drop(file);
    // MoveFileExW without REPLACE_EXISTING enforces absence at the rename itself,
    // including empty directories and names created by a competing publisher.
    let source = path.join(stage);
    let target = path.join(destination);
    #[cfg(test)]
    super::windows_tests::at_rename_boundary(&source, &target)?;
    atomicwrites::move_atomic(&source, &target)
}

pub fn publish_directory_noreplace(stage: &Path, destination: &Path) -> io::Result<()> {
    if !stage.is_absolute() || !destination.is_absolute() || stage.parent() != destination.parent()
    {
        return Err(invalid("stage and destination must be absolute siblings"));
    }
    let (parent, name) = parent(stage)?;
    let target = leaf(Path::new(
        destination
            .file_name()
            .ok_or_else(|| invalid("missing leaf"))?,
    ))?;
    publish_dir_at(&parent.file, &parent.path, &name, target)
}

fn walk_tree(
    file: &File,
    relative: &Path,
    excluded: &[PathBuf],
    snapshot: &mut TreeSnapshot,
) -> io::Result<()> {
    for name in names(file)? {
        let child_path = relative.join(&name);
        if tree_path_is_excluded(&child_path, excluded) {
            continue;
        }
        let mut child = open_at(file, &name, FILE_GENERIC_READ, FILE_SHARE_READ, false)?;
        if reparse(&child)? {
            return Err(invalid("tree contains a reparse point"));
        }
        if child.metadata()?.is_dir() {
            snapshot.directories.push(TreeDirectory {
                relative_path: child_path.clone(),
                mode: 0o700,
            });
            walk_tree(&child, &child_path, excluded, snapshot)?;
        } else {
            regular(&child)?;
            let mut bytes = Vec::new();
            child.read_to_end(&mut bytes)?;
            snapshot.files.push(TreeFile {
                relative_path: child_path,
                bytes,
                mode: 0o600,
            });
        }
    }
    Ok(())
}

pub fn read_tree_nofollow_excluding(
    root: &Path,
    expected: DirectoryIdentity,
    relative: &Path,
    excluded: &[PathBuf],
) -> io::Result<TreeSnapshot> {
    for path in excluded {
        validate_relative_tree_path(path, false)?;
    }
    let root = PinnedDir::exact(root, expected)?;
    let tree = match root.walk(relative, false) {
        Ok(tree) => tree,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(TreeSnapshot::default()),
        Err(error) => return Err(error),
    };
    let mut snapshot = TreeSnapshot::default();
    walk_tree(&tree.file, Path::new(""), excluded, &mut snapshot)?;
    snapshot
        .directories
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    snapshot
        .files
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(snapshot)
}

pub fn read_tree_nofollow(
    root: &Path,
    expected: DirectoryIdentity,
    relative: &Path,
) -> io::Result<TreeSnapshot> {
    read_tree_nofollow_excluding(root, expected, relative, &[])
}

pub fn read_regular_tree_nofollow(
    root: &Path,
    expected: DirectoryIdentity,
    relative: &Path,
) -> io::Result<Vec<TreeFile>> {
    read_tree_nofollow(root, expected, relative).map(|tree| tree.files)
}

pub(super) fn clear(file: &File) -> io::Result<()> {
    for name in names(file)? {
        remove_entry_at(file, &name, None)?;
    }
    Ok(())
}

pub(super) fn remove_entry_at(
    parent: &File,
    name: &OsStr,
    expected: Option<DirectoryIdentity>,
) -> io::Result<()> {
    let file = open_at(
        parent,
        name,
        FILE_GENERIC_READ | DELETE,
        FILE_SHARE_READ,
        false,
    )?;
    if let Some(expected) = expected {
        directory(&file)?;
        if identity(&file)? != expected {
            return Err(invalid("directory identity changed"));
        }
    }
    if !reparse(&file)? && file.metadata()?.is_dir() {
        clear(&file)?;
    }
    // Delete the opened object while still holding its identity, even for a
    // junction. Never drop the handle and reopen a potentially substituted name.
    file.delete_by_handle().map_err(|(_, error)| error)
}

pub fn remove_relative_entry_exact(
    root: &Path,
    expected: DirectoryIdentity,
    relative: &Path,
) -> io::Result<()> {
    let guard = PinnedDir::exact(root, expected)?;
    let result = relative_parent(root, expected, relative, false)
        .and_then(|(parent, name)| remove_entry_at(&parent.file, &name, None));
    drop(guard);
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

pub fn clear_directory_contents_exact(path: &Path, expected: DirectoryIdentity) -> io::Result<()> {
    clear(&PinnedDir::exact(path, expected)?.file)
}

pub fn remove_directory_tree_exact(path: &Path, expected: DirectoryIdentity) -> io::Result<()> {
    let (parent, name) = parent(path)?;
    remove_entry_at(&parent.file, &name, Some(expected))
}

#[path = "windows_locked_parent.rs"]
mod locked_parent;
pub use locked_parent::try_lock_exclusive;
