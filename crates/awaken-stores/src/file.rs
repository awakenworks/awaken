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

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        let mut stems = scan_json_stems(&self.threads_dir()).await?;
        stems.sort();
        Ok(stems.into_iter().skip(offset).take(limit).collect())
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
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        validate_id(thread_id, "thread id")?;
        let path = self.messages_dir().join(format!("{thread_id}.json"));
        read_json(&path).await
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::lifecycle::RunStatus;
    use tempfile::TempDir;

    fn make_run(run_id: &str, thread_id: &str, updated_at: u64) -> RunRecord {
        RunRecord {
            run_id: run_id.to_owned(),
            thread_id: thread_id.to_owned(),
            agent_id: "agent-1".to_owned(),
            parent_run_id: None,
            status: RunStatus::Running,
            termination_code: None,
            created_at: updated_at,
            updated_at,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        }
    }

    fn make_mailbox_entry(id: &str, mailbox: &str) -> MailboxEntry {
        MailboxEntry {
            entry_id: id.to_string(),
            mailbox_id: mailbox.to_string(),
            payload: serde_json::json!({"text": id}),
            created_at: 1000,
        }
    }

    // ── ThreadStore ──

    #[tokio::test]
    async fn thread_store_save_and_load() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        let thread = Thread::with_id("t-1").with_message(Message::user("hello"));
        store.save_thread(&thread).await.unwrap();

        let loaded = store.load_thread("t-1").await.unwrap().unwrap();
        assert_eq!(loaded.id, "t-1");
        assert_eq!(loaded.message_count(), 1);
    }

    #[tokio::test]
    async fn thread_store_load_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        let result = store.load_thread("missing").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn thread_store_list_paginated() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        for i in 0..5 {
            store
                .save_thread(&Thread::with_id(format!("t-{i}")))
                .await
                .unwrap();
        }
        let page1 = store.list_threads(0, 3).await.unwrap();
        assert_eq!(page1.len(), 3);
        let page2 = store.list_threads(3, 3).await.unwrap();
        assert_eq!(page2.len(), 2);
    }

    #[tokio::test]
    async fn thread_store_overwrite() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        let thread = Thread::with_id("t-1").with_message(Message::user("hello"));
        store.save_thread(&thread).await.unwrap();

        let updated = thread.with_message(Message::assistant("hi"));
        store.save_thread(&updated).await.unwrap();

        let loaded = store.load_thread("t-1").await.unwrap().unwrap();
        assert_eq!(loaded.message_count(), 2);
    }

    #[tokio::test]
    async fn thread_store_invalid_id() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        let result = store.load_thread("../escape").await;
        assert!(result.is_err());
    }

    // ── RunStore ──

    #[tokio::test]
    async fn run_store_create_and_load() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        let run = make_run("run-1", "t-1", 100);
        store.create_run(&run).await.unwrap();

        let loaded = RunStore::load_run(&store, "run-1").await.unwrap().unwrap();
        assert_eq!(loaded.thread_id, "t-1");
    }

    #[tokio::test]
    async fn run_store_create_duplicate_errors() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        let run = make_run("run-1", "t-1", 100);
        store.create_run(&run).await.unwrap();
        let err = store.create_run(&run).await.unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn run_store_latest_run() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        store.create_run(&make_run("r1", "t-1", 100)).await.unwrap();
        store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();

        let latest = RunStore::latest_run(&store, "t-1").await.unwrap().unwrap();
        assert_eq!(latest.run_id, "r2");
    }

    #[tokio::test]
    async fn run_store_list_with_filter() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        store.create_run(&make_run("r1", "t-1", 100)).await.unwrap();
        store.create_run(&make_run("r2", "t-2", 200)).await.unwrap();

        let page = store
            .list_runs(&RunQuery {
                thread_id: Some("t-1".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 1);
    }

    // ── MailboxStore ──

    #[tokio::test]
    async fn mailbox_push_and_peek() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        store
            .push_message(&make_mailbox_entry("e1", "inbox-a"))
            .await
            .unwrap();
        store
            .push_message(&make_mailbox_entry("e2", "inbox-a"))
            .await
            .unwrap();

        let peeked = store.peek_messages("inbox-a", 10).await.unwrap();
        assert_eq!(peeked.len(), 2);
    }

    #[tokio::test]
    async fn mailbox_pop_removes_files() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        store
            .push_message(&make_mailbox_entry("e1", "inbox-a"))
            .await
            .unwrap();
        store
            .push_message(&make_mailbox_entry("e2", "inbox-a"))
            .await
            .unwrap();

        let popped = store.pop_messages("inbox-a", 1).await.unwrap();
        assert_eq!(popped.len(), 1);

        let remaining = store.peek_messages("inbox-a", 10).await.unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[tokio::test]
    async fn mailbox_pop_empty() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        let popped = store.pop_messages("nonexistent", 10).await.unwrap();
        assert!(popped.is_empty());
    }

    // ── ThreadRunStore ──

    #[tokio::test]
    async fn checkpoint_and_load() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        let run = make_run("run-x", "thread-x", 42);
        let messages = vec![Message::user("u1"), Message::assistant("a1")];

        store.checkpoint("thread-x", &messages, &run).await.unwrap();

        let loaded_messages = store.load_messages("thread-x").await.unwrap().unwrap();
        assert_eq!(loaded_messages.len(), 2);

        let loaded_run = ThreadRunStore::load_run(&store, "run-x")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded_run.thread_id, "thread-x");
    }

    #[tokio::test]
    async fn checkpoint_overwrites_messages() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        let run1 = make_run("run-1", "t-1", 100);
        store
            .checkpoint("t-1", &[Message::user("old")], &run1)
            .await
            .unwrap();

        let run2 = make_run("run-2", "t-1", 200);
        store
            .checkpoint("t-1", &[Message::user("new")], &run2)
            .await
            .unwrap();

        let msgs = store.load_messages("t-1").await.unwrap().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text(), "new");
    }

    #[tokio::test]
    async fn load_messages_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        let result = store.load_messages("missing").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn latest_run_via_thread_run_store() {
        let tmp = TempDir::new().unwrap();
        let store = FileStore::new(tmp.path());
        let msgs = vec![Message::user("m")];
        store
            .checkpoint("t-1", &msgs, &make_run("r1", "t-1", 100))
            .await
            .unwrap();
        store
            .checkpoint("t-1", &msgs, &make_run("r2", "t-1", 200))
            .await
            .unwrap();

        let latest = ThreadRunStore::latest_run(&store, "t-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.run_id, "r2");
    }
}
