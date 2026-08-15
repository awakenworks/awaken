//! Cross-module Managed Agents overview -> Dream E2E.

use awaken_scenario_host::build_dream_router;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri).header(
        "anthropic-beta",
        "managed-agents-2026-04-01,dreaming-2026-04-21",
    );
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let (status, value) = call(app, method, uri, body).await;
    assert_eq!(status, StatusCode::OK, "{method} {uri}: {value}");
    value
}

async fn terminal_dream(app: &Router, id: &str) -> Value {
    for _ in 0..200 {
        let value = ok(app, "GET", &format!("/v1/dreams/{id}?beta=true"), None).await;
        if matches!(
            value["status"].as_str(),
            Some("completed" | "failed" | "canceled")
        ) {
            return value;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("Dream remained non-terminal")
}

#[tokio::test]
async fn agent_session_events_files_memory_and_dream_share_one_runtime_and_data_plane() {
    // Managed-overview cause/effect graph:
    // C0 scope-less local scenario request -> E0 the startup edge stamps the
    // Host's default Workspace before every data and policy handler;
    // C1 Agent-referenced Session + user event -> E1 durable full event history;
    // C2 ordinary MemoryStore + selected Session -> E2 asynchronous Dream;
    // C3 frozen transcript export -> E3 transient JSONL Files are mounted and
    // removed after execution; C4 consolidation execution -> E4 ordinary archived
    // Session + an independent output MemoryStore reached through a real write
    // tool call; C5
    // source/output separation -> E5 source stays unchanged;
    // C6 explicit `view=full` -> E6 list projections include Dream contents (the
    // official default `basic` projection intentionally omits them); C7 the Runtime
    // terminal lifecycle cursor is consumed after the Session archive CAS -> E7 the
    // delayed event projection cannot reverse `terminated` to `idle`. Decision rule
    // R1 covers the successful end-to-end combination of all eight causes, including
    // the late-feed ordering selected by the shared ephemeral Runtime authority.
    // Route/unit suites own invalid, default-basic, and nonterminal alternatives.
    let app = build_dream_router();

    let session = ok(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "assistant",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        })),
    )
    .await;
    let session_id = session["id"].as_str().unwrap();
    ok(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/events"),
        Some(json!({
            "events":[{
                "type":"user.message",
                "content":[{"type":"text","text":"Remember that Project Atlas uses Rust."}]
            }]
        })),
    )
    .await;
    let events = ok(
        &app,
        "GET",
        &format!("/v1/sessions/{session_id}/events"),
        None,
    )
    .await;
    assert!(events["data"].as_array().unwrap().len() >= 4);

    let store = ok(
        &app,
        "POST",
        "/v1/memory_stores",
        Some(json!({"name":"Project memory"})),
    )
    .await;
    let store_id = store["id"].as_str().unwrap();
    let default_policy = ok(
        &app,
        "GET",
        &format!("/v1/awaken/memory-stores/{store_id}/dream-policy"),
        None,
    )
    .await;
    assert_eq!(default_policy["type"], "dream_policy");
    assert_eq!(default_policy["enabled"], false);
    let configured_policy = ok(
        &app,
        "PUT",
        &format!("/v1/awaken/memory-stores/{store_id}/dream-policy"),
        Some(json!({
            "enabled": false,
            "interval_seconds": 3600,
            "min_new_sessions": 1,
            "max_sessions": 25,
            "model": {"id":"claude-sonnet-5","speed":"standard"},
            "instructions": "Retain verified project conventions."
        })),
    )
    .await;
    assert_eq!(configured_policy["max_sessions"], 25);
    assert!(configured_policy["next_due_at"].is_string());
    ok(
        &app,
        "POST",
        &format!("/v1/memory_stores/{store_id}/memories"),
        Some(json!({"path":"/MEMORY.md","content":"# Existing\n- Keep me.\n"})),
    )
    .await;

    let dream = ok(
        &app,
        "POST",
        "/v1/dreams?beta=true",
        Some(json!({
            "inputs":[
                {"type":"memory_store","memory_store_id":store_id},
                {"type":"sessions","session_ids":[session_id]}
            ],
            "model":"claude-sonnet-5",
            "instructions":"Retain verified project conventions."
        })),
    )
    .await;
    let terminal = terminal_dream(&app, dream["id"].as_str().unwrap()).await;
    assert_eq!(terminal["status"], "completed", "{terminal}");
    let output_id = terminal["outputs"][0]["memory_store_id"].as_str().unwrap();
    assert_ne!(output_id, store_id);

    let source = ok(
        &app,
        "GET",
        &format!("/v1/memory_stores/{store_id}/memories?view=full"),
        None,
    )
    .await;
    let output = ok(
        &app,
        "GET",
        &format!("/v1/memory_stores/{output_id}/memories?view=full"),
        None,
    )
    .await;
    let auxiliary_events = ok(
        &app,
        "GET",
        &format!(
            "/v1/sessions/{}/events",
            terminal["session_id"].as_str().unwrap()
        ),
        None,
    )
    .await;
    assert_eq!(source["data"][0]["content"], "# Existing\n- Keep me.\n");
    assert_eq!(output["data"][0]["content"], source["data"][0]["content"]);
    assert!(
        auxiliary_events["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| {
                event["type"] == "agent.tool_use"
                    && event["name"] == "write"
                    && event["input"]["path"] == "/mnt/dream/output-memory/MEMORY.md"
            })
    );

    let auxiliary = ok(
        &app,
        "GET",
        &format!("/v1/sessions/{}", terminal["session_id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(auxiliary["status"], "terminated");
    assert_eq!(auxiliary["metadata"]["awaken.session.origin"], "dream");

    let files = ok(&app, "GET", "/v1/files", None).await;
    let transcript = files["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| {
            file["filename"]
                .as_str()
                .is_some_and(|name| name == format!("{session_id}.jsonl"))
        })
        .cloned();
    assert!(
        transcript.is_none(),
        "transient transcript Files must be deleted after terminal cleanup"
    );
}
