//! File-system storage backend.
//!
//! Layout:
//! ```text
//! <base_path>/
//!   threads/<thread_id>.json         — Thread
//!   messages/<thread_id>.json        — Vec<Message>
//!   runs/<run_id>.json               — RunRecord
//! ```
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use awaken_contract::contract::config_store::{ConfigStore, extract_meta_revision};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::profile_store::{ProfileEntry, ProfileOwner, ProfileStore};
use awaken_contract::contract::storage::{
    ChildThreadDeleteStrategy, MessagePage, MessageQuery, RunPage, RunQuery, RunRecord, RunStore,
    StorageError, ThreadPage, ThreadQuery, ThreadRunStore, ThreadStore,
    checkpoint_parent_thread_id, paginate_message_records, paginate_threads,
    sort_threads_by_recent_activity,
};
use awaken_contract::thread::{Thread, normalize_lineage_id};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// File-system storage backend.
pub struct FileStore {
    base_path: PathBuf,
    hierarchy_lock: Arc<Mutex<()>>,
    /// In-process mutex serialising config CAS (read-check-write) operations.
    ///
    /// FileStore's CAS is process-local; cross-process atomicity is not
    /// guaranteed. For multi-process deployments use PostgresStore.
    config_cas_lock: Arc<Mutex<()>>,
}

impl FileStore {
    /// Create a new file store rooted at `base_path`.
    ///
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        let base_path = base_path.into();
        if let Err(error) = recover_checkpoint_journal_sync(&base_path) {
            tracing::warn!(
                path = %base_path.display(),
                error = %error,
                "failed to recover incomplete file-store checkpoint journal"
            );
        }
        cleanup_orphan_checkpoint_backups_sync(&base_path);
        Self {
            hierarchy_lock: shared_hierarchy_lock(&base_path),
            config_cas_lock: shared_config_cas_lock(&base_path),
            base_path,
        }
    }

    fn threads_dir(&self) -> PathBuf {
        self.base_path.join("threads")
    }

    fn messages_dir(&self) -> PathBuf {
        self.base_path.join("messages")
    }

    fn runs_dir(&self) -> PathBuf {
        self.base_path.join("runs")
    }

    fn profiles_dir(&self) -> PathBuf {
        self.base_path.join("profiles")
    }

    fn thread_path(&self, thread_id: &str) -> PathBuf {
        self.threads_dir().join(format!("{thread_id}.json"))
    }

    fn messages_path(&self, thread_id: &str) -> PathBuf {
        self.messages_dir().join(format!("{thread_id}.json"))
    }

    fn config_dir(&self, namespace: &str) -> PathBuf {
        self.base_path.join("config").join(namespace)
    }

    async fn delete_thread_with_strategy_locked(
        &self,
        thread_id: &str,
        strategy: ChildThreadDeleteStrategy,
    ) -> Result<(), StorageError> {
        if self.load_thread(thread_id).await?.is_none() {
            return Err(StorageError::NotFound(thread_id.to_owned()));
        }

        let mut ops = Vec::new();
        match strategy {
            ChildThreadDeleteStrategy::Reject => {
                let children = self.list_child_threads(thread_id).await?;
                if !children.is_empty() {
                    return Err(StorageError::Validation(format!(
                        "thread '{thread_id}' has child threads; choose 'detach' or 'cascade'"
                    )));
                }
            }
            ChildThreadDeleteStrategy::Detach => {
                let mut children = self.list_child_threads(thread_id).await?;
                let updated_at = current_millis();
                for child in &mut children {
                    child.parent_thread_id = None;
                    child.normalize_lineage();
                    child.touch(updated_at);
                    let payload = serde_json::to_string_pretty(child)
                        .map_err(|e| StorageError::Serialization(e.to_string()))?;
                    let staged = match stage_write(
                        &self.threads_dir(),
                        &format!("{}.json", child.id),
                        &payload,
                    )
                    .await
                    {
                        Ok(staged) => staged,
                        Err(error) => {
                            cleanup_staged_file_ops(&ops).await;
                            return Err(error);
                        }
                    };
                    ops.push(StagedFileOp::Write(staged));
                }
            }
            ChildThreadDeleteStrategy::Cascade => {
                let mut visited = std::collections::HashSet::new();
                let mut stack = vec![(thread_id.to_owned(), false)];
                let mut delete_order = Vec::new();

                while let Some((current_thread_id, expanded)) = stack.pop() {
                    if expanded {
                        delete_order.push(current_thread_id);
                        continue;
                    }

                    if !visited.insert(current_thread_id.clone()) {
                        return Err(StorageError::Validation(format!(
                            "thread hierarchy cycle detected while deleting '{thread_id}'"
                        )));
                    }

                    stack.push((current_thread_id.clone(), true));
                    let mut children = self.list_child_threads(&current_thread_id).await?;
                    children.sort_by(|left, right| left.id.cmp(&right.id));
                    for child in children.into_iter().rev() {
                        stack.push((child.id, false));
                    }
                }

                for id in delete_order {
                    ops.push(StagedFileOp::Delete(stage_delete(self.thread_path(&id))?));
                    ops.push(StagedFileOp::Delete(stage_delete(self.messages_path(&id))?));
                }
            }
        }

        if !matches!(strategy, ChildThreadDeleteStrategy::Cascade) {
            ops.push(StagedFileOp::Delete(stage_delete(
                self.thread_path(thread_id),
            )?));
            ops.push(StagedFileOp::Delete(stage_delete(
                self.messages_path(thread_id),
            )?));
        }

        if let Err(error) = commit_staged_file_ops(&self.base_path, &ops).await {
            cleanup_staged_file_ops(&ops).await;
            return Err(error);
        }

        Ok(())
    }

    async fn checkpoint_locked(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        let now = current_millis();
        let mut thread = self
            .load_thread(thread_id)
            .await?
            .unwrap_or_else(|| Thread::with_id(thread_id));
        self.validate_thread_hierarchy(thread_id, checkpoint_parent_thread_id(Some(&thread), run))
            .await?;
        thread.touch(now);
        thread.apply_run_projection(run);
        thread.normalize_lineage();

        let thread_payload = serde_json::to_string_pretty(&thread)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let msg_payload = serde_json::to_string_pretty(messages)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let run_payload = serde_json::to_string_pretty(run)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        let thread_file = &format!("{thread_id}.json");
        let run_file = &format!("{}.json", run.run_id);

        let staged_thread = stage_write(&self.threads_dir(), thread_file, &thread_payload).await?;
        let staged_msgs = match stage_write(&self.messages_dir(), thread_file, &msg_payload).await {
            Ok(staged) => staged,
            Err(error) => {
                cleanup_staged_writes(&[staged_thread]).await;
                return Err(error);
            }
        };
        let staged_run = match stage_write(&self.runs_dir(), run_file, &run_payload).await {
            Ok(staged) => staged,
            Err(error) => {
                cleanup_staged_writes(&[staged_thread, staged_msgs]).await;
                return Err(error);
            }
        };

        let writes = [staged_thread, staged_msgs, staged_run];
        if let Err(error) = commit_staged_writes(&self.base_path, &writes).await {
            cleanup_staged_writes(&writes).await;
            return Err(error);
        }

        Ok(())
    }
}

fn shared_config_cas_lock(base_path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();

    let key = hierarchy_lock_key(base_path);
    let locks = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.retain(|_, lock| lock.strong_count() > 0);

    if let Some(lock) = guard.get(&key).and_then(Weak::upgrade) {
        return lock;
    }

    let lock = Arc::new(Mutex::new(()));
    guard.insert(key, Arc::downgrade(&lock));
    lock
}

