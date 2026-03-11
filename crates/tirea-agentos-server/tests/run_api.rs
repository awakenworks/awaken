mod common;

use axum::http::StatusCode;
use common::{get_json_text, post_sse, SlowTerminatePlugin, TerminatePlugin};
use serde_json::json;
use serde_json::Value;
use std::sync::{Arc, OnceLock};
use tirea_agentos::composition::{AgentDefinition, AgentOsBuilder};
use tirea_agentos::contracts::storage::{MailboxWriter, ThreadReader};
use tirea_agentos::contracts::RunRequest;
use tirea_agentos::runtime::{AgentOs, RunStream};
use tirea_agentos_server::service::AppState;

const TEST_AGENT_ID: &str = "test";
use tirea_contract::storage::{
    MailboxEntry, MailboxEntryStatus, RunOrigin, RunQuery, RunReader, RunRecord, RunStatus,
    RunWriter,
};
use tirea_store_adapters::MemoryStore;
use uuid::Uuid;

fn shared_store() -> Arc<MemoryStore> {
    static TEST_STORE: OnceLock<Arc<MemoryStore>> = OnceLock::new();
    TEST_STORE
        .get_or_init(|| Arc::new(MemoryStore::new()))
        .clone()
}

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

fn make_os(store: Arc<MemoryStore>) -> Arc<AgentOs> {
    let def = AgentDefinition {
        id: "test".to_string(),
        behavior_ids: vec!["terminate_behavior_requested_test".into()],
        ..Default::default()
    };
    Arc::new(
        AgentOsBuilder::new()
            .with_registered_behavior(
                "terminate_behavior_requested_test",
                Arc::new(TerminatePlugin::new("terminate_behavior_requested_test")),
            )
            .with_agent("test", def)
            .with_agent_state_store(store)
            .build()
            .expect("build AgentOs"),
    )
}

fn make_app_with_os() -> (axum::Router, Arc<AgentOs>) {
    let thread_store = shared_store();
    let read_store: Arc<dyn ThreadReader> = thread_store.clone();
    let os = make_os(thread_store);
    let state = AppState::new(os.clone(), read_store).with_mailbox_store(shared_store());
    // Explicitly opt in to run projection routes (not part of the default public API).
    let app = axum::Router::new()
        .merge(tirea_agentos_server::http::run_routes())
        .merge(tirea_agentos_server::http::thread_routes())
        .merge(tirea_agentos_server::http::health_routes())
        .with_state(state);
    (app, os)
}

fn make_app() -> axum::Router {
    make_app_with_os().0
}

fn make_slow_interrupt_app() -> (axum::Router, Arc<AgentOs>, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let behavior_id = format!("slow-terminate-{}", Uuid::new_v4().simple());
    let os = Arc::new(
        AgentOsBuilder::new()
            .with_registered_behavior(
                &behavior_id,
                Arc::new(SlowTerminatePlugin::new(
                    behavior_id.clone(),
                    std::time::Duration::from_secs(30),
                )),
            )
            .with_agent(
                TEST_AGENT_ID,
                AgentDefinition {
                    id: TEST_AGENT_ID.to_string(),
                    behavior_ids: vec![behavior_id],
                    ..Default::default()
                },
            )
            .with_agent_state_store(store.clone())
            .build()
            .expect("build AgentOs"),
    );
    let read_store: Arc<dyn ThreadReader> = store.clone();
    let app = axum::Router::new()
        .merge(tirea_agentos_server::http::run_routes())
        .merge(tirea_agentos_server::http::thread_routes())
        .merge(tirea_agentos_server::http::health_routes())
        .with_state(AppState::new(os.clone(), read_store).with_mailbox_store(store.clone()));
    (app, os, store)
}

