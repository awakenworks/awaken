use super::*;
use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::storage::{
    Committed, MessagePage, MessageQuery, RunPage, RunQuery, RunRecord, ThreadHead, ThreadListPage,
    ThreadListQuery, ThreadReader, ThreadStore, ThreadStoreError, ThreadWriter,
    VersionPrecondition,
};
use crate::contracts::thread::{Thread, ThreadChangeSet};
use crate::runtime::background_tasks::{BackgroundCapable, TaskStore, TASK_THREAD_PREFIX};
use async_trait::async_trait;

struct TaskLoadFailingStore {
    inner: Arc<tirea_store_adapters::MemoryStore>,
}

#[async_trait]
impl ThreadReader for TaskLoadFailingStore {
    async fn load(&self, thread_id: &str) -> Result<Option<ThreadHead>, ThreadStoreError> {
        if thread_id.starts_with(TASK_THREAD_PREFIX) {
            return Err(ThreadStoreError::Io(std::io::Error::other(
                "injected task load failure",
            )));
        }
        self.inner.load(thread_id).await
    }

    async fn list_threads(
        &self,
        query: &ThreadListQuery,
    ) -> Result<ThreadListPage, ThreadStoreError> {
        self.inner.list_threads(query).await
    }

    async fn load_messages(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, ThreadStoreError> {
        self.inner.load_messages(thread_id, query).await
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, ThreadStoreError> {
        self.inner.load_run(run_id).await
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, ThreadStoreError> {
        self.inner.list_runs(query).await
    }

    async fn active_run_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Option<RunRecord>, ThreadStoreError> {
        self.inner.active_run_for_thread(thread_id).await
    }
}

#[async_trait]
impl ThreadWriter for TaskLoadFailingStore {
    async fn create(&self, thread: &Thread) -> Result<Committed, ThreadStoreError> {
        self.inner.create(thread).await
    }

    async fn append(
        &self,
        thread_id: &str,
        delta: &ThreadChangeSet,
        precondition: VersionPrecondition,
    ) -> Result<Committed, ThreadStoreError> {
        self.inner.append(thread_id, delta, precondition).await
    }

    async fn delete(&self, thread_id: &str) -> Result<(), ThreadStoreError> {
        self.inner.delete(thread_id).await
    }

    async fn save(&self, thread: &Thread) -> Result<(), ThreadStoreError> {
        self.inner.save(thread).await
    }
}

fn build_worker_os_with_store(
    include_slow_terminate: bool,
) -> (AgentOs, Arc<tirea_store_adapters::MemoryStore>) {
    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let mut builder = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>);
    if include_slow_terminate {
        builder = builder.with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        );
    }
    let worker = if include_slow_terminate {
        crate::runtime::AgentDefinition::new("gpt-4o-mini")
            .with_behavior_id("slow_terminate_behavior_requested")
    } else {
        crate::runtime::AgentDefinition::new("gpt-4o-mini")
    };
    let os = builder.with_agent("worker", worker).build().unwrap();
    (os, storage)
}

async fn persist_agent_run(
    storage: Arc<tirea_store_adapters::MemoryStore>,
    owner_thread_id: &str,
    run_id: &str,
    agent_id: &str,
    thread_id: &str,
    status: crate::runtime::background_tasks::TaskStatus,
    error: Option<&str>,
    parent_task_id: Option<&str>,
) {
    let task_store = crate::runtime::background_tasks::TaskStore::new(
        storage as Arc<dyn crate::contracts::storage::ThreadStore>,
    );
    task_store
        .create_task(crate::runtime::background_tasks::NewTaskSpec {
            task_id: run_id.to_string(),
            owner_thread_id: owner_thread_id.to_string(),
            task_type: AGENT_RUN_TOOL_ID.to_string(),
            description: format!("agent:{agent_id}"),
            parent_task_id: parent_task_id.map(str::to_string),
            supports_resume: true,
            metadata: json!({
                "thread_id": thread_id,
                "agent_id": agent_id
            }),
        })
        .await
        .unwrap();
    if status != crate::runtime::background_tasks::TaskStatus::Running {
        task_store
            .persist_foreground_result(run_id, status, error.map(str::to_string), None)
            .await
            .unwrap();
    }
}

/// Helper: wrap AgentRunTool in BackgroundCapable with optional task store.
fn wrap_with_bg(
    os: AgentOs,
    bg_mgr: Arc<BackgroundTaskManager>,
    storage: Option<Arc<dyn ThreadStore>>,
) -> BackgroundCapable<AgentRunTool> {
    let task_store = storage.map(|s| Arc::new(TaskStore::new(s)));
    BackgroundCapable::new(AgentRunTool::new(os), bg_mgr).with_task_store(task_store)
}

// ── AgentRunTool validation tests (no background/resume) ─────────────────────

#[tokio::test]
async fn agent_run_tool_requires_scope_context() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let tool = AgentRunTool::new(os);
    let fix = TestFixture::new();
    let result = tool
        .execute(
            json!({"agent_id":"worker","prompt":"hi"}),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("missing caller thread context"));
}

