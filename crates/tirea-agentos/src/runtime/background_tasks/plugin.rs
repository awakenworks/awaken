//! `BackgroundTasksPlugin` — behavior that caches a lightweight derived view of
//! background tasks on the owner thread and injects task reminders into the
//! prompt.

use super::manager::BackgroundTaskManager;
use super::{
    derived_task_view_from_doc, BackgroundTaskView, BackgroundTaskViewAction,
    BackgroundTaskViewState, TaskStore, TaskSummary,
};
use crate::contracts::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use crate::contracts::runtime::phase::{
    ActionSet, AfterToolExecuteAction, BeforeInferenceAction, LifecycleAction,
};
use crate::contracts::runtime::state::AnyStateAction;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub const BACKGROUND_TASKS_PLUGIN_ID: &str = "background_tasks";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// Behavior that keeps a lightweight owner-thread task projection in sync and
/// injects reminders about active background tasks.
///
/// Durable task truth remains in task threads; the owner-thread state is only a
/// prompt/UI cache.
pub struct BackgroundTasksPlugin {
    manager: Arc<BackgroundTaskManager>,
    task_store: Option<Arc<TaskStore>>,
}

impl BackgroundTasksPlugin {
    pub fn new(manager: Arc<BackgroundTaskManager>) -> Self {
        Self {
            manager,
            task_store: None,
        }
    }

    pub fn with_task_store(mut self, task_store: Option<Arc<TaskStore>>) -> Self {
        self.task_store = task_store;
        self
    }

    async fn collect_task_summaries(&self, thread_id: &str) -> Vec<TaskSummary> {
        let mut by_id: HashMap<String, TaskSummary> = HashMap::new();

        if let Some(task_store) = &self.task_store {
            if let Ok(tasks) = task_store.list_tasks_for_owner(thread_id).await {
                for task in tasks {
                    by_id.insert(task.id.clone(), task.summary());
                }
            }
        }

        for task in self.manager.list(thread_id, None).await {
            by_id.insert(task.task_id.clone(), task);
        }

        let mut tasks: Vec<_> = by_id.into_values().collect();
        tasks.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.task_id.cmp(&b.task_id))
        });
        tasks
    }

    fn derive_task_view(tasks: &[TaskSummary]) -> HashMap<String, BackgroundTaskView> {
        tasks
            .iter()
            .filter(|task| !task.status.is_terminal())
            .map(|task| (task.task_id.clone(), BackgroundTaskView::from_summary(task)))
            .collect()
    }

    fn sync_action_if_changed(
        &self,
        snapshot: &Value,
        tasks: &HashMap<String, BackgroundTaskView>,
    ) -> Option<AnyStateAction> {
        let current = derived_task_view_from_doc(snapshot);
        if current.tasks == *tasks {
            return None;
        }

        Some(AnyStateAction::new::<BackgroundTaskViewState>(
            BackgroundTaskViewAction::Replace {
                tasks: tasks.clone(),
                synced_at_ms: now_ms(),
            },
        ))
    }

    fn render_task_view(tasks: &HashMap<String, BackgroundTaskView>) -> Option<String> {
        if tasks.is_empty() {
            return None;
        }

        let mut entries: Vec<_> = tasks.iter().collect();
        entries.sort_by(|(left_id, _), (right_id, _)| left_id.cmp(right_id));

        let mut out = String::new();
        out.push_str("<background_tasks>\n");
        for (task_id, task) in entries {
            out.push_str(&format!(
                "<task id=\"{}\" type=\"{}\" status=\"{}\" description=\"{}\"",
                task_id,
                task.task_type,
                task.status.as_str(),
                task.description,
            ));
            if let Some(parent_task_id) = task.parent_task_id.as_deref() {
                out.push_str(&format!(" parent_task_id=\"{}\"", parent_task_id));
            }
            if let Some(agent_id) = task.agent_id.as_deref() {
                out.push_str(&format!(" agent_id=\"{}\"", agent_id));
            }
            out.push_str("/>\n");
        }
        out.push_str("</background_tasks>\n");
        out.push_str(
            "Use tool \"task_status\" to check progress, \"task_output\" to read results, or \"task_cancel\" to cancel active tasks.",
        );
        Some(out)
    }
}

#[async_trait]
impl AgentBehavior for BackgroundTasksPlugin {
    fn id(&self) -> &str {
        BACKGROUND_TASKS_PLUGIN_ID
    }

    tirea_contract::declare_plugin_states!(BackgroundTaskViewState);