fn mailbox_entry(run_id: &str, thread_id: &str) -> MailboxEntry {
    MailboxEntry {
        entry_id: format!("entry-{run_id}"),
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        agent_id: TEST_AGENT_ID.to_string(),
        generation: 0,
        status: MailboxEntryStatus::Queued,
        request: RunRequest {
            agent_id: TEST_AGENT_ID.to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: Some(run_id.to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: RunOrigin::User,
            state: None,
            messages: vec![],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        },
        dedupe_key: None,
        kind: None,
        sender_id: None,
        priority: 0,
        available_at: now_unix_millis(),
        attempt_count: 0,
        last_error: None,
        claim_token: None,
        claimed_by: None,
        lease_until: None,
        accepted_run_id: None,
        created_at: now_unix_millis(),
        updated_at: now_unix_millis(),
    }
}

async fn start_active_run(
    os: &Arc<AgentOs>,
    agent_id: &str,
    thread_id: &str,
    run_id: &str,
) -> RunStream {
    let resolved = os.resolve(agent_id).expect("resolve agent");
    let request = RunRequest {
        agent_id: agent_id.to_string(),
        thread_id: Some(thread_id.to_string()),
        run_id: Some(run_id.to_string()),
        parent_run_id: None,
        parent_thread_id: None,
        resource_id: None,
        origin: RunOrigin::default(),
        state: None,
        messages: vec![],
        initial_decisions: vec![],
        source_mailbox_entry_id: None,
    };
    os.start_active_run_with_persistence(agent_id, request, resolved, true, false)
        .await
        .expect("start active run")
}

async fn seed_completed_run(store: &MemoryStore, run_id: &str, thread_id: &str, origin: RunOrigin) {
    let mut record = RunRecord::new(
        run_id.to_string(),
        thread_id.to_string(),
        "test",
        origin,
        RunStatus::Done,
        now_unix_millis(),
    );
    record.termination_code = Some("natural".to_string());
    record.updated_at = now_unix_millis();
    store.upsert_run(&record).await.expect("seed completed run");
}

async fn wait_for_child_run(
    store: &MemoryStore,
    thread_id: &str,
    parent_run_id: &str,
) -> Option<RunRecord> {
    for _ in 0..50 {
        let page = RunReader::list_runs(
            store,
            &RunQuery {
                thread_id: Some(thread_id.to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("list runs should succeed");
        if let Some(child) = page
            .items
            .iter()
            .find(|r| r.parent_run_id.as_deref() == Some(parent_run_id))
        {
            return Some(child.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    None
}

#[tokio::test]
async fn get_run_returns_record() {
    let store = shared_store();
    let app = make_app();
    let run_id = format!("run-{}", Uuid::new_v4().simple());
    let thread_id = format!("thread-{}", Uuid::new_v4().simple());

    seed_completed_run(store.as_ref(), &run_id, &thread_id, RunOrigin::AgUi).await;

    let uri = format!("/v1/runs/{run_id}");
    let (status, body) = get_json_text(app, &uri).await;
    assert_eq!(status, StatusCode::OK);

    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["run_id"].as_str(), Some(run_id.as_str()));
    assert_eq!(payload["thread_id"].as_str(), Some(thread_id.as_str()));
    assert_eq!(payload["status"].as_str(), Some("done"));
    assert_eq!(payload["origin"].as_str(), Some("ag_ui"));
}

#[tokio::test]
async fn list_runs_supports_filters() {
    let store = shared_store();
    let app = make_app();
    let thread_id = format!("thread-{}", Uuid::new_v4().simple());
    let completed_run = format!("run-completed-{}", Uuid::new_v4().simple());
    let working_run = format!("run-working-{}", Uuid::new_v4().simple());

    seed_completed_run(store.as_ref(), &completed_run, &thread_id, RunOrigin::AgUi).await;
    let mut working = RunRecord::new(
        working_run.clone(),
        thread_id.clone(),
        "test",
        RunOrigin::AiSdk,
        RunStatus::Running,
        now_unix_millis(),
    );
    working.updated_at = now_unix_millis();
    store.upsert_run(&working).await.expect("seed working run");

    let uri = format!("/v1/runs?thread_id={thread_id}&status=done&origin=ag_ui");
    let (status, body) = get_json_text(app, &uri).await;
    assert_eq!(status, StatusCode::OK);

    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["total"].as_u64(), Some(1));
    let items = payload["items"]
        .as_array()
        .expect("items should be an array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["run_id"].as_str(), Some(completed_run.as_str()));
    assert_eq!(items[0]["status"].as_str(), Some("done"));
    assert_eq!(items[0]["origin"].as_str(), Some("ag_ui"));
}

#[tokio::test]
async fn list_runs_supports_time_range_filters() {
    let store = shared_store();
    let app = make_app();
    let thread_id = format!("thread-time-{}", Uuid::new_v4().simple());
    let run_id = format!("run-time-{}", Uuid::new_v4().simple());
    seed_completed_run(store.as_ref(), &run_id, &thread_id, RunOrigin::AgUi).await;

    let record = RunReader::load_run(store.as_ref(), &run_id)
        .await
        .expect("query seeded run")
        .expect("seeded run exists");
    let uri = format!(
        "/v1/runs?created_at_from={}&created_at_to={}&updated_at_from={}&updated_at_to={}&thread_id={}",
        record.created_at,
        record.created_at,
        record.updated_at,
        record.updated_at,
        thread_id
    );
    let (status, body) = get_json_text(app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    let items = payload["items"]
        .as_array()
        .expect("items should be array for run listing");
    assert!(
        items
            .iter()
            .any(|item| item["run_id"].as_str() == Some(run_id.as_str())),
        "expected run to satisfy time filters, payload={payload}"
    );
}

#[tokio::test]
async fn get_run_returns_not_found_for_missing_id() {
    let app = make_app();
    let missing_id = format!("missing-{}", Uuid::new_v4().simple());

    let uri = format!("/v1/runs/{missing_id}");
    let (status, body) = get_json_text(app, &uri).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .contains("run not found"),
        "unexpected payload: {payload}"
    );
}

#[tokio::test]
async fn interrupt_thread_cancels_active_run_and_pending_mailbox_entries() {
    let (app, os, store) = make_slow_interrupt_app();
    let thread_id = format!("thread-interrupt-{}", Uuid::new_v4().simple());
    let active_run_id = format!("run-active-{}", Uuid::new_v4().simple());
    let queued_run_id = format!("run-queued-{}", Uuid::new_v4().simple());

    let _active_run = start_active_run(&os, TEST_AGENT_ID, &thread_id, &active_run_id).await;
    store
        .enqueue_mailbox_entry(&mailbox_entry(&queued_run_id, &thread_id))
        .await
        .expect("enqueue queued mailbox entry");

    let uri = format!("/v1/threads/{thread_id}/interrupt");
    let (status, body) = post_sse(app, &uri, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["status"].as_str(), Some("interrupt_requested"));
    assert_eq!(payload["thread_id"].as_str(), Some(thread_id.as_str()));
    assert_eq!(
        payload["cancelled_run_id"].as_str(),
        Some(active_run_id.as_str())
    );
    assert_eq!(payload["generation"].as_u64(), Some(1));
    assert_eq!(payload["superseded_pending_count"].as_u64(), Some(1));
    assert!(payload["superseded_pending_run_ids"]
        .as_array()
        .expect("cancelled pending ids should be present")
        .iter()
        .any(|value| value.as_str() == Some(queued_run_id.as_str())));

    let queued_entry =
        tirea_agentos::contracts::storage::MailboxReader::load_mailbox_entry_by_run_id(
            store.as_ref(),
            &queued_run_id,
        )
        .await
        .expect("load queued mailbox entry")
        .expect("queued mailbox entry should exist");
    assert_eq!(queued_entry.status, MailboxEntryStatus::Superseded);
}

#[tokio::test]
async fn start_run_endpoint_streams_events_and_persists_record() {
    let app = make_app();

    let (status, body) = post_sse(
        app,
        "/v1/runs",
        json!({
            "agentId": "test",
            "messages": [],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("\"type\":\"run_start\""),
        "missing run_start event: {body}"
    );
    assert!(
        body.contains("\"type\":\"run_finish\""),
        "missing run_finish event: {body}"
    );

    let run_id = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|event| {
            if event["type"].as_str() == Some("run_start") {
                event["run_id"].as_str().map(ToString::to_string)
            } else {
                None
            }
        })
        .expect("run_start event should include run_id");

    let query_app = make_app();
    let uri = format!("/v1/runs/{run_id}");
    let (status, payload) = get_json_text(query_app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&payload).expect("valid json");
    assert_eq!(value["run_id"].as_str(), Some(run_id.as_str()));
}

#[tokio::test]
async fn inputs_endpoint_forwards_decisions_by_run_id() {
    let (app, os) = make_app_with_os();
    let run_id = format!("run-inputs-{}", Uuid::new_v4().simple());
    let thread_id = format!("thread-inputs-{}", Uuid::new_v4().simple());
    let _active_run = start_active_run(&os, TEST_AGENT_ID, &thread_id, &run_id).await;

    let uri = format!("/v1/runs/{run_id}/inputs");
    let (status, body) = post_sse(
        app,
        &uri,
        json!({
            "decisions": [
                {
                    "target_id": "tool-1",
                    "decision_id": "decision-1",
                    "action": "resume",
                    "result": {"approved": true},
                    "updated_at": 1
                }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["status"].as_str(), Some("decision_forwarded"));
    assert_eq!(payload["run_id"].as_str(), Some(run_id.as_str()));
}

#[tokio::test]
async fn inputs_endpoint_supports_message_and_decision_continuation() {
    let store = shared_store();
    let app = make_app();
    let parent_run_id = format!("run-parent-{}", Uuid::new_v4().simple());
    let parent_thread_id = format!("thread-parent-{}", Uuid::new_v4().simple());
    seed_completed_run(
        store.as_ref(),
        &parent_run_id,
        &parent_thread_id,
        RunOrigin::AgUi,
    )
    .await;

    let uri = format!("/v1/runs/{parent_run_id}/inputs");
    let (status, body) = post_sse(
        app,
        &uri,
        json!({
            "agentId": "test",
            "messages": [
                {"role": "user", "content": "continue this task"}
            ],
            "decisions": [
                {
                    "target_id": "tool-any",
                    "decision_id": "decision-any",
                    "action": "resume",
                    "result": {"approved": true},
                    "updated_at": 1
                }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "unexpected response: {body}");

    let payload: Value = serde_json::from_str(&body).expect("valid continuation payload");
    assert_eq!(payload["status"].as_str(), Some("continuation_started"));
    assert_eq!(
        payload["parent_run_id"].as_str(),
        Some(parent_run_id.as_str())
    );
    assert_eq!(
        payload["thread_id"].as_str(),
        Some(parent_thread_id.as_str())
    );
    // run_id must not be exposed in the public response.
    assert!(
        payload.get("run_id").is_none(),
        "continuation response must not expose internal run_id"
    );

    // Verify the child run was actually created via the run service.
    let child = wait_for_child_run(store.as_ref(), &parent_thread_id, &parent_run_id)
        .await
        .expect("child run should be persisted for the thread");
    assert_eq!(child.thread_id, parent_thread_id);
    assert_eq!(child.parent_run_id.as_deref(), Some(parent_run_id.as_str()));
}

#[tokio::test]
async fn cancel_endpoint_cancels_active_run() {
    let (app, os) = make_app_with_os();
    let run_id = format!("run-cancel-{}", Uuid::new_v4().simple());
    let thread_id = format!("thread-cancel-{}", Uuid::new_v4().simple());
    let _active_run = start_active_run(&os, TEST_AGENT_ID, &thread_id, &run_id).await;

    let uri = format!("/v1/runs/{run_id}/cancel");
    let (status, body) = post_sse(app, &uri, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["status"].as_str(), Some("cancel_requested"));
    assert_eq!(payload["run_id"].as_str(), Some(run_id.as_str()));
}
