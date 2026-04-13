//! Thread types for persistent conversation state.

use std::collections::HashMap;

use crate::contract::lifecycle::RunStatus;
use crate::contract::storage::RunRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Thread metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadMetadata {
    /// Creation timestamp (unix millis).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
    /// Last update timestamp (unix millis).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    /// Optional thread title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Custom metadata key-value pairs.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom: HashMap<String, Value>,
}

/// A persistent conversation thread (metadata only).
///
/// Messages are stored separately via `ThreadStore::load_messages` /
/// `ThreadStore::save_messages` to maintain a single source of truth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    /// Unique thread identifier (UUID v7).
    pub id: String,
    /// Thread metadata (timestamps, title, custom data).
    #[serde(default)]
    pub metadata: ThreadMetadata,
    /// Run currently executing on a worker for this thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_id: Option<String>,
    /// Current unfinished user intent for this thread. Waiting runs remain open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_run_id: Option<String>,
    /// Most recently known run for this thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run_id: Option<String>,
}

impl Thread {
    /// Create a new thread with a generated UUID v7 identifier.
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            metadata: ThreadMetadata::default(),
            active_run_id: None,
            open_run_id: None,
            latest_run_id: None,
        }
    }

    /// Create a new thread with a specific identifier.
    pub fn with_id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            metadata: ThreadMetadata::default(),
            active_run_id: None,
            open_run_id: None,
            latest_run_id: None,
        }
    }

    /// Set the title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.metadata.title = Some(title.into());
        self
    }

    /// Update the thread's run pointers from a durable run record.
    pub fn apply_run_projection(&mut self, run: &RunRecord) {
        self.latest_run_id = Some(run.run_id.clone());
        match run.status {
            RunStatus::Created => {
                self.active_run_id = None;
                self.open_run_id = Some(run.run_id.clone());
            }
            RunStatus::Running => {
                self.active_run_id = Some(run.run_id.clone());
                self.open_run_id = Some(run.run_id.clone());
            }
            RunStatus::Waiting => {
                self.active_run_id = None;
                self.open_run_id = Some(run.run_id.clone());
            }
            RunStatus::Done => {
                if self.active_run_id.as_deref() == Some(run.run_id.as_str()) {
                    self.active_run_id = None;
                }
                if self.open_run_id.as_deref() == Some(run.run_id.as_str()) {
                    self.open_run_id = None;
                }
            }
        }
    }
}

impl Default for Thread {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::lifecycle::RunStatus;
    use crate::contract::storage::RunRecord;
    use serde_json::json;

    #[test]
    fn thread_new_generates_uuid_v7() {
        let thread = Thread::new();
        assert_eq!(thread.id.len(), 36);
        assert_eq!(&thread.id[14..15], "7", "should be UUID v7");
        assert!(thread.metadata.title.is_none());
    }

    #[test]
    fn thread_with_id() {
        let thread = Thread::with_id("my-thread-1");
        assert_eq!(thread.id, "my-thread-1");
    }

    #[test]
    fn thread_with_title() {
        let thread = Thread::new().with_title("Test Chat");
        assert_eq!(thread.metadata.title.as_deref(), Some("Test Chat"));
    }

    #[test]
    fn thread_serialization_roundtrip() {
        let mut thread = Thread::with_id("t-1").with_title("My Thread");
        thread.metadata.created_at = Some(1000);
        thread.metadata.updated_at = Some(2000);
        thread
            .metadata
            .custom
            .insert("env".to_string(), json!("prod"));

        let json_str = serde_json::to_string(&thread).unwrap();
        let restored: Thread = serde_json::from_str(&json_str).unwrap();

        assert_eq!(restored.id, "t-1");
        assert_eq!(restored.metadata.title.as_deref(), Some("My Thread"));
        assert_eq!(restored.metadata.created_at, Some(1000));
        assert_eq!(restored.metadata.updated_at, Some(2000));
        assert_eq!(restored.metadata.custom["env"], json!("prod"));
    }

    #[test]
    fn thread_metadata_default() {
        let meta = ThreadMetadata::default();
        assert!(meta.created_at.is_none());
        assert!(meta.updated_at.is_none());
        assert!(meta.title.is_none());
        assert!(meta.custom.is_empty());
    }

    #[test]
    fn thread_metadata_omits_empty_fields() {
        let meta = ThreadMetadata::default();
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("created_at"));
        assert!(!json.contains("updated_at"));
        assert!(!json.contains("title"));
        assert!(!json.contains("custom"));
    }

    #[test]
    fn thread_default_is_new() {
        let thread = Thread::default();
        assert_eq!(thread.id.len(), 36);
    }

    #[test]
    fn distinct_threads_get_distinct_ids() {
        let a = Thread::new();
        let b = Thread::new();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn thread_with_custom_metadata() {
        let mut thread = Thread::with_id("t-1");
        thread.metadata.created_at = Some(1000);
        thread.metadata.updated_at = Some(2000);
        thread
            .metadata
            .custom
            .insert("env".to_string(), json!("prod"));

        assert_eq!(thread.metadata.created_at, Some(1000));
        assert_eq!(thread.metadata.custom["env"], json!("prod"));
    }

    #[test]
    fn thread_with_title_chaining() {
        let thread = Thread::with_id("t-1").with_title("Test");
        assert_eq!(thread.metadata.title.as_deref(), Some("Test"));
    }

    #[test]
    fn thread_metadata_custom_preserved_in_serde() {
        let mut thread = Thread::with_id("t-1");
        thread.metadata.custom.insert("key".to_string(), json!(42));
        let json_str = serde_json::to_string(&thread).unwrap();
        let restored: Thread = serde_json::from_str(&json_str).unwrap();
        assert_eq!(restored.metadata.custom["key"], json!(42));
    }

    #[test]
    fn thread_empty_metadata_is_compact() {
        let thread = Thread::with_id("t-1");
        let json_str = serde_json::to_string(&thread).unwrap();
        // Empty custom map should be omitted
        assert!(!json_str.contains("custom"));
    }

    fn run_record(run_id: &str, status: RunStatus) -> RunRecord {
        RunRecord {
            run_id: run_id.to_string(),
            thread_id: "thread-1".to_string(),
            agent_id: "agent-1".to_string(),
            parent_run_id: None,
            request: None,
            input: None,
            output: None,
            status,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: None,
            outcome: None,
            created_at: 1,
            started_at: None,
            finished_at: None,
            updated_at: 1,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        }
    }

    #[test]
    fn thread_run_projection_keeps_waiting_run_open_but_not_active() {
        let mut thread = Thread::with_id("thread-1");
        thread.apply_run_projection(&run_record("run-1", RunStatus::Created));
        assert_eq!(thread.open_run_id.as_deref(), Some("run-1"));
        assert!(thread.active_run_id.is_none());

        thread.apply_run_projection(&run_record("run-1", RunStatus::Running));
        assert_eq!(thread.open_run_id.as_deref(), Some("run-1"));
        assert_eq!(thread.active_run_id.as_deref(), Some("run-1"));

        thread.apply_run_projection(&run_record("run-1", RunStatus::Waiting));
        assert_eq!(thread.open_run_id.as_deref(), Some("run-1"));
        assert!(thread.active_run_id.is_none());

        thread.apply_run_projection(&run_record("run-1", RunStatus::Done));
        assert!(thread.open_run_id.is_none());
        assert!(thread.active_run_id.is_none());
        assert_eq!(thread.latest_run_id.as_deref(), Some("run-1"));
    }
}
