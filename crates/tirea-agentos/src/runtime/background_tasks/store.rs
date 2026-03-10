use super::types::{
    new_task_id, task_thread_id, TaskAction, TaskId, TaskResultRef, TaskState, TaskStatus,
    TaskSummary, TASK_THREAD_KIND_METADATA_KEY, TASK_THREAD_KIND_METADATA_VALUE,
};
use crate::contracts::runtime::state::{reduce_state_actions, AnyStateAction, ScopeContext};
use crate::contracts::storage::{
    ThreadListQuery, ThreadStore, ThreadStoreError, VersionPrecondition,
};
use crate::contracts::thread::{CheckpointReason, Message, Role, Thread, ThreadChangeSet};
use serde_json::{json, Value};
use std::sync::Arc;
use thiserror::Error;
use tirea_state::State;

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, Clone)]
pub struct NewTaskSpec {
    pub task_id: TaskId,
    pub owner_thread_id: String,
    pub task_type: String,
    pub description: String,
    pub parent_task_id: Option<TaskId>,
    pub supports_resume: bool,
    pub metadata: Value,
}

#[derive(Debug, Error)]
pub enum TaskStoreError {
    #[error(transparent)]
    ThreadStore(#[from] ThreadStoreError),
    #[error(transparent)]
    State(#[from] tirea_state::TireaError),
    #[error("task thread '{0}' is missing durable task state")]
    MissingTaskState(String),
    #[error(
        "task '{task_id}' belongs to owner '{actual_owner_thread_id}' instead of '{expected_owner_thread_id}'"
    )]
    OwnerMismatch {
        task_id: String,
        expected_owner_thread_id: String,
        actual_owner_thread_id: String,
    },
    #[error("task thread '{thread_id}' contains invalid durable task state")]
    InvalidTaskState {
        thread_id: String,
        #[source]
        error: tirea_state::TireaError,
    },
}

#[derive(Clone)]
pub struct TaskStore {
    threads: Arc<dyn ThreadStore>,
}

impl std::fmt::Debug for TaskStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskStore").finish()
    }
}

impl TaskStore {
    pub fn new(threads: Arc<dyn ThreadStore>) -> Self {
        Self { threads }
    }

    pub fn thread_id_for(task_id: &str) -> String {
        task_thread_id(task_id)
    }

    pub fn alloc_task_id() -> TaskId {
        new_task_id()
    }

    pub async fn create_task(&self, spec: NewTaskSpec) -> Result<TaskState, TaskStoreError> {
        let task_id = spec.task_id.clone();
        let thread_id = task_thread_id(&task_id);
        let created_at_ms = now_ms();
        let mut thread =
            Thread::new(thread_id.clone()).with_parent_thread_id(spec.owner_thread_id.clone());
        thread.metadata.extra.insert(
            TASK_THREAD_KIND_METADATA_KEY.to_string(),
            json!(TASK_THREAD_KIND_METADATA_VALUE),
        );
        thread
            .metadata
            .extra
            .insert("task_id".to_string(), json!(spec.task_id));
        thread
            .metadata
            .extra
            .insert("owner_thread_id".to_string(), json!(spec.owner_thread_id));
        self.threads.create(&thread).await?;

        let task = TaskState {
            id: spec.task_id,
            task_type: spec.task_type,
            description: spec.description,
            owner_thread_id: spec.owner_thread_id,
            parent_task_id: spec.parent_task_id,
            status: TaskStatus::Running,
            error: None,
            result: None,
            result_ref: None,
            checkpoint: None,
            supports_resume: spec.supports_resume,
            attempt: 1,
            created_at_ms,
            updated_at_ms: created_at_ms,
            completed_at_ms: None,
            cancel_requested_at_ms: None,
            metadata: spec.metadata,
        };
        self.append_task_action(
            &thread_id,
            task_id.as_str(),
            TaskAction::Register { task: task.clone() },
            Some(Message::internal_system(format!(
                "background task {} registered as running",
                task.id
            ))),
        )
        .await?;
        Ok(task)
    }

    pub async fn load_task(&self, task_id: &str) -> Result<Option<TaskState>, TaskStoreError> {
        let Some(head) = self.threads.load(&task_thread_id(task_id)).await? else {
            return Ok(None);
        };
        Ok(Some(Self::task_state_from_thread(&head.thread)?))
    }

