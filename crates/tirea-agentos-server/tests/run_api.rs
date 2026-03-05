mod common;

use axum::http::StatusCode;
use common::{compose_http_app, get_json_text, post_sse, TerminatePlugin};
use serde_json::json;
use serde_json::Value;
use std::sync::{Arc, Once};
use tirea_agentos::contracts::storage::ThreadReader;
use tirea_agentos::contracts::TerminationReason;
use tirea_agentos::orchestrator::{AgentDefinition, AgentOs, AgentOsBuilder};
use tirea_agentos::runtime::loop_runner::RunCancellationToken;
use tirea_agentos_server::run_service::{global_run_service, init_run_service, RunService};
use tirea_agentos_server::service::{
    active_run_key, register_active_run, register_active_run_cancellation, remove_active_run,
    AppState,
};
use tirea_contract::storage::{RunOrigin, RunRecord};
use tirea_contract::{AgentEvent, RuntimeInput};
use tirea_store_adapters::{MemoryRunStore, MemoryStore};
use uuid::Uuid;

fn ensure_run_service() -> Arc<RunService> {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = init_run_service(Arc::new(MemoryRunStore::new()));
    });
    global_run_service().expect("run service should be initialized")
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

fn make_app() -> axum::Router {
    let thread_store = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = thread_store.clone();
    let os = make_os(thread_store);
    compose_http_app(AppState { os, read_store })
}

async fn seed_completed_run(
    service: &RunService,
    run_id: &str,
    thread_id: &str,
    origin: RunOrigin,
) {
    service
        .begin_intent(run_id, thread_id, origin, None, None)
        .await
        .expect("begin intent");
    service
        .apply_event(
            run_id,
            thread_id,
            origin,
            &AgentEvent::RunStart {
                thread_id: thread_id.to_string(),
                run_id: run_id.to_string(),
                parent_run_id: None,
            },
        )
        .await
        .expect("apply run start");
    service
        .apply_event(
            run_id,
            thread_id,
            origin,
            &AgentEvent::RunFinish {
                thread_id: thread_id.to_string(),
                run_id: run_id.to_string(),
                result: None,
                termination: TerminationReason::NaturalEnd,
            },
        )
        .await
        .expect("apply run finish");
}

