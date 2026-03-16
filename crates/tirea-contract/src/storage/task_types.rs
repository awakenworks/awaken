use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Status of a task on the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

/// A task on the shared task board.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub task_id: String,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Agent that owns this task. `None` = unowned/claimable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub status: TaskStatus,
    /// Task IDs that this task blocks (downstream dependents).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<String>,
    /// Task IDs that block this task (upstream dependencies).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    /// Team/group this task belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    pub created_at: u64,
    pub updated_at: u64,
    /// Who created this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

/// Partial update for a task.
#[derive(Debug, Clone, Default)]
pub struct TaskUpdate {
    pub subject: Option<String>,
    pub description: Option<Option<String>>,
    pub owner: Option<Option<String>>,
    pub status: Option<TaskStatus>,
    pub blocks: Option<Vec<String>>,
    pub blocked_by: Option<Vec<String>>,
    pub metadata: Option<Option<Value>>,
}

/// Query options for listing tasks.
#[derive(Debug, Clone, Default)]
pub struct TaskQuery {
    pub team_id: Option<String>,
    pub owner: Option<String>,
    pub status: Option<TaskStatus>,
    pub offset: usize,
    pub limit: usize,
}

/// Paginated task list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPage {
    pub items: Vec<Task>,
    pub total: usize,
    pub has_more: bool,
}

/// In-memory task pagination helper.
pub fn paginate_tasks(tasks: &[Task], query: &TaskQuery) -> TaskPage {
    let mut filtered: Vec<Task> = tasks
        .iter()
        .filter(|t| match query.team_id.as_deref() {
            Some(team_id) => t.team_id.as_deref() == Some(team_id),
            None => true,
        })
        .filter(|t| match query.owner.as_deref() {
            Some(owner) => t.owner.as_deref() == Some(owner),
            None => true,
        })
        .filter(|t| match query.status {
            Some(status) => t.status == status,
            None => true,
        })
        .cloned()
        .collect();

    filtered.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.task_id.cmp(&b.task_id))
    });

    let total = filtered.len();
    let limit = query.limit.clamp(1, 200);
    let offset = query.offset.min(total);
    let end = (offset + limit + 1).min(total);
    let slice = &filtered[offset..end];
    let has_more = slice.len() > limit;
    let items = slice.iter().take(limit).cloned().collect();

    TaskPage {
        items,
        total,
        has_more,
    }
}

/// Task storage errors.
#[derive(Debug, Error)]
pub enum TaskStoreError {
    #[error("task not found: {0}")]
    NotFound(String),

    #[error("task already exists: {0}")]
    AlreadyExists(String),

    #[error("task claim conflict: {0}")]
    ClaimConflict(String),

    #[error("task is blocked by unfinished dependencies: {0}")]
    Blocked(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("backend error: {0}")]
    Backend(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(id: &str, status: TaskStatus, owner: Option<&str>) -> Task {
        Task {
            task_id: id.to_string(),
            subject: format!("Task {id}"),
            description: None,
            owner: owner.map(|s| s.to_string()),
            status,
            blocks: vec![],
            blocked_by: vec![],
            team_id: Some("team-1".to_string()),
            metadata: None,
            created_at: id.as_bytes().first().copied().unwrap_or(0) as u64,
            updated_at: 0,
            created_by: None,
        }
    }

    #[test]
    fn paginate_tasks_filters_and_pages() {
        let tasks = vec![
            make_task("1", TaskStatus::Pending, None),
            make_task("2", TaskStatus::InProgress, Some("agent-a")),
            make_task("3", TaskStatus::Completed, Some("agent-a")),
            make_task("4", TaskStatus::Pending, None),
        ];

        let page = paginate_tasks(
            &tasks,
            &TaskQuery {
                status: Some(TaskStatus::Pending),
                limit: 10,
                ..Default::default()
            },
        );
        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 2);

        let page = paginate_tasks(
            &tasks,
            &TaskQuery {
                owner: Some("agent-a".to_string()),
                limit: 10,
                ..Default::default()
            },
        );
        assert_eq!(page.total, 2);

        let page = paginate_tasks(
            &tasks,
            &TaskQuery {
                limit: 1,
                ..Default::default()
            },
        );
        assert_eq!(page.items.len(), 1);
        assert!(page.has_more);
    }
}
