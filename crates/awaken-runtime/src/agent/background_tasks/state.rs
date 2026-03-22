use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::state::StateKey;

use super::types::{TaskId, TaskStatus, TaskSummary};

/// Cached task view stored in the state store for prompt injection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackgroundTaskView {
    pub tasks: HashMap<String, TaskViewEntry>,
}

/// Lightweight view of a single background task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskViewEntry {
    pub task_type: String,
    pub description: String,
    pub status: TaskStatus,
}

/// Action for the background task view state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BackgroundTaskViewAction {
    Replace {
        tasks: HashMap<String, TaskViewEntry>,
    },
    Clear,
}

impl BackgroundTaskView {
    pub(crate) fn reduce(&mut self, action: BackgroundTaskViewAction) {
        match action {
            BackgroundTaskViewAction::Replace { tasks } => {
                self.tasks = tasks;
            }
            BackgroundTaskViewAction::Clear => {
                self.tasks.clear();
            }
        }
    }
}

/// State key for the cached background task view.
pub struct BackgroundTaskViewKey;

impl StateKey for BackgroundTaskViewKey {
    const KEY: &'static str = "background_tasks";
    type Value = BackgroundTaskView;
    type Update = BackgroundTaskViewAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

/// Persisted metadata for a single background task.
///
/// Task payloads (the actual futures) are NOT persisted — only metadata
/// (id, name, status, error message, timestamps).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedTaskMeta {
    pub task_id: TaskId,
    pub task_type: String,
    pub description: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
}

impl PersistedTaskMeta {
    /// Build from a [`TaskSummary`].
    pub fn from_summary(summary: &TaskSummary) -> Self {
        Self {
            task_id: summary.task_id.clone(),
            task_type: summary.task_type.clone(),
            description: summary.description.clone(),
            status: summary.status,
            error: summary.error.clone(),
            created_at_ms: summary.created_at_ms,
            completed_at_ms: summary.completed_at_ms,
        }
    }
}

/// Persisted state for all background tasks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackgroundTaskStateSnapshot {
    pub tasks: HashMap<TaskId, PersistedTaskMeta>,
}

/// Actions applied to the persisted background task state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BackgroundTaskStateAction {
    /// Upsert a single task's metadata.
    Upsert(PersistedTaskMeta),
    /// Replace the entire task map (used on restore/sync).
    ReplaceAll {
        tasks: HashMap<TaskId, PersistedTaskMeta>,
    },
}

impl BackgroundTaskStateSnapshot {
    pub(crate) fn reduce(&mut self, action: BackgroundTaskStateAction) {
        match action {
            BackgroundTaskStateAction::Upsert(meta) => {
                self.tasks.insert(meta.task_id.clone(), meta);
            }
            BackgroundTaskStateAction::ReplaceAll { tasks } => {
                self.tasks = tasks;
            }
        }
    }
}

/// State key for persisted background task metadata.
///
/// Scoped to `Thread` so it survives across runs. On task completion or
/// failure the manager writes a state update; on resume, the plugin
/// restores known task metadata from this key.
pub struct BackgroundTaskStateKey;

impl StateKey for BackgroundTaskStateKey {
    const KEY: &'static str = "background_task_state";
    type Value = BackgroundTaskStateSnapshot;
    type Update = BackgroundTaskStateAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}