fn shared_hierarchy_lock(base_path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();

    let key = hierarchy_lock_key(base_path);
    let locks = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.retain(|_, lock| lock.strong_count() > 0);

    if let Some(lock) = guard.get(&key).and_then(Weak::upgrade) {
        return lock;
    }

    let lock = Arc::new(Mutex::new(()));
    guard.insert(key, Arc::downgrade(&lock));
    lock
}

fn hierarchy_lock_key(base_path: &Path) -> String {
    let absolute = if base_path.is_absolute() {
        base_path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(base_path)
    };

    let (existing_ancestor, canonical_ancestor) = absolute
        .ancestors()
        .find_map(|ancestor| {
            std::fs::canonicalize(ancestor)
                .ok()
                .map(|path| (ancestor, path))
        })
        .unwrap_or_else(|| (Path::new(""), PathBuf::new()));
    let remainder = absolute
        .strip_prefix(existing_ancestor)
        .unwrap_or_else(|_| Path::new(""));

    normalize_path_components(canonical_ancestor, remainder)
        .to_string_lossy()
        .into_owned()
}

fn normalize_path_components(mut base: PathBuf, suffix: &Path) -> PathBuf {
    for component in suffix.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                base.pop();
            }
            Component::Normal(segment) => base.push(segment),
            Component::RootDir => base.push(component.as_os_str()),
            Component::Prefix(prefix) => base.push(prefix.as_os_str()),
        }
    }
    base
}

// ── Filesystem helpers ──────────────────────────────────────────────
pub(crate) fn validate_id(id: &str, label: &str) -> Result<(), StorageError> {
    if id.trim().is_empty() {
        return Err(StorageError::Io(format!("{label} cannot be empty")));
    }
    if id.contains('/')
        || id.contains('\\')
        || id.contains("..")
        || id.contains('\0')
        || id.chars().any(|c| c.is_control())
    {
        return Err(StorageError::Io(format!(
            "{label} contains invalid characters: {id:?}"
        )));
    }
    Ok(())
}
pub(crate) async fn atomic_write(
    dir: &Path,
    filename: &str,
    content: &str,
) -> Result<(), StorageError> {
    if !dir.exists() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
    }
    let target = dir.join(filename);
    let tmp_path = dir.join(format!(
        ".{}.{}.tmp",
        filename.trim_end_matches(".json"),
        uuid::Uuid::now_v7().simple()
    ));
    let write_result = async {
        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        file.write_all(content.as_bytes())
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        file.flush()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        file.sync_all()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        drop(file);
        tokio::fs::rename(&tmp_path, &target)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        // Sync parent directory to ensure rename is durable on Linux ext4/XFS
        dir_fsync(dir).await?;
        Ok::<(), StorageError>(())
    }
    .await;

    if let Err(e) = write_result {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(e);
    }
    Ok(())
}
/// Like [`atomic_write`] but fails with [`StorageError::AlreadyExists`] if the
/// target file already exists, using `O_CREAT | O_EXCL` to avoid TOCTOU races.
async fn atomic_write_exclusive(
    dir: &Path,
    filename: &str,
    content: &str,
    exists_id: &str,
) -> Result<(), StorageError> {
    if !dir.exists() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
    }

    let target = dir.join(filename);

    // Atomically claim the target path — fails if another writer got there first.
    let lock_result = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .await;

    match lock_result {
        Ok(_lock_file) => { /* drop immediately; we'll overwrite via rename */ }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(StorageError::AlreadyExists(exists_id.to_owned()));
        }
        Err(e) => return Err(StorageError::Io(e.to_string())),
    }

    // Write to a temp file and rename over the lock file.
    let tmp_path = dir.join(format!(
        ".{}.{}.tmp",
        filename.trim_end_matches(".json"),
        uuid::Uuid::now_v7().simple()
    ));

    let write_result = async {
        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        file.write_all(content.as_bytes())
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        file.flush()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        file.sync_all()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        drop(file);
        tokio::fs::rename(&tmp_path, &target)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        dir_fsync(dir).await?;
        Ok::<(), StorageError>(())
    }
    .await;

    if let Err(e) = write_result {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        // Also clean up the lock file we created
        let _ = tokio::fs::remove_file(&target).await;
        return Err(e);
    }
    Ok(())
}

/// Fsync a directory to ensure metadata (renames) are durable.
async fn dir_fsync(dir: &Path) -> Result<(), StorageError> {
    let dir_file = tokio::fs::File::open(dir)
        .await
        .map_err(|e| StorageError::Io(e.to_string()))?;
    dir_file
        .sync_all()
        .await
        .map_err(|e| StorageError::Io(e.to_string()))?;
    Ok(())
}

/// A prepared (but not yet committed) temp file, ready to be renamed into place.
#[derive(Debug, Clone)]
struct StagedWrite {
    tmp_path: PathBuf,
    target: PathBuf,
    dir: PathBuf,
}

/// A prepared delete operation, ready to atomically remove a target file.
#[derive(Debug, Clone)]
struct StagedDelete {
    target: PathBuf,
    dir: PathBuf,
}

#[derive(Debug, Clone)]
enum StagedFileOp {
    Write(StagedWrite),
    Delete(StagedDelete),
}

#[derive(Debug, Serialize, Deserialize)]
struct CheckpointJournal {
    writes: Vec<CheckpointJournalWrite>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CheckpointJournalWrite {
    target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tmp: Option<String>,
    backup: String,
    had_target: bool,
}

fn checkpoint_marker_path(base_dir: &Path) -> PathBuf {
    base_dir.join(".checkpoint_pending")
}

fn checkpoint_backup_path(target: &Path, tx_id: &str) -> PathBuf {
    let filename = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("checkpoint");
    target.with_file_name(format!(".{filename}.{tx_id}.bak"))
}

fn rel_path(base_dir: &Path, path: &Path) -> Result<String, StorageError> {
    path.strip_prefix(base_dir)
        .map_err(|e| StorageError::Io(format!("checkpoint path outside base dir: {e}")))?
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| StorageError::Io("checkpoint path is not valid UTF-8".into()))
}

fn join_rel(base_dir: &Path, rel: &str) -> PathBuf {
    base_dir.join(rel)
}

fn dir_fsync_sync(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

fn recover_checkpoint_journal_sync(base_dir: &Path) -> Result<(), StorageError> {
    let marker = checkpoint_marker_path(base_dir);
    if !marker.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&marker).map_err(|e| StorageError::Io(e.to_string()))?;
    let journal: CheckpointJournal = match serde_json::from_str(&content) {
        Ok(journal) => journal,
        Err(error) => {
            tracing::warn!(
                path = %marker.display(),
                error = %error,
                "removing legacy or unreadable file-store checkpoint marker"
            );
            let _ = std::fs::remove_file(&marker);
            let _ = dir_fsync_sync(base_dir);
            return Ok(());
        }
    };

    for write in journal.writes.iter().rev() {
        let target = join_rel(base_dir, &write.target);
        let tmp = write.tmp.as_deref().map(|tmp| join_rel(base_dir, tmp));
        let backup = join_rel(base_dir, &write.backup);

        if write.had_target && backup.exists() {
            if target.exists() {
                std::fs::remove_file(&target).map_err(|e| StorageError::Io(e.to_string()))?;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| StorageError::Io(e.to_string()))?;
            }
            std::fs::rename(&backup, &target).map_err(|e| StorageError::Io(e.to_string()))?;
        } else if !write.had_target && target.exists() {
            std::fs::remove_file(&target).map_err(|e| StorageError::Io(e.to_string()))?;
        } else if backup.exists() {
            std::fs::remove_file(&backup).map_err(|e| StorageError::Io(e.to_string()))?;
        }
        if let Some(tmp) = tmp.as_ref()
            && tmp.exists()
        {
            std::fs::remove_file(tmp).map_err(|e| StorageError::Io(e.to_string()))?;
        }
        if let Some(parent) = target.parent() {
            let _ = dir_fsync_sync(parent);
        }
    }

    std::fs::remove_file(&marker).map_err(|e| StorageError::Io(e.to_string()))?;
    let _ = dir_fsync_sync(base_dir);
    Ok(())
}

