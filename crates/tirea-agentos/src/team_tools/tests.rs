use std::sync::Arc;

use super::*;
use crate::contracts::runtime::tool_call::{Tool, ToolCallContext};
use crate::contracts::storage::{Task, TaskStatus};
use serde_json::json;
use tirea_contract::runtime::activity::NoOpActivityManager;
use tirea_contract::RunPolicy;
use tirea_state::DocCell;

fn make_stores() -> TeamStores {
    let store = Arc::new(tirea_store_adapters::MemoryStore::new());
    TeamStores {
        mailbox: store.clone(),
        tasks: store,
    }
}

fn make_ctx<'a>(
    doc: &'a DocCell,
    ops: &'a std::sync::Mutex<Vec<tirea_state::Op>>,
    messages: &'a std::sync::Mutex<Vec<Arc<tirea_contract::thread::Message>>>,
    run_policy: &'a RunPolicy,
) -> ToolCallContext<'a> {
    let activity = Arc::new(NoOpActivityManager);
    ToolCallContext::new(doc, ops, "call-1", "test", run_policy, messages, activity)
}

// ────────────────────────────────────────────────────────────────────────
// SendMessageTool tests
// ────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn send_message_enqueues_to_mailbox() {
    let stores = make_stores();
    let tool = SendMessageTool::new(stores.mailbox.clone());

    let doc = DocCell::new(json!({}));
    let ops = std::sync::Mutex::new(Vec::new());
    let msgs = std::sync::Mutex::new(Vec::new());
    let run_policy = RunPolicy::default();
    let ctx = make_ctx(&doc, &ops, &msgs, &run_policy);

    let result = tool
        .execute(
            json!({
                "recipient_thread_id": "thread-bob",
                "recipient_agent_id": "agent-bob",
                "message": "Hello Bob!"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(result.is_success());
    let data = result.data;
    assert_eq!(data["status"], "queued");
    assert!(!data["entry_id"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn send_message_rejects_empty_fields() {
    let stores = make_stores();
    let tool = SendMessageTool::new(stores.mailbox.clone());

    let doc = DocCell::new(json!({}));
    let ops = std::sync::Mutex::new(Vec::new());
    let msgs = std::sync::Mutex::new(Vec::new());
    let run_policy = RunPolicy::default();
    let ctx = make_ctx(&doc, &ops, &msgs, &run_policy);

    let result = tool
        .execute(
            json!({
                "recipient_thread_id": "",
                "recipient_agent_id": "bob",
                "message": "hi"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!result.is_success());
}

// ────────────────────────────────────────────────────────────────────────
// TaskListTool tests
// ────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn task_list_returns_filtered_tasks() {
    let stores = make_stores();
    let task = Task {
        task_id: "t1".to_string(),
        subject: "Fix bug".to_string(),
        description: None,
        owner: None,
        status: TaskStatus::Pending,
        blocks: vec![],
        blocked_by: vec![],
        team_id: Some("team-a".to_string()),
        metadata: None,
        created_at: 100,
        updated_at: 100,
        created_by: Some("lead".to_string()),
    };
    stores.tasks.create_task(&task).await.unwrap();

    let tool = TaskListTool::new(stores.tasks.clone());
    let doc = DocCell::new(json!({}));
    let ops = std::sync::Mutex::new(Vec::new());
    let msgs = std::sync::Mutex::new(Vec::new());
    let run_policy = RunPolicy::default();
    let ctx = make_ctx(&doc, &ops, &msgs, &run_policy);

    let result = tool
        .execute(json!({ "team_id": "team-a", "limit": 10 }), &ctx)
        .await
        .unwrap();

    assert!(result.is_success());
    let data = result.data;
    assert_eq!(data["total"], 1);
    assert_eq!(data["items"][0]["task_id"], "t1");
}

// ────────────────────────────────────────────────────────────────────────
// TaskUpdateTool tests
// ────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn task_update_create_and_complete() {
    let stores = make_stores();
    let tool = TaskUpdateTool::new(stores.tasks.clone());

    let doc = DocCell::new(json!({}));
    let ops = std::sync::Mutex::new(Vec::new());
    let msgs = std::sync::Mutex::new(Vec::new());
    let run_policy = RunPolicy::default();
    let ctx = make_ctx(&doc, &ops, &msgs, &run_policy);

    // Create
    let result = tool
        .execute(
            json!({
                "action": "create",
                "task_id": "t1",
                "subject": "Implement feature X",
                "team_id": "team-a"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.is_success());
    let data = result.data;
    assert_eq!(data["action"], "created");

    // Complete
    let result = tool
        .execute(
            json!({
                "action": "complete",
                "task_id": "t1"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.is_success());
    let data = result.data;
    assert_eq!(data["action"], "completed");
    assert_eq!(data["task"]["status"], "completed");
}

#[tokio::test]
async fn task_update_claim_conflict() {
    let stores = make_stores();
    let tool = TaskUpdateTool::new(stores.tasks.clone());

    let task = Task {
        task_id: "t1".to_string(),
        subject: "Task".to_string(),
        description: None,
        owner: Some("agent-a".to_string()),
        status: TaskStatus::InProgress,
        blocks: vec![],
        blocked_by: vec![],
        team_id: None,
        metadata: None,
        created_at: 100,
        updated_at: 100,
        created_by: None,
    };
    stores.tasks.create_task(&task).await.unwrap();

    let doc = DocCell::new(json!({}));
    let ops = std::sync::Mutex::new(Vec::new());
    let msgs = std::sync::Mutex::new(Vec::new());
    let run_policy = RunPolicy::default();
    let ctx = make_ctx(&doc, &ops, &msgs, &run_policy);

    let result = tool
        .execute(
            json!({
                "action": "claim",
                "task_id": "t1",
                "agent_id": "agent-b"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!result.is_success());
}

#[tokio::test]
async fn task_update_assign_overrides() {
    let stores = make_stores();
    let tool = TaskUpdateTool::new(stores.tasks.clone());

    let task = Task {
        task_id: "t1".to_string(),
        subject: "Task".to_string(),
        description: None,
        owner: Some("agent-a".to_string()),
        status: TaskStatus::InProgress,
        blocks: vec![],
        blocked_by: vec![],
        team_id: None,
        metadata: None,
        created_at: 100,
        updated_at: 100,
        created_by: None,
    };
    stores.tasks.create_task(&task).await.unwrap();

    let doc = DocCell::new(json!({}));
    let ops = std::sync::Mutex::new(Vec::new());
    let msgs = std::sync::Mutex::new(Vec::new());
    let run_policy = RunPolicy::default();
    let ctx = make_ctx(&doc, &ops, &msgs, &run_policy);

    let result = tool
        .execute(
            json!({
                "action": "assign",
                "task_id": "t1",
                "agent_id": "agent-b"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(result.is_success());
    let data = result.data;
    assert_eq!(data["task"]["owner"], "agent-b");
}

#[tokio::test]
async fn task_update_unknown_action() {
    let stores = make_stores();
    let tool = TaskUpdateTool::new(stores.tasks.clone());

    let doc = DocCell::new(json!({}));
    let ops = std::sync::Mutex::new(Vec::new());
    let msgs = std::sync::Mutex::new(Vec::new());
    let run_policy = RunPolicy::default();
    let ctx = make_ctx(&doc, &ops, &msgs, &run_policy);

    let result = tool
        .execute(json!({ "action": "bogus" }), &ctx)
        .await
        .unwrap();
    assert!(!result.is_success());
}
