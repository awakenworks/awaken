mod common;

use axum::body::to_bytes;
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use axum::http::Request;
use axum::http::StatusCode;
use common::{compose_http_app, get_json_text, post_sse, TerminatePlugin};
use serde_json::{json, Value};
use std::sync::{Arc, Once};
use tirea_agentos::contracts::storage::ThreadReader;
use tirea_agentos::contracts::TerminationReason;
use tirea_agentos::orchestrator::{AgentDefinition, AgentOs, AgentOsBuilder};
use tirea_agentos::runtime::loop_runner::RunCancellationToken;
use tirea_agentos_server::run_service::{global_run_service, init_run_service, RunService};
use tirea_agentos_server::service::{
    active_run_key, register_active_run_cancellation, register_active_run_with_id,
    remove_active_run, AppState,
};
use tirea_contract::storage::RunOrigin;
use tirea_contract::{AgentEvent, RuntimeInput, ToolCallDecision};
use tirea_store_adapters::{MemoryRunStore, MemoryStore};
use tower::ServiceExt;
use uuid::Uuid;

fn ensure_run_service() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = init_run_service(Arc::new(MemoryRunStore::new()));
    });
    assert!(
        global_run_service().is_some(),
        "run service should initialize"
    );
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

async fn fetch_well_known_etag(app: axum::Router) -> String {
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/.well-known/agent-card.json")
                .body(axum::body::Body::empty())
                .expect("request build should succeed"),
        )
        .await
        .expect("app should handle request");
    assert_eq!(response.status(), StatusCode::OK);
    response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
        .expect("well-known should include etag")
}

fn make_os(store: Arc<MemoryStore>, agent_ids: &[&str]) -> Arc<AgentOs> {
    let mut builder = AgentOsBuilder::new()
        .with_registered_behavior(
            "terminate_behavior_requested_test",
            Arc::new(TerminatePlugin::new("terminate_behavior_requested_test")),
        )
        .with_agent_state_store(store);
    for agent_id in agent_ids {
        let definition = AgentDefinition {
            id: (*agent_id).to_string(),
            behavior_ids: vec!["terminate_behavior_requested_test".into()],
            ..Default::default()
        };
        builder = builder.with_agent(*agent_id, definition);
    }
    Arc::new(builder.build().expect("build AgentOs"))
}

fn make_app_with_agents(agent_ids: &[&str]) -> axum::Router {
    let store = Arc::new(MemoryStore::new());
    let read_store: Arc<dyn ThreadReader> = store.clone();
    let os = make_os(store, agent_ids);
    compose_http_app(AppState { os, read_store })
}

fn make_app() -> axum::Router {
    make_app_with_agents(&["alpha", "beta"])
}

#[tokio::test]
async fn a2a_discovery_endpoints_work() {
    ensure_run_service();
    let app = make_app();

    let (status, body) = get_json_text(app.clone(), "/.well-known/agent-card.json").await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).expect("valid well-known json");
    assert_eq!(payload["url"].as_str(), Some("/v1/a2a/agents"));
    assert_eq!(payload["capabilities"]["agentCount"].as_u64(), Some(2));

    let (status, body) = get_json_text(app.clone(), "/v1/a2a/agents").await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).expect("valid agent list json");
    let items = payload.as_array().expect("agent list should be array");
    assert!(
        items
            .iter()
            .any(|item| item["agentId"].as_str() == Some("alpha")),
        "missing alpha agent: {payload}"
    );
    assert!(
        items
            .iter()
            .any(|item| item["agentId"].as_str() == Some("beta")),
        "missing beta agent: {payload}"
    );

    let (status, body) = get_json_text(app, "/v1/a2a/agents/alpha/agent-card").await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).expect("valid agent card json");
    assert_eq!(
        payload["url"].as_str(),
        Some("/v1/a2a/agents/alpha/message:send")
    );
}

#[tokio::test]
async fn a2a_well_known_single_agent_points_to_agent_send_url() {
    ensure_run_service();
    let app = make_app_with_agents(&["solo"]);

    let (status, body) = get_json_text(app.clone(), "/.well-known/agent-card.json").await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).expect("valid well-known json");
    assert_eq!(payload["name"].as_str(), Some("tirea-agent-solo"));
    assert_eq!(
        payload["url"].as_str(),
        Some("/v1/a2a/agents/solo/message:send")
    );
    assert_eq!(payload["capabilities"]["agentCount"].as_u64(), Some(1));
    assert_eq!(payload["capabilities"]["agents"][0].as_str(), Some("solo"));

    let (status, body) = get_json_text(app, "/v1/a2a/agents").await;
    assert_eq!(status, StatusCode::OK);
    let items: Value = serde_json::from_str(&body).expect("valid agent list");
    assert_eq!(items.as_array().map(Vec::len), Some(1));
    assert_eq!(items[0]["agentId"].as_str(), Some("solo"));
}