fn cleanup_orphan_checkpoint_backups_sync(base_dir: &Path) {
    for subdir in ["threads", "messages", "runs"] {
        let dir = base_dir.join(subdir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with('.') && name.ends_with(".bak") {
                let _ = std::fs::remove_file(path);
            }
        }
        let _ = dir_fsync_sync(&dir);
    }
}

/// Write content to a temp file in `dir`, fsync it, and return the staged write
/// without performing the rename. The caller is responsible for calling
/// [`commit_staged_writes`] to atomically install all staged files.
async fn stage_write(
    dir: &Path,
    filename: &str,
    content: &str,
) -> Result<StagedWrite, StorageError> {
    if !dir.exists() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
    }
    let target = dir.join(filename);
    let tmp_path = dir.join(format!(
        ".{}.{}.tmp",
        filename.trim_end_matches(".json"),
        uuid::Uuid::now_v7().simple()
    ));
    let write_result = async {
        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        file.write_all(content.as_bytes())
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        file.flush()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        file.sync_all()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        drop(file);
        Ok::<(), StorageError>(())
    }
    .await;
    if let Err(error) = write_result {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(error);
    }
    Ok(StagedWrite {
        tmp_path,
        target,
        dir: dir.to_path_buf(),
    })
}

fn stage_delete(target: PathBuf) -> Result<StagedDelete, StorageError> {
    let dir = target
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| StorageError::Io("delete target must have a parent directory".into()))?;
    Ok(StagedDelete { target, dir })
}

fn staged_op_target(op: &StagedFileOp) -> &Path {
    match op {
        StagedFileOp::Write(write) => &write.target,
        StagedFileOp::Delete(delete) => &delete.target,
    }
}

fn staged_op_tmp(op: &StagedFileOp) -> Option<&Path> {
    match op {
        StagedFileOp::Write(write) => Some(&write.tmp_path),
        StagedFileOp::Delete(_) => None,
    }
}

fn staged_op_dir(op: &StagedFileOp) -> &Path {
    match op {
        StagedFileOp::Write(write) => &write.dir,
        StagedFileOp::Delete(delete) => &delete.dir,
    }
}

/// Rename all staged temp files into their targets and fsync each parent dir.
async fn commit_staged_writes(base_dir: &Path, writes: &[StagedWrite]) -> Result<(), StorageError> {
    let ops: Vec<StagedFileOp> = writes.iter().cloned().map(StagedFileOp::Write).collect();
    commit_staged_file_ops(base_dir, &ops).await
}

/// Rename staged temp files and/or remove staged delete targets atomically.
async fn commit_staged_file_ops(base_dir: &Path, ops: &[StagedFileOp]) -> Result<(), StorageError> {
    tokio::fs::create_dir_all(base_dir)
        .await
        .map_err(|e| StorageError::Io(e.to_string()))?;
    recover_checkpoint_journal_sync(base_dir)?;

    let tx_id = uuid::Uuid::now_v7().simple().to_string();
    let marker = checkpoint_marker_path(base_dir);
    let mut journal_writes = Vec::with_capacity(ops.len());
    for op in ops {
        let target = staged_op_target(op);
        let backup = checkpoint_backup_path(target, &tx_id);
        journal_writes.push(CheckpointJournalWrite {
            target: rel_path(base_dir, target)?,
            tmp: staged_op_tmp(op)
                .map(|tmp| rel_path(base_dir, tmp))
                .transpose()?,
            backup: rel_path(base_dir, &backup)?,
            had_target: target.exists(),
        });
    }
    let journal = CheckpointJournal {
        writes: journal_writes,
    };
    let marker_payload = serde_json::to_vec_pretty(&journal)
        .map_err(|e| StorageError::Serialization(e.to_string()))?;
    tokio::fs::write(&marker, marker_payload)
        .await
        .map_err(|e| StorageError::Io(e.to_string()))?;
    dir_fsync(base_dir).await?;

    let mut synced_dirs = std::collections::HashSet::new();

    let commit_result = async {
        for (op, journal_write) in ops.iter().zip(journal.writes.iter()) {
            let target = staged_op_target(op);
            let backup = join_rel(base_dir, &journal_write.backup);
            if journal_write.had_target {
                tokio::fs::rename(target, &backup)
                    .await
                    .map_err(|e| StorageError::Io(e.to_string()))?;
            }
            if let Some(tmp_path) = staged_op_tmp(op) {
                tokio::fs::rename(tmp_path, target)
                    .await
                    .map_err(|e| StorageError::Io(e.to_string()))?;
            }
            if journal_write.had_target || staged_op_tmp(op).is_some() {
                synced_dirs.insert(staged_op_dir(op).to_path_buf());
            }
        }

        for dir in &synced_dirs {
            dir_fsync(dir).await?;
        }

        tokio::fs::remove_file(&marker)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        dir_fsync(base_dir).await?;

        for journal_write in &journal.writes {
            let backup = join_rel(base_dir, &journal_write.backup);
            let _ = tokio::fs::remove_file(&backup).await;
        }
        for dir in &synced_dirs {
            let _ = dir_fsync(dir).await;
        }
        Ok::<(), StorageError>(())
    }
    .await;

    if let Err(error) = commit_result {
        if let Err(recovery_error) = recover_checkpoint_journal_sync(base_dir) {
            tracing::warn!(error = %recovery_error, "failed to roll back incomplete checkpoint");
        }
        return Err(error);
    }

    Ok(())
}

/// Clean up staged temp files on error.
async fn cleanup_staged_writes(writes: &[StagedWrite]) {
    let ops: Vec<StagedFileOp> = writes.iter().cloned().map(StagedFileOp::Write).collect();
    cleanup_staged_file_ops(&ops).await;
}

/// Clean up staged file operations on error.
async fn cleanup_staged_file_ops(ops: &[StagedFileOp]) {
    for op in ops {
        if let Some(tmp_path) = staged_op_tmp(op) {
            let _ = tokio::fs::remove_file(tmp_path).await;
        }
    }
}
pub(crate) async fn read_json<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<Option<T>, StorageError> {
    if !path.exists() {
        return Ok(None);
    }
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| StorageError::Io(e.to_string()))?;
    let value =
        serde_json::from_str(&content).map_err(|e| StorageError::Serialization(e.to_string()))?;
    Ok(Some(value))
}

async fn scan_json_dir<T: serde::de::DeserializeOwned>(dir: &Path) -> Result<Vec<T>, StorageError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| StorageError::Io(e.to_string()))?;
    let mut results = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| StorageError::Io(e.to_string()))?
    {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let value: T = serde_json::from_str(&content)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        results.push(value);
    }
    Ok(results)
}

// ── ThreadStore ─────────────────────────────────────────────────────

