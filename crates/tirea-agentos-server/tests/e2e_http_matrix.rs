use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::sync::Arc;
use tirea_agentos::composition::{AgentDefinition, AgentOsBuilder};
use tirea_agentos::contracts::storage::ThreadReader;
use tirea_agentos_server::service::AppState;
use tirea_store_adapters::MemoryStore;
use tower::ServiceExt;

mod common;

use common::{compose_http_app, TerminatePlugin};

fn make_os(store: Arc<MemoryStore>) -> tirea_agentos::runtime::AgentOs {
    let def = AgentDefinition {
        id: "test".to_string(),
        behavior_ids: vec!["terminate_behavior_requested_e2e_http_matrix".into()],
        ..Default::default()
    };

    AgentOsBuilder::new()
        .with_registered_behavior(
            "terminate_behavior_requested_e2e_http_matrix",
            Arc::new(TerminatePlugin::new(
                "terminate_behavior_requested_e2e_http_matrix",
            )),
        )
        .with_agent("test", def)
        .with_agent_state_store(store)
        .build()
        .expect("failed to build AgentOs")
}

async fn post_json(
    app: axum::Router,
    uri: &str,
    payload: serde_json::Value,
) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .expect("request build should succeed"),
        )
        .await
        .expect("app should handle request");
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("response body should be readable");
    let text = String::from_utf8(body.to_vec()).expect("response body must be utf-8");
    (status, text)
}

fn ai_sdk_messages_payload(
    thread_id: impl Into<String>,
    input: impl Into<String>,
    run_id: Option<String>,
) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "id".to_string(),
        serde_json::Value::String(thread_id.into()),
    );
    payload.insert(
        "messages".to_string(),
        json!([{"role": "user", "content": input.into()}]),
    );
    if let Some(run_id) = run_id {
        payload.insert("runId".to_string(), serde_json::Value::String(run_id));
    }
    serde_json::Value::Object(payload)
}

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(axum::body::Body::empty())
                .expect("request build should succeed"),
        )
        .await
        .expect("app should handle request");
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("response body should be readable");
    let json = serde_json::from_slice::<serde_json::Value>(&body)
        .expect("response body must be valid json");
    (status, json)
}

