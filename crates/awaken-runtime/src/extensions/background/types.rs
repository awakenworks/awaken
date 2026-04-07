use serde::{Deserialize, Serialize};

use crate::cancellation::CancellationToken;
use crate::inbox::InboxSender;

/// Unique identifier for a background task.
pub type TaskId = String;

pub const BACKGROUND_TASKS_PLUGIN_ID: &str = "background_tasks";

/// Status of a background task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    #[default]
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Result produced by a background task on completion.
#[derive(Debug, Clone)]
pub enum TaskResult {
    Success(serde_json::Value),
    Failed(String),
    Cancelled,
}

impl TaskResult {
    pub fn status(&self) -> TaskStatus {
        match self {
            Self::Success(_) => TaskStatus::Completed,
            Self::Failed(_) => TaskStatus::Failed,
            Self::Cancelled => TaskStatus::Cancelled,
        }
    }
}

/// Optional parent execution context for background task lineage tracking.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskParentContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

impl TaskParentContext {
    /// Returns `true` when no lineage fields are set.
    pub fn is_empty(&self) -> bool {
        self.run_id.is_none() && self.call_id.is_none() && self.agent_id.is_none()
    }
}

/// Context provided to every background task closure.
///
/// Bundles the cancellation token with an optional inbox sender so tasks
/// can both respond to cancellation and push events to the owner agent.
#[derive(Clone)]
pub struct TaskContext {
    /// Unique identifier of this task.
    pub task_id: TaskId,
    /// Token that signals when the task should stop.
    pub cancel_token: CancellationToken,
    /// Sender for pushing messages to the owner agent's inbox.
    pub(crate) inbox: Option<InboxSender>,
}

impl TaskContext {
    /// Emit a custom event to the owner agent.
    ///
    /// The event is delivered to the agent's inbox and drained at step
    /// boundaries or when the agent is waiting for background tasks.
    /// Returns `false` if no inbox is bound or the agent has ended.
    pub fn emit(&self, event_type: &str, payload: serde_json::Value) -> bool {
        let event = TaskEvent::Custom {
            task_id: self.task_id.clone(),
            event_type: event_type.to_string(),
            payload,
        };
        match &self.inbox {
            Some(s) => s.send(serde_json::to_value(&event).unwrap_or_default()),
            None => false,
        }
    }

    /// Wait until cancellation is requested.
    ///
    /// Use this for tasks that do their work, emit a result, then park
    /// themselves until killed:
    ///
    /// ```ignore
    /// |ctx| async move {
    ///     let result = do_work().await;
    ///     ctx.emit("ready", serde_json::json!(result));
    ///     ctx.cancelled().await;      // park until kill
    ///     TaskResult::Cancelled
    /// }
    /// ```
    pub async fn cancelled(&self) {
        self.cancel_token.cancelled().await;
    }

    /// Returns `true` if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }
}

/// Event emitted by a background task during its lifecycle.
///
/// Agent developers can use [`TaskContext::emit`] to send custom events
/// (e.g. progress, intermediate data) and the system emits `Completed` /
/// `Failed` / `Cancelled` automatically when the task finishes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskEvent {
    /// Task completed successfully.
    Completed {
        task_id: TaskId,
        result: Option<serde_json::Value>,
    },
    /// Task failed.
    Failed { task_id: TaskId, error: String },
    /// Task was cancelled.
    Cancelled { task_id: TaskId },
    /// Custom event emitted by the task during execution.
    Custom {
        task_id: TaskId,
        event_type: String,
        payload: serde_json::Value,
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
    pub result: Option<serde_json::Value>,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "TaskParentContext::is_empty")]
    pub parent_context: TaskParentContext,
}
