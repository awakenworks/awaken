//! Thread types for persistent conversation state.

use std::collections::HashMap;

use crate::contract::lifecycle::RunStatus;
use crate::contract::storage::{RunRecord, StorageError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Normalize a lineage identifier by trimming whitespace and treating blanks as absent.
#[must_use]
pub fn normalize_lineage_id(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Normalize an owned lineage identifier by trimming whitespace and treating blanks as absent.
#[must_use]
pub fn normalize_lineage_id_owned(value: Option<String>) -> Option<String> {
    normalize_lineage_id(value.as_deref())
}

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
    /// External resource or tenant grouping for this thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    /// Parent thread for child or delegated conversations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
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
    /// `updated_at` watermark for the projected latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run_updated_at: Option<u64>,
}

impl Thread {
    /// Create a new thread with a generated UUID v7 identifier.
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            resource_id: None,
            parent_thread_id: None,
            metadata: ThreadMetadata::default(),
            active_run_id: None,
            open_run_id: None,
            latest_run_id: None,
            latest_run_updated_at: None,
        }
    }

    /// Create a new thread with a specific identifier.
    pub fn with_id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            resource_id: None,
            parent_thread_id: None,
            metadata: ThreadMetadata::default(),
            active_run_id: None,
            open_run_id: None,
            latest_run_id: None,
            latest_run_updated_at: None,
        }
    }

    /// Set the title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.metadata.title = Some(title.into());
        self
    }

    /// Set the external resource grouping.
    #[must_use]
    pub fn with_resource_id(mut self, resource_id: impl Into<String>) -> Self {
        self.resource_id = normalize_lineage_id_owned(Some(resource_id.into()));
        self
    }

    /// Set the parent thread identifier.
    #[must_use]
    pub fn with_parent_thread_id(mut self, parent_thread_id: impl Into<String>) -> Self {
        self.parent_thread_id = normalize_lineage_id_owned(Some(parent_thread_id.into()));
        self
    }

    /// Normalize lineage identifiers in-place.
    pub fn normalize_lineage(&mut self) {
        self.resource_id = normalize_lineage_id_owned(self.resource_id.take());
        self.parent_thread_id = normalize_lineage_id_owned(self.parent_thread_id.take());
    }

    /// Validate model-level invariants before persisting a thread projection.
    pub fn validate_for_persist(&self) -> Result<(), StorageError> {
        require_non_empty("thread id", &self.id)?;
        require_optional_non_empty("thread resource_id", self.resource_id.as_deref())?;
        require_optional_non_empty("thread parent_thread_id", self.parent_thread_id.as_deref())?;
        require_optional_non_empty("thread active_run_id", self.active_run_id.as_deref())?;
        require_optional_non_empty("thread open_run_id", self.open_run_id.as_deref())?;
        require_optional_non_empty("thread latest_run_id", self.latest_run_id.as_deref())?;

        if normalize_lineage_id(self.parent_thread_id.as_deref()).as_deref() == Some(self.id.trim())
        {
            return Err(StorageError::Validation(format!(
                "thread '{}' cannot parent itself",
                self.id
            )));
        }

        Ok(())
    }

    /// Ensure timestamps are initialized and mark the thread as updated.
    pub fn touch(&mut self, now: u64) {
        self.metadata.created_at.get_or_insert(now);
        self.metadata.updated_at = Some(now);
    }

    /// Update the thread's run pointers from a durable run record.
    pub fn apply_run_projection(&mut self, run: &RunRecord) {
        let is_current_projection = self
            .latest_run_updated_at
            .is_none_or(|latest_updated_at| run.updated_at >= latest_updated_at);
        if !is_current_projection {
            if run.status == RunStatus::Done {
                self.clear_run_projection_if_matches(&run.run_id);
            }
            return;
        }

        self.latest_run_id = Some(run.run_id.clone());
        self.latest_run_updated_at = Some(run.updated_at);
        if self.parent_thread_id.is_none() {
            self.parent_thread_id = normalize_lineage_id(
                run.request
                    .as_ref()
                    .and_then(|request| request.parent_thread_id.as_deref()),
            );
        }
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
                self.clear_run_projection_if_matches(&run.run_id);
            }
        }
    }

    fn clear_run_projection_if_matches(&mut self, run_id: &str) {
        if self.active_run_id.as_deref() == Some(run_id) {
            self.active_run_id = None;
        }
        if self.open_run_id.as_deref() == Some(run_id) {
            self.open_run_id = None;
        }
    }
}

fn require_non_empty(field: &str, value: &str) -> Result<(), StorageError> {
    if value.trim().is_empty() {
        return Err(StorageError::Validation(format!(
            "{field} must not be empty"
        )));
    }
    Ok(())
}

