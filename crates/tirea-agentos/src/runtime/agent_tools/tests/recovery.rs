use super::*;
use crate::contracts::runtime::phase::StepContext;
use crate::contracts::storage::ThreadStore;
use crate::loop_runtime::loop_runner::RunCancellationToken;
use crate::runtime::background_tasks::{NewTaskSpec, SpawnParams, TaskResult, TaskStatus, TaskStore};

fn recovery_task_store() -> Arc<TaskStore> {
    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    Arc::new(TaskStore::new(storage as Arc<dyn ThreadStore>))
}

async fn persist_agent_run(
    task_store: &TaskStore,
    owner_thread_id: &str,
    run_id: &str,
    agent_id: &str,
    status: TaskStatus,
    error: Option<&str>,
) {
    task_store
        .create_task(NewTaskSpec {
            task_id: run_id.to_string(),
            owner_thread_id: owner_thread_id.to_string(),
            task_type: AGENT_RUN_TOOL_ID.to_string(),
            description: format!("agent:{agent_id}"),
            parent_task_id: None,
            supports_resume: true,
            metadata: json!({
                "thread_id": format!("sub-agent-{run_id}"),
                "agent_id": agent_id
            }),
        })
        .await
        .unwrap();

    if status != TaskStatus::Running {
        task_store
            .persist_foreground_result(run_id, status, error.map(str::to_string), None)
            .await
            .unwrap();
    }
}

async fn persisted_status(task_store: &TaskStore, run_id: &str) -> TaskStatus {
    task_store
        .load_task(run_id)
        .await
        .unwrap()
        .expect("task should exist")
        .status
}

fn owner_step<'a>(fixture: &'a TestFixture) -> StepContext<'a> {
    StepContext::new(fixture.ctx(), "owner-1", &fixture.messages, vec![])
}

// ── AgentRecoveryPlugin tests ────────────────────────────────────────────────

#[tokio::test]
async fn recovery_plugin_detects_orphan_and_records_confirmation() {
    let task_store = recovery_task_store();
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-1",
        "worker",
        TaskStatus::Running,
        None,
    )
    .await;
    let plugin = AgentRecoveryPlugin::new(Arc::new(BackgroundTaskManager::new()))
        .with_task_store(Some(task_store.clone()));
    let thread = Thread::with_initial_state("owner-1", json!({}));
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = owner_step(&fixture);
    plugin.run_phase(Phase::RunStart, &mut step).await;
    assert!(matches!(
        step.run_action(),
        crate::contracts::RunAction::Continue
    ));

    assert_eq!(
        persisted_status(&task_store, "run-1").await,
        TaskStatus::Stopped
    );
    let updated = fixture.updated_state();
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["suspended_call"]["suspension"]
            ["action"],
        json!(AGENT_RECOVERY_INTERACTION_ACTION)
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["suspended_call"]["suspension"]
            ["parameters"]["run_id"],
        json!("run-1")
    );

    let fixture2 = TestFixture::new_with_state(updated);
    let mut before = owner_step(&fixture2);
    plugin.run_phase(Phase::BeforeInference, &mut before).await;
    assert!(
        matches!(before.run_action(), crate::contracts::RunAction::Continue),
        "recovery plugin should not control inference flow in BeforeInference"
    );
}

#[tokio::test]
async fn recovery_plugin_does_not_override_existing_suspended_interaction() {
    let task_store = recovery_task_store();
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-1",
        "worker",
        TaskStatus::Running,
        None,
    )
    .await;
    let plugin = AgentRecoveryPlugin::new(Arc::new(BackgroundTaskManager::new()))
        .with_task_store(Some(task_store));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "__tool_call_scope": {
                "existing_1": {
                    "suspended_call": {
                        "call_id": "existing_1",
                        "tool_name": "agent_run",
                        "suspension": {
                            "id": "existing_1",
                            "action": AGENT_RECOVERY_INTERACTION_ACTION
                        },
                        "arguments": {},
                        "pending": {
                            "id": "existing_1",
                            "name": AGENT_RECOVERY_INTERACTION_ACTION,
                            "arguments": {}
                        },
                        "resume_mode": "pass_decision_to_tool"
                    }
                }
            }
        }),
    );

    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = owner_step(&fixture);
    plugin.run_phase(Phase::RunStart, &mut step).await;
    assert!(
        matches!(step.run_action(), crate::contracts::RunAction::Continue),
        "existing suspended interaction should not be replaced"
    );

    let updated = fixture.updated_state();
    assert_eq!(
        updated["__tool_call_scope"]["existing_1"]["suspended_call"]["suspension"]["id"],
        json!("existing_1")
    );
}

