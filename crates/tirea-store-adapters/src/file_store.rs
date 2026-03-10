use crate::file_utils;
use async_trait::async_trait;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tirea_contract::storage::{
    paginate_runs_in_memory, Committed, RunPage, RunQuery, RunRecord, ThreadHead, ThreadListPage,
    ThreadListQuery, ThreadReader, ThreadStoreError, ThreadWriter, VersionPrecondition,
};
use tirea_contract::{Thread, ThreadChangeSet, Version};

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub struct FileStore {
    base_path: PathBuf,
}

impl FileStore {
    /// Create a new file storage with the given base path.
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
        }
    }

    pub(super) fn thread_path(&self, thread_id: &str) -> Result<PathBuf, ThreadStoreError> {
        Self::validate_thread_id(thread_id)?;
        Ok(self.base_path.join(format!("{}.json", thread_id)))
    }

    fn validate_thread_id(thread_id: &str) -> Result<(), ThreadStoreError> {
        file_utils::validate_fs_id(thread_id, "thread id").map_err(ThreadStoreError::InvalidId)
    }
}

#[async_trait]
impl ThreadWriter for FileStore {
    async fn create(&self, thread: &Thread) -> Result<Committed, ThreadStoreError> {
        let path = self.thread_path(&thread.id)?;
        if path.exists() {
            return Err(ThreadStoreError::AlreadyExists);
        }
        let mut thread = thread.clone();
        let now = now_millis();
        if thread.metadata.created_at.is_none() {
            thread.metadata.created_at = Some(now);
        }
        thread.metadata.updated_at = Some(now);
        let head = ThreadHead { thread, version: 0 };
        self.save_head(&head).await?;
        Ok(Committed { version: 0 })
    }

    async fn append(
        &self,
        thread_id: &str,
        delta: &ThreadChangeSet,
        precondition: VersionPrecondition,
    ) -> Result<Committed, ThreadStoreError> {
        let head = self
            .load_head(thread_id)
            .await?
            .ok_or_else(|| ThreadStoreError::NotFound(thread_id.to_string()))?;

        if let VersionPrecondition::Exact(expected) = precondition {
            if head.version != expected {
                return Err(ThreadStoreError::VersionConflict {
                    expected,
                    actual: head.version,
                });
            }
        }

        let mut thread = head.thread;
        delta.apply_to(&mut thread);
        thread.metadata.updated_at = Some(now_millis());
        let new_version = head.version + 1;
        let new_head = ThreadHead {
            thread,
            version: new_version,
        };
        self.save_head(&new_head).await?;
        self.upsert_run_from_changeset(thread_id, delta).await?;
        Ok(Committed {
            version: new_version,
        })
    }

    async fn delete(&self, thread_id: &str) -> Result<(), ThreadStoreError> {
        let path = self.thread_path(thread_id)?;
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }
        Ok(())
    }

    async fn save(&self, thread: &Thread) -> Result<(), ThreadStoreError> {
        let next_version = self
            .load_head(&thread.id)
            .await?
            .map_or(0, |head| head.version.saturating_add(1));
        let mut thread = thread.clone();
        let now = now_millis();
        thread.metadata.updated_at = Some(now);
        if thread.metadata.created_at.is_none() {
            thread.metadata.created_at = Some(now);
        }
        let head = ThreadHead {
            thread,
            version: next_version,
        };
        self.save_head(&head).await
    }
}

#[async_trait]
impl ThreadReader for FileStore {
    async fn load(&self, thread_id: &str) -> Result<Option<ThreadHead>, ThreadStoreError> {
        self.load_head(thread_id).await
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, ThreadStoreError> {
        self.load_run_record(run_id).await
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, ThreadStoreError> {
        let records = self.load_all_run_records().await?;
        Ok(paginate_runs_in_memory(&records, query))
    }

    async fn active_run_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Option<RunRecord>, ThreadStoreError> {
        let records = self.load_all_run_records().await?;
        Ok(records
            .into_iter()
            .filter(|r| r.thread_id == thread_id && !r.status.is_terminal())
            .max_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.updated_at.cmp(&b.updated_at))
                    .then_with(|| a.run_id.cmp(&b.run_id))
            }))
    }

    async fn list_threads(
        &self,
        query: &ThreadListQuery,
    ) -> Result<ThreadListPage, ThreadStoreError> {
        let mut all = file_utils::scan_json_stems(&self.base_path).await?;

        // Filter by resource_id if specified.
        if let Some(ref resource_id) = query.resource_id {
            let mut filtered = Vec::new();
            for id in &all {
                if let Some(head) = self.load(id).await? {
                    if head.thread.resource_id.as_deref() == Some(resource_id.as_str()) {
                        filtered.push(id.clone());
                    }
                }
            }
            all = filtered;
        }

        // Filter by parent_thread_id if specified.
        if let Some(ref parent_thread_id) = query.parent_thread_id {
            let mut filtered = Vec::new();
            for id in &all {
                if let Some(head) = self.load(id).await? {
                    if head.thread.parent_thread_id.as_deref() == Some(parent_thread_id.as_str()) {
                        filtered.push(id.clone());
                    }
                }
            }
            all = filtered;
        }

        all.sort();
        let total = all.len();
        let limit = query.limit.clamp(1, 200);
        let offset = query.offset.min(total);
        let end = (offset + limit + 1).min(total);
        let slice = &all[offset..end];
        let has_more = slice.len() > limit;
        let items: Vec<String> = slice.iter().take(limit).cloned().collect();
        Ok(ThreadListPage {
            items,
            total,
            has_more,
        })
    }
}