fn require_optional_non_empty(field: &str, value: Option<&str>) -> Result<(), StorageError> {
    if let Some(value) = value {
        require_non_empty(field, value)?;
    }
    Ok(())
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
        thread.resource_id = Some("resource-1".to_string());
        thread.parent_thread_id = Some("parent-1".to_string());

        let json_str = serde_json::to_string(&thread).unwrap();
        let restored: Thread = serde_json::from_str(&json_str).unwrap();

        assert_eq!(restored.id, "t-1");
        assert_eq!(restored.resource_id.as_deref(), Some("resource-1"));
        assert_eq!(restored.parent_thread_id.as_deref(), Some("parent-1"));
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
    fn thread_lineage_builders() {
        let thread = Thread::with_id("t-1")
            .with_resource_id("resource-1")
            .with_parent_thread_id("parent-1");

        assert_eq!(thread.resource_id.as_deref(), Some("resource-1"));
        assert_eq!(thread.parent_thread_id.as_deref(), Some("parent-1"));
    }

    #[test]
    fn normalize_lineage_id_trims_and_drops_blank_values() {
        assert_eq!(
            normalize_lineage_id(Some(" parent-1 ")),
            Some("parent-1".into())
        );
        assert_eq!(normalize_lineage_id(Some("   ")), None);
        assert_eq!(normalize_lineage_id(None), None);
    }

    #[test]
    fn normalize_lineage_updates_thread_fields() {
        let mut thread = Thread::with_id("t-1");
        thread.resource_id = Some(" resource-1 ".into());
        thread.parent_thread_id = Some("   ".into());

        thread.normalize_lineage();

        assert_eq!(thread.resource_id.as_deref(), Some("resource-1"));
        assert_eq!(thread.parent_thread_id, None);
    }

    #[test]
    fn thread_validate_rejects_empty_id() {
        let thread = Thread::with_id(" ");

        let err = thread.validate_for_persist().unwrap_err();

        assert!(matches!(err, StorageError::Validation(message) if message.contains("thread id")));
    }

    #[test]
    fn thread_validate_rejects_self_parent() {
        let thread = Thread::with_id("thread-1").with_parent_thread_id(" thread-1 ");

        let err = thread.validate_for_persist().unwrap_err();

        assert!(
            matches!(err, StorageError::Validation(message) if message.contains("parent itself"))
        );
    }

    #[test]
    fn touch_initializes_created_and_updated_at() {
        let mut thread = Thread::with_id("t-1");

        thread.touch(1234);

        assert_eq!(thread.metadata.created_at, Some(1234));
        assert_eq!(thread.metadata.updated_at, Some(1234));
    }

    #[test]
    fn touch_preserves_created_at_and_refreshes_updated_at() {
        let mut thread = Thread::with_id("t-1");
        thread.metadata.created_at = Some(1000);
        thread.metadata.updated_at = Some(1500);

        thread.touch(2000);

        assert_eq!(thread.metadata.created_at, Some(1000));
        assert_eq!(thread.metadata.updated_at, Some(2000));
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
        assert!(!json_str.contains("resource_id"));
        assert!(!json_str.contains("parent_thread_id"));
    }

    fn run_record(run_id: &str, status: RunStatus) -> RunRecord {
        RunRecord {
            run_id: run_id.to_string(),
            thread_id: "thread-1".to_string(),
            agent_id: "agent-1".to_string(),
            parent_run_id: None,
            resolution_id: None,
            activation: None,
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

    #[test]
    fn apply_run_projection_ignores_older_run_projection() {
        let mut thread = Thread::with_id("thread-1");
        let mut newer = run_record("run-new", RunStatus::Running);
        newer.updated_at = 20;
        let mut older = run_record("run-old", RunStatus::Running);
        older.updated_at = 10;

        thread.apply_run_projection(&newer);
        thread.apply_run_projection(&older);

        assert_eq!(thread.latest_run_id.as_deref(), Some("run-new"));
        assert_eq!(thread.active_run_id.as_deref(), Some("run-new"));
        assert_eq!(thread.open_run_id.as_deref(), Some("run-new"));
    }

    #[test]
    fn apply_run_projection_sets_parent_thread_id_when_missing() {
        let mut thread = Thread::with_id("thread-1");
        let mut run = run_record("run-1", RunStatus::Created);
        run.request = Some(crate::contract::storage::RunRequestSnapshot {
            parent_thread_id: Some(" parent-thread ".to_string()),
            ..Default::default()
        });

        thread.apply_run_projection(&run);

        assert_eq!(thread.parent_thread_id.as_deref(), Some("parent-thread"));
    }

    #[test]
    fn apply_run_projection_preserves_existing_parent_thread_id() {
        let mut thread = Thread::with_id("thread-1").with_parent_thread_id("existing-parent");
        let mut run = run_record("run-1", RunStatus::Created);
        run.request = Some(crate::contract::storage::RunRequestSnapshot {
            parent_thread_id: Some("new-parent".to_string()),
            ..Default::default()
        });

        thread.apply_run_projection(&run);

        assert_eq!(thread.parent_thread_id.as_deref(), Some("existing-parent"));
    }
}