#[tokio::test]
async fn e2e_http_matrix_96() {
    let store = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(store.clone()));
    let app = compose_http_app(AppState {
        os,
        read_store: store.clone(),
    });

    let content_cases = [
        "hello",
        "plain text",
        "line1\\nline2",
        "json-like {\"a\":1}",
        "symbols !@#$%^&*()",
        "1234567890",
        "keep it short",
        "context test",
        "state test",
        "final case",
        "extra case 01",
        "extra case 02",
        "extra case 03",
        "extra case 04",
        "extra case 05",
        "extra case 06",
    ];

    let run_modes = ["fixed", "alt", "auto_like"];

    let mut executed = 0usize;

    for (content_idx, content) in content_cases.iter().enumerate() {
        for (run_mode_idx, run_mode) in run_modes.iter().enumerate() {
            let thread_id = format!("matrix-ai-{content_idx}-{run_mode_idx}");
            let ai_run_id = match *run_mode {
                "fixed" => Some(format!("r-fixed-{content_idx}-{run_mode_idx}")),
                "alt" => Some(format!("r-alt-{content_idx}-{run_mode_idx}")),
                _ => None,
            };

            let ai_payload =
                ai_sdk_messages_payload(thread_id.clone(), content.to_string(), ai_run_id);
            let (status, body) =
                post_json(app.clone(), "/v1/ai-sdk/agents/test/runs", ai_payload).await;
            assert_eq!(status, StatusCode::OK);
            assert!(
                body.contains(r#""type":"start""#),
                "missing ai-sdk start event: {body}"
            );
            assert!(
                body.contains(r#""type":"finish""#),
                "missing ai-sdk finish event: {body}"
            );

            let ai_saved = store
                .load_thread(&thread_id)
                .await
                .expect("load should not fail")
                .expect("thread should be persisted");
            assert!(
                ai_saved.messages.iter().any(|m| m.content == *content),
                "persisted ai-sdk thread missing user content"
            );
            executed += 1;

            let ag_thread_id = format!("matrix-ag-{content_idx}-{run_mode_idx}");
            let ag_run_id = match *run_mode {
                "fixed" => format!("ag-fixed-{content_idx}-{run_mode_idx}"),
                "alt" => format!("ag-alt-{content_idx}-{run_mode_idx}"),
                _ => format!("ag-auto-like-{content_idx}-{run_mode_idx}"),
            };

            let ag_payload = json!({
                "threadId": ag_thread_id,
                "runId": ag_run_id,
                "messages": [{"role": "user", "content": content}],
                "tools": []
            });
            let (status, body) =
                post_json(app.clone(), "/v1/ag-ui/agents/test/runs", ag_payload).await;
            assert_eq!(status, StatusCode::OK);
            assert!(
                body.contains(r#""type":"RUN_STARTED""#),
                "missing ag-ui RUN_STARTED event: {body}"
            );
            assert!(
                body.contains(r#""type":"RUN_FINISHED""#),
                "missing ag-ui RUN_FINISHED event: {body}"
            );

            let ag_saved = store
                .load_thread(&format!("matrix-ag-{content_idx}-{run_mode_idx}"))
                .await
                .expect("load should not fail")
                .expect("thread should be persisted");
            assert!(
                ag_saved.messages.iter().any(|m| m.content == *content),
                "persisted ag-ui thread missing user content"
            );
            executed += 1;
        }
    }

    assert_eq!(executed, 96, "e2e scenario count drifted");
}

#[tokio::test]
async fn e2e_http_concurrent_48_all_persisted() {
    let store = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(store.clone()));
    let app = compose_http_app(AppState {
        os,
        read_store: store.clone(),
    });

    let mut handles = Vec::new();

    for i in 0..24usize {
        let app_clone = app.clone();
        handles.push(tokio::spawn(async move {
            let thread_id = format!("concurrent-ai-{i}");
            let payload = ai_sdk_messages_payload(
                thread_id.clone(),
                format!("hi-ai-{i}"),
                Some(format!("run-ai-{i}")),
            );
            post_json(app_clone, "/v1/ai-sdk/agents/test/runs", payload).await
        }));
    }

    for i in 0..24usize {
        let app_clone = app.clone();
        handles.push(tokio::spawn(async move {
            let thread_id = format!("concurrent-ag-{i}");
            let payload = json!({
                "threadId": thread_id,
                "runId": format!("run-ag-{i}"),
                "messages": [{"role": "user", "content": format!("hi-ag-{i}")}],
                "tools": []
            });
            post_json(app_clone, "/v1/ag-ui/agents/test/runs", payload).await
        }));
    }

    for handle in handles {
        let (status, body) = handle.await.expect("task should join");
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("finish") || body.contains("RUN_FINISHED"),
            "missing finish event in response: {body}"
        );
    }

    for i in 0..24usize {
        let ai = store
            .load_thread(&format!("concurrent-ai-{i}"))
            .await
            .expect("load should not fail")
            .expect("ai thread should exist");
        assert!(
            ai.messages
                .iter()
                .any(|m| m.content == format!("hi-ai-{i}")),
            "ai persisted content missing for index {i}"
        );

        let ag = store
            .load_thread(&format!("concurrent-ag-{i}"))
            .await
            .expect("load should not fail")
            .expect("ag thread should exist");
        assert!(
            ag.messages
                .iter()
                .any(|m| m.content == format!("hi-ag-{i}")),
            "ag persisted content missing for index {i}"
        );
    }
}

#[tokio::test]
async fn e2e_http_multiturn_history_endpoints_are_consistent() {
    let store = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(store.clone()));
    let app = compose_http_app(AppState {
        os,
        read_store: store.clone(),
    });

    let first_payload = ai_sdk_messages_payload(
        "history-e2e-thread",
        "first-turn",
        Some("history-r1".to_string()),
    );
    let (status, body) = post_json(app.clone(), "/v1/ai-sdk/agents/test/runs", first_payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#""type":"finish""#));

    let second_payload = json!({
        "id": "history-e2e-thread",
        "runId": "history-r2",
        "messages": [
            {"role": "user", "content": "first-turn"},
            {"role": "user", "content": "second-turn"}
        ]
    });
    let (status, body) =
        post_json(app.clone(), "/v1/ai-sdk/agents/test/runs", second_payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#""type":"finish""#));

    let (status, raw_page) = get_json(app.clone(), "/v1/threads/history-e2e-thread/messages").await;
    assert_eq!(status, StatusCode::OK);
    let raw_page_text = raw_page.to_string();
    assert!(
        raw_page_text.contains("first-turn"),
        "missing first turn in raw page: {raw_page_text}"
    );
    assert!(
        raw_page_text.contains("second-turn"),
        "missing second turn in raw page: {raw_page_text}"
    );

    let (status, encoded_page) =
        get_json(app, "/v1/ai-sdk/threads/history-e2e-thread/messages").await;
    assert_eq!(status, StatusCode::OK);
    let encoded_page_text = encoded_page.to_string();
    assert!(
        encoded_page_text.contains("first-turn"),
        "missing first turn in ai-sdk page: {encoded_page_text}"
    );
    assert!(
        encoded_page_text.contains("second-turn"),
        "missing second turn in ai-sdk page: {encoded_page_text}"
    );
}

#[tokio::test]
async fn e2e_http_ai_sdk_large_payload_roundtrip() {
    let store = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(store.clone()));
    let app = compose_http_app(AppState {
        os,
        read_store: store.clone(),
    });

    let large_input = "x".repeat(256 * 1024);
    let payload = ai_sdk_messages_payload(
        "large-payload-thread",
        large_input,
        Some("large-payload-run".to_string()),
    );
    let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#""type":"finish""#),
        "missing finish for large payload run: {body}"
    );

    let saved = store
        .load_thread("large-payload-thread")
        .await
        .expect("load should not fail")
        .expect("thread should be persisted");
    let persisted = saved
        .messages
        .iter()
        .find(|m| m.content.len() == 256 * 1024)
        .expect("expected persisted large user message");
    assert_eq!(persisted.content.len(), 256 * 1024);
}