#[tokio::test]
async fn recovery_plugin_auto_approve_when_permission_allow() {
    let task_store = recovery_task_store();
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-1",
        "worker",
        TaskStatus::Running,
        None,
    )
    .await;
    let plugin = AgentRecoveryPlugin::new(Arc::new(BackgroundTaskManager::new()))
        .with_task_store(Some(task_store.clone()));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "ask",
                "tools": {
                    "recover_agent_run": "allow"
                }
            }
        }),
    );
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = owner_step(&fixture);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let updated = fixture.updated_state();
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["tool_call_state"]["status"],
        json!("resuming")
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["tool_call_state"]["resume"]["action"],
        json!("resume")
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["suspended_call"]["tool_name"],
        json!("agent_run")
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["suspended_call"]["suspension"]
            ["parameters"]["run_id"],
        json!("run-1")
    );
    assert_eq!(
        persisted_status(&task_store, "run-1").await,
        TaskStatus::Stopped
    );
}

#[cfg(feature = "permission")]
#[tokio::test]
async fn recovery_plugin_auto_deny_when_permission_deny() {
    let task_store = recovery_task_store();
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-1",
        "worker",
        TaskStatus::Running,
        None,
    )
    .await;
    let plugin = AgentRecoveryPlugin::new(Arc::new(BackgroundTaskManager::new()))
        .with_task_store(Some(task_store.clone()));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "ask",
                "tools": {
                    "recover_agent_run": "deny"
                }
            }
        }),
    );
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = owner_step(&fixture);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let updated = fixture.updated_state();
    assert!(
        updated
            .get("__tool_call_scope")
            .and_then(|scopes| scopes.get("agent_recovery_run-1"))
            .and_then(|scope| scope.get("tool_call_state"))
            .is_none(),
        "deny should not set recovery tool-call resume state"
    );
    assert_eq!(
        persisted_status(&task_store, "run-1").await,
        TaskStatus::Stopped
    );
    assert!(updated
        .get("__suspended_tool_calls")
        .and_then(|v| v.get("calls"))
        .and_then(|v| v.as_object())
        .map_or(true, |calls| calls.is_empty()));
}

#[tokio::test]
async fn recovery_plugin_auto_approve_from_default_behavior_allow() {
    let task_store = recovery_task_store();
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-1",
        "worker",
        TaskStatus::Running,
        None,
    )
    .await;
    let plugin = AgentRecoveryPlugin::new(Arc::new(BackgroundTaskManager::new()))
        .with_task_store(Some(task_store));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "allow",
                "tools": {}
            }
        }),
    );
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = owner_step(&fixture);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let updated = fixture.updated_state();
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["tool_call_state"]["status"],
        json!("resuming")
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["tool_call_state"]["resume"]["action"],
        json!("resume")
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["suspended_call"]["tool_name"],
        json!("agent_run")
    );
}

#[cfg(feature = "permission")]
#[tokio::test]
async fn recovery_plugin_auto_deny_from_default_behavior_deny() {
    let task_store = recovery_task_store();
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-1",
        "worker",
        TaskStatus::Running,
        None,
    )
    .await;
    let plugin = AgentRecoveryPlugin::new(Arc::new(BackgroundTaskManager::new()))
        .with_task_store(Some(task_store));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "deny",
                "tools": {}
            }
        }),
    );
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = owner_step(&fixture);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let updated = fixture.updated_state();
    assert!(
        updated
            .get("__tool_call_scope")
            .and_then(|scopes| scopes.get("agent_recovery_run-1"))
            .and_then(|scope| scope.get("tool_call_state"))
            .is_none(),
        "deny should not set recovery tool-call resume state"
    );
    assert!(updated
        .get("__suspended_tool_calls")
        .and_then(|v| v.get("calls"))
        .and_then(|v| v.as_object())
        .map_or(true, |calls| calls.is_empty()));
}

