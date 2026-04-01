//! File-system storage backend.
//!
//! Layout:
//! ```text
//! <base_path>/
//!   threads/<thread_id>.json         — Thread
//!   messages/<thread_id>.json        — Vec<Message>
//!   runs/<run_id>.json               — RunRecord
//! ```

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::profile_store::{ProfileEntry, ProfileOwner, ProfileStore};
use awaken_contract::contract::storage::{
    RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::Thread;
use tokio::io::AsyncWriteExt;

/// File-system storage backend.
pub struct FileStore {
    base_path: PathBuf,
}

impl FileStore {
    /// Create a new file store rooted at `base_path`.
    ///
    /// If a `.checkpoint_pending` marker is detected (leftover from a crash
    /// during [`ThreadRunStore::checkpoint`]), a warning is logged. The data
    /// files are individually atomic (complete or absent) so the store is
    /// still usable, but the three checkpoint files may be mutually
    /// inconsistent.
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        let base_path = base_path.into();
        let marker = base_path.join(".checkpoint_pending");
        if marker.exists() {
            tracing::warn!(
                path = %marker.display(),
                "stale .checkpoint_pending marker detected — a previous checkpoint \
                 may be incomplete; thread/messages/run state could be inconsistent"
            );
            // Remove the stale marker so we don't warn on every restart.
            let _ = std::fs::remove_file(&marker);
        }
        Self { base_path }
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
}

// ── Filesystem helpers ──────────────────────────────────────────────

fn validate_id(id: &str, label: &str) -> Result<(), StorageError> {
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

async fn atomic_write(dir: &Path, filename: &str, content: &str) -> Result<(), StorageError> {
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
struct StagedWrite {
    tmp_path: PathBuf,
    target: PathBuf,
    dir: PathBuf,
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
    Ok(StagedWrite {
        tmp_path,
        target,
        dir: dir.to_path_buf(),
    })
}

/// Rename all staged temp files into their targets and fsync each parent dir.
/// A `.checkpoint_pending` marker in `base_dir` brackets the rename sequence so
/// that crash-recovery can detect incomplete checkpoints.
async fn commit_staged_writes(base_dir: &Path, writes: &[StagedWrite]) -> Result<(), StorageError> {
    let marker = base_dir.join(".checkpoint_pending");
    // Create marker before renames
    tokio::fs::write(&marker, b"pending")
        .await
        .map_err(|e| StorageError::Io(e.to_string()))?;
    dir_fsync(base_dir).await?;

    // Collect unique parent dirs that need fsync
    let mut synced_dirs = std::collections::HashSet::new();

    for w in writes {
        tokio::fs::rename(&w.tmp_path, &w.target)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        synced_dirs.insert(w.dir.clone());
    }

    // Fsync each unique parent directory
    for dir in &synced_dirs {
        dir_fsync(dir).await?;
    }

    // Remove marker — checkpoint is fully committed
    let _ = tokio::fs::remove_file(&marker).await;
    Ok(())
}

/// Clean up staged temp files on error.
async fn cleanup_staged_writes(writes: &[StagedWrite]) {
    for w in writes {
        let _ = tokio::fs::remove_file(&w.tmp_path).await;
    }
}

async fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, StorageError> {
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
        let path = self.threads_dir().join(format!("{thread_id}.json"));
        read_json(&path).await
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        validate_id(&thread.id, "thread id")?;
        let payload = serde_json::to_string_pretty(thread)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        atomic_write(
            &self.threads_dir(),
            &format!("{}.json", thread.id),
            &payload,
        )
        .await
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

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        let mut threads: Vec<Thread> = scan_json_dir(&self.threads_dir()).await?;
        threads.sort_by(|a, b| {
            let a_updated = a.metadata.updated_at.or(a.metadata.created_at).unwrap_or(0);
            let b_updated = b.metadata.updated_at.or(b.metadata.created_at).unwrap_or(0);
            b_updated.cmp(&a_updated).then_with(|| a.id.cmp(&b.id))
        });
        Ok(threads
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|thread| thread.id)
            .collect())
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        validate_id(thread_id, "thread id")?;
        let path = self.messages_dir().join(format!("{thread_id}.json"));
        read_json(&path).await
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

/// Returns the current time in milliseconds since the UNIX epoch.
///
/// # Panics
///
/// Panics if the system clock is set before the UNIX epoch (1970-01-01).
/// This is a truly exceptional condition that indicates a severely
/// misconfigured system and cannot be meaningfully recovered from.
fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

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

        let now = current_millis();
        let mut thread = self
            .load_thread(thread_id)
            .await?
            .unwrap_or_else(|| Thread::with_id(thread_id));
        thread.metadata.created_at.get_or_insert(now);
        thread.metadata.updated_at = Some(now);

        // Serialize all payloads up-front so we fail before any I/O.
        let thread_payload = serde_json::to_string_pretty(&thread)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let msg_payload = serde_json::to_string_pretty(messages)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let run_payload = serde_json::to_string_pretty(run)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        // Stage all three writes (temp files fsynced but not yet renamed).
        let thread_file = &format!("{thread_id}.json");
        let run_file = &format!("{}.json", run.run_id);

        let staged_thread = stage_write(&self.threads_dir(), thread_file, &thread_payload).await?;
        let staged_msgs = match stage_write(&self.messages_dir(), thread_file, &msg_payload).await {
            Ok(s) => s,
            Err(e) => {
                cleanup_staged_writes(&[staged_thread]).await;
                return Err(e);
            }
        };
        let staged_run = match stage_write(&self.runs_dir(), run_file, &run_payload).await {
            Ok(s) => s,
            Err(e) => {
                cleanup_staged_writes(&[staged_thread, staged_msgs]).await;
                return Err(e);
            }
        };

        let writes = [staged_thread, staged_msgs, staged_run];

        // Commit: marker → renames → dir fsyncs → remove marker.
        if let Err(e) = commit_staged_writes(&self.base_path, &writes).await {
            cleanup_staged_writes(&writes).await;
            return Err(e);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::message::Message;
    use awaken_contract::contract::storage::{
        RunQuery, RunRecord, RunStore, ThreadRunStore, ThreadStore,
    };
    use awaken_contract::thread::Thread;
    use tempfile::TempDir;

    fn make_run(run_id: &str, thread_id: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.to_string(),
            thread_id: thread_id.to_string(),
            agent_id: "agent".to_string(),
            parent_run_id: None,
            status: RunStatus::Running,
            termination_code: None,
            created_at: 100,
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
    async fn file_store_thread_load_missing() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        assert!(store.load_thread("no-such").await.unwrap().is_none());
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
        let entry = store.get(&owner, "lang").await.unwrap().unwrap();
        assert_eq!(entry.key, "lang");
        assert_eq!(entry.value, serde_json::json!("en"));
        assert!(entry.updated_at > 0);
    }

    #[tokio::test]
    async fn profile_file_get_missing() {
        let td = TempDir::new().unwrap();
        let store = FileStore::new(td.path());
        let result = store
            .get(&ProfileOwner::System, "nonexistent")
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
        store.delete(&owner, "missing").await.unwrap();

        // Set, delete, verify gone
        store.set(&owner, "k", serde_json::json!(1)).await.unwrap();
        store.delete(&owner, "k").await.unwrap();
        assert!(store.get(&owner, "k").await.unwrap().is_none());

        // Clear owner
        store.set(&owner, "a", serde_json::json!(1)).await.unwrap();
        store.set(&owner, "b", serde_json::json!(2)).await.unwrap();
        store.clear_owner(&owner).await.unwrap();
        assert!(store.list(&owner).await.unwrap().is_empty());

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

        let entries = store.list(&alice).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "a");
        assert_eq!(entries[1].key, "z");

        // Bob's entries are isolated
        assert_eq!(store.list(&bob).await.unwrap().len(), 1);
    }
}
