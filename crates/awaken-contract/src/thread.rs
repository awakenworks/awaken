//! Thread types for persistent conversation state.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract::message::Message;

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

/// A persistent conversation thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    /// Unique thread identifier (UUID v7).
    pub id: String,
    /// Thread metadata (timestamps, title, custom data).
    #[serde(default)]
    pub metadata: ThreadMetadata,
    /// Ordered message history.
    pub messages: Vec<Message>,
}

impl Thread {
    /// Create a new thread with a generated UUID v7 identifier.
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            metadata: ThreadMetadata::default(),
            messages: Vec::new(),
        }
    }

    /// Create a new thread with a specific identifier.
    pub fn with_id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            metadata: ThreadMetadata::default(),
            messages: Vec::new(),
        }
    }

    /// Set the title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.metadata.title = Some(title.into());
        self
    }

    /// Add a message.
    #[must_use]
    pub fn with_message(mut self, msg: Message) -> Self {
        self.messages.push(msg);
        self
    }

    /// Add multiple messages.
    #[must_use]
    pub fn with_messages(mut self, msgs: impl IntoIterator<Item = Message>) -> Self {
        self.messages.extend(msgs);
        self
    }

    /// Get the number of messages.
    pub fn message_count(&self) -> usize {
        self.messages.len()
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
        assert!(thread.messages.is_empty());
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
    fn thread_with_messages() {
        let thread = Thread::new()
            .with_message(Message::user("hello"))
            .with_message(Message::assistant("hi"));
        assert_eq!(thread.message_count(), 2);
    }

    #[test]
    fn thread_with_messages_batch() {
        let msgs = vec![
            Message::user("a"),
            Message::assistant("b"),
            Message::user("c"),
        ];
        let thread = Thread::new().with_messages(msgs);
        assert_eq!(thread.message_count(), 3);
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
        let thread = thread
            .with_message(Message::user("hello"))
            .with_message(Message::assistant("world"));

        let json_str = serde_json::to_string(&thread).unwrap();
        let restored: Thread = serde_json::from_str(&json_str).unwrap();

        assert_eq!(restored.id, "t-1");
        assert_eq!(restored.metadata.title.as_deref(), Some("My Thread"));
        assert_eq!(restored.metadata.created_at, Some(1000));
        assert_eq!(restored.metadata.updated_at, Some(2000));
        assert_eq!(restored.metadata.custom["env"], json!("prod"));
        assert_eq!(restored.message_count(), 2);
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
        assert!(thread.messages.is_empty());
    }

    #[test]
    fn distinct_threads_get_distinct_ids() {
        let a = Thread::new();
        let b = Thread::new();
        assert_ne!(a.id, b.id);
    }
}
