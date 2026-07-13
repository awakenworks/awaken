//! Cross-directory / cross-machine recovery of a local-dir ACP CLI's session.
//!
//! A CLI like Claude Code or Codex keeps its conversation under a subtree of its
//! config home (`projects/`, `sessions/`). Our per-turn relaunch and distributed
//! dispatch mean a later turn may run in a different directory — or on a different
//! worker — where that subtree is absent, so `session/load` finds nothing. This
//! module restores the thread's session subtree into the config home before a run
//! and harvests it back after, keyed by (thread, adapter), so the session survives
//! the move.
//!
//! Two boundaries make it safe: the harvest is scoped to the declared **session
//! subtree only** (the config home's credentials / local config at the root are
//! never carried into the portable blob), and it is **best-effort** — a missing or
//! failed blob just falls back to `session/new` + the neutral thread history, which
//! is always the authority. The durable backing is an injected [`SessionBlobStore`];
//! a shared (content-addressed) one makes recovery cross-machine.
//!
//! This is the *reference* implementation of the crate's [`SessionHomeProvider`]
//! port. It depends only on the port + the [`ConfigHome`] path convention (both in
//! this crate) and std — never on the heavy host service layer — so a host reuses it
//! for cross-machine recovery without linking the management plane.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::config_home::ConfigHome;
use crate::{SessionHomeKey, SessionHomePlan, SessionHomeProvider};

/// Durable storage for a thread's portable session subtree, keyed by (thread,
/// adapter). Dependency-inverted so a host swaps a local directory for a shared,
/// content-addressed store (the cross-machine backing) without touching the
/// provider.
#[async_trait]
pub trait SessionBlobStore: Send + Sync {
    /// Copy the stored session tree for `key` into `dest` (created if needed).
    /// Returns `false` when nothing is stored — a fresh session.
    async fn fetch(&self, key: &SessionHomeKey, dest: &Path) -> io::Result<bool>;
    /// Store the tree at `src` under `key`, replacing any prior copy.
    async fn store(&self, key: &SessionHomeKey, src: &Path) -> io::Result<()>;
}

/// A [`SessionBlobStore`] backed by a local directory: `root/<thread>/<adapter>/`.
/// For a single machine this is the durable store; point `root` at a shared mount
/// (or replace with a content-addressed adapter) for cross-machine recovery.
pub struct FsSessionBlobStore {
    root: PathBuf,
}

impl FsSessionBlobStore {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn key_dir(&self, key: &SessionHomeKey) -> PathBuf {
        self.root.join(&key.thread_id).join(&key.adapter)
    }
}

#[async_trait]
impl SessionBlobStore for FsSessionBlobStore {
    async fn fetch(&self, key: &SessionHomeKey, dest: &Path) -> io::Result<bool> {
        let src = self.key_dir(key);
        if !src.is_dir() {
            return Ok(false);
        }
        copy_tree(&src, dest)?;
        Ok(true)
    }

    async fn store(&self, key: &SessionHomeKey, src: &Path) -> io::Result<()> {
        let dst = self.key_dir(key);
        // Replace any prior copy (a whole-tree snapshot, not a per-file merge).
        if dst.exists() {
            std::fs::remove_dir_all(&dst)?;
        }
        copy_tree(src, &dst)
    }
}

/// Recovers a local-dir CLI's session across directories/machines by restoring and
/// harvesting its declared session subtree under the thread's stable config home.
/// The provider is constructed knowing the config-home base and its blob store; the
/// tenant / data-subject scope is applied by the host when it wires them.
pub struct DirSessionHome {
    store_dir: Option<PathBuf>,
    blobs: Arc<dyn SessionBlobStore>,
}

impl DirSessionHome {
    #[must_use]
    pub fn new(store_dir: Option<PathBuf>, blobs: Arc<dyn SessionBlobStore>) -> Self {
        Self { store_dir, blobs }
    }