#[cfg(feature = "permission")]
#[tokio::test]
async fn recovery_plugin_tool_rule_overrides_default_behavior() {
    let task_store = recovery_task_store();
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-1",
        "worker",
        TaskStatus::Running,
        None,
    )
    .await;
    let plugin = AgentRecoveryPlugin::new(Arc::new(BackgroundTaskManager::new()))
        .with_task_store(Some(task_store));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "allow",
                "tools": {
                    "recover_agent_run": "ask"
                }
            }
        }),
    );
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = owner_step(&fixture);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let updated = fixture.updated_state();
    assert!(
        updated
            .get("__tool_call_scope")
            .and_then(|scopes| scopes.get("agent_recovery_run-1"))
            .and_then(|scope| scope.get("tool_call_state"))
            .is_none(),
        "tool-level ask should not set recovery tool-call resume state"
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["suspended_call"]["suspension"]
            ["action"],
        json!(AGENT_RECOVERY_INTERACTION_ACTION)
    );
}

#[tokio::test]
async fn recovery_plugin_detects_multiple_orphans_creates_one_suspension() {
    let task_store = recovery_task_store();
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-1",
        "worker-a",
        TaskStatus::Running,
        None,
    )
    .await;
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-2",
        "worker-b",
        TaskStatus::Running,
        None,
    )
    .await;
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-3",
        "worker-c",
        TaskStatus::Running,
        None,
    )
    .await;
    let plugin = AgentRecoveryPlugin::new(Arc::new(BackgroundTaskManager::new()))
        .with_task_store(Some(task_store.clone()));
    let thread = Thread::with_initial_state("owner-1", json!({}));
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = owner_step(&fixture);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    assert_eq!(
        persisted_status(&task_store, "run-1").await,
        TaskStatus::Stopped
    );
    assert_eq!(
        persisted_status(&task_store, "run-2").await,
        TaskStatus::Stopped
    );
    assert_eq!(
        persisted_status(&task_store, "run-3").await,
        TaskStatus::Stopped
    );

    let updated = fixture.updated_state();
    let scope = &updated["__tool_call_scope"];
    let suspended_count = scope
        .as_object()
        .map(|obj| {
            obj.values()
                .filter(|value| value.get("suspended_call").is_some())
                .count()
        })
        .unwrap_or(0);
    assert_eq!(
        suspended_count, 1,
        "only one recovery suspension should be created"
    );
}

#[tokio::test]
async fn recovery_plugin_only_marks_orphans_when_some_have_live_handles() {
    let task_store = recovery_task_store();
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-1",
        "worker-a",
        TaskStatus::Running,
        None,
    )
    .await;
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-2",
        "worker-b",
        TaskStatus::Running,
        None,
    )
    .await;

    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let token = RunCancellationToken::new();
    bg_mgr
        .spawn_with_id(
            SpawnParams {
                task_id: "run-1".to_string(),
                owner_thread_id: "owner-1".to_string(),
                task_type: "agent_run".to_string(),
                description: "agent:worker-a".to_string(),
                parent_task_id: None,
                metadata: json!({"agent_id": "worker-a", "thread_id": "sub-agent-run-1"}),
            },
            token.clone(),
            |cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await;

    let plugin = AgentRecoveryPlugin::new(bg_mgr).with_task_store(Some(task_store.clone()));
    let thread = Thread::with_initial_state("owner-1", json!({}));
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = owner_step(&fixture);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    assert_eq!(
        persisted_status(&task_store, "run-1").await,
        TaskStatus::Running
    );
    assert_eq!(
        persisted_status(&task_store, "run-2").await,
        TaskStatus::Stopped
    );
}