impl FileStore {
    fn runs_dir(&self) -> PathBuf {
        self.base_path.join("_runs")
    }

    fn run_path(&self, run_id: &str) -> Result<PathBuf, ThreadStoreError> {
        file_utils::validate_fs_id(run_id, "run id").map_err(ThreadStoreError::InvalidId)?;
        Ok(self.runs_dir().join(format!("{run_id}.json")))
    }

    async fn save_run_record(&self, record: &RunRecord) -> Result<(), ThreadStoreError> {
        let payload = serde_json::to_string_pretty(record)
            .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;
        let filename = format!("{}.json", record.run_id);
        file_utils::atomic_json_write(&self.runs_dir(), &filename, &payload)
            .await
            .map_err(ThreadStoreError::Io)
    }

    async fn load_run_record(&self, run_id: &str) -> Result<Option<RunRecord>, ThreadStoreError> {
        let path = self.run_path(run_id)?;
        if !path.exists() {
            return Ok(None);
        }
        let content = tokio::fs::read_to_string(path).await?;
        let record: RunRecord = serde_json::from_str(&content)
            .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;
        Ok(Some(record))
    }

    async fn load_all_run_records(&self) -> Result<Vec<RunRecord>, ThreadStoreError> {
        let dir = self.runs_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut entries = tokio::fs::read_dir(&dir).await?;
        let mut records = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let content = tokio::fs::read_to_string(path).await?;
            let record: RunRecord = serde_json::from_str(&content)
                .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;
            records.push(record);
        }
        Ok(records)
    }

    async fn upsert_run_from_changeset(
        &self,
        thread_id: &str,
        delta: &ThreadChangeSet,
    ) -> Result<(), ThreadStoreError> {
        if delta.run_id.is_empty() {
            return Ok(());
        }
        let now = now_millis();
        if let Some(meta) = &delta.run_meta {
            let mut record = self
                .load_run_record(&delta.run_id)
                .await?
                .unwrap_or_else(|| {
                    RunRecord::new(
                        &delta.run_id,
                        thread_id,
                        &meta.agent_id,
                        meta.origin,
                        meta.status,
                        now,
                    )
                });
            record.status = meta.status;
            record.agent_id.clone_from(&meta.agent_id);
            record.origin = meta.origin;
            record.thread_id = thread_id.to_string();
            if record.parent_run_id.is_none() {
                record.parent_run_id.clone_from(&delta.parent_run_id);
            }
            if record.parent_thread_id.is_none() {
                record.parent_thread_id.clone_from(&meta.parent_thread_id);
            }
            record.termination_code.clone_from(&meta.termination_code);
            record
                .termination_detail
                .clone_from(&meta.termination_detail);
            record.updated_at = now;
            self.save_run_record(&record).await?;
        } else if let Some(mut record) = self.load_run_record(&delta.run_id).await? {
            record.updated_at = now;
            self.save_run_record(&record).await?;
        }
        Ok(())
    }

    /// Load a thread head (thread + version) from file.
    async fn load_head(&self, thread_id: &str) -> Result<Option<ThreadHead>, ThreadStoreError> {
        let path = self.thread_path(thread_id)?;
        if !path.exists() {
            return Ok(None);
        }
        let content = tokio::fs::read_to_string(&path).await?;
        // Try to parse as ThreadHead first (new format with version).
        if let Ok(head) = serde_json::from_str::<VersionedThread>(&content) {
            let thread: Thread = serde_json::from_str(&content)
                .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;
            Ok(Some(ThreadHead {
                thread,
                version: head._version.unwrap_or(0),
            }))
        } else {
            let thread: Thread = serde_json::from_str(&content)
                .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;
            Ok(Some(ThreadHead { thread, version: 0 }))
        }
    }

    /// Save a thread head (thread + version) to file atomically.
    async fn save_head(&self, head: &ThreadHead) -> Result<(), ThreadStoreError> {
        // Embed version into the JSON
        let mut v = serde_json::to_value(&head.thread)
            .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("_version".to_string(), serde_json::json!(head.version));
        }
        let content = serde_json::to_string_pretty(&v)
            .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;

        let filename = format!("{}.json", head.thread.id);
        file_utils::atomic_json_write(&self.base_path, &filename, &content)
            .await
            .map_err(ThreadStoreError::Io)
    }
}

