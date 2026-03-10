//! Core types for the background task system.
//!
//! Background tasks are modeled as durable task threads plus an in-memory live
//! execution table. `TaskState` is the persisted truth for query/recovery,
//! while `BackgroundTaskManager` tracks active handles and cancellation tokens.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tirea_state::{get_at_path, parse_path, State};

/// Unique identifier for a background task.
pub type TaskId = String;

/// Prefix for task journal threads persisted in [`ThreadStore`].
pub const TASK_THREAD_PREFIX: &str = "task:";
pub const TASK_THREAD_KIND_METADATA_KEY: &str = "__thread_kind";
pub const TASK_THREAD_KIND_METADATA_VALUE: &str = "background_task";

pub fn new_task_id() -> TaskId {
    format!("bg_{}", uuid::Uuid::now_v7().simple())
}

pub fn task_thread_id(task_id: &str) -> String {
    format!("{TASK_THREAD_PREFIX}{task_id}")
}

fn default_metadata() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Parameters for spawning a background task.
pub struct SpawnParams {
    /// Unique task identifier.
    pub task_id: TaskId,
    /// Thread that owns this task.
    pub owner_thread_id: String,
    /// Task type label (e.g. "agent_run").
    pub task_type: String,
    /// Human-readable description.
    pub description: String,
    /// Optional parent task for hierarchical tracking.
    pub parent_task_id: Option<String>,
    /// Arbitrary metadata attached to the task.
    pub metadata: Value,
}

fn is_null_or_empty_object(v: &Value) -> bool {
    v.is_null() || v.as_object().is_some_and(|m| m.is_empty())
}

/// Status of a background task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    #[default]
    Running,
    Completed,
    Failed,
    Cancelled,
    /// Resumable — the task was stopped but can be restarted (e.g. agent runs).
    Stopped,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Stopped => "stopped",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running | Self::Stopped)
    }
}

/// Result produced by a background task on completion.
#[derive(Debug, Clone)]
pub enum TaskResult {
    /// Task completed successfully with a result value.
    Success(Value),
    /// Task failed with an error message.
    Failed(String),
    /// Task was cancelled (terminal).
    Cancelled,
    /// Task was stopped but can be resumed later.
    Stopped,
}

impl TaskResult {
    pub fn status(&self) -> TaskStatus {
        match self {
            Self::Success(_) => TaskStatus::Completed,
            Self::Failed(_) => TaskStatus::Failed,
            Self::Cancelled => TaskStatus::Cancelled,
            Self::Stopped => TaskStatus::Stopped,
        }
    }
}

/// Durable reference to task output stored elsewhere.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskResultRef {
    ThreadMessage {
        thread_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    External {
        uri: String,
    },
}

/// Summary of a background task visible to tools and plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task_id: TaskId,
    pub task_type: String,
    pub description: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<TaskResultRef>,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<TaskId>,
    #[serde(default)]
    pub supports_resume: bool,
    #[serde(default)]
    pub attempt: u32,
    #[serde(
        default = "default_metadata",
        skip_serializing_if = "is_null_or_empty_object"
    )]
    pub metadata: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence_error: Option<String>,
}

/// Lightweight derived projection for prompt injection and UI summaries on the
/// owner thread. This is a cache, not the source of truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTaskView {
    pub task_type: String,
    pub description: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

impl BackgroundTaskView {
    pub fn from_summary(summary: &TaskSummary) -> Self {
        Self {
            task_type: summary.task_type.clone(),
            description: summary.description.clone(),
            status: summary.status,
            parent_task_id: summary.parent_task_id.clone(),
            agent_id: summary
                .metadata
                .get("agent_id")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        }
    }
}

