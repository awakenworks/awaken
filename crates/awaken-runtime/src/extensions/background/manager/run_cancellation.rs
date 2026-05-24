use super::{BackgroundTaskManager, TaskHandle};
use crate::extensions::background::{BackgroundTaskStateKey, TaskId, TaskParentContext};

impl BackgroundTaskManager {
    fn parent_run_id(parent_context: &TaskParentContext) -> Option<&str> {
        parent_context
            .run_id
            .as_deref()
            .map(str::trim)
            .filter(|run_id| !run_id.is_empty())
    }

    pub(super) fn is_parent_run_cancelled(
        &self,
        parent_context: &TaskParentContext,
    ) -> Option<String> {
        let run_id = Self::parent_run_id(parent_context)?;
        self.cancelled_run_ids
            .read()
            .contains(run_id)
            .then(|| run_id.to_string())
    }

    pub(super) async fn insert_handle_and_cancel_if_parent_run_cancelled(
        &self,
        task_id: TaskId,
        handle: TaskHandle,
        parent_context: &TaskParentContext,
    ) {
        let mut handles = self.handles.lock().await;
        handles.insert(task_id.clone(), handle);
        if self.is_parent_run_cancelled(parent_context).is_some()
            && let Some(handle) = handles.get(&task_id)
        {
            handle.cancel_handle.cancel();
        }
    }

    pub async fn cancel_descendants_for_run(&self, parent_run_id: &str) -> usize {
        // Retain cancelled run ids for this manager's lifetime. This is what
        // rejects late descendant spawns after the one-shot live handle sweep.
        self.cancelled_run_ids
            .write()
            .insert(parent_run_id.to_string());
        let root_task_ids = self
            .store()
            .and_then(|store| store.read::<BackgroundTaskStateKey>())
            .map(|snapshot| {
                snapshot
                    .tasks
                    .values()
                    .filter(|meta| {
                        !meta.status.is_terminal()
                            && meta.parent_context.run_id.as_deref() == Some(parent_run_id)
                            && meta.parent_context.task_id.is_none()
                    })
                    .map(|meta| meta.task_id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let mut cancelled = 0usize;
        for task_id in root_task_ids {
            cancelled += self.cancel_tree(&task_id).await;
        }
        cancelled
    }
}