async fn wait_for_run(service: &RunService, run_id: &str) -> Option<RunRecord> {
    for _ in 0..50 {
        if let Some(record) = service
            .get_run(run_id)
            .await
            .expect("run lookup should succeed")
        {
            return Some(record);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    None
}

#[tokio::test]
async fn get_run_returns_record() {
    let service = ensure_run_service();
    let app = make_app();
    let run_id = format!("run-{}", Uuid::new_v4().simple());
    let thread_id = format!("thread-{}", Uuid::new_v4().simple());

    seed_completed_run(&service, &run_id, &thread_id, RunOrigin::AgUi).await;

    let uri = format!("/v1/runs/{run_id}");
    let (status, body) = get_json_text(app, &uri).await;
    assert_eq!(status, StatusCode::OK);

    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["run_id"].as_str(), Some(run_id.as_str()));
    assert_eq!(payload["thread_id"].as_str(), Some(thread_id.as_str()));
    assert_eq!(payload["status"].as_str(), Some("completed"));
    assert_eq!(payload["origin"].as_str(), Some("ag_ui"));
}

#[tokio::test]
async fn list_runs_supports_filters() {
    let service = ensure_run_service();
    let app = make_app();
    let thread_id = format!("thread-{}", Uuid::new_v4().simple());
    let completed_run = format!("run-completed-{}", Uuid::new_v4().simple());
    let working_run = format!("run-working-{}", Uuid::new_v4().simple());

    seed_completed_run(&service, &completed_run, &thread_id, RunOrigin::AgUi).await;
    service
        .begin_intent(&working_run, &thread_id, RunOrigin::AiSdk, None, None)
        .await
        .expect("begin working run");
    service
        .apply_event(
            &working_run,
            &thread_id,
            RunOrigin::AiSdk,
            &AgentEvent::RunStart {
                thread_id: thread_id.clone(),
                run_id: working_run.clone(),
                parent_run_id: None,
            },
        )
        .await
        .expect("start working run");

    let uri = format!("/v1/runs?thread_id={thread_id}&status=completed&origin=ag_ui");
    let (status, body) = get_json_text(app, &uri).await;
    assert_eq!(status, StatusCode::OK);

    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["total"].as_u64(), Some(1));
    let items = payload["items"]
        .as_array()
        .expect("items should be an array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["run_id"].as_str(), Some(completed_run.as_str()));
    assert_eq!(items[0]["status"].as_str(), Some("completed"));
    assert_eq!(items[0]["origin"].as_str(), Some("ag_ui"));
}

#[tokio::test]
async fn list_runs_supports_time_range_filters() {
    let service = ensure_run_service();
    let app = make_app();
    let thread_id = format!("thread-time-{}", Uuid::new_v4().simple());
    let run_id = format!("run-time-{}", Uuid::new_v4().simple());
    seed_completed_run(&service, &run_id, &thread_id, RunOrigin::AgUi).await;

    let record = service
        .get_run(&run_id)
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
    let _service = ensure_run_service();
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
async fn start_run_endpoint_streams_events_and_persists_record() {
    let _service = ensure_run_service();
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
    let _service = ensure_run_service();
    let app = make_app();
    let run_id = format!("run-inputs-{}", Uuid::new_v4().simple());
    let key = active_run_key("run_api", "test", "thread-inputs", &run_id);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeInput>();
    register_active_run(key.clone(), tx).await;
    register_active_run_cancellation(run_id.clone(), RunCancellationToken::new()).await;

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

    let forwarded = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("decision should arrive before timeout")
        .expect("channel should yield runtime input");
    match forwarded {
        RuntimeInput::Decision(decision) => {
            assert_eq!(decision.target_id, "tool-1");
        }
        other => panic!("expected RuntimeInput::Decision, got {other:?}"),
    }

    remove_active_run(&key).await;
}

#[tokio::test]
async fn inputs_endpoint_supports_message_and_decision_continuation() {
    let service = ensure_run_service();
    let app = make_app();
    let parent_run_id = format!("run-parent-{}", Uuid::new_v4().simple());
    let parent_thread_id = format!("thread-parent-{}", Uuid::new_v4().simple());
    seed_completed_run(&service, &parent_run_id, &parent_thread_id, RunOrigin::AgUi).await;

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
    let child_run_id = payload["run_id"]
        .as_str()
        .expect("child run_id should exist")
        .to_string();

    let child = wait_for_run(&service, &child_run_id)
        .await
        .expect("child run should be persisted");
    assert_eq!(child.run_id, child_run_id);
    assert_eq!(child.thread_id, parent_thread_id);
    assert_eq!(child.parent_run_id.as_deref(), Some(parent_run_id.as_str()));
}

#[tokio::test]
async fn cancel_endpoint_cancels_active_run() {
    let _service = ensure_run_service();
    let app = make_app();
    let run_id = format!("run-cancel-{}", Uuid::new_v4().simple());
    let key = active_run_key("run_api", "test", "thread-cancel", &run_id);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeInput>();
    let token = RunCancellationToken::new();
    register_active_run(key.clone(), tx).await;
    register_active_run_cancellation(run_id.clone(), token.clone()).await;

    let uri = format!("/v1/runs/{run_id}/cancel");
    let (status, body) = post_sse(app, &uri, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let payload: Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(payload["status"].as_str(), Some("cancel_requested"));
    assert_eq!(payload["run_id"].as_str(), Some(run_id.as_str()));
    assert!(
        token.is_cancelled(),
        "cancellation token should be cancelled"
    );

    let forwarded = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("cancel should arrive before timeout")
        .expect("channel should yield runtime input");
    assert!(matches!(forwarded, RuntimeInput::Cancel));

    remove_active_run(&key).await;
}
