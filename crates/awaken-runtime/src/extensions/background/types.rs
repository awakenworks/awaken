use serde::{Deserialize, Serialize};
use tokio::sync::watch;

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
}

/// Handle for cancelling a running task.
#[derive(Clone)]
pub struct TaskCancellationHandle {
    sender: watch::Sender<bool>,
}

impl TaskCancellationHandle {
    pub(crate) fn new() -> (Self, TaskCancellationToken) {
        let (tx, rx) = watch::channel(false);
        (Self { sender: tx }, TaskCancellationToken { receiver: rx })
    }

    pub fn cancel(&self) {
        let _ = self.sender.send(true);
    }
}

/// Token that a task checks for cancellation.
#[derive(Clone)]
pub struct TaskCancellationToken {
    receiver: watch::Receiver<bool>,
}

impl TaskCancellationToken {
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    pub async fn cancelled(&mut self) {
        while !*self.receiver.borrow() {
            if self.receiver.changed().await.is_err() {
                return;
            }
        }
    }
}
