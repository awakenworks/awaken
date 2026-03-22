//! File-system storage backend.
//!
//! Layout:
//! ```text
//! <base_path>/
//!   threads/<thread_id>.json         — Thread
//!   messages/<thread_id>.json        — Vec<Message>
//!   runs/<run_id>.json               — RunRecord
//!   mailbox/<mailbox_id>/<entry_id>.json — MailboxEntry
//! ```

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    MailboxEntry, MailboxStore, RunPage, RunQuery, RunRecord, RunStore, StorageError,
    ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::Thread;
use tokio::io::AsyncWriteExt;

/// File-system storage backend.
pub struct FileStore {
    base_path: PathBuf,
}

impl FileStore {
    /// Create a new file store rooted at `base_path`.
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
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

    fn mailbox_dir(&self) -> PathBuf {
        self.base_path.join("mailbox")
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
        Ok::<(), StorageError>(())
    }
    .await;

    if let Err(e) = write_result {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(e);
    }
    Ok(())
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

async fn scan_json_stems(dir: &Path) -> Result<Vec<String>, StorageError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| StorageError::Io(e.to_string()))?;
    let mut stems = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| StorageError::Io(e.to_string()))?
    {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            stems.push(stem.to_string());
        }
    }
    Ok(stems)
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
        let mut stems = scan_json_stems(&self.threads_dir()).await?;
        stems.sort();
        Ok(stems.into_iter().skip(offset).take(limit).collect())
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

    async fn delete_thread(&self, id: &str) -> Result<(), StorageError> {
        validate_id(id, "thread id")?;
        let path = self.threads_dir().join(format!("{id}.json"));
        if !path.exists() {
            return Err(StorageError::NotFound(id.to_owned()));
        }
        tokio::fs::remove_file(&path)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))
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
        let path = self.runs_dir().join(format!("{}.json", record.run_id));
        if path.exists() {
            return Err(StorageError::AlreadyExists(record.run_id.clone()));
        }
        let payload = serde_json::to_string_pretty(record)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        atomic_write(
            &self.runs_dir(),
            &format!("{}.json", record.run_id),
            &payload,
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

// ── MailboxStore ────────────────────────────────────────────────────

#[async_trait]
impl MailboxStore for FileStore {
    async fn push_message(&self, entry: &MailboxEntry) -> Result<(), StorageError> {
        validate_id(&entry.mailbox_id, "mailbox id")?;
        validate_id(&entry.entry_id, "entry id")?;
        let dir = self.mailbox_dir().join(&entry.mailbox_id);
        let payload = serde_json::to_string_pretty(entry)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        atomic_write(&dir, &format!("{}.json", entry.entry_id), &payload).await
    }

    async fn pop_messages(
        &self,
        mailbox_id: &str,
        limit: usize,
    ) -> Result<Vec<MailboxEntry>, StorageError> {
        validate_id(mailbox_id, "mailbox id")?;
        let dir = self.mailbox_dir().join(mailbox_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut entries: Vec<MailboxEntry> = scan_json_dir(&dir).await?;
        entries.sort_by_key(|e| e.created_at);
        let drain_count = limit.min(entries.len());
        let popped: Vec<MailboxEntry> = entries.drain(..drain_count).collect();

        // Remove popped files
        for entry in &popped {
            let path = dir.join(format!("{}.json", entry.entry_id));
            let _ = tokio::fs::remove_file(path).await;
        }
        Ok(popped)
    }

    async fn peek_messages(
        &self,
        mailbox_id: &str,
        limit: usize,
    ) -> Result<Vec<MailboxEntry>, StorageError> {
        validate_id(mailbox_id, "mailbox id")?;
        let dir = self.mailbox_dir().join(mailbox_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut entries: Vec<MailboxEntry> = scan_json_dir(&dir).await?;
        entries.sort_by_key(|e| e.created_at);
        entries.truncate(limit);
        Ok(entries)
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

        // Write messages
        let msg_payload = serde_json::to_string_pretty(messages)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        atomic_write(
            &self.messages_dir(),
            &format!("{thread_id}.json"),
            &msg_payload,
        )
        .await?;

        // Write run record
        let run_payload = serde_json::to_string_pretty(run)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        atomic_write(
            &self.runs_dir(),
            &format!("{}.json", run.run_id),
            &run_payload,
        )
        .await
    }
}

// Unit tests removed: all scenarios are covered by integration tests
// in `tests/file_store.rs`.
