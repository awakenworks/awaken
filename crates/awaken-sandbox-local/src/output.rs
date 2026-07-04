//! Output collection: scan a host directory or tar archive and ingest each
//! file into the [`FileStore`], returning an [`Artifact`] per file.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use awaken_file_store::{ContentId, FileStore};

use crate::error::SandboxError;

/// A single collected output artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// BLAKE3 content hash — use as the `id` for [`FileStore::get`].
    pub id: ContentId,
    /// Relative path of the file within the output directory (or tar entry name).
    pub name: String,
}

/// Collects output files from a local directory or a tar archive and stores
/// them in the backing [`FileStore`].
///
/// `read_artifact(id)` is just [`FileStore::get`] — no separate lookup needed.
pub struct OutputCollector {
    store: Arc<FileStore>,
}

impl OutputCollector {
    pub fn new(store: Arc<FileStore>) -> Self {
        Self { store }
    }

    /// Walk `dir` recursively, ingest every regular file, and return one
    /// [`Artifact`] per file.  Symlinks are followed; non-regular entries
    /// (directories, pipes, …) are skipped.
    ///
    /// Names are relative paths from `dir` using `/` as separator, so they
    /// are portable across platforms.
    pub async fn collect_dir(&self, dir: &Path) -> Result<Vec<Artifact>, SandboxError> {
        let mut artifacts = Vec::new();
        collect_dir_recursive(&self.store, dir, dir, &mut artifacts).await?;
        Ok(artifacts)
    }

    /// Parse a tar archive from `data`, ingest every regular file entry, and
    /// return one [`Artifact`] per entry.  Non-regular entries are skipped.
    ///
    /// This is the Docker tar fallback path: `docker cp` and `docker export`
    /// both produce tar streams; this method accepts both.
    pub async fn collect_tar(&self, data: &[u8]) -> Result<Vec<Artifact>, SandboxError> {
        // tar crate is synchronous; run it on the blocking thread pool so we
        // don't block the async executor.
        let data = data.to_vec();
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let mut archive = tar::Archive::new(std::io::Cursor::new(&data));
            let entries = archive
                .entries()
                .map_err(|e| SandboxError::Tar(e.to_string()))?;

            let mut collected: Vec<(String, Vec<u8>)> = Vec::new();
            for entry in entries {
                let mut entry = entry.map_err(|e| SandboxError::Tar(e.to_string()))?;
                if entry.header().entry_type() != tar::EntryType::Regular {
                    continue;
                }
                let name = entry
                    .path()
                    .map_err(|e| SandboxError::Tar(e.to_string()))?
                    .to_string_lossy()
                    .trim_start_matches("./")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let mut content = Vec::new();
                entry
                    .read_to_end(&mut content)
                    .map_err(|e| SandboxError::Tar(e.to_string()))?;
                collected.push((name, content));
            }
            Ok::<_, SandboxError>(collected)
        })
        .await
        .map_err(|e| SandboxError::Tar(e.to_string()))??
        .into_iter()
        .map(|(name, content)| {
            let s = Arc::clone(&store);
            async move {
                let id = s.put(&content).await?;
                Ok::<Artifact, SandboxError>(Artifact { id, name })
            }
        })
        .collect::<futures::future::JoinAll<_>>()
        .await
        .into_iter()
        .collect()
    }

    /// Retrieve the content of an artifact by its [`ContentId`].
    ///
    /// This is the canonical `read_artifact` operation — a simple CAS lookup.
    pub async fn read_artifact(&self, id: &str) -> Result<bytes::Bytes, SandboxError> {
        Ok(self.store.get(id).await?)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

fn portable_name(base: &Path, full: &Path) -> String {
    full.strip_prefix(base)
        .unwrap_or(full)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

// Recursive async directory walk using a stack to avoid deep futures chains.
async fn collect_dir_recursive(
    store: &FileStore,
    base: &Path,
    dir: &Path,
    out: &mut Vec<Artifact>,
) -> Result<(), SandboxError> {
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&current).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() || ft.is_symlink() {
                let content = tokio::fs::read(&path).await?;
                let id = store.put(&content).await?;
                let name = portable_name(base, &path);
                out.push(Artifact { id, name });
            }
        }
    }
    Ok(())
}
