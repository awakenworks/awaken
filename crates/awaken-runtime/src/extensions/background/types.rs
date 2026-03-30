use serde::{Deserialize, Serialize};

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