    async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        let snapshot = ctx.snapshot();
        let tasks = self.collect_task_summaries(ctx.thread_id()).await;
        let view = Self::derive_task_view(&tasks);

        self.sync_action_if_changed(&snapshot, &view)
            .map(LifecycleAction::State)
            .map(ActionSet::single)
            .unwrap_or_else(ActionSet::empty)
    }

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let view = derived_task_view_from_doc(&ctx.snapshot());
        Self::render_task_view(&view.tasks)
            .map(BeforeInferenceAction::AddSystemContext)
            .map(ActionSet::single)
            .unwrap_or_else(ActionSet::empty)
    }

    async fn after_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        let snapshot = ctx.snapshot();
        let tasks = self.collect_task_summaries(ctx.thread_id()).await;
        let view = Self::derive_task_view(&tasks);

        let mut actions = ActionSet::empty();
        if let Some(state) = self.sync_action_if_changed(&snapshot, &view) {
            actions = actions.and(AfterToolExecuteAction::State(state));
        }
        if let Some(reminder) = Self::render_task_view(&view) {
            actions = actions.and(AfterToolExecuteAction::AddSystemReminder(reminder));
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::phase::Phase;
    use crate::contracts::runtime::state::{reduce_state_actions, ScopeContext};
    use crate::contracts::storage::ThreadStore;
    use crate::contracts::RunConfig;
    use serde_json::{json, Value};
    use tirea_state::DocCell;

    fn make_ctx<'a>(
        phase: Phase,
        thread_id: &'a str,
        run_config: &'a RunConfig,
        doc: &'a DocCell,
    ) -> ReadOnlyContext<'a> {
        ReadOnlyContext::new(phase, thread_id, &[], run_config, doc)
    }

    fn apply_state_actions(doc: &DocCell, actions: Vec<AnyStateAction>) {
        if actions.is_empty() {
            return;
        }
        let snapshot = doc.snapshot();
        let patches = reduce_state_actions(actions, &snapshot, "test", &ScopeContext::run())
            .expect("state actions should reduce");
        for patch in patches {
            for op in patch.patch().ops() {
                doc.apply(op).expect("state patch op should apply");
            }
        }
    }

    fn lifecycle_state_actions(actions: ActionSet<LifecycleAction>) -> Vec<AnyStateAction> {
        actions
            .into_iter()
            .map(|action| match action {
                LifecycleAction::State(action) => action,
            })
            .collect()
    }

    fn after_tool_parts(
        actions: ActionSet<AfterToolExecuteAction>,
    ) -> (Vec<AnyStateAction>, Vec<String>) {
        let mut state_actions = Vec::new();
        let mut reminders = Vec::new();
        for action in actions {
            match action {
                AfterToolExecuteAction::State(action) => state_actions.push(action),
                AfterToolExecuteAction::AddSystemReminder(text) => reminders.push(text),
                AfterToolExecuteAction::AddUserMessage(_) => {}
            }
        }
        (state_actions, reminders)
    }

    fn before_inference_parts(
        actions: ActionSet<BeforeInferenceAction>,
    ) -> (Vec<AnyStateAction>, Vec<String>) {
        let mut state_actions = Vec::new();
        let mut contexts = Vec::new();
        for action in actions {
            match action {
                BeforeInferenceAction::State(action) => state_actions.push(action),
                BeforeInferenceAction::AddSystemContext(text) => contexts.push(text),
                BeforeInferenceAction::AddSessionContext(_)
                | BeforeInferenceAction::ExcludeTool(_)
                | BeforeInferenceAction::IncludeOnlyTools(_)
                | BeforeInferenceAction::AddRequestTransform(_)
                | BeforeInferenceAction::Terminate(_) => {}
            }
        }
        (state_actions, contexts)
    }

    fn derived_view(doc: &DocCell) -> BackgroundTaskViewState {
        let snapshot = doc.snapshot();
        derived_task_view_from_doc(&snapshot)
    }

    #[test]
    fn plugin_id_is_background_tasks() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let plugin = BackgroundTasksPlugin::new(mgr);
        assert_eq!(plugin.id(), BACKGROUND_TASKS_PLUGIN_ID);
    }

    #[test]
    fn plugin_registers_lattice_and_scope() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let plugin = BackgroundTasksPlugin::new(mgr);

        let mut lattice = tirea_state::LatticeRegistry::new();
        plugin.register_lattice_paths(&mut lattice);

        let mut scope_reg = tirea_contract::runtime::state::StateScopeRegistry::new();
        plugin.register_state_scopes(&mut scope_reg);

        let mut state_action_deserializer_registry =
            tirea_contract::runtime::state::StateActionDeserializerRegistry::new();
        plugin.register_state_action_deserializers(&mut state_action_deserializer_registry);
    }

    #[tokio::test]
    async fn run_start_syncs_derived_view_state_from_task_store() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let thread_store = Arc::new(tirea_store_adapters::MemoryStore::new());
        let task_store = Arc::new(TaskStore::new(thread_store as Arc<dyn ThreadStore>));
        task_store
            .create_task(super::super::NewTaskSpec {
                task_id: "task-1".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "agent_run".to_string(),
                description: "delegate to writer".to_string(),
                parent_task_id: Some("root".to_string()),
                supports_resume: true,
                metadata: json!({"agent_id":"writer"}),
            })
            .await
            .expect("task should persist");

        let plugin = BackgroundTasksPlugin::new(mgr).with_task_store(Some(task_store));
        let doc = DocCell::new(json!({}));
        let rc = RunConfig::new();
        let ctx = make_ctx(Phase::RunStart, "thread-1", &rc, &doc);

        let actions = plugin.run_start(&ctx).await;
        apply_state_actions(&doc, lifecycle_state_actions(actions));

        let derived = derived_view(&doc);
        let task = derived.tasks.get("task-1").expect("task view should exist");
        assert_eq!(task.task_type, "agent_run");
        assert_eq!(task.description, "delegate to writer");
        assert_eq!(task.status.as_str(), "running");
        assert_eq!(task.parent_task_id.as_deref(), Some("root"));
        assert_eq!(task.agent_id.as_deref(), Some("writer"));
    }

    #[tokio::test]
    async fn run_start_replaces_stale_derived_view_with_store_snapshot() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let thread_store = Arc::new(tirea_store_adapters::MemoryStore::new());
        let task_store = Arc::new(TaskStore::new(thread_store as Arc<dyn ThreadStore>));
        task_store
            .create_task(super::super::NewTaskSpec {
                task_id: "task-fresh".to_string(),
                owner_thread_id: "thread-1".to_string(),
                task_type: "shell".to_string(),
                description: "fresh task".to_string(),
                parent_task_id: None,
                supports_resume: false,
                metadata: json!({}),
            })
            .await
            .expect("task should persist");

        let plugin = BackgroundTasksPlugin::new(mgr).with_task_store(Some(task_store));
        let doc = DocCell::new(json!({
            "__derived": {
                "background_tasks": {
                    "tasks": {
                        "stale-task": {
                            "task_type": "shell",
                            "description": "stale task",
                            "status": "running"
                        }
                    },
                    "synced_at_ms": 1
                }
            }
        }));
        let rc = RunConfig::new();
        let ctx = make_ctx(Phase::RunStart, "thread-1", &rc, &doc);

        let actions = plugin.run_start(&ctx).await;
        apply_state_actions(&doc, lifecycle_state_actions(actions));

        let derived = derived_view(&doc);
        assert!(!derived.tasks.contains_key("stale-task"));
        assert!(derived.tasks.contains_key("task-fresh"));
    }

    #[tokio::test]
    async fn before_inference_uses_cached_view() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let plugin = BackgroundTasksPlugin::new(mgr);
        let doc = DocCell::new(json!({
            "__derived": {
                "background_tasks": {
                    "tasks": {
                        "task-1": {
                            "task_type": "agent_run",
                            "description": "delegate to writer",
                            "status": "running",
                            "parent_task_id": "root",
                            "agent_id": "writer"
                        }
                    },
                    "synced_at_ms": 123
                }
            }
        }));
        let rc = RunConfig::new();
        let ctx = make_ctx(Phase::BeforeInference, "thread-1", &rc, &doc);

        let actions = plugin.before_inference(&ctx).await;
        let (state_actions, contexts) = before_inference_parts(actions);
        assert!(state_actions.is_empty());
        assert_eq!(contexts.len(), 1);
        assert!(contexts[0].contains("<background_tasks>"));
        assert!(contexts[0].contains("task-1"));
        assert!(contexts[0].contains("delegate to writer"));
        assert!(contexts[0].contains("task_cancel"));
    }

    #[tokio::test]
    async fn after_tool_execute_empty_when_no_tasks() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let plugin = BackgroundTasksPlugin::new(mgr);

        let doc = DocCell::new(json!({}));
        let rc = RunConfig::new();
        let ctx = make_ctx(Phase::AfterToolExecute, "thread-1", &rc, &doc);

        let actions = plugin.after_tool_execute(&ctx).await;
        let (state_actions, reminders) = after_tool_parts(actions);
        assert!(state_actions.is_empty());
        assert!(reminders.is_empty());
    }

    #[tokio::test]
    async fn after_tool_execute_shows_running_tasks_and_updates_view() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        mgr.spawn(
            "thread-1",
            "shell",
            "building project",
            |cancel| async move {
                cancel.cancelled().await;
                super::super::types::TaskResult::Cancelled
            },
        )
        .await;

        let plugin = BackgroundTasksPlugin::new(mgr);
        let doc = DocCell::new(json!({}));
        let rc = RunConfig::new();
        let ctx = make_ctx(Phase::AfterToolExecute, "thread-1", &rc, &doc);

        let actions = plugin.after_tool_execute(&ctx).await;
        let (state_actions, reminders) = after_tool_parts(actions);
        assert_eq!(reminders.len(), 1);
        assert!(reminders[0].contains("<background_tasks>"));
        assert!(reminders[0].contains("building project"));
        assert!(reminders[0].contains("task_status"));
        assert!(reminders[0].contains("task_output"));
        apply_state_actions(&doc, state_actions);

        let derived = derived_view(&doc);
        assert_eq!(derived.tasks.len(), 1);
        let task = derived
            .tasks
            .values()
            .find(|task| task.description == "building project")
            .expect("running task view should exist");
        assert_eq!(task.task_type, "shell");
        assert_eq!(task.status.as_str(), "running");
    }

    #[tokio::test]
    async fn after_tool_execute_ignores_completed_tasks() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        mgr.spawn("thread-1", "http", "fetch data", |_| async {
            super::super::types::TaskResult::Success(Value::Null)
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plugin = BackgroundTasksPlugin::new(mgr);
        let doc = DocCell::new(json!({}));
        let rc = RunConfig::new();
        let ctx = make_ctx(Phase::AfterToolExecute, "thread-1", &rc, &doc);

        let actions = plugin.after_tool_execute(&ctx).await;
        let (state_actions, reminders) = after_tool_parts(actions);
        assert!(
            reminders.is_empty(),
            "completed tasks should not trigger reminder"
        );
        assert!(
            state_actions.is_empty(),
            "empty cached view should remain unchanged"
        );
    }

    #[tokio::test]
    async fn after_tool_execute_clears_stale_derived_view_when_no_tasks() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        let plugin = BackgroundTasksPlugin::new(mgr);
        let doc = DocCell::new(json!({
            "__derived": {
                "background_tasks": {
                    "tasks": {
                        "task-1": {
                            "task_type": "shell",
                            "description": "stale task",
                            "status": "running"
                        }
                    },
                    "synced_at_ms": 1
                }
            }
        }));
        let rc = RunConfig::new();
        let ctx = make_ctx(Phase::AfterToolExecute, "thread-1", &rc, &doc);

        let actions = plugin.after_tool_execute(&ctx).await;
        let (state_actions, reminders) = after_tool_parts(actions);
        assert!(reminders.is_empty());
        assert_eq!(state_actions.len(), 1);
        apply_state_actions(&doc, state_actions);
        assert!(derived_view(&doc).tasks.is_empty());
    }

    #[tokio::test]
    async fn after_tool_execute_thread_isolation() {
        let mgr = Arc::new(BackgroundTaskManager::new());
        mgr.spawn("thread-A", "shell", "private task", |cancel| async move {
            cancel.cancelled().await;
            super::super::types::TaskResult::Cancelled
        })
        .await;

        let plugin = BackgroundTasksPlugin::new(mgr);
        let doc = DocCell::new(json!({}));
        let rc = RunConfig::new();

        let ctx_b = make_ctx(Phase::AfterToolExecute, "thread-B", &rc, &doc);
        let actions = plugin.after_tool_execute(&ctx_b).await;
        let (state_actions, reminders) = after_tool_parts(actions);
        assert!(state_actions.is_empty());
        assert!(reminders.is_empty());

        let ctx_a = make_ctx(Phase::AfterToolExecute, "thread-A", &rc, &doc);
        let actions = plugin.after_tool_execute(&ctx_a).await;
        let (_, reminders) = after_tool_parts(actions);
        assert_eq!(reminders.len(), 1);
    }
}
