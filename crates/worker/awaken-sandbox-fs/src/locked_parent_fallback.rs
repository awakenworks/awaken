use super::*;

impl ExclusiveFileLock {
    #[cfg(not(windows))]
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
        #[cfg(windows)]
        return super::windows::validate_parent_path(self);
        #[cfg(not(windows))]
        self.unsupported("parent validation")
    }

    pub fn classify_sibling_nofollow(&self, leaf: &Path) -> std::io::Result<PathEntry> {
        #[cfg(windows)]
        return super::windows::classify_sibling(self, leaf);
        #[cfg(not(windows))]
        {
            let _ = leaf;
            self.unsupported("sibling classification")
        }
    }

    pub fn sibling_names(&self) -> std::io::Result<Vec<std::ffi::OsString>> {
        #[cfg(windows)]
        return super::windows::sibling_names(self);
        #[cfg(not(windows))]
        self.unsupported("sibling enumeration")
    }

    pub fn read_sibling_regular_file_nofollow(&self, leaf: &Path) -> std::io::Result<Vec<u8>> {
        #[cfg(windows)]
        return super::windows::read_sibling_regular_file(self, leaf);
        #[cfg(not(windows))]
        {
            let _ = leaf;
            self.unsupported("sibling file read")
        }
    }

    pub fn publish_sibling_file_noreplace(
        &self,
        destination_leaf: &Path,
        contents: &[u8],
    ) -> std::io::Result<()> {
        #[cfg(windows)]
        return super::windows::publish_sibling_file_noreplace(self, destination_leaf, contents);
        #[cfg(not(windows))]
        {
            let _ = (destination_leaf, contents);
            self.unsupported("sibling file publication")
        }
    }

    pub fn replace_sibling_regular_file_atomic(
        &self,
        destination_leaf: &Path,
        contents: &[u8],
    ) -> std::io::Result<()> {
        self.transform_sibling_regular_file_atomic(destination_leaf, |_, stage| {
            use std::io::Write as _;
            stage.write_all(contents)
        })
    }

    pub fn transform_sibling_regular_file_atomic<F>(
        &self,
        destination_leaf: &Path,
        transform: F,
    ) -> std::io::Result<()>
    where
        F: FnOnce(&mut File, &mut File) -> std::io::Result<()>,
    {
        #[cfg(windows)]
        return super::windows::transform_sibling_regular_file_atomic(
            self,
            destination_leaf,
            transform,
        );
        #[cfg(not(windows))]
        {
            let _ = (destination_leaf, transform);
            self.unsupported("sibling file transformation")
        }
    }

    pub fn remove_sibling_regular_file_nofollow(&self, leaf: &Path) -> std::io::Result<()> {
        #[cfg(windows)]
        return super::windows::remove_sibling_regular_file_nofollow(self, leaf);
        #[cfg(not(windows))]
        {
            let _ = leaf;
            self.unsupported("sibling file removal")
        }
    }

    pub fn create_sibling_directory_noreplace(
        &self,
        leaf: &Path,
    ) -> std::io::Result<DirectoryIdentity> {
        #[cfg(windows)]
        return super::windows::create_sibling_directory_noreplace(self, leaf);
        #[cfg(not(windows))]
        {
            let _ = leaf;
            self.unsupported("sibling directory creation")
        }
    }

    pub fn publish_sibling_directory_noreplace(
        &self,
        stage_leaf: &Path,
        destination_leaf: &Path,
    ) -> std::io::Result<()> {
        #[cfg(windows)]
        return super::windows::publish_sibling_directory_noreplace(
            self,
            stage_leaf,
            destination_leaf,
        );
        #[cfg(not(windows))]
        {
            let _ = (stage_leaf, destination_leaf);
            self.unsupported("sibling directory publication")
        }
    }

    pub fn clear_sibling_directory_contents_exact(
        &self,
        leaf: &Path,
        expected: DirectoryIdentity,
    ) -> std::io::Result<()> {
        #[cfg(windows)]
        return super::windows::clear_sibling_directory_contents_exact(self, leaf, expected);
        #[cfg(not(windows))]
        {
            let _ = (leaf, expected);
            self.unsupported("sibling directory clearing")
        }
    }

    pub fn remove_sibling_directory_tree_exact(
        &self,
        leaf: &Path,
        expected: DirectoryIdentity,
    ) -> std::io::Result<()> {
        #[cfg(windows)]
        return super::windows::remove_sibling_directory_tree_exact(self, leaf, expected);
        #[cfg(not(windows))]
        {
            let _ = (leaf, expected);
            self.unsupported("sibling directory removal")
        }
    }
}

#[cfg(windows)]
pub fn try_lock_exclusive(path: &Path) -> std::io::Result<ExclusiveFileLock> {
    super::windows::try_lock_exclusive(path)
}

#[cfg(not(windows))]
pub fn try_lock_exclusive(path: &Path) -> std::io::Result<ExclusiveFileLock> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "exclusive no-follow file locks are unsupported for `{}` on this platform",
            path.display()
        ),
    ))
}
