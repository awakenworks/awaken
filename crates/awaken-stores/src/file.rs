//! File-system storage backend.
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use awaken_server_contract::contract::config_store::{ConfigStore, extract_meta_revision};
use awaken_server_contract::contract::message::{
    Message, MessageRecord, PendingMessageRecord, strip_unpaired_tool_calls_from_owned_view,
};
use awaken_server_contract::contract::profile_store::{ProfileEntry, ProfileOwner, ProfileStore};
use awaken_server_contract::contract::storage::{
    ChildThreadDeleteStrategy, MessagePage, MessageQuery, RunPage, RunQuery, RunRecord, RunStore,
    StorageError, ThreadPage, ThreadQuery, ThreadRunStore, ThreadStore,
    checkpoint_parent_thread_id, message_append, paginate_message_records, paginate_threads,
    sort_threads_by_recent_activity,
};
use awaken_server_contract::thread::{Thread, normalize_lineage_id};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

mod pending;

/// File-system storage backend.
pub struct FileStore {
    base_path: PathBuf,
    hierarchy_lock: Arc<Mutex<()>>,
    /// Process-local mutex serialising config CAS operations.
    config_cas_lock: Arc<Mutex<()>>,
}

impl FileStore {
    /// Create a file store rooted at `base_path`.
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

    fn message_records_dir(&self) -> PathBuf {
        self.base_path.join("message_records")
    }

    fn thread_message_records_dir(&self, thread_id: &str) -> PathBuf {
        self.message_records_dir().join(thread_id)
    }

    fn pending_messages_dir(&self) -> PathBuf {
        self.base_path.join("pending_messages")
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

    #[cfg(test)]
    fn message_record_path(&self, thread_id: &str, seq: u64) -> PathBuf {
        self.thread_message_records_dir(thread_id)
            .join(format!("{seq:020}.json"))
    }

    fn pending_messages_path(&self, thread_id: &str) -> PathBuf {
        self.pending_messages_dir()
            .join(format!("{thread_id}.json"))
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
                    ops.push(StagedFileOp::Delete(stage_delete(
                        self.pending_messages_path(&id),
                    )?));
                    self.stage_delete_message_records(&id, &mut ops).await?;
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
            ops.push(StagedFileOp::Delete(stage_delete(
                self.pending_messages_path(thread_id),
            )?));
            self.stage_delete_message_records(thread_id, &mut ops)
                .await?;
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
        let run_payload = serde_json::to_string_pretty(run)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        let thread_file = &format!("{thread_id}.json");
        let run_file = &format!("{}.json", run.run_id);

        let staged_thread = stage_write(&self.threads_dir(), thread_file, &thread_payload).await?;
        let mut ops = Vec::new();
        ops.push(StagedFileOp::Write(staged_thread));
        if let Err(error) = self
            .stage_replace_message_records(thread_id, messages, &mut ops)
            .await
        {
            cleanup_staged_file_ops(&ops).await;
            return Err(error);
        }
        let staged_run = match stage_write(&self.runs_dir(), run_file, &run_payload).await {
            Ok(staged) => staged,
            Err(error) => {
                cleanup_staged_file_ops(&ops).await;
                return Err(error);
            }
        };
        ops.push(StagedFileOp::Write(staged_run));

        if let Err(error) = commit_staged_file_ops(&self.base_path, &ops).await {
            cleanup_staged_file_ops(&ops).await;
            return Err(error);
        }

        Ok(())
    }

    async fn load_committed_message_records_locked(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<MessageRecord>>, StorageError> {
        let dir = self.thread_message_records_dir(thread_id);
        let mut records = Vec::new();
        if dir.exists() {
            let mut entries = tokio::fs::read_dir(&dir)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?
            {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "json") {
                    continue;
                }
                if let Some(record) = read_json::<MessageRecord>(&path).await? {
                    records.push(record);
                }
            }
            records.sort_by_key(|record| record.seq);
            if !records.is_empty() {
                return Ok(Some(records));
            }
        }

        let Some(messages) = read_json::<Vec<Message>>(&self.messages_path(thread_id)).await?
        else {
            return Ok(None);
        };
        Ok(Some(
            messages
                .into_iter()
                .enumerate()
                .map(|(index, message)| {
                    MessageRecord::from_message(thread_id.to_owned(), index as u64 + 1, message)
                })
                .collect(),
        ))
    }

    async fn stage_delete_message_records(
        &self,
        thread_id: &str,
        ops: &mut Vec<StagedFileOp>,
    ) -> Result<(), StorageError> {
        let dir = self.thread_message_records_dir(thread_id);
        if !dir.exists() {
            return Ok(());
        }
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?
        {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                ops.push(StagedFileOp::Delete(stage_delete(path)?));
            }
        }
        Ok(())
    }