#[async_trait]
impl ThreadStore for FileStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        validate_id(thread_id, "thread id")?;
        read_json(&self.thread_path(thread_id)).await
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        validate_id(&thread.id, "thread id")?;
        let mut normalized = thread.clone();
        normalized.normalize_lineage();
        let payload = serde_json::to_string_pretty(&normalized)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        atomic_write(
            &self.threads_dir(),
            &format!("{}.json", thread.id),
            &payload,
        )
        .await
    }

    async fn save_thread_validated(&self, thread: &Thread) -> Result<(), StorageError> {
        validate_id(&thread.id, "thread id")?;
        let _guard = self.hierarchy_lock.lock().await;
        self.validate_thread_hierarchy(&thread.id, thread.parent_thread_id.as_deref())
            .await?;
        self.save_thread(thread).await
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        validate_id(thread_id, "thread id")?;
        let thread_path = self.threads_dir().join(format!("{thread_id}.json"));
        let messages_path = self.messages_dir().join(format!("{thread_id}.json"));
        // Remove thread file (ignore not-found)
        if thread_path.exists() {
            tokio::fs::remove_file(&thread_path)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        // Remove messages file (ignore not-found)
        if messages_path.exists() {
            tokio::fs::remove_file(&messages_path)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn delete_thread_with_strategy(
        &self,
        thread_id: &str,
        strategy: ChildThreadDeleteStrategy,
    ) -> Result<(), StorageError> {
        validate_id(thread_id, "thread id")?;
        let _guard = self.hierarchy_lock.lock().await;
        self.delete_thread_with_strategy_locked(thread_id, strategy)
            .await
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        let mut threads: Vec<Thread> = scan_json_dir(&self.threads_dir()).await?;
        awaken_contract::contract::storage::sort_threads_by_recent_activity(&mut threads);
        Ok(threads
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|thread| thread.id)
            .collect())
    }

    async fn list_threads_query(&self, query: &ThreadQuery) -> Result<ThreadPage, StorageError> {
        let query = query.normalized();
        let threads: Vec<Thread> = scan_json_dir(&self.threads_dir()).await?;
        Ok(paginate_threads(threads, &query))
    }

    async fn list_child_threads(
        &self,
        parent_thread_id: &str,
    ) -> Result<Vec<Thread>, StorageError> {
        validate_id(parent_thread_id, "parent thread id")?;
        let Some(parent_thread_id) = normalize_lineage_id(Some(parent_thread_id)) else {
            return Ok(Vec::new());
        };
        let mut children: Vec<Thread> = scan_json_dir::<Thread>(&self.threads_dir())
            .await?
            .into_iter()
            .filter(|thread| thread.parent_thread_id.as_deref() == Some(parent_thread_id.as_str()))
            .collect();
        sort_threads_by_recent_activity(&mut children);
        Ok(children)
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        validate_id(thread_id, "thread id")?;
        let path = self.messages_dir().join(format!("{thread_id}.json"));
        read_json(&path).await
    }

    async fn list_message_records(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, StorageError> {
        validate_id(thread_id, "thread id")?;
        let Some(messages) = self.load_messages(thread_id).await? else {
            return Ok(MessagePage::empty());
        };
        let records = messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| {
                awaken_contract::contract::message::MessageRecord::from_message(
                    thread_id.to_owned(),
                    index as u64 + 1,
                    message,
                )
            })
            .collect();
        Ok(paginate_message_records(records, query))
    }

    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        validate_id(thread_id, "thread id")?;
        let payload = serde_json::to_string_pretty(messages)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        atomic_write(&self.messages_dir(), &format!("{thread_id}.json"), &payload).await
    }

    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        validate_id(thread_id, "thread id")?;
        let thread_path = self.threads_dir().join(format!("{thread_id}.json"));
        if !thread_path.exists() {
            return Err(StorageError::NotFound(thread_id.to_owned()));
        }
        let msg_path = self.messages_dir().join(format!("{thread_id}.json"));
        if msg_path.exists() {
            tokio::fs::remove_file(&msg_path)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: awaken_contract::thread::ThreadMetadata,
    ) -> Result<(), StorageError> {
        validate_id(id, "thread id")?;
        let path = self.threads_dir().join(format!("{id}.json"));
        let mut thread: Thread = read_json(&path)
            .await?
            .ok_or_else(|| StorageError::NotFound(id.to_owned()))?;
        thread.metadata = metadata;
        let payload = serde_json::to_string_pretty(&thread)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        atomic_write(&self.threads_dir(), &format!("{id}.json"), &payload).await
    }
}

// ── RunStore ────────────────────────────────────────────────────────

#[async_trait]
impl RunStore for FileStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        validate_id(&record.run_id, "run id")?;
        let payload = serde_json::to_string_pretty(record)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        atomic_write_exclusive(
            &self.runs_dir(),
            &format!("{}.json", record.run_id),
            &payload,
            &record.run_id,
        )
        .await
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        validate_id(run_id, "run id")?;
        let path = self.runs_dir().join(format!("{run_id}.json"));
        read_json(&path).await
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        let records: Vec<RunRecord> = scan_json_dir(&self.runs_dir()).await?;
        Ok(records
            .into_iter()
            .filter(|r| r.thread_id == thread_id)
            .max_by_key(|r| r.updated_at))
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        let records: Vec<RunRecord> = scan_json_dir(&self.runs_dir()).await?;
        let mut filtered: Vec<RunRecord> = records
            .into_iter()
            .filter(|r| query.thread_id.as_deref().is_none_or(|t| r.thread_id == t))
            .filter(|r| query.status.is_none_or(|s| r.status == s))
            .collect();
        filtered.sort_by_key(|r| r.created_at);
        let total = filtered.len();
        let offset = query.offset.min(total);
        let limit = query.limit.clamp(1, 200);
        let items: Vec<RunRecord> = filtered.into_iter().skip(offset).take(limit).collect();
        let has_more = offset + items.len() < total;
        Ok(RunPage {
            items,
            total,
            has_more,
        })
    }
}

// ── ProfileStore ────────────────────────────────────────────────────