#[tokio::test]
async fn agent_run_tool_rejects_disallowed_target_agent() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .with_agent(
            "reviewer",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let tool = AgentRunTool::new(os);
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);
    fix.run_policy
        .set_allowed_agents_if_absent(Some(&["worker".to_string()]));
    let result = tool
        .execute(
            json!({"agent_id":"reviewer","prompt":"hi","run_id":"test-run"}),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("Unknown or unavailable agent_id"));
}

#[tokio::test]
async fn agent_run_tool_rejects_self_target_agent() {
    let os = AgentOs::builder()
        .with_agent(
            "caller",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let tool = AgentRunTool::new(os);
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);
    let result = tool
        .execute(
            json!({"agent_id":"caller","prompt":"hi","run_id":"test-run"}),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("Unknown or unavailable agent_id"));
}

#[tokio::test]
async fn background_agent_run_tool_rejects_disallowed_target_agent() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .with_agent(
            "reviewer",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, None);
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);
    fix.run_policy
        .set_allowed_agents_if_absent(Some(&["worker".to_string()]));

    let result = wrapped
        .execute(
            json!({
                "agent_id":"reviewer",
                "prompt":"hi",
                "run_in_background":true
            }),
            &fix.ctx_with("call-bg", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("Unknown or unavailable agent_id"));
}

#[tokio::test]
async fn background_agent_run_tool_rejects_self_target_agent() {
    let os = AgentOs::builder()
        .with_agent(
            "caller",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, None);
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    let result = wrapped
        .execute(
            json!({
                "agent_id":"caller",
                "prompt":"hi",
                "run_in_background":true
            }),
            &fix.ctx_with("call-bg", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("Unknown or unavailable agent_id"));
}

#[tokio::test]
async fn background_agent_run_tool_rejects_excluded_agent_without_persisting_task() {
    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .with_agent(
            "secret",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(
        os,
        bg_mgr.clone(),
        Some(storage.clone() as Arc<dyn ThreadStore>),
    );
    let task_store = TaskStore::new(storage as Arc<dyn ThreadStore>);
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);
    fix.run_policy
        .set_excluded_agents_if_absent(Some(&["secret".to_string()]));

    let result = wrapped
        .execute(
            json!({
                "agent_id":"secret",
                "prompt":"hi",
                "run_in_background": true
            }),
            &fix.ctx_with("call-bg", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("Unknown or unavailable agent_id"));

    let persisted = task_store
        .list_tasks_for_owner("owner-thread")
        .await
        .expect("list should succeed");
    assert!(
        persisted.is_empty(),
        "validation failure should not persist a task"
    );
    assert!(
        bg_mgr.list("owner-thread", None).await.is_empty(),
        "validation failure should not spawn a live background task"
    );
}

#[tokio::test]
async fn agent_run_tool_surfaces_task_store_load_failure_on_start() {
    let mem_store = Arc::new(tirea_store_adapters::MemoryStore::new());
    let storage: Arc<dyn ThreadStore> = Arc::new(TaskLoadFailingStore { inner: mem_store });
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone())
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let task_store = Some(Arc::new(TaskStore::new(storage)));
    let wrapped = BackgroundCapable::new(AgentRunTool::new(os), bg_mgr).with_task_store(task_store);
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    let result = wrapped
        .execute(
            json!({"agent_id":"worker","prompt":"hi","run_in_background":true}),
            &fix.ctx_with("call-load-fail", "tool:agent_run"),
        )
        .await;

    // The wrapper returns ToolError when persist_start fails.
    let err = result.expect_err("should fail with ToolError");
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("task persist failed") || err_msg.contains("injected task load failure"),
        "expected task persist error, got: {err_msg}"
    );
}