#[tokio::test]
async fn e2e_http_mixed_large_payload_concurrency_64() {
    let store = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(store.clone()));
    let app = compose_http_app(AppState {
        os,
        read_store: store.clone(),
    });

    let total = 64usize;
    let mut handles = Vec::with_capacity(total);
    for i in 0..total {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            if i % 2 == 0 {
                let input = if i % 4 == 0 {
                    format!("large-ai-{i}-{}", "x".repeat(64 * 1024))
                } else {
                    format!("small-ai-{i}")
                };
                let payload = ai_sdk_messages_payload(
                    format!("mixed-ai-{i}"),
                    input.clone(),
                    Some(format!("mixed-ai-run-{i}")),
                );
                let (status, body) = post_json(app, "/v1/ai-sdk/agents/test/runs", payload).await;
                (format!("mixed-ai-{i}"), input, status, body)
            } else {
                let input = if i % 5 == 1 {
                    format!("large-ag-{i}-{}", "y".repeat(48 * 1024))
                } else {
                    format!("small-ag-{i}")
                };
                let payload = json!({
                    "threadId": format!("mixed-ag-{i}"),
                    "runId": format!("mixed-ag-run-{i}"),
                    "messages": [{"role": "user", "content": input.clone()}],
                    "tools": []
                });
                let (status, body) = post_json(app, "/v1/ag-ui/agents/test/runs", payload).await;
                (format!("mixed-ag-{i}"), input, status, body)
            }
        }));
    }

    let mut results = Vec::with_capacity(total);
    for h in handles {
        let (thread_id, input, status, body) = h.await.expect("task should finish");
        assert_eq!(status, StatusCode::OK, "unexpected status for {thread_id}");
        assert!(
            body.contains("finish") || body.contains("RUN_FINISHED"),
            "missing finish marker for {thread_id}: {body}"
        );
        results.push((thread_id, input));
    }

    for (thread_id, input) in results {
        let saved = store
            .load_thread(&thread_id)
            .await
            .expect("load should not fail")
            .unwrap_or_else(|| panic!("thread should exist: {thread_id}"));
        assert!(
            saved.messages.iter().any(|m| m.content == input),
            "persisted thread {thread_id} missing input payload (len={})",
            input.len()
        );
    }
}