#[tokio::test]
async fn recovery_plugin_no_action_when_all_running_have_live_handles() {
    let task_store = recovery_task_store();
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-1",
        "worker-a",
        TaskStatus::Running,
        None,
    )
    .await;
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-2",
        "worker-b",
        TaskStatus::Running,
        None,
    )
    .await;

    let bg_mgr = Arc::new(BackgroundTaskManager::new());
    let token1 = RunCancellationToken::new();
    bg_mgr
        .spawn_with_id(
            SpawnParams {
                task_id: "run-1".to_string(),
                owner_thread_id: "owner-1".to_string(),
                task_type: "agent_run".to_string(),
                description: "agent:worker-a".to_string(),
                parent_task_id: None,
                metadata: json!({"agent_id": "worker-a", "thread_id": "sub-agent-run-1"}),
            },
            token1.clone(),
            |cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await;
    let token2 = RunCancellationToken::new();
    bg_mgr
        .spawn_with_id(
            SpawnParams {
                task_id: "run-2".to_string(),
                owner_thread_id: "owner-1".to_string(),
                task_type: "agent_run".to_string(),
                description: "agent:worker-b".to_string(),
                parent_task_id: None,
                metadata: json!({"agent_id": "worker-b", "thread_id": "sub-agent-run-2"}),
            },
            token2.clone(),
            |cancel| async move {
                cancel.cancelled().await;
                TaskResult::Cancelled
            },
        )
        .await;

    let plugin = AgentRecoveryPlugin::new(bg_mgr).with_task_store(Some(task_store.clone()));
    let thread = Thread::with_initial_state("owner-1", json!({}));
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = owner_step(&fixture);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    assert_eq!(
        persisted_status(&task_store, "run-1").await,
        TaskStatus::Running
    );
    assert_eq!(
        persisted_status(&task_store, "run-2").await,
        TaskStatus::Running
    );

    let updated = fixture.updated_state();
    let has_suspended = updated
        .get("__tool_call_scope")
        .and_then(|scope| scope.as_object())
        .map(|obj| {
            obj.values()
                .any(|value| value.get("suspended_call").is_some())
        })
        .unwrap_or(false);
    assert!(
        !has_suspended,
        "no recovery suspension should be created when all have live handles"
    );
}

#[tokio::test]
async fn recovery_plugin_ignores_completed_stopped_failed_in_persisted_state() {
    let task_store = recovery_task_store();
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-completed",
        "worker",
        TaskStatus::Completed,
        None,
    )
    .await;
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-failed",
        "worker",
        TaskStatus::Failed,
        Some("oops"),
    )
    .await;
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-stopped",
        "worker",
        TaskStatus::Stopped,
        None,
    )
    .await;

    let plugin = AgentRecoveryPlugin::new(Arc::new(BackgroundTaskManager::new()))
        .with_task_store(Some(task_store.clone()));
    let thread = Thread::with_initial_state("owner-1", json!({}));
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = owner_step(&fixture);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    assert_eq!(
        persisted_status(&task_store, "run-completed").await,
        TaskStatus::Completed
    );
    assert_eq!(
        persisted_status(&task_store, "run-failed").await,
        TaskStatus::Failed
    );
    assert_eq!(
        persisted_status(&task_store, "run-stopped").await,
        TaskStatus::Stopped
    );

    let updated = fixture.updated_state();
    let has_suspended = updated
        .get("__tool_call_scope")
        .and_then(|scope| scope.as_object())
        .map(|obj| {
            obj.values()
                .any(|value| value.get("suspended_call").is_some())
        })
        .unwrap_or(false);
    assert!(!has_suspended);
}

#[cfg(not(feature = "permission"))]
#[tokio::test]
async fn recovery_plugin_fallback_always_approves_despite_deny_state() {
    let task_store = recovery_task_store();
    persist_agent_run(
        &task_store,
        "owner-1",
        "run-1",
        "worker",
        TaskStatus::Running,
        None,
    )
    .await;
    let plugin = AgentRecoveryPlugin::new(Arc::new(BackgroundTaskManager::new()))
        .with_task_store(Some(task_store));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "deny",
                "tools": {
                    "recover_agent_run": "deny"
                }
            }
        }),
    );
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = owner_step(&fixture);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let updated = fixture.updated_state();
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["tool_call_state"]["status"],
        json!("resuming"),
        "fallback should auto-approve when permission feature is off"
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["tool_call_state"]["resume"]["action"],
        json!("resume"),
    );
}
