//! Shared persistence change-set types shared by runtime and storage.

use crate::runtime::state::SerializedStateAction;
use crate::runtime::RunStatus;
use crate::storage::RunOrigin;
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

/// Run-level metadata carried in a [`ThreadChangeSet`].
///
/// When present, the thread store uses this to maintain a run index.
/// Set on the first changeset of a run (to create the record) and the last
/// (to finalize status / termination).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMeta {
    pub agent_id: String,
    pub origin: RunOrigin,
    pub status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination_detail: Option<String>,
}

/// An incremental change to a thread produced by a single step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadChangeSet {
    /// Which run produced this delta.
    pub run_id: String,
    /// Parent run (for sub-agent deltas).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    /// Run-level metadata for run index maintenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_meta: Option<RunMeta>,
    /// Why this delta was created.
    pub reason: CheckpointReason,
    /// New messages appended in this step.
    pub messages: Vec<Arc<Message>>,
    /// New patches appended in this step.
    pub patches: Vec<TrackedPatch>,
    /// Serialized state actions captured during this step (intent log).
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "actions")]
    pub state_actions: Vec<SerializedStateAction>,
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
        state_actions: Vec<SerializedStateAction>,
        snapshot: Option<Value>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            parent_run_id,
            run_meta: None,
            reason,
            messages,
            patches,
            state_actions,
            snapshot,
        }
    }

    /// Attach run-level metadata for run index maintenance.
    #[must_use]
    pub fn with_run_meta(mut self, meta: RunMeta) -> Self {
        self.run_meta = Some(meta);
        self
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

        let mut existing_ids: HashSet<String> = thread
            .messages
            .iter()
            .filter_map(|m| m.id.clone())
            .collect();
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

    fn sample_changeset_with_state_actions() -> ThreadChangeSet {
        ThreadChangeSet {
            run_id: "run-1".into(),
            parent_run_id: None,
            run_meta: None,
            reason: CheckpointReason::AssistantTurnCommitted,
            messages: vec![Arc::new(Message::assistant("hello"))],
            patches: vec![],
            state_actions: vec![SerializedStateAction {
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
    fn test_changeset_serde_roundtrip_with_state_actions() {
        let cs = sample_changeset_with_state_actions();
        assert_eq!(cs.state_actions.len(), 1);

        let json = serde_json::to_string(&cs).unwrap();
        let restored: ThreadChangeSet = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.state_actions.len(), 1);
        assert_eq!(restored.state_actions[0].state_type_name, "TestCounter");
        assert_eq!(restored.state_actions[0].payload, json!({"Increment": 1}));
    }

    #[test]
    fn test_changeset_serde_backward_compat_without_state_actions() {
        // Simulate old JSON that has no `actions` field.
        let json = r#"{
            "run_id": "run-1",
            "reason": "RunFinished",
            "messages": [],
            "patches": []
        }"#;
        let cs: ThreadChangeSet = serde_json::from_str(json).unwrap();
        assert!(cs.state_actions.is_empty());
    }

    #[test]
    fn test_apply_to_deduplicates_messages() {
        let msg = Arc::new(Message::user("hello"));
        let delta = ThreadChangeSet {
            run_id: "run-1".into(),
            parent_run_id: None,
            run_meta: None,
            reason: CheckpointReason::AssistantTurnCommitted,
            messages: vec![msg.clone()],
            patches: vec![],
            state_actions: vec![],
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