#[tokio::test]
async fn agent_run_tool_fork_context_filters_messages() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, None);

    let fork_messages = vec![
        crate::contracts::thread::Message::system("parent-system"),
        crate::contracts::thread::Message::internal_system("parent-internal-system"),
        crate::contracts::thread::Message::user("parent-user-1"),
        crate::contracts::thread::Message::assistant_with_tool_calls(
            "parent-assistant-tool-call",
            vec![
                crate::contracts::thread::ToolCall::new(
                    "call-paired",
                    "search",
                    json!({"q":"paired"}),
                ),
                crate::contracts::thread::ToolCall::new(
                    "call-missing",
                    "search",
                    json!({"q":"missing"}),
                ),
            ],
        ),
        crate::contracts::thread::Message::tool("call-paired", "tool paired result"),
        crate::contracts::thread::Message::tool("call-orphan", "tool orphan result"),
        crate::contracts::thread::Message::assistant_with_tool_calls(
            "assistant-unpaired-only",
            vec![crate::contracts::thread::ToolCall::new(
                "call-only-assistant",
                "search",
                json!({"q":"only-assistant"}),
            )],
        ),
        crate::contracts::thread::Message::assistant("parent-assistant-plain"),
    ];

    let mut fix = TestFixture::new();
    apply_caller_context_with_state_run_and_messages(
        &mut fix,
        json!({"forked": true}),
        "parent-run-42",
        fork_messages,
    );

    let started = wrapped
        .execute(
            json!({
                "agent_id":"worker",
                "prompt":"child-prompt",
                "run_in_background": true,
                "fork_context": true
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(started.status, ToolStatus::Success);
    assert_eq!(started.data["status"], json!("running_in_background"));
    assert!(started.data["task_id"].as_str().is_some());
}

#[tokio::test]
async fn background_stop_then_resume_completes() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr.clone(), None);

    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);
    let started = wrapped
        .execute(
            json!({
                "agent_id":"worker",
                "prompt":"start",
                "run_in_background": true
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(started.status, ToolStatus::Success);
    assert_eq!(started.data["status"], json!("running_in_background"));
    let task_id = started.data["task_id"]
        .as_str()
        .expect("task_id should exist")
        .to_string();

    // Cancel via BackgroundTaskManager directly.
    bg_mgr.cancel("owner-thread", &task_id).await.unwrap();

    // Give cancelled background task a chance to flush stale completion.
    tokio::time::sleep(Duration::from_millis(30)).await;

    // Resume foreground: the wrapper sees cancelled status and (since
    // supports_resume is true) re-executes the task.
    let resumed = wrapped
        .execute(
            json!({
                "run_id": task_id,
                "prompt":"resume"
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, ToolStatus::Success);
    let status = resumed.data["status"].as_str().unwrap();
    assert!(
        status == "completed" || status == "failed" || status == "cancelled",
        "expected terminal status, got: {status}"
    );
}

#[tokio::test]
async fn agent_run_tool_persists_task_thread_state() {
    let (os, storage) = build_worker_os_with_store(true);
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, Some(storage.clone() as Arc<dyn ThreadStore>));

    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);
    let started = wrapped
        .execute(
            json!({
                "agent_id":"worker",
                "prompt":"start",
                "run_in_background": true
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(started.status, ToolStatus::Success);
    let task_id = started.data["task_id"]
        .as_str()
        .expect("task_id should exist")
        .to_string();
    let task_store = crate::runtime::background_tasks::TaskStore::new(
        storage as Arc<dyn crate::contracts::storage::ThreadStore>,
    );
    let task = task_store
        .load_task(&task_id)
        .await
        .unwrap()
        .expect("task should be persisted");
    assert_eq!(
        task.status,
        crate::runtime::background_tasks::TaskStatus::Running
    );
    assert_eq!(task.parent_task_id.as_deref(), Some("parent-run-default"));
    assert_eq!(task.metadata["agent_id"], json!("worker"));
    assert!(task.metadata["thread_id"].as_str().is_some());
}

#[tokio::test]
async fn agent_run_tool_binds_scope_run_id_and_parent_lineage() {
    let (os, storage) = build_worker_os_with_store(true);
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, Some(storage.clone() as Arc<dyn ThreadStore>));

    let mut fix = TestFixture::new();
    apply_caller_context_with_state_and_run(&mut fix, json!({"forked": true}), "parent-run-42");
    let started = wrapped
        .execute(
            json!({
                "agent_id":"worker",
                "prompt":"start",
                "run_in_background": true
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(started.status, ToolStatus::Success);
    let task_id = started.data["task_id"]
        .as_str()
        .expect("task_id should exist")
        .to_string();
    let task_store = crate::runtime::background_tasks::TaskStore::new(
        storage as Arc<dyn crate::contracts::storage::ThreadStore>,
    );
    let task = task_store
        .load_task(&task_id)
        .await
        .unwrap()
        .expect("task should be persisted");
    assert_eq!(task.parent_task_id.as_deref(), Some("parent-run-42"));
}

#[tokio::test]
async fn agent_run_tool_query_existing_run_keeps_original_parent_lineage() {
    let (os, storage) = build_worker_os_with_store(false);
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, Some(storage.clone() as Arc<dyn ThreadStore>));
    persist_agent_run(
        storage.clone(),
        "owner-thread",
        "run-1",
        "worker",
        "custom-child-thread",
        crate::runtime::background_tasks::TaskStatus::Running,
        None,
        Some("original-parent"),
    )
    .await;
    let mut fix = TestFixture::new();
    apply_caller_context_with_state_and_run(&mut fix, json!({}), "query-parent-run");

    // The wrapper sees an orphaned Running task (no live handle), marks it
    // Stopped, then immediately resumes since supports_resume() is true.
    // The resumed execution produces a terminal result.
    let result = wrapped
        .execute(
            json!({
                "run_id":"run-1"
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);

    // After orphan detection + resume, the result should be terminal.
    let status = result.data["status"].as_str().unwrap();
    assert!(
        status == "completed" || status == "failed",
        "expected terminal status after orphan resume, got: {status}"
    );
    let task_store = crate::runtime::background_tasks::TaskStore::new(
        storage as Arc<dyn crate::contracts::storage::ThreadStore>,
    );
    let task = task_store.load_task("run-1").await.unwrap().unwrap();
    assert_eq!(task.metadata["thread_id"], json!("custom-child-thread"));
    assert_eq!(task.metadata["agent_id"], json!("worker"));
    assert_eq!(task.parent_task_id.as_deref(), Some("original-parent"));
}

#[tokio::test]
async fn agent_run_tool_resumes_from_persisted_state_without_live_record() {
    let (os, storage) = build_worker_os_with_store(true);
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, Some(storage.clone() as Arc<dyn ThreadStore>));
    persist_agent_run(
        storage,
        "owner-thread",
        "run-1",
        "worker",
        "sub-agent-run-1",
        crate::runtime::background_tasks::TaskStatus::Stopped,
        None,
        None,
    )
    .await;
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);
    let resumed = wrapped
        .execute(
            json!({
                "run_id":"run-1",
                "prompt":"resume"
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, ToolStatus::Success);
    let status = resumed.data["status"].as_str().unwrap();
    assert!(
        status == "completed" || status == "failed",
        "expected terminal status, got: {status}"
    );
}

#[tokio::test]
async fn agent_run_tool_marks_orphan_running_as_stopped_before_resume() {
    let (os, storage) = build_worker_os_with_store(true);
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, Some(storage.clone() as Arc<dyn ThreadStore>));
    persist_agent_run(
        storage.clone(),
        "owner-thread",
        "run-1",
        "worker",
        "sub-agent-run-1",
        crate::runtime::background_tasks::TaskStatus::Running,
        None,
        None,
    )
    .await;
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    // The wrapper detects an orphaned Running task (persisted but no live
    // handle), marks it Stopped, then immediately resumes since
    // supports_resume() is true. The resumed execution produces a terminal
    // result (completed or failed).
    let summary = wrapped
        .execute(
            json!({
                "run_id":"run-1"
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(summary.status, ToolStatus::Success);

    // After orphan detection + resume, the result should be terminal.
    let status = summary.data["status"].as_str().unwrap();
    assert!(
        status == "completed" || status == "failed",
        "expected terminal status after orphan resume, got: {status}"
    );

    // Verify the task store reflects the final state (resumed execution
    // persists terminal result after the intermediate Stopped mark).
    let task_store = crate::runtime::background_tasks::TaskStore::new(
        storage as Arc<dyn crate::contracts::storage::ThreadStore>,
    );
    let task = task_store.load_task("run-1").await.unwrap().unwrap();
    // The attempt count should have increased from the resume.
    assert!(
        task.attempt >= 2,
        "expected attempt >= 2 after resume, got: {}",
        task.attempt
    );
}

// ── AgentRunTool: resume completed/failed returns status ─────────────────────

#[tokio::test]
async fn agent_run_tool_returns_completed_status_when_resuming_completed_run() {
    let (os, storage) = build_worker_os_with_store(false);
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, Some(storage.clone() as Arc<dyn ThreadStore>));
    persist_agent_run(
        storage.clone(),
        "owner-thread",
        "run-1",
        "worker",
        "sub-agent-run-1",
        crate::runtime::background_tasks::TaskStatus::Completed,
        None,
        None,
    )
    .await;
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    let result = wrapped
        .execute(
            json!({ "run_id": "run-1" }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("completed"));
    assert_eq!(result.data["task_id"], json!("run-1"));
}

#[tokio::test]
async fn agent_run_tool_returns_failed_status_when_resuming_failed_run() {
    let (os, storage) = build_worker_os_with_store(false);
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, Some(storage.clone() as Arc<dyn ThreadStore>));
    persist_agent_run(
        storage.clone(),
        "owner-thread",
        "run-1",
        "worker",
        "sub-agent-run-1",
        crate::runtime::background_tasks::TaskStatus::Failed,
        Some("agent failed"),
        None,
    )
    .await;
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    let result = wrapped
        .execute(
            json!({ "run_id": "run-1" }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("failed"));
    assert_eq!(result.data["error"], json!("agent failed"));
    assert_eq!(result.data["task_id"], json!("run-1"));
}

// ── AgentRunTool: missing required args ──────────────────────────────────────

#[tokio::test]
async fn agent_run_tool_requires_prompt_for_new_run() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os);
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    let result = run_tool
        .execute(
            json!({ "agent_id": "worker", "run_id": "test-run" }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("missing 'prompt'"));
}

#[tokio::test]
async fn background_agent_run_tool_requires_prompt_for_new_run() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, None);
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    let result = wrapped
        .execute(
            json!({ "agent_id": "worker", "run_in_background": true }),
            &fix.ctx_with("call-bg", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("missing 'prompt'"));
}

#[tokio::test]
async fn agent_run_tool_requires_agent_id_for_new_run() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os);
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    let result = run_tool
        .execute(
            json!({ "prompt": "hello", "run_id": "test-run" }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("missing 'agent_id'"));
}

#[tokio::test]
async fn background_agent_run_tool_requires_agent_id_for_new_run() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, None);
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    let result = wrapped
        .execute(
            json!({ "prompt": "hello", "run_in_background": true }),
            &fix.ctx_with("call-bg", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("missing 'agent_id'"));
}

// ── AgentRunTool: excluded agents via scope ──────────────────────────────────

#[tokio::test]
async fn agent_run_tool_rejects_excluded_agent() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .with_agent(
            "secret",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os);
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);
    fix.run_policy
        .set_excluded_agents_if_absent(Some(&["secret".to_string()]));

    let result = run_tool
        .execute(
            json!({ "agent_id": "secret", "prompt": "hi", "run_id": "test-run" }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("Unknown or unavailable agent_id"));
}

// ── AgentRunTool: unknown run_id (no handle, no persisted) ───────────────────

#[tokio::test]
async fn agent_run_tool_returns_error_for_unknown_run_id() {
    let os = AgentOs::builder().build().unwrap();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, None);
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    let result = wrapped
        .execute(
            json!({ "run_id": "nonexistent" }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    let message = result.message.unwrap_or_default();
    assert!(
        message.contains("Unknown task"),
        "expected 'Unknown task' error, got: {message}"
    );
}

// ── AgentRunTool: persisted completed/failed returns without re-run ──────────

#[tokio::test]
async fn agent_run_tool_returns_persisted_completed_without_rerun() {
    let (os, storage) = build_worker_os_with_store(false);
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, Some(storage.clone() as Arc<dyn ThreadStore>));
    persist_agent_run(
        storage,
        "owner-thread",
        "run-1",
        "worker",
        "sub-agent-run-1",
        crate::runtime::background_tasks::TaskStatus::Completed,
        None,
        None,
    )
    .await;
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    let result = wrapped
        .execute(
            json!({ "run_id": "run-1" }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("completed"));
    assert_eq!(result.data["task_id"], json!("run-1"));
}

#[tokio::test]
async fn agent_run_tool_returns_persisted_failed_with_error() {
    let (os, storage) = build_worker_os_with_store(false);
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, Some(storage.clone() as Arc<dyn ThreadStore>));
    persist_agent_run(
        storage,
        "owner-thread",
        "run-1",
        "worker",
        "sub-agent-run-1",
        crate::runtime::background_tasks::TaskStatus::Failed,
        Some("something broke"),
        None,
    )
    .await;
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    let result = wrapped
        .execute(
            json!({ "run_id": "run-1" }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("failed"));
    assert_eq!(result.data["error"], json!("something broke"));
    assert_eq!(result.data["task_id"], json!("run-1"));
}

#[tokio::test]
async fn foreground_agent_run_persists_failed_task_status() {
    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_provider("p1", genai::Client::default())
        .with_model(
            "m1",
            crate::composition::ModelDefinition::new("p1", "gpt-4o-mini"),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("missing-model-ref"),
        )
        .build()
        .unwrap();
    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let wrapped = wrap_with_bg(os, bg_mgr, Some(storage.clone() as Arc<dyn ThreadStore>));
    let mut fix = TestFixture::new();
    apply_caller_context(&mut fix);

    let result = wrapped
        .execute(
            json!({ "agent_id": "worker", "prompt": "hi" }),
            &fix.ctx_with("call-fg", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("failed"));

    let run_id = result.data["run_id"]
        .as_str()
        .expect("foreground run should return run_id");
    let task_store = TaskStore::new(storage as Arc<dyn ThreadStore>);
    let task = task_store
        .load_task(run_id)
        .await
        .unwrap()
        .expect("foreground run task should persist");
    assert_eq!(
        task.status,
        crate::runtime::background_tasks::TaskStatus::Failed
    );
}

#[test]
fn foreground_agent_run_task_status_mapping_tracks_embedded_terminal_status() {
    let tool = AgentRunTool::new(AgentOs::builder().build().unwrap());

    let failed = ToolResult::success(
        AGENT_RUN_TOOL_ID,
        json!({"status":"failed","error":"child failed"}),
    );
    let stopped = ToolResult::success(
        AGENT_RUN_TOOL_ID,
        json!({"status":"stopped","error":"child stopped"}),
    );
    let cancelled = ToolResult::success(
        AGENT_RUN_TOOL_ID,
        json!({"status":"cancelled","error":"child cancelled"}),
    );
    let completed = ToolResult::success(AGENT_RUN_TOOL_ID, json!({"status":"completed"}));

    assert_eq!(
        <AgentRunTool as crate::runtime::background_tasks::BackgroundExecutable>::foreground_task_status(
            &tool,
            &failed,
        ),
        (
            crate::runtime::background_tasks::TaskStatus::Failed,
            Some("child failed".to_string())
        )
    );
    assert_eq!(
        <AgentRunTool as crate::runtime::background_tasks::BackgroundExecutable>::foreground_task_status(
            &tool,
            &stopped,
        ),
        (
            crate::runtime::background_tasks::TaskStatus::Stopped,
            Some("child stopped".to_string())
        )
    );
    assert_eq!(
        <AgentRunTool as crate::runtime::background_tasks::BackgroundExecutable>::foreground_task_status(
            &tool,
            &cancelled,
        ),
        (
            crate::runtime::background_tasks::TaskStatus::Cancelled,
            Some("child cancelled".to_string())
        )
    );
    assert_eq!(
        <AgentRunTool as crate::runtime::background_tasks::BackgroundExecutable>::foreground_task_status(
            &tool,
            &completed,
        ),
        (crate::runtime::background_tasks::TaskStatus::Completed, None)
    );
}
