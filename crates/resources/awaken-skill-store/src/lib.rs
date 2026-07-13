//! Durable skill catalog for the resources plane.
//!
//! The [`SkillStore`] port (mirroring awaken-file-store's `FileStore`) with pluggable
//! backends: [`InMemorySkillStore`], [`FsSkillStore`] (a `SKILL.md`-per-skill tree on
//! disk), and — feature-gated — `SqliteSkillStore` / `PgSkillStore` over one portable
//! bundle. It is the durable, *delivered* skill source: a skill written here survives
//! a restart, and on postgres is shared across nodes, so a catalog configured through
//! the management plane outlives (and spans) the process that received it.
//!
//! This crate is deliberately store-shaped and nothing more: the runtime's
//! `awaken-ext-skills` never sees it. The host reads the catalog out of here and
//! feeds the bytes to the extension's `SkillSource` port as plain file data, so the
//! runtime only ever perceives local files and the injected catalog — never a store.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;

#[cfg(feature = "postgres")]
mod postgres;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod schema;
#[cfg(feature = "sqlite")]
mod sqlite;

#[cfg(feature = "postgres")]
pub use postgres::{PgSkillStore, PgStoreError};
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub use schema::{BUNDLE_ID, skill_store_bundle};
#[cfg(feature = "sqlite")]
pub use sqlite::{SqliteSkillStore, StoreError};

/// The filename backing a skill id: `<safe-stem>.md`. Ids are typically already
/// safe slugs, but a managed client can post an arbitrary id — so it is reduced to a
/// single safe stem here, and the id a `list` reports round-trips back to that stem.
fn id_filename(id: &str) -> String {
    format!("{}.md", sanitize_stem(id))
}

/// Reduce `name` to a safe single file stem: keep alphanumerics, `-`, `_`; map every
/// other run to a single `-`; never empty; bounded length. A `put` cannot escape the
/// root and a crafted `../` id resolves to a harmless stem.
pub fn sanitize_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(120);
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "skill".to_string()
    } else {
        trimmed
    }
}

// The `SkillStore` port + its error live in the port-only contract crate; this crate
// implements them and re-exports so `awaken_skill_store::SkillStore` keeps resolving.
pub use awaken_resource_contract::{SkillStore, SkillStoreError};

/// In-memory [`SkillStore`] (tests / ephemeral single-process).
#[derive(Default)]
pub struct InMemorySkillStore {
    // workspace_id → (id → content)
    inner: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
}

impl InMemorySkillStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SkillStore for InMemorySkillStore {
    async fn put(
        &self,
        workspace_id: &str,
        id: &str,
        content: &str,
    ) -> Result<String, SkillStoreError> {
        let stem = sanitize_stem(id);
        self.inner
            .lock()
            .unwrap()
            .entry(workspace_id.to_string())
            .or_default()
            .insert(stem.clone(), content.to_string());
        Ok(stem)
    }

    async fn get(&self, workspace_id: &str, id: &str) -> Result<Option<String>, SkillStoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(workspace_id)
            .and_then(|ws| ws.get(&sanitize_stem(id)).cloned()))
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<(String, String)>, SkillStoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(workspace_id)
            .map(|ws| ws.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default())
    }

    async fn delete(&self, workspace_id: &str, id: &str) -> Result<bool, SkillStoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get_mut(workspace_id)
            .is_some_and(|ws| ws.remove(&sanitize_stem(id)).is_some()))
    }
}

/// Filesystem [`SkillStore`]: `<root>/<workspace>/<id>.md`, one file per skill.
/// Both `workspace_id` and `id` are sanitized to a single safe stem, so a crafted
/// `../` can never escape the root. Survives a restart because it is on disk. `open`
/// is synchronous (one-time directory setup); the per-skill operations are async.
pub struct FsSkillStore {
    root: PathBuf,
}

impl FsSkillStore {
    /// Open (creating if absent) the catalog rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The catalog's root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn ws_dir(&self, workspace_id: &str) -> PathBuf {
        self.root.join(sanitize_stem(workspace_id))
    }
}

#[async_trait]
impl SkillStore for FsSkillStore {
    async fn put(
        &self,
        workspace_id: &str,
        id: &str,
        content: &str,
    ) -> Result<String, SkillStoreError> {
        let stem = sanitize_stem(id);
        let dir = self.ws_dir(workspace_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| SkillStoreError::Io(e.to_string()))?;
        tokio::fs::write(dir.join(format!("{stem}.md")), content)
            .await
            .map_err(|e| SkillStoreError::Io(e.to_string()))?;
        Ok(stem)
    }

    async fn get(&self, workspace_id: &str, id: &str) -> Result<Option<String>, SkillStoreError> {
        let path = self.ws_dir(workspace_id).join(id_filename(id));
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SkillStoreError::Io(e.to_string())),
        }
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<(String, String)>, SkillStoreError> {
        let dir = self.ws_dir(workspace_id);
        let mut read_dir = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(SkillStoreError::Io(e.to_string())),
        };
        let mut out: Vec<(String, String)> = Vec::new();
        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|e| SkillStoreError::Io(e.to_string()))?
        {
            let path = entry.path();
            if path.extension().is_some_and(|x| x == "md")
                && let Some(id) = path.file_stem().and_then(|s| s.to_str())
                && let Ok(content) = tokio::fs::read_to_string(&path).await
            {
                out.push((id.to_string(), content));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn delete(&self, workspace_id: &str, id: &str) -> Result<bool, SkillStoreError> {
        let path = self.ws_dir(workspace_id).join(id_filename(id));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(SkillStoreError::Io(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("awaken-skillstore-{tag}-{stamp}"))
    }

    #[tokio::test]
    async fn crafted_ids_cannot_escape_root() {
        let root = scratch("escape");
        let store = FsSkillStore::open(&root).unwrap();
        let id = store.put("ws", "../../etc/passwd", "x").await.unwrap();
        assert_eq!(id, "etc-passwd");
        assert!(root.join("ws").join("etc-passwd.md").exists());
        assert!(!root.parent().unwrap().join("passwd.md").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
