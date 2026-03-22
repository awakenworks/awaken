//! Thread types for persistent conversation state.

use std::collections::HashMap;

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
}

impl Thread {
    /// Create a new thread with a generated UUID v7 identifier.
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            metadata: ThreadMetadata::default(),
        }
    }

    /// Create a new thread with a specific identifier.
    pub fn with_id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            metadata: ThreadMetadata::default(),
        }
    }

    /// Set the title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.metadata.title = Some(title.into());
        self
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
}
