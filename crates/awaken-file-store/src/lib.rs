//! Async BLAKE3 content-addressable file store.
//!
//! Files are keyed by a 64-hex-character BLAKE3 digest (`ContentId`).
//! Storage layout: `<root>/<first-2-hex>/<remaining-62-hex>.blob`.
//! Writes are atomic (temp-file + rename).

use std::path::{Path, PathBuf};

use bytes::Bytes;

mod error;
#[cfg(test)]
mod tests;

pub use error::FileStoreError;

/// 64-hex-character BLAKE3 digest identifying stored content.
pub type ContentId = String;

/// Async content-addressable store backed by the local filesystem.
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    /// Create a store rooted at `root`.  The directory is created on first
    /// [`put`][Self::put] if it does not already exist.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Store `data` and return its [`ContentId`].
    ///
    /// Idempotent: if the same content is put twice the blob is not
    /// overwritten (the rename becomes a no-op when destination exists).
    pub async fn put(&self, data: &[u8]) -> Result<ContentId, FileStoreError> {
        let id = blake3::hash(data).to_hex().to_string();
        let dest = self.blob_path(&id);
        if dest.exists() {
            return Ok(id);
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = dest.with_extension("tmp");
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, &dest).await?;
        Ok(id)
    }

    /// Retrieve content by its [`ContentId`].
    pub async fn get(&self, id: &str) -> Result<Bytes, FileStoreError> {
        validate_id(id)?;
        let path = self.blob_path(id);
        match tokio::fs::read(&path).await {
            Ok(data) => Ok(Bytes::from(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(FileStoreError::NotFound { id: id.to_owned() })
            }
            Err(e) => Err(FileStoreError::Io(e)),
        }
    }

    /// Return `true` if a blob with the given [`ContentId`] is present.
    pub async fn exists(&self, id: &str) -> bool {
        if validate_id(id).is_err() {
            return false;
        }
        self.blob_path(id).exists()
    }

    /// Return the root directory of this store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blob_path(&self, id: &str) -> PathBuf {
        let (prefix, tail) = id.split_at(2);
        self.root.join(prefix).join(format!("{tail}.blob"))
    }
}

fn validate_id(id: &str) -> Result<(), FileStoreError> {
    if id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(FileStoreError::InvalidId(id.to_owned()))
    }
}