#[tokio::test]
async fn a2a_well_known_emits_cache_headers_and_supports_if_none_match() {
    ensure_run_service();
    let app = make_app();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/.well-known/agent-card.json")
                .body(axum::body::Body::empty())
                .expect("request build should succeed"),
        )
        .await
        .expect("app should handle request");
    assert_eq!(response.status(), StatusCode::OK);

    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
        .expect("well-known should include etag");
    assert_eq!(
        response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("public, max-age=30, must-revalidate")
    );

    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body should be readable");
    let body_text = String::from_utf8(body.to_vec()).expect("body should be utf-8");
    let payload: Value = serde_json::from_str(&body_text).expect("well-known should be json");
    assert_eq!(payload["capabilities"]["agentCount"].as_u64(), Some(2));

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/.well-known/agent-card.json")
                .header(IF_NONE_MATCH, &etag)
                .body(axum::body::Body::empty())
                .expect("request build should succeed"),
        )
        .await
        .expect("app should handle request");
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("public, max-age=30, must-revalidate")
    );
    assert_eq!(
        response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok()),
        Some(etag.as_str())
    );
}

#[tokio::test]
async fn a2a_well_known_supports_if_none_match_star_and_csv() {
    ensure_run_service();
    let app = make_app();
    let etag = fetch_well_known_etag(app.clone()).await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/.well-known/agent-card.json")
                .header(IF_NONE_MATCH, "*")
                .body(axum::body::Body::empty())
                .expect("request build should succeed"),
        )
        .await
        .expect("app should handle request");
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/.well-known/agent-card.json")
                .header(IF_NONE_MATCH, format!("\"something-else\", {etag}"))
                .body(axum::body::Body::empty())
                .expect("request build should succeed"),
        )
        .await
        .expect("app should handle request");
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn a2a_well_known_etag_changes_when_registry_changes() {
    ensure_run_service();
    let one_agent_etag = fetch_well_known_etag(make_app_with_agents(&["solo"])).await;
    let two_agent_etag = fetch_well_known_etag(make_app_with_agents(&["alpha", "beta"])).await;
    assert_ne!(
        one_agent_etag, two_agent_etag,
        "etag should change when registry agent set changes"
    );
}

