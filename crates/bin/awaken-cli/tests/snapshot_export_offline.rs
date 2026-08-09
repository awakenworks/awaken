//! Whole-boundary proof for online export followed by embedded SDK execution.

use std::sync::Arc;

use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_cli::build_all_in_one_router_with_model;
use awaken_runtime::Runtime;
use awaken_runtime_contract::RuntimeRunContext;
use awaken_scenario_host::EchoModel;
use awaken_store_inmem::MemoryCommitCoordinator;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    let request = request.body(body).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Cause/effect decision table:
/// R1 authored+pinned native publication -> scoped HTTP export returns the exact
/// snapshot; R2 local model edit -> load recomputes identity; R3 configured
/// embedded Runtime -> unchanged `Runtime::run` reaches one committed terminal;
/// no Server, ConfigStore, or alternate snapshot representation enters R2/R3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_export_model_edit_and_offline_runtime_form_one_path() {
    let app = build_all_in_one_router_with_model(Arc::new(EchoModel), "kimi").await;
    let (status, body) = call(
        &app,
        "PUT",
        "/v1/config/agents/offline-agent",
        Some(json!({
            "name": "Offline Agent",
            "model": {
                "mode": "pinned",
                "provider_identity_ref": "default",
                "model_ref": "kimi",
                "backend_ref": "genai"
            },
            "system": "Run from the exported snapshot."
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1 authoring: {body:?}");
    let (status, published) = call(
        &app,
        "POST",
        "/v1/config/agents/offline-agent/publish",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1 publication: {published:?}");
    let fingerprint = published["fingerprint"].as_str().unwrap();
    let (status, mut exported) = call(
        &app,
        "GET",
        &format!("/v1/config/publications/{fingerprint}/export"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1 export: {exported:?}");

    exported["resolved_spec"]["model_binding"]["model_ref"] = "local-edited-model".into();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.snapshot.json");
    serde_json::to_writer_pretty(std::fs::File::create(&path).unwrap(), &exported).unwrap();

    let runtime = Runtime::new().with_llm(Arc::new(EchoModel));
    let snapshot = runtime.load_snapshot_file(&path).expect("R2 load");
    assert_eq!(
        snapshot.resolved_spec.model_binding.binding.model_ref,
        "local-edited-model"
    );
    assert_ne!(snapshot.fingerprint.0, fingerprint, "R2 identity");

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let state = runtime
        .run(
            &snapshot,
            "offline input",
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("R3 run");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        commit.committed().run_facts.last().map(|fact| &fact.state),
        Some(&RunState::Ended(EndCause::NaturalEnd)),
        "R3 one committed terminal authority"
    );
}
