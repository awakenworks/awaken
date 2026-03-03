//! Shared persistence change-set types shared by runtime and storage.

use crate::runtime::state::SerializedAction;
use crate::thread::{Message, Thread};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tirea_state::TrackedPatch;

/// Monotonically increasing version for optimistic concurrency.
pub type Version = u64;

/// Reason for a checkpoint (delta).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CheckpointReason {
    UserMessage,
    AssistantTurnCommitted,
    ToolResultsCommitted,
    RunFinished,
}

/// An incremental change to a thread produced by a single step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadChangeSet {
    /// Which run produced this delta.
    pub run_id: String,
    /// Parent run (for sub-agent deltas).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    /// Why this delta was created.
    pub reason: CheckpointReason,
    /// New messages appended in this step.
    pub messages: Vec<Arc<Message>>,
    /// New patches appended in this step.
    pub patches: Vec<TrackedPatch>,
    /// Serialized state actions captured during this step (intent log).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<SerializedAction>,
    /// If `Some`, a full state snapshot was taken (replaces base state).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Value>,
}

impl ThreadChangeSet {
    /// Build a `ThreadChangeSet` from explicit delta components.
    pub fn from_parts(
        run_id: impl Into<String>,
        parent_run_id: Option<String>,
        reason: CheckpointReason,
        messages: Vec<Arc<Message>>,
        patches: Vec<TrackedPatch>,
        actions: Vec<SerializedAction>,
        snapshot: Option<Value>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            parent_run_id,
            reason,
            messages,
            patches,
            actions,
            snapshot,
        }
    }

    /// Apply this delta to a thread in place.
    ///
    /// Messages are deduplicated by `id` — if a message with the same id
    /// already exists in the thread it is skipped. Messages without an id
    /// are always appended.
    pub fn apply_to(&self, thread: &mut Thread) {
        if let Some(ref snapshot) = self.snapshot {
            thread.state = snapshot.clone();
            thread.patches.clear();
        }

        let mut existing_ids: HashSet<String> =
            thread.messages.iter().filter_map(|m| m.id.clone()).collect();
        for msg in &self.messages {
            if let Some(ref id) = msg.id {
                if !existing_ids.insert(id.clone()) {
                    continue;
                }
            }
            thread.messages.push(msg.clone());
        }
        thread.patches.extend(self.patches.iter().cloned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::{Message, Thread};
    use serde_json::json;

    fn sample_changeset_with_actions() -> ThreadChangeSet {
        ThreadChangeSet {
            run_id: "run-1".into(),
            parent_run_id: None,
            reason: CheckpointReason::AssistantTurnCommitted,
            messages: vec![Arc::new(Message::assistant("hello"))],
            patches: vec![],
            actions: vec![SerializedAction {
                state_type_name: "TestCounter".into(),
                base_path: "test_counter".into(),
                scope: crate::runtime::state::StateScope::Thread,
                call_id_override: None,
                payload: json!({"Increment": 1}),
            }],
            snapshot: None,
        }
    }

    #[test]
    fn test_changeset_serde_roundtrip_with_actions() {
        let cs = sample_changeset_with_actions();
        assert_eq!(cs.actions.len(), 1);

        let json = serde_json::to_string(&cs).unwrap();
        let restored: ThreadChangeSet = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.actions.len(), 1);
        assert_eq!(restored.actions[0].state_type_name, "TestCounter");
        assert_eq!(restored.actions[0].payload, json!({"Increment": 1}));
    }

    #[test]
    fn test_changeset_serde_backward_compat_no_actions() {
        // Simulate old JSON that has no `actions` field.
        let json = r#"{
            "run_id": "run-1",
            "reason": "RunFinished",
            "messages": [],
            "patches": []
        }"#;
        let cs: ThreadChangeSet = serde_json::from_str(json).unwrap();
        assert!(cs.actions.is_empty());
    }

    #[test]
    fn test_apply_to_deduplicates_messages() {
        let msg = Arc::new(Message::user("hello"));
        let delta = ThreadChangeSet {
            run_id: "run-1".into(),
            parent_run_id: None,
            reason: CheckpointReason::AssistantTurnCommitted,
            messages: vec![msg.clone()],
            patches: vec![],
            actions: vec![],
            snapshot: None,
        };

        let mut thread = Thread::new("t1");
        delta.apply_to(&mut thread);
        delta.apply_to(&mut thread);

        // The same message (by id) applied twice should appear only once.
        assert_eq!(
            thread.messages.len(),
            1,
            "apply_to should deduplicate messages by id"
        );
    }
}
