use super::*;

pub(super) fn identity(path: &Path) -> std::io::Result<DirectoryIdentity> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("`{}` is not a stable directory", path.display()),
        ));
    }
    #[cfg(windows)]
    {
        use std::hash::{Hash as _, Hasher as _};
        let handle = same_file::Handle::from_path(path)?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        handle.hash(&mut hasher);
        Ok(DirectoryIdentity {
            device: 0,
            inode: hasher.finish(),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory identity is unsupported on this platform",
        ))
    }
}

fn leaf_path(parent: &Path, leaf: &Path) -> std::io::Result<PathBuf> {
    Ok(parent.join(validate_sibling_leaf(leaf)?))
}

impl ExclusiveFileLock {
    pub fn validate_parent_path(&self) -> std::io::Result<()> {
        if identity(&self.parent_path)? == self.parent_identity {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "locked parent changed physical identity",
            ))
        }
    }

    pub fn classify_sibling_nofollow(&self, leaf: &Path) -> std::io::Result<PathEntry> {
        self.validate_parent_path()?;
        let path = leaf_path(&self.parent_path, leaf)?;
        let result = match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PathEntry::Absent,
            Err(error) => return Err(error),
            Ok(metadata) if metadata.file_type().is_symlink() => PathEntry::Symlink,
            Ok(metadata) if metadata.is_dir() => PathEntry::Directory(identity(&path)?),
            Ok(metadata) if metadata.is_file() => PathEntry::RegularFile,
            Ok(_) => PathEntry::Other,
        };
        self.validate_parent_path()?;
        Ok(result)
    }

    pub fn sibling_names(&self) -> std::io::Result<Vec<std::ffi::OsString>> {
        self.validate_parent_path()?;
        let mut names = std::fs::read_dir(&self.parent_path)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    pub fn read_sibling_regular_file_nofollow(&self, leaf: &Path) -> std::io::Result<Vec<u8>> {
        let path = leaf_path(&self.parent_path, leaf)?;
        if self.classify_sibling_nofollow(leaf)? != PathEntry::RegularFile {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "sibling is not a regular file",
            ));
        }
        std::fs::read(path)
    }

    pub fn publish_sibling_file_noreplace(
        &self,
        leaf: &Path,
        contents: &[u8],
    ) -> std::io::Result<()> {
        publish_file_noreplace(&leaf_path(&self.parent_path, leaf)?, contents)
    }

    pub fn replace_sibling_regular_file_atomic(
        &self,
        leaf: &Path,
        contents: &[u8],
    ) -> std::io::Result<()> {
        let path = leaf_path(&self.parent_path, leaf)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "replacement sibling is not a regular file",
            ));
        }
        let stage = write_private_file_stage(&path, contents)?;
        let result = (|| {
            std::fs::remove_file(&path)?;
            std::fs::rename(&stage, &path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(stage);
        }
        result
    }

    pub fn remove_sibling_regular_file_nofollow(&self, leaf: &Path) -> std::io::Result<()> {
        let path = leaf_path(&self.parent_path, leaf)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "removed sibling is not a regular file",
            ));
        }
        std::fs::remove_file(path)
    }

    pub fn create_sibling_directory_noreplace(
        &self,
        leaf: &Path,
    ) -> std::io::Result<DirectoryIdentity> {
        let path = leaf_path(&self.parent_path, leaf)?;
        std::fs::create_dir(&path)?;
        identity(&path)
    }

    pub fn publish_sibling_directory_noreplace(
        &self,
        stage_leaf: &Path,
        destination_leaf: &Path,
    ) -> std::io::Result<()> {
        let stage = leaf_path(&self.parent_path, stage_leaf)?;
        let destination = leaf_path(&self.parent_path, destination_leaf)?;
        if destination.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "destination directory already exists",
            ));
        }
        std::fs::rename(stage, destination)
    }

    pub fn clear_sibling_directory_contents_exact(
        &self,
        leaf: &Path,
        expected: DirectoryIdentity,
    ) -> std::io::Result<()> {
        let path = leaf_path(&self.parent_path, leaf)?;
        if identity(&path)? != expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "directory identity changed",
            ));
        }
        for entry in std::fs::read_dir(&path)? {
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

    pub fn remove_sibling_directory_tree_exact(
        &self,
        leaf: &Path,
        expected: DirectoryIdentity,
    ) -> std::io::Result<()> {
        let path = leaf_path(&self.parent_path, leaf)?;
        if identity(&path)? != expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "directory identity changed",
            ));
        }
        std::fs::remove_dir_all(path)
    }
}

pub fn try_lock_exclusive(path: &Path) -> std::io::Result<ExclusiveFileLock> {
    let parent_path = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "lock has no parent")
    })?;
    std::fs::create_dir_all(parent_path)?;
    let metadata = std::fs::symlink_metadata(parent_path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "lock parent is not a directory",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "lock is not a regular file",
        ));
    }
    file.try_lock()?;
    let parent_identity = identity(parent_path)?;
    let parent = file.try_clone()?;
    Ok(ExclusiveFileLock {
        owner_pid: std::process::id(),
        parent_path: parent_path.to_owned(),
        parent_identity,
        parent,
        _file: file,
    })
}
