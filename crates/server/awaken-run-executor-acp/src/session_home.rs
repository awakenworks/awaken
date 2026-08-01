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
use awaken_runtime_contract::{ContentEraser, DataSubjectId, ErasureError};

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
#[derive(Clone)]
pub struct FsSessionBlobStore {
    root: PathBuf,
}

impl FsSessionBlobStore {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn digest(value: &str) -> String {
        awaken_runtime_contract::resolution::content_fingerprint(value)
            .expect("a string always serializes")
    }

    fn subject_dir(&self, subject: Option<&str>) -> PathBuf {
        let scope = subject
            .map(Self::digest)
            .unwrap_or_else(|| "unattributed".to_string());
        self.root.join(scope)
    }

    fn erasure_receipt_path(&self, subject: &str) -> PathBuf {
        self.root
            .join(".erasure_fences")
            .join(Self::digest(subject))
    }

    fn erasure_receipt(&self, subject: &str) -> io::Result<Option<usize>> {
        let path = self.erasure_receipt_path(subject);
        match std::fs::read_to_string(path) {
            Ok(value) => value
                .parse::<usize>()
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn persist_erasure_receipt(&self, subject: &str, removed: usize) -> io::Result<()> {
        let path = self.erasure_receipt_path(subject);
        std::fs::create_dir_all(path.parent().expect("receipt path has a parent"))?;
        std::fs::write(path, removed.to_string())
    }

    fn key_dir(&self, key: &SessionHomeKey) -> PathBuf {
        self.subject_dir(key.data_subject_id.as_deref())
            .join(Self::digest(&key.thread_id))
            .join(Self::digest(&key.adapter))
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
        if let Some(subject) = key.data_subject_id.as_deref()
            && self.erasure_receipt(subject)?.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "data subject has an erasure fence",
            ));
        }
        let dst = self.key_dir(key);
        // Replace any prior copy (a whole-tree snapshot, not a per-file merge).
        if dst.exists() {
            std::fs::remove_dir_all(&dst)?;
        }
        let copied = copy_tree(src, &dst);
        if let Some(subject) = key.data_subject_id.as_deref()
            && self.erasure_receipt(subject)?.is_some()
        {
            if dst.exists() {
                std::fs::remove_dir_all(&dst)?;
            }
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "data subject has an erasure fence",
            ));
        }
        copied
    }
}

#[async_trait]
impl ContentEraser for FsSessionBlobStore {
    async fn erase_subject(&self, subject: &DataSubjectId) -> Result<usize, ErasureError> {
        let dir = self.subject_dir(Some(subject.as_str()));
        if let Some(receipt) = self
            .erasure_receipt(subject.as_str())
            .map_err(|error| ErasureError(error.to_string()))?
        {
            if dir.exists() {
                std::fs::remove_dir_all(&dir).map_err(|error| ErasureError(error.to_string()))?;
            }
            return Ok(receipt);
        }
        if !dir.exists() {
            self.persist_erasure_receipt(subject.as_str(), 0)
                .map_err(|error| ErasureError(error.to_string()))?;
            return Ok(0);
        }
        let records = std::fs::read_dir(&dir)
            .map_err(|error| ErasureError(error.to_string()))?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .flat_map(|thread| {
                std::fs::read_dir(thread.path())
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
            })
            .filter(|entry| entry.path().is_dir())
            .count();
        // Persist the receipt/fence before deletion. If deletion fails, a retry
        // resumes the delete and returns the original count; a late harvest is
        // rejected instead of resurrecting erased subject content.
        self.persist_erasure_receipt(subject.as_str(), records)
            .map_err(|error| ErasureError(error.to_string()))?;
        std::fs::remove_dir_all(&dir).map_err(|error| ErasureError(error.to_string()))?;
        Ok(records)
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
        if let Ok(dir) = self.session_dir(key, plan)
            && dir.is_dir()
        {
            let _ = self.blobs.store(key, &dir).await;
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
            data_subject_id: Some("dsub_alice".to_string()),
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
            data_subject_id: Some("dsub_alice".to_string()),
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

    /// Cause/effect graph:
    /// - C1: two subjects use the same thread+adapter; C2: only Alice is erased.
    /// - E1: their opaque blobs never collide; E2: Alice's exact subtree is
    ///   removed and counted; E3: retry replays the count and a late harvest is
    ///   fenced; E4: Bob's blob remains fetchable.
    /// Decision-table rule R1 = C1(true) × C2(Alice) -> E1+E2+E3+E4.
    /// Unknown-subject no-op is covered by `restore_of_an_unknown_session_is_a_no_op`.
    #[tokio::test]
    async fn subject_scopes_are_isolated_and_independently_erasable() {
        let root = tempfile::tempdir().unwrap();
        let alice_source = tempfile::tempdir().unwrap();
        let bob_source = tempfile::tempdir().unwrap();
        std::fs::write(alice_source.path().join("session"), b"alice").unwrap();
        std::fs::write(bob_source.path().join("session"), b"bob").unwrap();
        let store = FsSessionBlobStore::new(root.path().to_path_buf());
        let alice = SessionHomeKey {
            data_subject_id: Some("dsub_alice".into()),
            thread_id: "shared-thread".into(),
            adapter: "codex".into(),
        };
        let bob = SessionHomeKey {
            data_subject_id: Some("dsub_bob".into()),
            ..alice.clone()
        };
        store.store(&alice, alice_source.path()).await.unwrap();
        store.store(&bob, bob_source.path()).await.unwrap();

        let removed = store
            .erase_subject(&DataSubjectId("dsub_alice".into()))
            .await
            .unwrap();
        let alice_dest = tempfile::tempdir().unwrap();
        let bob_dest = tempfile::tempdir().unwrap();
        assert_eq!(removed, 1);
        assert_eq!(
            store
                .erase_subject(&DataSubjectId("dsub_alice".into()))
                .await
                .unwrap(),
            1,
            "retry replays the stable erasure receipt"
        );
        assert_eq!(
            store
                .store(&alice, alice_source.path())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied,
            "late harvest cannot resurrect erased content"
        );
        assert!(!store.fetch(&alice, alice_dest.path()).await.unwrap());
        assert!(store.fetch(&bob, bob_dest.path()).await.unwrap());
        assert_eq!(
            std::fs::read(bob_dest.path().join("session")).unwrap(),
            b"bob"
        );
    }
}