/// Helper for extracting the `_version` field from serialized thread JSON.
#[derive(Deserialize)]
struct VersionedThread {
    #[serde(default)]
    _version: Option<Version>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tirea_contract::{
        storage::ThreadReader, CheckpointReason, Message, MessageQuery, ThreadWriter,
    };
    use tirea_state::{path, Op, Patch, TrackedPatch};

    fn make_thread_with_messages(thread_id: &str, n: usize) -> Thread {
        let mut thread = Thread::new(thread_id);
        for i in 0..n {
            thread = thread.with_message(Message::user(format!("msg-{i}")));
        }
        thread
    }

    #[tokio::test]
    async fn file_storage_save_load_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileStore::new(temp_dir.path());

        let thread = Thread::new("test-1").with_message(Message::user("hello"));
        storage.save(&thread).await.unwrap();

        let loaded = storage.load_thread("test-1").await.unwrap().unwrap();
        assert_eq!(loaded.id, "test-1");
        assert_eq!(loaded.message_count(), 1);
    }

    #[tokio::test]
    async fn file_storage_list_and_delete() {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileStore::new(temp_dir.path());

        storage.create(&Thread::new("thread-a")).await.unwrap();
        storage.create(&Thread::new("thread-b")).await.unwrap();
        storage.create(&Thread::new("thread-c")).await.unwrap();

        let mut ids = storage.list().await.unwrap();
        ids.sort();
        assert_eq!(ids, vec!["thread-a", "thread-b", "thread-c"]);

        storage.delete("thread-b").await.unwrap();
        let mut ids = storage.list().await.unwrap();
        ids.sort();
        assert_eq!(ids, vec!["thread-a", "thread-c"]);
    }

    #[tokio::test]
    async fn file_storage_message_queries() {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileStore::new(temp_dir.path());
        let thread = make_thread_with_messages("t1", 10);
        storage.save(&thread).await.unwrap();

        let page = storage
            .load_messages(
                "t1",
                &MessageQuery {
                    after: Some(4),
                    limit: 3,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.messages.len(), 3);
        assert_eq!(page.messages[0].cursor, 5);
        assert_eq!(page.messages[0].message.content, "msg-5");
        assert_eq!(storage.message_count("t1").await.unwrap(), 10);
    }

    #[tokio::test]
    async fn file_storage_append_and_versioning() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::new(temp_dir.path());
        store.create(&Thread::new("t1")).await.unwrap();

        let d1 = ThreadChangeSet {
            run_id: "run-1".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason: CheckpointReason::UserMessage,
            messages: vec![Arc::new(Message::user("hello"))],
            patches: vec![],
            state_actions: vec![],
            snapshot: None,
        };
        let c1 = store
            .append("t1", &d1, VersionPrecondition::Exact(0))
            .await
            .unwrap();
        assert_eq!(c1.version, 1);

        let d2 = ThreadChangeSet {
            run_id: "run-1".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason: CheckpointReason::AssistantTurnCommitted,
            messages: vec![Arc::new(Message::assistant("hi"))],
            patches: vec![TrackedPatch::new(
                Patch::new().with_op(Op::set(path!("greeted"), json!(true))),
            )],
            state_actions: vec![],
            snapshot: None,
        };
        let c2 = store
            .append("t1", &d2, VersionPrecondition::Exact(1))
            .await
            .unwrap();
        assert_eq!(c2.version, 2);

        let d3 = ThreadChangeSet {
            run_id: "run-1".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason: CheckpointReason::RunFinished,
            messages: vec![],
            patches: vec![],
            state_actions: vec![],
            snapshot: Some(json!({"greeted": true})),
        };
        let c3 = store
            .append("t1", &d3, VersionPrecondition::Exact(2))
            .await
            .unwrap();
        assert_eq!(c3.version, 3);

        let store2 = FileStore::new(temp_dir.path());
        let head = store2.load("t1").await.unwrap().unwrap();
        assert_eq!(head.version, 3);
        assert_eq!(head.thread.message_count(), 2);
        assert!(head.thread.patches.is_empty());
        assert_eq!(head.thread.state, json!({"greeted": true}));
    }

    #[tokio::test]
    async fn file_storage_tool_call_message_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileStore::new(temp_dir.path());

        let tool_call = tirea_contract::ToolCall::new("call_1", "search", json!({"query": "rust"}));
        let thread = Thread::new("tool-rt")
            .with_message(Message::user("Find info about Rust"))
            .with_message(Message::assistant_with_tool_calls(
                "Let me search for that.",
                vec![tool_call],
            ))
            .with_message(Message::tool(
                "call_1",
                r#"{"result": "Rust is a language"}"#,
            ))
            .with_message(Message::assistant(
                "Rust is a systems programming language.",
            ));

        storage.save(&thread).await.unwrap();
        let loaded = storage.load_thread("tool-rt").await.unwrap().unwrap();

        assert_eq!(loaded.message_count(), 4);

        // Assistant message with tool_calls
        let assistant_msg = &loaded.messages[1];
        assert_eq!(assistant_msg.role, tirea_contract::Role::Assistant);
        let calls = assistant_msg.tool_calls.as_ref().expect("tool_calls lost");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].arguments, json!({"query": "rust"}));

        // Tool response message with tool_call_id
        let tool_msg = &loaded.messages[2];
        assert_eq!(tool_msg.role, tirea_contract::Role::Tool);
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_msg.content, r#"{"result": "Rust is a language"}"#);
    }

    #[tokio::test]
    async fn file_storage_tool_call_message_roundtrip_via_append() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::new(temp_dir.path());
        store.create(&Thread::new("tool-append")).await.unwrap();

        let tool_call =
            tirea_contract::ToolCall::new("call_42", "calculator", json!({"expr": "6*7"}));
        let delta = ThreadChangeSet {
            run_id: "run-1".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason: CheckpointReason::AssistantTurnCommitted,
            messages: vec![
                Arc::new(Message::assistant_with_tool_calls(
                    "Calculating...",
                    vec![tool_call],
                )),
                Arc::new(Message::tool("call_42", r#"{"answer": 42}"#)),
            ],
            patches: vec![],
            state_actions: vec![],
            snapshot: None,
        };
        store
            .append("tool-append", &delta, VersionPrecondition::Exact(0))
            .await
            .unwrap();

        let head = store.load("tool-append").await.unwrap().unwrap();
        assert_eq!(head.thread.message_count(), 2);

        let calls = head.thread.messages[0]
            .tool_calls
            .as_ref()
            .expect("tool_calls lost after append");
        assert_eq!(calls[0].id, "call_42");
        assert_eq!(calls[0].name, "calculator");

        assert_eq!(
            head.thread.messages[1].tool_call_id.as_deref(),
            Some("call_42")
        );
    }

    #[tokio::test]
    async fn file_storage_timestamps_populated() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::new(temp_dir.path());

        // create() populates both timestamps
        store.create(&Thread::new("ts-1")).await.unwrap();
        let head = store.load("ts-1").await.unwrap().unwrap();
        assert!(head.thread.metadata.created_at.is_some());
        assert!(head.thread.metadata.updated_at.is_some());
        let created = head.thread.metadata.created_at.unwrap();
        let updated = head.thread.metadata.updated_at.unwrap();
        assert!(created > 0);
        assert_eq!(created, updated);

        // append() updates updated_at
        let delta = ThreadChangeSet {
            run_id: "run-1".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason: CheckpointReason::UserMessage,
            messages: vec![Arc::new(Message::user("hello"))],
            patches: vec![],
            state_actions: vec![],
            snapshot: None,
        };
        store
            .append("ts-1", &delta, VersionPrecondition::Exact(0))
            .await
            .unwrap();
        let head = store.load("ts-1").await.unwrap().unwrap();
        assert!(head.thread.metadata.updated_at.unwrap() >= updated);
        assert_eq!(head.thread.metadata.created_at.unwrap(), created);

        // save() populates created_at if missing, updates updated_at
        let thread = Thread::new("ts-2");
        assert!(thread.metadata.created_at.is_none());
        store.save(&thread).await.unwrap();
        let head = store.load("ts-2").await.unwrap().unwrap();
        assert!(head.thread.metadata.created_at.is_some());
        assert!(head.thread.metadata.updated_at.is_some());
    }

    #[test]
    fn file_storage_rejects_path_traversal() {
        let storage = FileStore::new("/base/path");
        assert!(storage.thread_path("../../etc/passwd").is_err());
        assert!(storage.thread_path("foo/bar").is_err());
        assert!(storage.thread_path("foo\\bar").is_err());
        assert!(storage.thread_path("").is_err());
        assert!(storage.thread_path("foo\0bar").is_err());
    }
}
