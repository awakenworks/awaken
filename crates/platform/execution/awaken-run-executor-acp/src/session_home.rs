// @formal surface requirement=protocol.acp.acp_normalization
// @formal reason: This source implements the repository-controlled acp product boundary and is classified
// @formal reason: against its product requirement; this link is not a formal-proof claim.
//! Cross-directory / cross-machine recovery of a local-dir ACP CLI's session.
//!
//! A CLI like Claude Code or Codex keeps its conversation under a subtree of its
//! config home (`projects/`, `sessions/`). Our per-Run relaunch and distributed
//! dispatch mean a later Run may execute in a different directory — or on a different
//! worker — where that subtree is absent, so `session/load` finds nothing. This
//! module restores the thread's session subtree into the config home before a run
//! and harvests it back after, keyed by (thread, adapter), so the session survives
//! the move.
//!
//! Two boundaries make it safe: the harvest is scoped to the declared **session
//! subtree only** (the config home's credentials / local config at the root are
//! never carried into the portable blob), and it is **best-effort** — a missing or
//! failed blob just falls back to `session/new` + the neutral thread history, which
//! is always the authority. The durable backing is an injected `SessionBlobStore`;
//! a shared (content-addressed) one makes recovery cross-machine.
//!
//! `DirSessionHome` adapts the catalog's blob custodian
//! (`awaken_acp_catalog::SessionBlobStore`) to the executor's
//! `SessionHomeProvider` port and `ConfigHome` path convention; no second byte
//! store or recovery codec exists.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_acp_catalog::{SessionBlobStore, SessionHomeKey};

use crate::config_home::ConfigHome;
use crate::{SessionHomePlan, SessionHomeProvider};

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
            let _ = self.blobs.fetch(key, &dir, &plan.exclude).await;
        }
    }

    async fn harvest(&self, key: &SessionHomeKey, plan: &SessionHomePlan) {
        if let Ok(dir) = self.session_dir(key, plan)
            && dir.is_dir()
        {
            let _ = self.blobs.store(key, &dir, &plan.exclude).await;
        }
    }
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
        // Causes: the fixtures below establish `recovers a session across config homes and leaves
        // credentials behind` with the concrete inputs, state, dependencies, and failure triggers
        // used by this case.
        // Effects: the observable result `all output, state, side-effect, error, and terminal
        // assertions below hold together` and every asserted state transition or side effect must
        // hold.
        // Constraints/invariants: the current fenced attempt and committed context are
        // authoritative; remote protocol state cannot become a parallel Run or transcript truth.
        // Coverage rationale: `recovers a session across config homes and leaves credentials
        // behind` is one independent branch selecting `all output, state, side-effect, error, and
        // terminal assertions below hold together`; a multi-row decision table is not applicable,
        // and sibling tests own alternate causes.
        let store_a = tempfile::tempdir().unwrap();
        let store_b = tempfile::tempdir().unwrap();
        let blob_root = tempfile::tempdir().unwrap();
        let blobs = Arc::new(awaken_acp_catalog::FsSessionBlobStore::new(
            blob_root.path().to_path_buf(),
        ));
        let key = SessionHomeKey {
            data_subject_id: Some("dsub_alice".to_string()),
            thread_id: "t1".to_string(),
            adapter: "claude".to_string(),
        };

        // Config home A holds a session file (under the harvested subtree) and a
        // credential at the config-home root (outside it).
        let home_a = ConfigHome::open(Some(store_a.path()), "t1").unwrap();
        home_a
            .write("projects/conv/session.jsonl", b"run-1")
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
            Some(&b"run-1"[..]),
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
        let blobs = Arc::new(awaken_acp_catalog::FsSessionBlobStore::new(
            blob_root.path().to_path_buf(),
        ));
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
}
#[tokio::test]
async fn declared_opencode_database_moves_without_credential_or_local_config_files() {
    // Causes: H1 catalog-declared data subtree; H2 nested credential name;
    // H3 fresh destination; H4 stale blob contains credentials. Effects:
    // exact opaque DB moves, credentials do not store or restore, and the
    // existing DirSessionHome/BlobStore remain the only copy mechanism.
    // H1+H2+H3=>all effects; H4=>restore filtering still excludes auth.
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let blob_root = tempfile::tempdir().unwrap();
    let blobs = Arc::new(awaken_acp_catalog::FsSessionBlobStore::new(
        blob_root.path().into(),
    ));
    let cli = crate::acp_cli("opencode").unwrap();
    let crate::SessionPersistence::LocalDir {
        session_subpath, ..
    } = cli.session_persistence
    else {
        panic!("portable OpenCode data")
    };
    assert_eq!(session_subpath, "opencode", "pinned vendor data path");
    let plan = SessionHomePlan {
        config_home_env: "OPENCODE_CONFIG_DIR".into(),
        session_subpath: session_subpath.into(),
        exclude: cli
            .session_export_excludes
            .iter()
            .map(|p| (*p).into())
            .collect(),
        keyed_by_cwd: false,
    };
    let key = SessionHomeKey {
        data_subject_id: None,
        thread_id: "opencode-home".into(),
        adapter: "opencode".into(),
    };
    let home = ConfigHome::open(Some(a.path()), &key.thread_id).unwrap();
    home.write("opencode/opencode.db", b"\0opaque-session-db\xff")
        .unwrap();
    home.write("opencode/auth.json", b"fixture-private-auth")
        .unwrap();
    DirSessionHome::new(Some(a.path().into()), blobs.clone())
        .harvest(&key, &plan)
        .await;
    assert!(
        !blobs.key_dir(&key).join("auth.json").exists(),
        "H2 never stored"
    );
    std::fs::write(blobs.key_dir(&key).join("auth.json"), b"stale-private-auth").unwrap();
    DirSessionHome::new(Some(b.path().into()), blobs)
        .restore(&key, &plan)
        .await;
    let restored = ConfigHome::open(Some(b.path()), &key.thread_id).unwrap();
    assert_eq!(
        restored.read("opencode/opencode.db").unwrap().as_deref(),
        Some(&b"\0opaque-session-db\xff"[..])
    );
    assert!(
        restored.read("opencode/auth.json").unwrap().is_none(),
        "H4 not restored"
    );
}