/// Lightweight cached task view stored on the owner thread for prompt
/// injection. The canonical task state remains in the task thread.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(
    path = "__derived.background_tasks",
    action = "BackgroundTaskViewAction",
    scope = "thread"
)]
pub struct BackgroundTaskViewState {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[tirea(default = "std::collections::HashMap::new()")]
    pub tasks: HashMap<String, BackgroundTaskView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BackgroundTaskViewAction {
    Replace {
        tasks: HashMap<String, BackgroundTaskView>,
        synced_at_ms: u64,
    },
}

impl BackgroundTaskViewState {
    fn reduce(&mut self, action: BackgroundTaskViewAction) {
        match action {
            BackgroundTaskViewAction::Replace {
                tasks,
                synced_at_ms,
            } => {
                self.tasks = tasks;
                self.synced_at_ms = Some(synced_at_ms);
            }
        }
    }
}

/// Durable task state stored inside a dedicated task thread (`task:<task_id>`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "__task", action = "TaskAction", scope = "thread")]
pub struct TaskState {
    pub id: TaskId,
    pub task_type: String,
    pub description: String,
    pub owner_thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<TaskId>,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<TaskResultRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<Value>,
    #[serde(default)]
    pub supports_resume: bool,
    #[serde(default)]
    pub attempt: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_requested_at_ms: Option<u64>,
    #[serde(
        default = "default_metadata",
        skip_serializing_if = "is_null_or_empty_object"
    )]
    pub metadata: Value,
}

impl TaskState {
    pub fn summary(&self) -> TaskSummary {
        TaskSummary {
            task_id: self.id.clone(),
            task_type: self.task_type.clone(),
            description: self.description.clone(),
            status: self.status,
            error: self.error.clone(),
            result: self.result.clone(),
            result_ref: self.result_ref.clone(),
            created_at_ms: self.created_at_ms,
            completed_at_ms: self.completed_at_ms,
            parent_task_id: self.parent_task_id.clone(),
            supports_resume: self.supports_resume,
            attempt: self.attempt,
            metadata: self.metadata.clone(),
            persistence_error: None,
        }
    }
}

/// Reducer actions for durable task state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskAction {
    Register {
        task: Box<TaskState>,
    },
    StartAttempt {
        attempt: u32,
        updated_at_ms: u64,
    },
    MarkCancelRequested {
        requested_at_ms: u64,
    },
    SetCheckpoint {
        checkpoint: Value,
        updated_at_ms: u64,
    },
    SetStatus {
        status: TaskStatus,
        error: Option<String>,
        result: Option<Value>,
        result_ref: Option<TaskResultRef>,
        completed_at_ms: Option<u64>,
        updated_at_ms: u64,
    },
}

impl TaskState {
    fn reduce(&mut self, action: TaskAction) {
        match action {
            TaskAction::Register { task } => {
                *self = *task;
            }
            TaskAction::StartAttempt {
                attempt,
                updated_at_ms,
            } => {
                self.status = TaskStatus::Running;
                self.error = None;
                self.result = None;
                self.result_ref = None;
                self.completed_at_ms = None;
                self.cancel_requested_at_ms = None;
                self.attempt = attempt;
                self.updated_at_ms = updated_at_ms;
            }
            TaskAction::MarkCancelRequested { requested_at_ms } => {
                self.cancel_requested_at_ms = Some(requested_at_ms);
                self.updated_at_ms = requested_at_ms;
            }
            TaskAction::SetCheckpoint {
                checkpoint,
                updated_at_ms,
            } => {
                self.checkpoint = Some(checkpoint);
                self.updated_at_ms = updated_at_ms;
            }
            TaskAction::SetStatus {
                status,
                error,
                result,
                result_ref,
                completed_at_ms,
                updated_at_ms,
            } => {
                self.status = status;
                self.error = error;
                self.result = result;
                self.result_ref = result_ref;
                self.completed_at_ms = completed_at_ms;
                self.updated_at_ms = updated_at_ms;
            }
        }
    }
}

pub(crate) fn derived_task_view_from_doc(doc: &Value) -> BackgroundTaskViewState {
    get_at_path(doc, &parse_path(BackgroundTaskViewState::PATH))
        .and_then(|value| BackgroundTaskViewState::from_value(value).ok())
        .unwrap_or_default()
}
