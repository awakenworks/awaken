use super::*;

impl ExclusiveFileLock {
    fn unsupported<T>(&self, operation: &str) -> std::io::Result<T> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "descriptor-relative {operation} is unsupported for locked parent `{}` on this platform",
                self.parent_path.display()
            ),
        ))
    }

    pub fn validate_parent_path(&self) -> std::io::Result<()> {
        self.unsupported("parent validation")
    }

    pub fn classify_sibling_nofollow(&self, _leaf: &Path) -> std::io::Result<PathEntry> {
        self.unsupported("sibling classification")
    }

    pub fn sibling_names(&self) -> std::io::Result<Vec<std::ffi::OsString>> {
        self.unsupported("sibling enumeration")
    }

    pub fn read_sibling_regular_file_nofollow(&self, _leaf: &Path) -> std::io::Result<Vec<u8>> {
        self.unsupported("sibling file read")
    }

    pub fn publish_sibling_file_noreplace(
        &self,
        _destination_leaf: &Path,
        _contents: &[u8],
    ) -> std::io::Result<()> {
        self.unsupported("sibling file publication")
    }

    pub fn replace_sibling_regular_file_atomic(
        &self,
        _destination_leaf: &Path,
        _contents: &[u8],
    ) -> std::io::Result<()> {
        self.unsupported("sibling file replacement")
    }

    pub fn remove_sibling_regular_file_nofollow(&self, _leaf: &Path) -> std::io::Result<()> {
        self.unsupported("sibling file removal")
    }

    pub fn create_sibling_directory_noreplace(
        &self,
        _leaf: &Path,
    ) -> std::io::Result<DirectoryIdentity> {
        self.unsupported("sibling directory creation")
    }

    pub fn publish_sibling_directory_noreplace(
        &self,
        _stage_leaf: &Path,
        _destination_leaf: &Path,
    ) -> std::io::Result<()> {
        self.unsupported("sibling directory publication")
    }

    pub fn clear_sibling_directory_contents_exact(
        &self,
        _leaf: &Path,
        _expected: DirectoryIdentity,
    ) -> std::io::Result<()> {
        self.unsupported("sibling directory clearing")
    }

    pub fn remove_sibling_directory_tree_exact(
        &self,
        _leaf: &Path,
        _expected: DirectoryIdentity,
    ) -> std::io::Result<()> {
        self.unsupported("sibling directory removal")
    }
}

pub fn try_lock_exclusive(path: &Path) -> std::io::Result<ExclusiveFileLock> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "exclusive no-follow file locks are unsupported for `{}` on this platform",
            path.display()
        ),
    ))
}