#[tokio::test]
async fn a2a_message_send_starts_task_and_get_task() {
    ensure_run_service();
    let app = make_app();

    let (status, body) = post_sse(
        app.clone(),
        "/v1/a2a/agents/alpha/message:send",
        json!({
            "input": "hello from a2a"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "unexpected response: {body}");
    let payload: Value = serde_json::from_str(&body).expect("valid send response");
    let task_id = payload["taskId"]
        .as_str()
        .expect("taskId should exist")
        .to_string();
    let context_id = payload["contextId"]
        .as_str()
        .expect("contextId should exist")
        .to_string();

    let uri = format!("/v1/a2a/agents/alpha/tasks/{task_id}");
    let (status, body) = get_json_text(app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).expect("valid task response");
    assert_eq!(payload["taskId"].as_str(), Some(task_id.as_str()));
    assert_eq!(payload["contextId"].as_str(), Some(context_id.as_str()));
}

#[tokio::test]
async fn a2a_cancel_and_decision_only_use_active_run_registry() {
    ensure_run_service();
    let app = make_app();
    let run_id = format!("a2a-run-{}", Uuid::new_v4().simple());
    let key = active_run_key("a2a", "alpha", "a2a-thread", &run_id);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeInput>();
    register_active_run_with_id(key.clone(), run_id.clone(), tx).await;
    let token = RunCancellationToken::new();
    register_active_run_cancellation(run_id.clone(), token.clone()).await;

    let decision = ToolCallDecision::resume("tool-1", json!({"approved": true}), 1);
    let (status, body) = post_sse(
        app.clone(),
        "/v1/a2a/agents/alpha/message:send",
        json!({
            "taskId": run_id,
            "decisions": [decision]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "unexpected response: {body}");
    let payload: Value = serde_json::from_str(&body).expect("valid response");
    assert_eq!(payload["status"].as_str(), Some("decision_forwarded"));

    let forwarded = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("decision should arrive")
        .expect("channel should produce runtime input");
    assert!(matches!(forwarded, RuntimeInput::Decision(_)));

    let cancel_uri = format!("/v1/a2a/agents/alpha/tasks/{run_id}:cancel");
    let (status, body) = post_sse(app, &cancel_uri, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let payload: Value = serde_json::from_str(&body).expect("valid cancel response");
    assert_eq!(payload["status"].as_str(), Some("cancel_requested"));
    assert!(token.is_cancelled(), "token should be cancelled");

    let forwarded = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("cancel should arrive")
        .expect("channel should produce runtime input");
    assert!(matches!(forwarded, RuntimeInput::Cancel));

    remove_active_run(&key).await;
}

#[tokio::test]
async fn a2a_decision_only_requires_task_id() {
    ensure_run_service();
    let app = make_app();
    let decision = ToolCallDecision::resume("tool-1", json!({"approved": true}), 1);
    let (status, _body) = post_sse(
        app,
        "/v1/a2a/agents/alpha/message:send",
        json!({
            "decisions": [decision]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a2a_decision_only_returns_bad_request_for_inactive_task() {
    ensure_run_service();
    let app = make_app();
    let service = global_run_service().expect("run service should initialize");
    let run_id = format!("inactive-a2a-{}", Uuid::new_v4().simple());
    service
        .begin_intent(&run_id, "inactive-thread", RunOrigin::A2a, None, None)
        .await
        .expect("seed inactive run");

    let decision = ToolCallDecision::resume("tool-1", json!({"approved": true}), 1);
    let (status, _body) = post_sse(
        app,
        "/v1/a2a/agents/alpha/message:send",
        json!({
            "taskId": run_id,
            "decisions": [decision]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a2a_decision_only_returns_not_found_for_missing_task() {
    ensure_run_service();
    let app = make_app();
    let decision = ToolCallDecision::resume("tool-1", json!({"approved": true}), 1);
    let missing = format!("missing-a2a-{}", Uuid::new_v4().simple());
    let (status, _body) = post_sse(
        app,
        "/v1/a2a/agents/alpha/message:send",
        json!({
            "taskId": missing,
            "decisions": [decision]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a2a_message_send_with_task_id_reuses_parent_context() {
    ensure_run_service();
    let app = make_app();
    let service = global_run_service().expect("run service should initialize");
    let parent_run_id = format!("parent-a2a-{}", Uuid::new_v4().simple());
    let parent_thread_id = format!("parent-thread-{}", Uuid::new_v4().simple());
    seed_completed_run(&service, &parent_run_id, &parent_thread_id, RunOrigin::A2a).await;

    let (status, body) = post_sse(
        app,
        "/v1/a2a/agents/alpha/message:send",
        json!({
            "taskId": parent_run_id,
            "input": "continue"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "unexpected response: {body}");
    let payload: Value = serde_json::from_str(&body).expect("valid response");
    assert_eq!(
        payload["contextId"].as_str(),
        Some(parent_thread_id.as_str())
    );
}

#[tokio::test]
async fn a2a_message_send_with_missing_task_id_returns_not_found() {
    ensure_run_service();
    let app = make_app();
    let missing = format!("missing-task-{}", Uuid::new_v4().simple());
    let (status, _body) = post_sse(
        app,
        "/v1/a2a/agents/alpha/message:send",
        json!({
            "taskId": missing,
            "input": "continue"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a2a_message_send_with_blank_context_id_creates_new_context() {
    ensure_run_service();
    let app = make_app();
    let (status, body) = post_sse(
        app,
        "/v1/a2a/agents/alpha/message:send",
        json!({
            "contextId": "   ",
            "input": "new conversation"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "unexpected response: {body}");
    let payload: Value = serde_json::from_str(&body).expect("valid response");
    assert!(
        payload["contextId"]
            .as_str()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false),
        "contextId should be generated"
    );
}

#[tokio::test]
async fn a2a_rejects_unsupported_message_action() {
    ensure_run_service();
    let app = make_app();
    let (status, _body) = post_sse(
        app,
        "/v1/a2a/agents/alpha/message:foo",
        json!({
            "input": "hello"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a2a_cancel_path_requires_post_and_cancel_suffix() {
    ensure_run_service();
    let app = make_app();
    let run_id = format!("cancel-path-{}", Uuid::new_v4().simple());
    let cancel_path = format!("/v1/a2a/agents/alpha/tasks/{run_id}:cancel");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&cancel_path)
                .body(axum::body::Body::empty())
                .expect("request build should succeed"),
        )
        .await
        .expect("app should handle request");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let plain_task_path = format!("/v1/a2a/agents/alpha/tasks/{run_id}");
    let (status, _body) = post_sse(app, &plain_task_path, json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