    async fn stage_replace_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
        ops: &mut Vec<StagedFileOp>,
    ) -> Result<(), StorageError> {
        // Write the replacement records first, then remove stale records only when the new
        // message set is shorter than the existing set. This avoids staging duplicate
        // delete+write operations on the same target path in one transaction.
        for (index, message) in messages.iter().enumerate() {
            self.stage_write_message_record(thread_id, index as u64 + 1, message, ops)
                .await?;
        }

        let records_dir = self.thread_message_records_dir(thread_id);
        let new_len = messages.len() as u64;
        if new_len == 0 {
            return self.stage_delete_message_records(thread_id, ops).await;
        }

        if !records_dir.exists() {
            return Ok(());
        }
        let mut entries = tokio::fs::read_dir(&records_dir)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?
        {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            let Ok(seq) = stem.parse::<u64>() else {
                continue;
            };
            if seq > new_len && path.extension().is_some_and(|ext| ext == "json") {
                ops.push(StagedFileOp::Delete(stage_delete(path)?));
            }
        }
        Ok(())
    }

    async fn stage_append_message_records(
        &self,
        thread_id: &str,
        start_seq: u64,
        messages: impl IntoIterator<Item = Message>,
        ops: &mut Vec<StagedFileOp>,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        let messages = messages.into_iter().collect::<Vec<_>>();
        message_append::validate_append_only_delta(&[], &messages)?;
        let mut appended = Vec::new();
        for (index, message) in messages.into_iter().enumerate() {
            let seq = start_seq + index as u64;
            self.stage_write_message_record(thread_id, seq, &message, ops)
                .await?;
            appended.push(MessageRecord::from_message(
                thread_id.to_owned(),
                seq,
                message,
            ));
        }
        Ok(appended)
    }

    async fn stage_checkpoint_append_message_records(
        &self,
        thread_id: &str,
        committed: &[MessageRecord],
        delta: &[Message],
        ops: &mut Vec<StagedFileOp>,
    ) -> Result<u64, StorageError> {
        let mut merged = committed
            .iter()
            .map(|record| record.message.clone())
            .collect::<Vec<_>>();
        message_append::merge_checkpoint_append_messages(&mut merged, delta)?;
        self.stage_replace_message_records(thread_id, &merged, ops)
            .await?;
        Ok(merged.len() as u64)
    }

    async fn stage_write_message_record(
        &self,
        thread_id: &str,
        seq: u64,
        message: &Message,
        ops: &mut Vec<StagedFileOp>,
    ) -> Result<(), StorageError> {
        let record = MessageRecord::from_message(thread_id.to_owned(), seq, message.clone());
        let payload = serde_json::to_string_pretty(&record)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let staged = stage_write(
            &self.thread_message_records_dir(thread_id),
            &format!("{seq:020}.json"),
            &payload,
        )
        .await?;
        ops.push(StagedFileOp::Write(staged));
        Ok(())
    }

    async fn read_pending_messages_locked(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        read_json::<Vec<PendingMessageRecord>>(&self.pending_messages_path(thread_id))
            .await
            .map(|records| records.unwrap_or_default())
    }

    async fn write_pending_messages_locked(
        &self,
        thread_id: &str,
        records: &[PendingMessageRecord],
    ) -> Result<StagedWrite, StorageError> {
        let payload = serde_json::to_string_pretty(records)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        stage_write(
            &self.pending_messages_dir(),
            &format!("{thread_id}.json"),
            &payload,
        )
        .await
    }

    fn normalize_pending_positions(records: &mut [PendingMessageRecord]) {
        for (index, record) in records.iter_mut().enumerate() {
            record.position = index as u64 + 1;
        }
    }

    async fn committed_message_exists(
        &self,
        thread_id: &str,
        message_id: &str,
    ) -> Result<bool, StorageError> {
        let Some(records) = self
            .load_committed_message_records_locked(thread_id)
            .await?
        else {
            return Ok(false);
        };
        Ok(records
            .iter()
            .any(|record| record.message.id.as_deref() == Some(message_id)))
    }

    fn pending_not_found(thread_id: &str, pending_id: &str) -> StorageError {
        StorageError::NotFound(format!(
            "pending message '{pending_id}' in thread '{thread_id}'"
        ))
    }

    fn already_consumed(pending_id: &str) -> StorageError {
        StorageError::Validation(format!(
            "pending message '{pending_id}' is already consumed"
        ))
    }

    fn duplicate_pending_id(pending_id: &str) -> StorageError {
        StorageError::Validation(format!("pending message '{pending_id}' already exists"))
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
/// Like [`atomic_write`] but fails if the target file already exists.
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

/// Write and fsync a temp file, returning a staged write for later commit.
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
        let pending_messages_path = self
            .pending_messages_dir()
            .join(format!("{thread_id}.json"));
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
        if pending_messages_path.exists() {
            tokio::fs::remove_file(&pending_messages_path)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        let records_dir = self.thread_message_records_dir(thread_id);
        if records_dir.exists() {
            tokio::fs::remove_dir_all(&records_dir)
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
        awaken_server_contract::contract::storage::sort_threads_by_recent_activity(&mut threads);
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
        let Some(records) = self
            .load_committed_message_records_locked(thread_id)
            .await?
        else {
            return Ok(None);
        };
        let messages = records.into_iter().map(|record| record.message).collect();
        Ok(Some(strip_unpaired_tool_calls_from_owned_view(messages)))
    }

    async fn load_committed_messages(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<Message>>, StorageError> {
        validate_id(thread_id, "thread id")?;
        let Some(records) = self
            .load_committed_message_records_locked(thread_id)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(
            records.into_iter().map(|record| record.message).collect(),
        ))
    }

    async fn load_message_records(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<MessageRecord>>, StorageError> {
        validate_id(thread_id, "thread id")?;
        let Some(records) = self
            .load_committed_message_records_locked(thread_id)
            .await?
        else {
            return Ok(None);
        };
        let messages = records
            .into_iter()
            .map(|record| record.message)
            .collect::<Vec<_>>();
        let messages = strip_unpaired_tool_calls_from_owned_view(messages);
        Ok(Some(
            messages
                .into_iter()
                .enumerate()
                .map(|(index, message)| {
                    MessageRecord::from_message(thread_id.to_owned(), index as u64 + 1, message)
                })
                .collect(),
        ))
    }

    async fn list_message_records(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, StorageError> {
        validate_id(thread_id, "thread id")?;
        let Some(records) = self.load_message_records(thread_id).await? else {
            return Ok(MessagePage::empty());
        };
        Ok(paginate_message_records(records, query))
    }

    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        validate_id(thread_id, "thread id")?;
        let mut ops = Vec::new();
        self.stage_replace_message_records(thread_id, messages, &mut ops)
            .await?;
        if let Err(error) = commit_staged_file_ops(&self.base_path, &ops).await {
            cleanup_staged_file_ops(&ops).await;
            return Err(error);
        }
        Ok(())
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
        let records_dir = self.thread_message_records_dir(thread_id);
        if records_dir.exists() {
            tokio::fs::remove_dir_all(&records_dir)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: awaken_server_contract::thread::ThreadMetadata,
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

    /// Process-local compare-and-set for the config revision field.
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

    async fn checkpoint_append(
        &self,
        thread_id: &str,
        messages: &[Message],
        expected_version: Option<u64>,
        run: &RunRecord,
    ) -> Result<u64, StorageError> {
        validate_id(thread_id, "thread id")?;
        validate_id(&run.run_id, "run id")?;
        let _guard = self.hierarchy_lock.lock().await;

        let committed = self
            .load_committed_message_records_locked(thread_id)
            .await?
            .unwrap_or_default();
        let actual = committed.len() as u64;
        if let Some(expected) = expected_version
            && expected != actual
        {
            return Err(StorageError::VersionConflict { expected, actual });
        }

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
        let run_payload = serde_json::to_string_pretty(run)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        let mut ops = Vec::new();
        let thread_write = stage_write(
            &self.threads_dir(),
            &format!("{thread_id}.json"),
            &thread_payload,
        )
        .await?;
        ops.push(StagedFileOp::Write(thread_write));
        let new_version = match self
            .stage_checkpoint_append_message_records(thread_id, &committed, messages, &mut ops)
            .await
        {
            Ok(new_version) => new_version,
            Err(error) => {
                cleanup_staged_file_ops(&ops).await;
                return Err(error);
            }
        };
        let run_write = match stage_write(
            &self.runs_dir(),
            &format!("{}.json", run.run_id),
            &run_payload,
        )
        .await
        {
            Ok(staged) => staged,
            Err(error) => {
                cleanup_staged_file_ops(&ops).await;
                return Err(error);
            }
        };
        ops.push(StagedFileOp::Write(run_write));

        if let Err(error) = commit_staged_file_ops(&self.base_path, &ops).await {
            cleanup_staged_file_ops(&ops).await;
            return Err(error);
        }
        Ok(new_version)
    }
}

#[cfg(test)]
mod tests;
