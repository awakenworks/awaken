//! Cross-module Managed Agents overview -> Dream E2E.

use std::path::Path;
use std::sync::Arc;

use awaken_memory_store::MemoryRepository;
use awaken_runtime_host::{MemoryMount, MemoryMounter, MountAccess, Realization, SandboxError};
use awaken_scenario_host::{EchoModel, build_router_and_host};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct DeterministicWriteThroughMounter {
    memory: Arc<dyn MemoryRepository>,
}

struct DeterministicWriteThroughMount;

#[async_trait::async_trait]
impl MemoryMount for DeterministicWriteThroughMount {
    fn realization(&self) -> Realization {
        Realization::Fuse
    }

    async fn teardown(self: Box<Self>) {}
}

#[async_trait::async_trait]
impl MemoryMounter for DeterministicWriteThroughMounter {
    async fn mount(
        &self,
        store_id: &str,
        host_path: &Path,
        _access: MountAccess,
    ) -> Result<Box<dyn MemoryMount>, SandboxError> {
        std::fs::create_dir_all(host_path).map_err(|error| SandboxError::new(error.to_string()))?;
        for memory in self
            .memory
            .snapshot_heads(store_id)
            .await
            .map_err(|error| SandboxError::new(error.to_string()))?
        {
            let path = host_path.join(memory.path.trim_start_matches('/'));
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| SandboxError::new(error.to_string()))?;
            }
            std::fs::write(&path, memory.content.unwrap_or_default())
                .map_err(|error| SandboxError::new(error.to_string()))?;
        }
        Ok(Box::new(DeterministicWriteThroughMount))
    }
}

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
        tokio::task::yield_now().await;
    }
    panic!("Dream remained non-terminal")
}

#[tokio::test]
async fn agent_session_events_files_memory_and_dream_share_one_runtime_and_data_plane() {
    // Managed-overview cause/effect graph:
    // C1 Agent-referenced Session + user event -> E1 durable full event history;
    // C2 ordinary MemoryStore + selected Session -> E2 asynchronous Dream;
    // C3 frozen transcript export -> E3 Files JSONL preserving committed messages;
    // C4 consolidation execution -> E4 ordinary archived Session + independent
    // output MemoryStore; C5 source/output separation -> E5 source stays unchanged.
    // Decision rule R1 covers the successful end-to-end combination of all five
    // causes. Route/unit suites own invalid and terminal alternatives.
    let (app, host) = build_router_and_host(Arc::new(EchoModel), "claude-sonnet-5");
    host.install_memory_mounter(Arc::new(DeterministicWriteThroughMounter {
        memory: host.memory_repository(),
    }));

    let session = ok(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent":"assistant"})),
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
        &format!("/v1/memory_stores/{store_id}/memories"),
        None,
    )
    .await;
    let output = ok(
        &app,
        "GET",
        &format!("/v1/memory_stores/{output_id}/memories"),
        None,
    )
    .await;
    assert_eq!(source["data"][0]["content"], "# Existing\n- Keep me.\n");
    assert_eq!(output["data"][0]["content"], source["data"][0]["content"]);

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
        .expect("Dream exports one JSONL artifact per selected Session");
    let content_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/v1/files/{}/content",
                    transcript["id"].as_str().unwrap()
                ))
                .header(
                    "anthropic-beta",
                    "managed-agents-2026-04-01,dreaming-2026-04-21",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(content_response.status(), StatusCode::OK);
    let jsonl = String::from_utf8(
        content_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(jsonl.contains("Remember that Project Atlas uses Rust."));
    assert!(
        jsonl
            .lines()
            .all(|line| serde_json::from_str::<Value>(line).is_ok())
    );
}
