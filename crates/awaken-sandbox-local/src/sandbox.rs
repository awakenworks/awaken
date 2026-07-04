use std::path::{Path, PathBuf};

/// A temporary sandbox directory.
///
/// The directory is removed when the `Sandbox` is dropped unless
/// [`Sandbox::into_path`] is called first.
pub struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    pub(crate) fn new(dir: tempfile::TempDir) -> Self {
        Self { dir }
    }

    /// Absolute path to the sandbox root.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Resolve a relative path inside the sandbox to an absolute path.
    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// Consume the sandbox and return the underlying path without deleting it.
    pub fn into_path(self) -> PathBuf {
        self.dir.keep()
    }
}
