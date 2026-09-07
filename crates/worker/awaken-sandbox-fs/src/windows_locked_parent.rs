use super::*;

impl ExclusiveFileLock {
    pub fn validate_parent_path(&self) -> io::Result<()> {
        directory(&self.parent)?;
        if identity(&self.parent)? != self.parent_identity
            || directory_identity_nofollow(&self.parent_path)? != self.parent_identity
        {
            return Err(invalid("locked parent changed physical identity"));
        }
        Ok(())
    }

    pub fn classify_sibling_nofollow(&self, name: &Path) -> io::Result<PathEntry> {
        self.validate_parent_path()?;
        classify_at(&self.parent, leaf(name)?)
    }

    pub fn sibling_names(&self) -> io::Result<Vec<OsString>> {
        self.validate_parent_path()?;
        names(&self.parent)
    }

    pub fn read_sibling_regular_file_nofollow(&self, name: &Path) -> io::Result<Vec<u8>> {
        self.validate_parent_path()?;
        read_at(&self.parent, leaf(name)?)
    }

    pub fn publish_sibling_file_noreplace(&self, name: &Path, contents: &[u8]) -> io::Result<()> {
        self.validate_parent_path()?;
        write_at(
            &self.parent,
            &self.parent_path,
            leaf(name)?,
            contents,
            false,
        )
    }

    pub fn replace_sibling_regular_file_atomic(
        &self,
        name: &Path,
        contents: &[u8],
    ) -> io::Result<()> {
        self.validate_parent_path()?;
        write_at(&self.parent, &self.parent_path, leaf(name)?, contents, true)
    }

    pub fn remove_sibling_regular_file_nofollow(&self, name: &Path) -> io::Result<()> {
        self.validate_parent_path()?;
        remove_file_at(&self.parent, leaf(name)?)
    }

    pub fn create_sibling_directory_noreplace(&self, name: &Path) -> io::Result<DirectoryIdentity> {
        self.validate_parent_path()?;
        create_dir_at(&self.parent, leaf(name)?)
    }

    pub fn publish_sibling_directory_noreplace(
        &self,
        stage: &Path,
        destination: &Path,
    ) -> io::Result<()> {
        self.validate_parent_path()?;
        publish_dir_at(
            &self.parent,
            &self.parent_path,
            leaf(stage)?,
            leaf(destination)?,
        )
    }

    pub fn clear_sibling_directory_contents_exact(
        &self,
        name: &Path,
        expected: DirectoryIdentity,
    ) -> io::Result<()> {
        self.validate_parent_path()?;
        let child = open_at(
            &self.parent,
            leaf(name)?,
            FILE_GENERIC_READ,
            FILE_SHARE_READ,
            false,
        )?;
        directory(&child)?;
        if identity(&child)? != expected {
            return Err(invalid("directory identity changed"));
        }
        clear(&child)
    }

    pub fn remove_sibling_directory_tree_exact(
        &self,
        name: &Path,
        expected: DirectoryIdentity,
    ) -> io::Result<()> {
        self.validate_parent_path()?;
        remove_entry_at(&self.parent, leaf(name)?, Some(expected))
    }
}

pub fn try_lock_exclusive(path: &Path) -> io::Result<ExclusiveFileLock> {
    let (parent, name) = parent(path)?;
    let file = match open_at(
        &parent.file,
        &name,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        true,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => open_at(
            &parent.file,
            &name,
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            false,
        )?,
        Err(error) => return Err(error),
    };
    regular(&file)?;
    file.try_lock()?;
    Ok(ExclusiveFileLock {
        owner_pid: std::process::id(),
        parent_identity: identity(&parent.file)?,
        parent_path: parent.path,
        parent: parent.file,
        _ancestors: parent.ancestors,
        _file: file,
    })
}