    /// The absolute path of this run's session subtree under the config home.
    fn session_dir(&self, key: &SessionHomeKey, plan: &SessionHomePlan) -> io::Result<PathBuf> {
        let home = ConfigHome::open(self.store_dir.as_deref(), &key.thread_id)?;
        Ok(home.root().join(&plan.session_subpath))
    }
}

#[async_trait]
impl SessionHomeProvider for DirSessionHome {
    async fn restore(&self, key: &SessionHomeKey, plan: &SessionHomePlan) {
        // Best-effort: a failure just means the CLI starts fresh and the neutral
        // thread history rehydrates the run.
        if let Ok(dir) = self.session_dir(key, plan) {
            let _ = self.blobs.fetch(key, &dir).await;
        }
    }

    async fn harvest(&self, key: &SessionHomeKey, plan: &SessionHomePlan) {
        if let Ok(dir) = self.session_dir(key, plan) {
            if dir.is_dir() {
                let _ = self.blobs.store(key, &dir).await;
            }
        }
    }
}

/// Recursively copy `src` into `dest` (byte-for-byte, binary-safe). The session
/// subtree is opaque CLI data, so it is snapshotted whole, never interpreted.
fn copy_tree(src: &Path, dest: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> SessionHomePlan {
        SessionHomePlan {
            config_home_env: "CLAUDE_CONFIG_DIR".to_string(),
            session_subpath: "projects".to_string(),
            exclude: vec![".credentials.json".to_string()],
            keyed_by_cwd: true,
        }
    }

    #[tokio::test]
    async fn recovers_a_session_across_config_homes_and_leaves_credentials_behind() {
        let store_a = tempfile::tempdir().unwrap();
        let store_b = tempfile::tempdir().unwrap();
        let blob_root = tempfile::tempdir().unwrap();
        let blobs = Arc::new(FsSessionBlobStore::new(blob_root.path().to_path_buf()));
        let key = SessionHomeKey {
            thread_id: "t1".to_string(),
            adapter: "claude".to_string(),
        };

        // Config home A holds a session file (under the harvested subtree) and a
        // credential at the config-home root (outside it).
        let home_a = ConfigHome::open(Some(store_a.path()), "t1").unwrap();
        home_a
            .write("projects/conv/session.jsonl", b"turn-1")
            .unwrap();
        home_a
            .write(".credentials.json", b"local-auth-bytes")
            .unwrap();

        DirSessionHome::new(Some(store_a.path().to_path_buf()), blobs.clone())
            .harvest(&key, &plan())
            .await;

        // Restore into a *different* config home (a different directory/machine).
        DirSessionHome::new(Some(store_b.path().to_path_buf()), blobs.clone())
            .restore(&key, &plan())
            .await;

        let home_b = ConfigHome::open(Some(store_b.path()), "t1").unwrap();
        assert_eq!(
            home_b
                .read("projects/conv/session.jsonl")
                .unwrap()
                .as_deref(),
            Some(&b"turn-1"[..]),
            "the session recovered across the directory change"
        );
        assert!(
            home_b.read(".credentials.json").unwrap().is_none(),
            "credentials at the config-home root are never harvested"
        );
    }

    #[tokio::test]
    async fn restore_of_an_unknown_session_is_a_no_op() {
        let store = tempfile::tempdir().unwrap();
        let blob_root = tempfile::tempdir().unwrap();
        let blobs = Arc::new(FsSessionBlobStore::new(blob_root.path().to_path_buf()));
        let key = SessionHomeKey {
            thread_id: "never-seen".to_string(),
            adapter: "claude".to_string(),
        };
        // No panic, nothing recovered — the run falls back to a fresh session.
        DirSessionHome::new(Some(store.path().to_path_buf()), blobs)
            .restore(&key, &plan())
            .await;
        let home = ConfigHome::open(Some(store.path()), "never-seen").unwrap();
        assert!(home.read("projects/conv/session.jsonl").unwrap().is_none());
    }
}