/// Sanitize an agent ID for use as a directory name.
fn sanitize_id_for_dir(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn owner_dir_name(owner: &ProfileOwner) -> String {
    match owner {
        ProfileOwner::Agent(id) => format!("agent_{}", sanitize_id_for_dir(id)),
        ProfileOwner::System => "system".to_string(),
    }
}

use crate::current_millis;

#[async_trait]
impl ProfileStore for FileStore {
    async fn get(
        &self,
        owner: &ProfileOwner,
        key: &str,
    ) -> Result<Option<ProfileEntry>, StorageError> {
        let dir = self.profiles_dir().join(owner_dir_name(owner));
        let path = dir.join(format!("{key}.json"));
        read_json(&path).await
    }

    async fn set(
        &self,
        owner: &ProfileOwner,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), StorageError> {
        let dir = self.profiles_dir().join(owner_dir_name(owner));
        let entry = ProfileEntry {
            key: key.to_owned(),
            value,
            updated_at: current_millis(),
        };
        let payload = serde_json::to_string_pretty(&entry)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        atomic_write(&dir, &format!("{key}.json"), &payload).await
    }

    async fn delete(&self, owner: &ProfileOwner, key: &str) -> Result<(), StorageError> {
        let dir = self.profiles_dir().join(owner_dir_name(owner));
        let path = dir.join(format!("{key}.json"));
        if path.exists() {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn list(&self, owner: &ProfileOwner) -> Result<Vec<ProfileEntry>, StorageError> {
        let dir = self.profiles_dir().join(owner_dir_name(owner));
        let mut entries: Vec<ProfileEntry> = scan_json_dir(&dir).await?;
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(entries)
    }

    async fn clear_owner(&self, owner: &ProfileOwner) -> Result<(), StorageError> {
        let dir = self.profiles_dir().join(owner_dir_name(owner));
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        Ok(())
    }
}

// ── ConfigStore ─────────────────────────────────────────────────────

#[async_trait]
impl ConfigStore for FileStore {
    async fn get(
        &self,
        namespace: &str,
        id: &str,
    ) -> Result<Option<serde_json::Value>, StorageError> {
        validate_id(namespace, "config namespace")?;
        validate_id(id, "config id")?;
        let path = self.config_dir(namespace).join(format!("{id}.json"));
        read_json(&path).await
    }

    async fn list(
        &self,
        namespace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, serde_json::Value)>, StorageError> {
        validate_id(namespace, "config namespace")?;
        let dir = self.config_dir(namespace);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut read_dir = tokio::fs::read_dir(&dir)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        let mut items = Vec::new();
        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?
        {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Some(value) = read_json(&path).await? else {
                continue;
            };
            items.push((stem.to_string(), value));
        }

        items.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(items.into_iter().skip(offset).take(limit).collect())
    }

    async fn put(
        &self,
        namespace: &str,
        id: &str,
        value: &serde_json::Value,
    ) -> Result<(), StorageError> {
        validate_id(namespace, "config namespace")?;
        validate_id(id, "config id")?;
        let _guard = self.config_cas_lock.lock().await;
        let payload = serde_json::to_string_pretty(value)
            .map_err(|error| StorageError::Serialization(error.to_string()))?;
        atomic_write(&self.config_dir(namespace), &format!("{id}.json"), &payload).await
    }

    async fn put_if_absent(
        &self,
        namespace: &str,
        id: &str,
        value: &serde_json::Value,
    ) -> Result<(), StorageError> {
        validate_id(namespace, "config namespace")?;
        validate_id(id, "config id")?;
        let _guard = self.config_cas_lock.lock().await;
        let payload = serde_json::to_string_pretty(value)
            .map_err(|error| StorageError::Serialization(error.to_string()))?;
        atomic_write_exclusive(
            &self.config_dir(namespace),
            &format!("{id}.json"),
            &payload,
            &format!("{namespace}/{id}"),
        )
        .await
    }

    async fn delete(&self, namespace: &str, id: &str) -> Result<(), StorageError> {
        validate_id(namespace, "config namespace")?;
        validate_id(id, "config id")?;
        let _guard = self.config_cas_lock.lock().await;
        let path = self.config_dir(namespace).join(format!("{id}.json"));
        if path.exists() {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|error| StorageError::Io(error.to_string()))?;
        }
        Ok(())
    }

    /// Atomic compare-and-set for the config revision field.
    ///
    /// Atomicity is in-process only (via `config_cas_lock`). Cross-process
    /// writers to the same file-system path are not protected. For multi-process
    /// deployments use `PostgresStore` which uses `SELECT FOR UPDATE`.
    async fn put_if_revision(
        &self,
        namespace: &str,
        id: &str,
        value: &serde_json::Value,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        validate_id(namespace, "config namespace")?;
        validate_id(id, "config id")?;
        let _guard = self.config_cas_lock.lock().await;

        // Re-read under the lock to avoid TOCTOU.
        let path = self.config_dir(namespace).join(format!("{id}.json"));
        let existing: Option<serde_json::Value> = read_json(&path).await?;
        let actual = existing
            .as_ref()
            .and_then(extract_meta_revision)
            .unwrap_or(0);
        if actual != expected_revision {
            return Err(StorageError::VersionConflict {
                expected: expected_revision,
                actual,
            });
        }

        let payload = serde_json::to_string_pretty(value)
            .map_err(|error| StorageError::Serialization(error.to_string()))?;
        atomic_write(&self.config_dir(namespace), &format!("{id}.json"), &payload).await
    }

    async fn delete_if_revision(
        &self,
        namespace: &str,
        id: &str,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        validate_id(namespace, "config namespace")?;
        validate_id(id, "config id")?;
        let _guard = self.config_cas_lock.lock().await;
        let path = self.config_dir(namespace).join(format!("{id}.json"));
        let existing: Option<serde_json::Value> = read_json(&path).await?;
        let actual = existing
            .as_ref()
            .and_then(extract_meta_revision)
            .unwrap_or(0);
        if actual != expected_revision {
            return Err(StorageError::VersionConflict {
                expected: expected_revision,
                actual,
            });
        }
        if path.exists() {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|error| StorageError::Io(error.to_string()))?;
        }
        Ok(())
    }
}

// ── ThreadRunStore ──────────────────────────────────────────────────

#[async_trait]
impl ThreadRunStore for FileStore {
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        validate_id(thread_id, "thread id")?;
        validate_id(&run.run_id, "run id")?;
        let _guard = self.hierarchy_lock.lock().await;
        self.checkpoint_locked(thread_id, messages, run).await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::message::Message;
    use awaken_contract::contract::storage::{
        ChildThreadDeleteStrategy, RunRecord, RunStore, ThreadRunStore, ThreadStore,
    };
    use awaken_contract::thread::Thread;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Barrier;
    use tokio::time::{Duration, sleep};

    fn make_run(run_id: &str, thread_id: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.to_string(),
            thread_id: thread_id.to_string(),
            agent_id: "agent".to_string(),
            parent_run_id: None,
            registry_manifest: None,
            activation: None,
            request: None,
            input: None,
            output: None,
            status: RunStatus::Running,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: None,
            outcome: None,
            created_at: 100,
            started_at: None,
            finished_at: None,
            updated_at: 100,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        }
    }

    // ── validate_id ──

    #[test]
    fn validate_id_rejects_slash() {
        assert!(validate_id("a/b", "id").is_err());
    }

    #[test]
    fn validate_id_rejects_backslash() {
        assert!(validate_id("a\\b", "id").is_err());
    }

    #[test]
    fn validate_id_rejects_null_char() {
        assert!(validate_id("a\0b", "id").is_err());
    }

    #[test]
    fn validate_id_rejects_dot_dot() {
        assert!(validate_id("a..b", "id").is_err());
    }

    #[test]
    fn validate_id_rejects_empty() {
        assert!(validate_id("", "id").is_err());
        assert!(validate_id("  ", "id").is_err());
    }

    #[test]
    fn validate_id_rejects_control_chars() {
        assert!(validate_id("a\tb", "id").is_err());
        assert!(validate_id("a\nb", "id").is_err());
    }

    #[test]
    fn validate_id_accepts_valid() {
        assert!(validate_id("abc-123", "id").is_ok());
        assert!(validate_id("thread_001", "id").is_ok());
    }

    // ── atomic_write ──

    #[tokio::test]
    async fn atomic_write_creates_parent_dirs() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("deep").join("nested");
        atomic_write(&dir, "test.json", r#"{"ok": true}"#)
            .await
            .unwrap();
        assert!(dir.join("test.json").exists());
    }

    #[tokio::test]
    async fn atomic_write_overwrites_existing() {
        let td = TempDir::new().unwrap();
        let dir = td.path().to_path_buf();
        atomic_write(&dir, "test.json", r#"{"v": 1}"#)
            .await
            .unwrap();
        atomic_write(&dir, "test.json", r#"{"v": 2}"#)
            .await
            .unwrap();
        let content = tokio::fs::read_to_string(dir.join("test.json"))
            .await
            .unwrap();
        assert!(content.contains("\"v\": 2"));
    }

    // ── Corrupted JSON handling ──

    #[tokio::test]
    async fn read_json_returns_error_for_corrupted_json() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("bad.json");
        tokio::fs::write(&path, "not valid json{{{").await.unwrap();
        let result: Result<Option<Thread>, StorageError> = read_json(&path).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            StorageError::Serialization(_)
        ));
    }

    #[tokio::test]
    async fn read_json_returns_none_for_missing_file() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("nonexistent.json");
        let result: Result<Option<Thread>, StorageError> = read_json(&path).await;
        assert!(result.unwrap().is_none());
    }

    // ── FileStore::new ──

    #[test]
    fn file_store_new_does_not_create_dirs_eagerly() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("store");
        let _store = FileStore::new(&path);
        // Dirs are NOT created at construction time
        assert!(!path.exists());
    }

    // ── ThreadStore ──

    #[tokio::test]
    async fn file_store_thread_save_load_delete() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let thread = Thread::new();
        store.save_thread(&thread).await.unwrap();

        let loaded = store.load_thread(&thread.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, thread.id);

        store.delete_thread(&thread.id).await.unwrap();
        assert!(store.load_thread(&thread.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn file_store_save_thread_normalizes_lineage() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let mut thread = Thread::with_id("t-normalized");
        thread.resource_id = Some(" resource-a ".to_string());
        thread.parent_thread_id = Some(" parent-1 ".to_string());

        store.save_thread(&thread).await.unwrap();

        let loaded = store.load_thread("t-normalized").await.unwrap().unwrap();
        assert_eq!(loaded.resource_id.as_deref(), Some("resource-a"));
        assert_eq!(loaded.parent_thread_id.as_deref(), Some("parent-1"));
    }

    #[tokio::test]
    async fn file_store_thread_load_missing() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        assert!(store.load_thread("no-such").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn file_store_save_thread_validated_serializes_concurrent_cycle_updates() {
        let td = TempDir::new().unwrap();
        let store = Arc::new(FileStore::new(td.path()));
        store.save_thread(&Thread::with_id("a")).await.unwrap();
        store.save_thread(&Thread::with_id("b")).await.unwrap();

        let guard = store.hierarchy_lock.lock().await;
        let barrier = Arc::new(Barrier::new(3));
        let spawn_update = |thread_id: &'static str, parent_thread_id: &'static str| {
            let store = store.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                store
                    .save_thread_validated(
                        &Thread::with_id(thread_id).with_parent_thread_id(parent_thread_id),
                    )
                    .await
            })
        };

        let left = spawn_update("a", "b");
        let right = spawn_update("b", "a");
        barrier.wait().await;
        tokio::task::yield_now().await;
        sleep(Duration::from_millis(20)).await;
        assert!(!left.is_finished());
        assert!(!right.is_finished());
        drop(guard);

        let left = left.await.unwrap();
        let right = right.await.unwrap();
        assert_ne!(left.is_ok(), right.is_ok());

        let a = store.load_thread("a").await.unwrap().unwrap();
        let b = store.load_thread("b").await.unwrap().unwrap();
        assert!(
            !(a.parent_thread_id.as_deref() == Some("b")
                && b.parent_thread_id.as_deref() == Some("a"))
        );
    }

    #[tokio::test]
    async fn file_store_instances_share_hierarchy_lock_for_same_path() {
        let td = TempDir::new().unwrap();
        let canonical_path = td.path().join("store");
        let alias_anchor = td.path().join("alias");
        std::fs::create_dir_all(&alias_anchor).unwrap();
        let aliased_path = alias_anchor.join("..").join("store");
        let left_store = Arc::new(FileStore::new(&canonical_path));
        let right_store = Arc::new(FileStore::new(&aliased_path));
        left_store.save_thread(&Thread::with_id("a")).await.unwrap();
        left_store.save_thread(&Thread::with_id("b")).await.unwrap();

        let barrier = Arc::new(Barrier::new(3));
        let spawn_update =
            |store: Arc<FileStore>, thread_id: &'static str, parent_thread_id: &'static str| {
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    store
                        .save_thread_validated(
                            &Thread::with_id(thread_id).with_parent_thread_id(parent_thread_id),
                        )
                        .await
                })
            };

        let left = spawn_update(left_store.clone(), "a", "b");
        let right = spawn_update(right_store.clone(), "b", "a");
        barrier.wait().await;

        let left = left.await.unwrap();
        let right = right.await.unwrap();
        assert_ne!(left.is_ok(), right.is_ok());

        let a = left_store.load_thread("a").await.unwrap().unwrap();
        let b = right_store.load_thread("b").await.unwrap().unwrap();
        assert!(
            !(a.parent_thread_id.as_deref() == Some("b")
                && b.parent_thread_id.as_deref() == Some("a"))
        );
    }

    #[tokio::test]
    async fn file_store_list_threads() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        for i in 0..3 {
            let mut t = Thread::new();
            t.id = format!("t-{i:02}");
            store.save_thread(&t).await.unwrap();
        }
        let ids = store.list_threads(0, 100).await.unwrap();
        assert_eq!(ids.len(), 3);
    }

    #[tokio::test]
    async fn file_store_messages_save_load_delete() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let thread = Thread::new();
        store.save_thread(&thread).await.unwrap();

        let msgs = vec![Message::user("hello")];
        store.save_messages(&thread.id, &msgs).await.unwrap();

        let loaded = store.load_messages(&thread.id).await.unwrap().unwrap();
        assert_eq!(loaded.len(), 1);

        store.delete_messages(&thread.id).await.unwrap();
        assert!(store.load_messages(&thread.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn file_store_delete_messages_missing_thread_returns_not_found() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let err = store.delete_messages("no-such").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    // ── RunStore ──

    #[tokio::test]
    async fn file_store_run_create_load() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let run = make_run("r-1", "t-1");
        store.create_run(&run).await.unwrap();
        let loaded = store.load_run("r-1").await.unwrap().unwrap();
        assert_eq!(loaded.thread_id, "t-1");
    }

    #[tokio::test]
    async fn file_store_run_create_duplicate_returns_already_exists() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let run = make_run("r-1", "t-1");
        store.create_run(&run).await.unwrap();
        let err = store.create_run(&run).await.unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn file_store_run_latest() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let mut r1 = make_run("r-1", "t-1");
        r1.updated_at = 100;
        let mut r2 = make_run("r-2", "t-1");
        r2.updated_at = 200;
        store.create_run(&r1).await.unwrap();
        store.create_run(&r2).await.unwrap();

        let latest = store.latest_run("t-1").await.unwrap().unwrap();
        assert_eq!(latest.run_id, "r-2");
    }

    // ── Checkpoint ──

    #[tokio::test]
    async fn file_store_checkpoint_saves_messages_and_run() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let msgs = vec![Message::user("cp")];
        let run = make_run("r-cp", "t-1");

        store.checkpoint("t-1", &msgs, &run).await.unwrap();

        let loaded_msgs = store.load_messages("t-1").await.unwrap().unwrap();
        assert_eq!(loaded_msgs.len(), 1);
        let loaded_run = store.load_run("r-cp").await.unwrap().unwrap();
        assert_eq!(loaded_run.thread_id, "t-1");
    }

    #[tokio::test]
    async fn file_store_checkpoint_waits_for_hierarchy_lock() {
        let td = TempDir::new().unwrap();
        let store = Arc::new(FileStore::new(td.path()));
        let guard = store.hierarchy_lock.lock().await;
        let handle = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .checkpoint(
                        "t-locked",
                        &[Message::user("cp")],
                        &make_run("r-locked", "t-locked"),
                    )
                    .await
            })
        };

        tokio::task::yield_now().await;
        sleep(Duration::from_millis(20)).await;
        assert!(!handle.is_finished());
        drop(guard);

        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn file_store_delete_thread_with_strategy_waits_for_hierarchy_lock() {
        let td = TempDir::new().unwrap();
        let store = Arc::new(FileStore::new(td.path()));
        store.save_thread(&Thread::with_id("root")).await.unwrap();
        store
            .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
            .await
            .unwrap();

        let guard = store.hierarchy_lock.lock().await;
        let handle = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .delete_thread_with_strategy("root", ChildThreadDeleteStrategy::Detach)
                    .await
            })
        };

        tokio::task::yield_now().await;
        sleep(Duration::from_millis(20)).await;
        assert!(!handle.is_finished());
        drop(guard);

        handle.await.unwrap().unwrap();
        assert!(store.load_thread("root").await.unwrap().is_none());
        assert_eq!(
            store
                .load_thread("child")
                .await
                .unwrap()
                .and_then(|thread| thread.parent_thread_id),
            None
        );
    }

    #[tokio::test]
    async fn file_store_new_rolls_back_incomplete_checkpoint_journal() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let old_run = make_run("r-old", "t-rollback");
        store
            .checkpoint("t-rollback", &[Message::user("old")], &old_run)
            .await
            .unwrap();

        let new_run = make_run("r-new", "t-rollback");
        let mut new_thread = store.load_thread("t-rollback").await.unwrap().unwrap();
        new_thread.apply_run_projection(&new_run);
        let thread_payload = serde_json::to_string_pretty(&new_thread).unwrap();
        let messages_payload = serde_json::to_string_pretty(&[Message::user("new")]).unwrap();
        let run_payload = serde_json::to_string_pretty(&new_run).unwrap();

        let staged_thread = stage_write(&store.threads_dir(), "t-rollback.json", &thread_payload)
            .await
            .unwrap();
        let staged_messages =
            stage_write(&store.messages_dir(), "t-rollback.json", &messages_payload)
                .await
                .unwrap();
        let staged_run = stage_write(&store.runs_dir(), "r-new.json", &run_payload)
            .await
            .unwrap();
        let staged = [staged_thread, staged_messages, staged_run];
        let tx_id = "rollback-test";
        let journal = CheckpointJournal {
            writes: staged
                .iter()
                .map(|write| {
                    let backup = checkpoint_backup_path(&write.target, tx_id);
                    CheckpointJournalWrite {
                        target: rel_path(td.path(), &write.target).unwrap(),
                        tmp: Some(rel_path(td.path(), &write.tmp_path).unwrap()),
                        backup: rel_path(td.path(), &backup).unwrap(),
                        had_target: write.target.exists(),
                    }
                })
                .collect(),
        };
        std::fs::write(
            checkpoint_marker_path(td.path()),
            serde_json::to_vec_pretty(&journal).unwrap(),
        )
        .unwrap();

        // Simulate a crash after the thread file was replaced but before
        // messages/run files were committed.
        let thread_backup = join_rel(td.path(), &journal.writes[0].backup);
        std::fs::rename(&staged[0].target, &thread_backup).unwrap();
        std::fs::rename(&staged[0].tmp_path, &staged[0].target).unwrap();

        let recovered = FileStore::new(td.path());
        let thread = recovered.load_thread("t-rollback").await.unwrap().unwrap();
        assert_eq!(thread.latest_run_id.as_deref(), Some("r-old"));
        let messages = recovered
            .load_messages("t-rollback")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(messages[0].text(), "old");
        assert!(recovered.load_run("r-new").await.unwrap().is_none());
        assert!(!checkpoint_marker_path(td.path()).exists());
    }

    #[tokio::test]
    async fn file_store_new_rolls_back_incomplete_hierarchy_delete_journal() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        store.save_thread(&Thread::with_id("root")).await.unwrap();
        store
            .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
            .await
            .unwrap();
        store
            .save_messages("root", &[Message::user("root message")])
            .await
            .unwrap();

        let mut updated_child = store.load_thread("child").await.unwrap().unwrap();
        updated_child.parent_thread_id = None;
        updated_child.touch(2_000);
        let child_payload = serde_json::to_string_pretty(&updated_child).unwrap();
        let child_write = stage_write(&store.threads_dir(), "child.json", &child_payload)
            .await
            .unwrap();
        let root_thread_delete = stage_delete(store.thread_path("root")).unwrap();
        let root_messages_delete = stage_delete(store.messages_path("root")).unwrap();
        let ops = [
            StagedFileOp::Write(child_write.clone()),
            StagedFileOp::Delete(root_thread_delete.clone()),
            StagedFileOp::Delete(root_messages_delete.clone()),
        ];
        let tx_id = "delete-rollback-test";
        let journal = CheckpointJournal {
            writes: ops
                .iter()
                .map(|op| {
                    let target = staged_op_target(op);
                    let backup = checkpoint_backup_path(target, tx_id);
                    CheckpointJournalWrite {
                        target: rel_path(td.path(), target).unwrap(),
                        tmp: staged_op_tmp(op).map(|tmp| rel_path(td.path(), tmp).unwrap()),
                        backup: rel_path(td.path(), &backup).unwrap(),
                        had_target: target.exists(),
                    }
                })
                .collect(),
        };
        std::fs::write(
            checkpoint_marker_path(td.path()),
            serde_json::to_vec_pretty(&journal).unwrap(),
        )
        .unwrap();

        // Simulate a crash after the child update and root thread delete were
        // committed, but before root messages were deleted.
        let child_backup = join_rel(td.path(), &journal.writes[0].backup);
        std::fs::rename(&child_write.target, &child_backup).unwrap();
        std::fs::rename(&child_write.tmp_path, &child_write.target).unwrap();
        let root_thread_backup = join_rel(td.path(), &journal.writes[1].backup);
        std::fs::rename(store.thread_path("root"), &root_thread_backup).unwrap();

        let recovered = FileStore::new(td.path());
        let root = recovered.load_thread("root").await.unwrap().unwrap();
        let child = recovered.load_thread("child").await.unwrap().unwrap();
        let messages = recovered.load_messages("root").await.unwrap().unwrap();

        assert_eq!(root.id, "root");
        assert_eq!(child.parent_thread_id.as_deref(), Some("root"));
        assert_eq!(messages[0].text(), "root message");
        assert!(!checkpoint_marker_path(td.path()).exists());
    }

    // ── Missing directory recovery ──

    #[tokio::test]
    async fn file_store_operations_create_dirs_on_demand() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path().join("fresh"));
        // This should work even though the dirs don't exist yet
        let thread = Thread::new();
        store.save_thread(&thread).await.unwrap();
        let loaded = store.load_thread(&thread.id).await.unwrap();
        assert!(loaded.is_some());
    }

    // ── validate_id edge cases for IDs used in operations ──

    #[tokio::test]
    async fn file_store_rejects_traversal_thread_id() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let err = store.load_thread("../escape").await.unwrap_err();
        assert!(matches!(err, StorageError::Io(_)));
    }

    #[tokio::test]
    async fn file_store_rejects_slash_in_run_id() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let err = store.load_run("a/b").await.unwrap_err();
        assert!(matches!(err, StorageError::Io(_)));
    }

    // ── ProfileStore ──

    #[tokio::test]
    async fn profile_file_set_and_get() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let owner = ProfileOwner::Agent("alice".into());
        store
            .set(&owner, "lang", serde_json::json!("en"))
            .await
            .unwrap();
        let entry = ProfileStore::get(&store, &owner, "lang")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.key, "lang");
        assert_eq!(entry.value, serde_json::json!("en"));
        assert!(entry.updated_at > 0);
    }

    #[tokio::test]
    async fn profile_file_get_missing() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let result = ProfileStore::get(&store, &ProfileOwner::System, "nonexistent")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn profile_file_delete_and_clear() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let owner = ProfileOwner::Agent("bob".into());

        // Delete non-existent is fine
        ProfileStore::delete(&store, &owner, "missing")
            .await
            .unwrap();

        // Set, delete, verify gone
        store.set(&owner, "k", serde_json::json!(1)).await.unwrap();
        ProfileStore::delete(&store, &owner, "k").await.unwrap();
        assert!(
            ProfileStore::get(&store, &owner, "k")
                .await
                .unwrap()
                .is_none()
        );

        // Clear owner
        store.set(&owner, "a", serde_json::json!(1)).await.unwrap();
        store.set(&owner, "b", serde_json::json!(2)).await.unwrap();
        store.clear_owner(&owner).await.unwrap();
        assert!(ProfileStore::list(&store, &owner).await.unwrap().is_empty());

        // Clear again is idempotent
        store.clear_owner(&owner).await.unwrap();
    }

    #[tokio::test]
    async fn profile_file_list_sorted() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let alice = ProfileOwner::Agent("alice".into());
        let bob = ProfileOwner::Agent("bob".into());
        store
            .set(&alice, "z", serde_json::json!("last"))
            .await
            .unwrap();
        store
            .set(&alice, "a", serde_json::json!("first"))
            .await
            .unwrap();
        store
            .set(&bob, "x", serde_json::json!("other"))
            .await
            .unwrap();

        let entries = ProfileStore::list(&store, &alice).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "a");
        assert_eq!(entries[1].key, "z");

        // Bob's entries are isolated
        assert_eq!(ProfileStore::list(&store, &bob).await.unwrap().len(), 1);
    }

    // ── ConfigStore::put_if_revision ──

    #[tokio::test]
    async fn file_store_put_if_revision_basic() {
        use awaken_contract::contract::config_store::ConfigStore;
        use awaken_contract::contract::storage::StorageError;

        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());

        let value_r1 = serde_json::json!({"spec": {"id": "k"}, "meta": {"source": {"kind": "user"}, "revision": 1}});
        // Insert: no record, expected=0 → succeeds.
        store
            .put_if_revision("ns", "k", &value_r1, 0)
            .await
            .unwrap();
        let stored = ConfigStore::get(&store, "ns", "k").await.unwrap().unwrap();
        assert_eq!(stored["meta"]["revision"], 1);

        // Conflict: expected=0 again should fail.
        let err = store
            .put_if_revision("ns", "k", &value_r1, 0)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            StorageError::VersionConflict {
                expected: 0,
                actual: 1
            }
        ));

        // Correct CAS: expected=1 → update to revision 2.
        let value_r2 = serde_json::json!({"spec": {"id": "k"}, "meta": {"source": {"kind": "user"}, "revision": 2}});
        store
            .put_if_revision("ns", "k", &value_r2, 1)
            .await
            .unwrap();
        let stored = ConfigStore::get(&store, "ns", "k").await.unwrap().unwrap();
        assert_eq!(stored["meta"]["revision"], 2);
    }

    #[tokio::test]
    async fn file_store_put_if_absent_and_delete_if_revision() {
        use awaken_contract::contract::config_store::ConfigStore;
        use awaken_contract::contract::storage::StorageError;

        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());

        let value = serde_json::json!({
            "spec": {"id": "k"},
            "meta": {"source": {"kind": "user"}, "revision": 7}
        });
        store.put_if_absent("ns", "k", &value).await.unwrap();

        let err = store.put_if_absent("ns", "k", &value).await.unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists(id) if id == "ns/k"));

        let err = store.delete_if_revision("ns", "k", 6).await.unwrap_err();
        assert!(matches!(
            err,
            StorageError::VersionConflict {
                expected: 6,
                actual: 7
            }
        ));
        assert!(ConfigStore::get(&store, "ns", "k").await.unwrap().is_some());

        store.delete_if_revision("ns", "k", 7).await.unwrap();
        assert!(ConfigStore::get(&store, "ns", "k").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn file_store_put_if_revision_is_atomic_across_store_instances() {
        use awaken_contract::contract::config_store::ConfigStore;
        use awaken_contract::contract::storage::StorageError;

        const WRITERS: usize = 16;
        let td = TempDir::new().unwrap();
        let barrier = Arc::new(Barrier::new(WRITERS));
        let mut handles = Vec::with_capacity(WRITERS);

        for i in 0..WRITERS {
            let path = td.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                let store = FileStore::new(path);
                let value = serde_json::json!({
                    "spec": {"id": "race", "winner": i},
                    "meta": {"source": {"kind": "user"}, "revision": 1}
                });
                barrier.wait().await;
                store.put_if_revision("ns", "race", &value, 0).await
            }));
        }

        let results = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|result| result.expect("task join"))
            .collect::<Vec<_>>();
        let successes = results.iter().filter(|result| result.is_ok()).count();
        let conflicts = results
            .iter()
            .filter(|result| {
                matches!(
                    result,
                    Err(StorageError::VersionConflict {
                        expected: 0,
                        actual: 1
                    })
                )
            })
            .count();

        assert_eq!(successes, 1, "exactly one concurrent create may win");
        assert_eq!(
            conflicts,
            WRITERS - 1,
            "every losing create must observe the winning revision"
        );

        let store = FileStore::new(td.path());
        let stored = ConfigStore::get(&store, "ns", "race")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored["meta"]["revision"], 1);
    }

    #[tokio::test]
    async fn file_store_delete_and_update_same_revision_are_mutually_exclusive() {
        use awaken_contract::contract::config_store::ConfigStore;

        let td = TempDir::new().unwrap();
        let seed_store = FileStore::new(td.path());
        let value_r1 = serde_json::json!({
            "spec": {"id": "duel", "value": 1},
            "meta": {"source": {"kind": "user"}, "revision": 1}
        });
        seed_store
            .put_if_revision("ns", "duel", &value_r1, 0)
            .await
            .unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let delete_path = td.path().to_path_buf();
        let update_path = td.path().to_path_buf();
        let delete_barrier = Arc::clone(&barrier);
        let update_barrier = Arc::clone(&barrier);

        let delete = tokio::spawn(async move {
            let store = FileStore::new(delete_path);
            delete_barrier.wait().await;
            store.delete_if_revision("ns", "duel", 1).await
        });
        let update = tokio::spawn(async move {
            let store = FileStore::new(update_path);
            let value_r2 = serde_json::json!({
                "spec": {"id": "duel", "value": 2},
                "meta": {"source": {"kind": "user"}, "revision": 2}
            });
            update_barrier.wait().await;
            store.put_if_revision("ns", "duel", &value_r2, 1).await
        });

        let delete_ok = delete.await.unwrap().is_ok();
        let update_ok = update.await.unwrap().is_ok();
        assert_ne!(
            delete_ok, update_ok,
            "same-revision delete and update must not both succeed or both fail"
        );

        let store = FileStore::new(td.path());
        let stored = ConfigStore::get(&store, "ns", "duel").await.unwrap();
        if delete_ok {
            assert!(stored.is_none(), "successful delete must remove the record");
        } else {
            assert_eq!(
                stored.expect("successful update must leave record")["meta"]["revision"],
                2
            );
        }
    }
}