    pub async fn load_task_for_owner(
        &self,
        owner_thread_id: &str,
        task_id: &str,
    ) -> Result<Option<TaskState>, TaskStoreError> {
        let Some(task) = self.load_task(task_id).await? else {
            return Ok(None);
        };
        if task.owner_thread_id != owner_thread_id {
            return Ok(None);
        }
        Ok(Some(task))
    }

    pub async fn list_tasks_for_owner(
        &self,
        owner_thread_id: &str,
    ) -> Result<Vec<TaskState>, TaskStoreError> {
        let mut offset = 0usize;
        let mut out = Vec::new();
        loop {
            let page = self
                .threads
                .list_threads(&ThreadListQuery {
                    offset,
                    limit: 200,
                    resource_id: None,
                    parent_thread_id: Some(owner_thread_id.to_string()),
                })
                .await?;
            for thread_id in &page.items {
                let Some(head) = self.threads.load(thread_id).await? else {
                    continue;
                };
                if !Self::is_task_thread(&head.thread) {
                    continue;
                }
                let task = Self::task_state_from_thread(&head.thread)?;
                if task.owner_thread_id == owner_thread_id {
                    out.push(task);
                }
            }
            if !page.has_more {
                break;
            }
            offset += page.items.len();
        }
        out.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms));
        Ok(out)
    }

    pub async fn start_task_attempt(&self, task_id: &str) -> Result<TaskState, TaskStoreError> {
        let thread_id = task_thread_id(task_id);
        let task = self
            .load_task(task_id)
            .await?
            .ok_or_else(|| TaskStoreError::MissingTaskState(thread_id.clone()))?;
        let next_attempt = task.attempt.max(1) + 1;
        self.append_task_action(
            &thread_id,
            task_id,
            TaskAction::StartAttempt {
                attempt: next_attempt,
                updated_at_ms: now_ms(),
            },
            Some(Message::internal_system(format!(
                "background task {} resumed (attempt {})",
                task_id, next_attempt
            ))),
        )
        .await?;
        self.load_task(task_id)
            .await?
            .ok_or_else(|| TaskStoreError::MissingTaskState(thread_id))
    }

    pub async fn mark_cancel_requested(&self, task_id: &str) -> Result<(), TaskStoreError> {
        let thread_id = task_thread_id(task_id);
        self.append_task_action(
            &thread_id,
            task_id,
            TaskAction::MarkCancelRequested {
                requested_at_ms: now_ms(),
            },
            Some(Message::internal_system(format!(
                "background task {} cancellation requested",
                task_id
            ))),
        )
        .await
    }

    pub async fn set_checkpoint(
        &self,
        task_id: &str,
        checkpoint: Value,
    ) -> Result<(), TaskStoreError> {
        let thread_id = task_thread_id(task_id);
        self.append_task_action(
            &thread_id,
            task_id,
            TaskAction::SetCheckpoint {
                checkpoint,
                updated_at_ms: now_ms(),
            },
            None,
        )
        .await
    }

    pub async fn persist_summary(&self, summary: &TaskSummary) -> Result<(), TaskStoreError> {
        let thread_id = task_thread_id(&summary.task_id);
        let task = self
            .load_task(&summary.task_id)
            .await?
            .ok_or_else(|| TaskStoreError::MissingTaskState(thread_id.clone()))?;
        let result_ref = if summary.task_type == "agent_run"
            && matches!(summary.status, TaskStatus::Completed | TaskStatus::Stopped)
        {
            self.resolve_agent_output_ref(&task).await?
        } else {
            None
        };

        self.append_task_action(
            &thread_id,
            &summary.task_id,
            TaskAction::SetStatus {
                status: summary.status,
                error: summary.error.clone(),
                result: if summary.task_type == "agent_run" {
                    None
                } else {
                    summary.result.clone()
                },
                result_ref,
                completed_at_ms: summary.completed_at_ms.or_else(|| Some(now_ms())),
                updated_at_ms: now_ms(),
            },
            Some(Message::internal_system(format!(
                "background task {} finished with status {}",
                summary.task_id,
                summary.status.as_str()
            ))),
        )
        .await
    }

    pub async fn persist_foreground_result(
        &self,
        task_id: &str,
        status: TaskStatus,
        error: Option<String>,
        result: Option<Value>,
    ) -> Result<(), TaskStoreError> {
        let thread_id = task_thread_id(task_id);
        let task = self
            .load_task(task_id)
            .await?
            .ok_or_else(|| TaskStoreError::MissingTaskState(thread_id.clone()))?;
        let result_ref = if task.task_type == "agent_run"
            && matches!(status, TaskStatus::Completed | TaskStatus::Stopped)
        {
            self.resolve_agent_output_ref(&task).await?
        } else {
            None
        };

        self.append_task_action(
            &thread_id,
            task_id,
            TaskAction::SetStatus {
                status,
                error,
                result: if task.task_type == "agent_run" {
                    None
                } else {
                    result
                },
                result_ref,
                completed_at_ms: Some(now_ms()),
                updated_at_ms: now_ms(),
            },
            Some(Message::internal_system(format!(
                "background task {} persisted terminal status {}",
                task_id,
                status.as_str()
            ))),
        )
        .await
    }

    pub async fn load_output_text(
        &self,
        task: &TaskState,
    ) -> Result<Option<String>, TaskStoreError> {
        let Some(result_ref) = task.result_ref.as_ref() else {
            if task.task_type == "agent_run" {
                if let Some(thread_id) = task.metadata.get("thread_id").and_then(Value::as_str) {
                    return self.load_thread_message_text(thread_id, None).await;
                }
            }
            return Ok(None);
        };
        match result_ref {
            TaskResultRef::ThreadMessage {
                thread_id,
                message_id,
            } => {
                self.load_thread_message_text(thread_id, message_id.as_deref())
                    .await
            }
            TaskResultRef::External { uri } => Ok(Some(uri.clone())),
        }
    }

    pub async fn descendant_ids_for_owner(
        &self,
        owner_thread_id: &str,
        root_task_id: &str,
    ) -> Result<Vec<TaskId>, TaskStoreError> {
        let tasks = self.list_tasks_for_owner(owner_thread_id).await?;
        let mut by_parent: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for task in &tasks {
            if let Some(parent) = task.parent_task_id.as_ref() {
                by_parent
                    .entry(parent.clone())
                    .or_default()
                    .push(task.id.clone());
            }
        }
        let mut out = Vec::new();
        let mut stack = vec![root_task_id.to_string()];
        while let Some(current) = stack.pop() {
            if tasks.iter().any(|task| task.id == current) {
                out.push(current.clone());
            }
            if let Some(children) = by_parent.get(&current) {
                for child in children {
                    stack.push(child.clone());
                }
            }
        }
        Ok(out)
    }

    fn is_task_thread(thread: &Thread) -> bool {
        thread
            .metadata
            .extra
            .get(TASK_THREAD_KIND_METADATA_KEY)
            .and_then(Value::as_str)
            == Some(TASK_THREAD_KIND_METADATA_VALUE)
    }

    fn task_state_from_thread(thread: &Thread) -> Result<TaskState, TaskStoreError> {
        let snapshot = thread.rebuild_state()?;
        let Some(value) = snapshot.get(TaskState::PATH) else {
            return Err(TaskStoreError::MissingTaskState(thread.id.clone()));
        };
        TaskState::from_value(value).map_err(|error| TaskStoreError::InvalidTaskState {
            thread_id: thread.id.clone(),
            error,
        })
    }

    async fn resolve_agent_output_ref(
        &self,
        task: &TaskState,
    ) -> Result<Option<TaskResultRef>, TaskStoreError> {
        let Some(thread_id) = task.metadata.get("thread_id").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Some(head) = self.threads.load(thread_id).await? else {
            return Ok(Some(TaskResultRef::ThreadMessage {
                thread_id: thread_id.to_string(),
                message_id: None,
            }));
        };
        let message_id = head
            .thread
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .and_then(|m| m.id.clone());
        Ok(Some(TaskResultRef::ThreadMessage {
            thread_id: thread_id.to_string(),
            message_id,
        }))
    }

    async fn load_thread_message_text(
        &self,
        thread_id: &str,
        message_id: Option<&str>,
    ) -> Result<Option<String>, TaskStoreError> {
        let Some(head) = self.threads.load(thread_id).await? else {
            return Ok(None);
        };
        let msg = if let Some(message_id) = message_id {
            head.thread
                .messages
                .iter()
                .find(|m| m.id.as_deref() == Some(message_id))
                .map(|m| m.content.clone())
        } else {
            head.thread
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.content.clone())
        };
        Ok(msg)
    }

    async fn append_task_action(
        &self,
        thread_id: &str,
        task_id: &str,
        action: TaskAction,
        audit_message: Option<Message>,
    ) -> Result<(), TaskStoreError> {
        let head = self
            .threads
            .load(thread_id)
            .await?
            .ok_or_else(|| TaskStoreError::MissingTaskState(thread_id.to_string()))?;
        let mut snapshot = head.thread.rebuild_state()?;
        if snapshot.get(TaskState::PATH).is_none() {
            let default_task = serde_json::to_value(TaskState::default())
                .map_err(tirea_state::TireaError::from)?;
            match snapshot.as_object_mut() {
                Some(obj) => {
                    obj.insert(TaskState::PATH.to_string(), default_task);
                }
                None => {
                    snapshot = json!({ TaskState::PATH: default_task });
                }
            }
        }
        let state_action = AnyStateAction::new::<TaskState>(action);
        let serialized = vec![state_action.to_serialized_state_action()];
        let patches = reduce_state_actions(
            vec![state_action],
            &snapshot,
            "background_task",
            &ScopeContext::run(),
        )?;
        let changeset = ThreadChangeSet::from_parts(
            task_id.to_string(),
            None,
            CheckpointReason::ToolResultsCommitted,
            audit_message.into_iter().map(std::sync::Arc::new).collect(),
            patches,
            serialized,
            None,
        );
        self.threads
            .append(
                thread_id,
                &changeset,
                VersionPrecondition::Exact(head.version),
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::storage::{ThreadReader, ThreadStore, ThreadWriter};

    #[tokio::test]
    async fn create_task_persists_task_thread_state() {
        let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
        let store = TaskStore::new(storage.clone() as Arc<dyn ThreadStore>);

        let task = store
            .create_task(NewTaskSpec {
                task_id: "task-1".to_string(),
                owner_thread_id: "owner-1".to_string(),
                task_type: "shell".to_string(),
                description: "echo hi".to_string(),
                parent_task_id: Some("root".to_string()),
                supports_resume: false,
                metadata: json!({"kind":"test"}),
            })
            .await
            .expect("task should persist");

        assert_eq!(task.id, "task-1");
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.parent_task_id.as_deref(), Some("root"));

        let loaded = store
            .load_task("task-1")
            .await
            .expect("load should succeed")
            .expect("task should exist");
        assert_eq!(loaded.id, "task-1");
        assert_eq!(loaded.owner_thread_id, "owner-1");
        assert_eq!(loaded.metadata["kind"], json!("test"));

        let head = storage
            .load(&task_thread_id("task-1"))
            .await
            .expect("thread load should succeed")
            .expect("task thread should exist");
        assert_eq!(head.thread.parent_thread_id.as_deref(), Some("owner-1"));
    }

    #[tokio::test]
    async fn list_tasks_for_owner_ignores_non_task_children() {
        let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
        let store = TaskStore::new(storage.clone() as Arc<dyn ThreadStore>);

        storage
            .create(&Thread::new("child-thread").with_parent_thread_id("owner-1"))
            .await
            .expect("non-task child thread should persist");

        store
            .create_task(NewTaskSpec {
                task_id: "task-1".to_string(),
                owner_thread_id: "owner-1".to_string(),
                task_type: "shell".to_string(),
                description: "owner one".to_string(),
                parent_task_id: None,
                supports_resume: false,
                metadata: json!({}),
            })
            .await
            .expect("owner task should persist");
        store
            .create_task(NewTaskSpec {
                task_id: "task-2".to_string(),
                owner_thread_id: "owner-2".to_string(),
                task_type: "shell".to_string(),
                description: "owner two".to_string(),
                parent_task_id: None,
                supports_resume: false,
                metadata: json!({}),
            })
            .await
            .expect("other owner task should persist");

        let tasks = store
            .list_tasks_for_owner("owner-1")
            .await
            .expect("list should succeed");

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "task-1");
    }

    #[tokio::test]
    async fn mark_cancel_requested_persists_timestamp() {
        let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
        let store = TaskStore::new(storage as Arc<dyn ThreadStore>);

        let task = store
            .create_task(NewTaskSpec {
                task_id: "task-1".to_string(),
                owner_thread_id: "owner-1".to_string(),
                task_type: "shell".to_string(),
                description: "cancel me".to_string(),
                parent_task_id: None,
                supports_resume: false,
                metadata: json!({}),
            })
            .await
            .expect("task should persist");

        store
            .mark_cancel_requested("task-1")
            .await
            .expect("cancel request should persist");

        let loaded = store
            .load_task("task-1")
            .await
            .expect("load should succeed")
            .expect("task should exist");
        assert!(loaded.cancel_requested_at_ms.is_some());
        assert!(loaded.updated_at_ms >= task.updated_at_ms);
    }

    #[tokio::test]
    async fn persist_summary_for_agent_run_captures_output_ref() {
        let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
        let store = TaskStore::new(storage.clone() as Arc<dyn ThreadStore>);

        storage
            .create(
                &Thread::new("exec-1")
                    .with_message(Message::assistant("draft").with_id("msg-1".to_string()))
                    .with_message(Message::assistant("final").with_id("msg-2".to_string())),
            )
            .await
            .expect("execution thread should persist");

        let task = store
            .create_task(NewTaskSpec {
                task_id: "task-1".to_string(),
                owner_thread_id: "owner-1".to_string(),
                task_type: "agent_run".to_string(),
                description: "delegate".to_string(),
                parent_task_id: None,
                supports_resume: true,
                metadata: json!({"thread_id":"exec-1","agent_id":"writer"}),
            })
            .await
            .expect("task should persist");

        let mut summary = task.summary();
        summary.status = TaskStatus::Completed;
        summary.completed_at_ms = Some(task.created_at_ms + 1);

        store
            .persist_summary(&summary)
            .await
            .expect("summary should persist");

        let loaded = store
            .load_task("task-1")
            .await
            .expect("load should succeed")
            .expect("task should exist");
        assert_eq!(loaded.status, TaskStatus::Completed);
        assert_eq!(
            loaded.result_ref,
            Some(TaskResultRef::ThreadMessage {
                thread_id: "exec-1".to_string(),
                message_id: Some("msg-2".to_string()),
            })
        );
        assert_eq!(
            store
                .load_output_text(&loaded)
                .await
                .expect("output should load")
                .as_deref(),
            Some("final")
        );
    }

    #[tokio::test]
    async fn descendant_ids_for_owner_returns_only_owner_subtree() {
        let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
        let store = TaskStore::new(storage as Arc<dyn ThreadStore>);

        for (task_id, owner, parent) in [
            ("root", "owner-1", None),
            ("child", "owner-1", Some("root")),
            ("grandchild", "owner-1", Some("child")),
            ("other-root", "owner-2", None),
            ("other-child", "owner-2", Some("root")),
        ] {
            store
                .create_task(NewTaskSpec {
                    task_id: task_id.to_string(),
                    owner_thread_id: owner.to_string(),
                    task_type: "agent_run".to_string(),
                    description: task_id.to_string(),
                    parent_task_id: parent.map(str::to_string),
                    supports_resume: true,
                    metadata: json!({}),
                })
                .await
                .expect("task should persist");
        }

        let mut descendants = store
            .descendant_ids_for_owner("owner-1", "root")
            .await
            .expect("descendants should load");
        descendants.sort();

        assert_eq!(descendants, vec!["child", "grandchild", "root"]);
    }

    #[tokio::test]
    async fn load_task_reports_invalid_task_state() {
        let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
        let store = TaskStore::new(storage.clone() as Arc<dyn ThreadStore>);
        let thread_id = task_thread_id("broken-task");

        let mut thread = Thread::with_initial_state(
            thread_id.clone(),
            json!({
                TaskState::PATH: {
                    "id": "broken-task",
                    "status": 123
                }
            }),
        )
        .with_parent_thread_id("owner-1");
        thread.metadata.extra.insert(
            TASK_THREAD_KIND_METADATA_KEY.to_string(),
            json!(TASK_THREAD_KIND_METADATA_VALUE),
        );
        storage
            .create(&thread)
            .await
            .expect("broken task thread should persist");

        let err = store
            .load_task("broken-task")
            .await
            .expect_err("invalid task state should error");

        match err {
            TaskStoreError::InvalidTaskState {
                thread_id: err_thread_id,
                ..
            } => assert_eq!(err_thread_id, thread_id),
            other => panic!("expected InvalidTaskState, got {other:?}"),
        }
    }
}
