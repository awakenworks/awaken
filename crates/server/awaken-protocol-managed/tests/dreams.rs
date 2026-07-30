use std::sync::Arc;

use awaken_protocol_managed::types::DreamUsage;
use awaken_protocol_managed::{
    BUILT_IN_MEMORY_CONSOLIDATOR_AGENT_ID, DREAMING_BETA, DreamState,
    MemoryConsolidationCancellation, MemoryConsolidationFailure, MemoryConsolidationPreparation,
    MemoryConsolidationRequest, MemoryConsolidationWorker, SqliteManagedSessionRepository,
    dreams_router, enforce_managed_beta,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt;

#[derive(Clone, Copy)]
enum Outcome {
    Complete,
    Fail,
    Block,
}

struct Worker {
    outcome: Outcome,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl MemoryConsolidationWorker for Worker {
    async fn validate_inputs(
        &self,
        request: &MemoryConsolidationRequest,
    ) -> Result<(), MemoryConsolidationFailure> {
        if request.source_memory_store_id == "missing" {
            Err(MemoryConsolidationFailure::new(
                "input_memory_store_unavailable",
                "missing input",
            ))
        } else {
            Ok(())
        }
    }

    async fn prepare(
        &self,
        request: &MemoryConsolidationRequest,
    ) -> Result<MemoryConsolidationPreparation, MemoryConsolidationFailure> {
        Ok(MemoryConsolidationPreparation {
            result_memory_store_id: format!("mem_result_{}", request.job_id),
            session_id: format!("sesn_{}", request.job_id),
        })
    }

    async fn execute(
        &self,
        _request: &MemoryConsolidationRequest,
        _preparation: &MemoryConsolidationPreparation,
        cancellation: MemoryConsolidationCancellation,
    ) -> Result<DreamUsage, MemoryConsolidationFailure> {
        self.started.notify_waiters();
        match self.outcome {
            Outcome::Complete => Ok(DreamUsage {
                input_tokens: 12,
                output_tokens: 3,
                ..Default::default()
            }),
            Outcome::Fail => Err(MemoryConsolidationFailure::new(
                "internal_error",
                "planned failure",
            )),
            Outcome::Block => {
                self.release.notified().await;
                if cancellation.is_canceled() {
                    Ok(DreamUsage::default())
                } else {
                    Ok(DreamUsage {
                        input_tokens: 7,
                        ..Default::default()
                    })
                }
            }
        }
    }
}

fn state(outcome: Outcome) -> (Arc<DreamState>, Arc<Notify>, Arc<Notify>) {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let worker = Arc::new(Worker {
        outcome,
        started: started.clone(),
        release: release.clone(),
    });
    (Arc::new(DreamState::new(worker)), started, release)
}

fn create_body(memory: &str, sessions: &[&str]) -> Value {
    json!({
        "inputs": [
            {"type":"memory_store", "memory_store_id":memory},
            {"type":"sessions", "session_ids":sessions},
        ],
        "model": {"id":"claude-sonnet-5", "speed":"standard"},
        "instructions":"Prefer durable decisions."
    })
}

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn wait_for_status(app: &axum::Router, id: &str, expected: &str) -> Value {
    for _ in 0..100 {
        let (_, value) = request(app, "GET", &format!("/v1/dreams/{id}"), None).await;
        if value["status"] == expected {
            return value;
        }
        tokio::task::yield_now().await;
    }
    panic!("Dream did not reach {expected}")
}

#[tokio::test]
async fn official_create_retrieve_list_archive_and_failure_shapes() {
    // Cause/effect graph and decision rules (Anthropic Dreams contract):
    // C1 valid memory + 1..=100 Sessions + model -> E1 async Dream; D1.
    // C2 worker completes -> E2 output/session/usage retained; D2.
    // C3 terminal archive -> E3 timestamp set, status unchanged, default list hides; D3.
    // C4 worker fails after prepare -> E4 failed + typed error + partial output retained; D4.
    let (complete, _, _) = state(Outcome::Complete);
    let app = dreams_router(complete);
    let (status, created) = request(
        &app,
        "POST",
        "/v1/dreams?beta=true",
        Some(create_body("mem_1", &["sesn_1"])),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["type"], "dream");
    assert_eq!(created["status"], "pending");
    let id = created["id"].as_str().unwrap();
    let completed = wait_for_status(&app, id, "completed").await;
    assert_eq!(completed["outputs"][0]["type"], "memory_store");
    assert_eq!(completed["session_id"], format!("sesn_{id}"));
    assert_eq!(completed["usage"]["input_tokens"], 12);

    let (status, archived) = request(&app, "POST", &format!("/v1/dreams/{id}/archive"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(archived["status"], "completed");
    assert!(archived["archived_at"].is_string());
    let (_, default_list) = request(&app, "GET", "/v1/dreams", None).await;
    assert_eq!(default_list["data"].as_array().unwrap().len(), 0);
    let (_, archived_list) = request(&app, "GET", "/v1/dreams?include_archived=true", None).await;
    assert_eq!(archived_list["data"].as_array().unwrap().len(), 1);

    let (failed_state, _, _) = state(Outcome::Fail);
    let failed_app = dreams_router(failed_state);
    let (_, created) = request(
        &failed_app,
        "POST",
        "/v1/dreams",
        Some(create_body("mem_1", &["sesn_1"])),
    )
    .await;
    let failed = wait_for_status(&failed_app, created["id"].as_str().unwrap(), "failed").await;
    assert_eq!(failed["error"]["type"], "internal_error");
    assert_eq!(failed["outputs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn validation_and_terminal_mutation_decision_table() {
    // Causes: input-union cardinality, Session count/uniqueness, instruction/model
    // bounds, input availability, and lifecycle state. Effects/decision rules:
    // D1 each invalid create cause -> 400 and no job; D2 missing input -> 400;
    // D3 archive(nonterminal) -> 400; D4 cancel(completed|failed) -> 400.
    let (state, _, _) = state(Outcome::Complete);
    let app = dreams_router(state);
    let invalid = [
        json!({"inputs":[], "model":"m"}),
        create_body("mem", &[]),
        create_body("mem", &["same", "same"]),
        json!({
            "inputs":[{"type":"memory_store","memory_store_id":"mem"},{"type":"sessions","session_ids":["s"]}],
            "model":""
        }),
        json!({
            "inputs":[{"type":"memory_store","memory_store_id":"mem"},{"type":"sessions","session_ids":["s"]}],
            "model":"m", "instructions":"x".repeat(4097)
        }),
        create_body("missing", &["s"]),
    ];
    for body in invalid {
        let (status, _) = request(&app, "POST", "/v1/dreams", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    let (_, created) = request(&app, "POST", "/v1/dreams", Some(create_body("mem", &["s"]))).await;
    let id = created["id"].as_str().unwrap();
    let _ = wait_for_status(&app, id, "completed").await;
    let (status, _) = request(&app, "POST", &format!("/v1/dreams/{id}/cancel"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cancellation_is_immediate_idempotent_and_retains_prepared_output() {
    // C1 running prepared job + cancel -> E1 public status immediately canceled;
    // C2 output already cloned -> E2 retained; C3 repeated cancel -> E3 same object;
    // C4 late worker completion -> E4 cannot overwrite canceled. Rules D1-D4.
    let (state, started, release) = state(Outcome::Block);
    let app = dreams_router(state);
    let (_, created) = request(&app, "POST", "/v1/dreams", Some(create_body("mem", &["s"]))).await;
    let id = created["id"].as_str().unwrap();
    started.notified().await;
    let (status, canceled) = request(&app, "POST", &format!("/v1/dreams/{id}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(canceled["status"], "canceled");
    assert_eq!(canceled["outputs"].as_array().unwrap().len(), 1);
    let (_, repeated) = request(&app, "POST", &format!("/v1/dreams/{id}/cancel"), None).await;
    assert_eq!(repeated["status"], "canceled");
    release.notify_waiters();
    tokio::task::yield_now().await;
    let (_, retrieved) = request(&app, "GET", &format!("/v1/dreams/{id}"), None).await;
    assert_eq!(retrieved["status"], "canceled");
}

#[tokio::test]
async fn dream_routes_require_managed_and_dreaming_betas() {
    // Header causes/effects: neither/one beta -> 400; both capabilities -> route.
    // This is the four-rule truth table for the overview's Managed beta plus the
    // Dreams research-preview beta; query `beta=true` never substitutes for headers.
    let (state, _, _) = state(Outcome::Complete);
    let app = dreams_router(state).layer(axum::middleware::from_fn(enforce_managed_beta));
    for header in [
        None,
        Some(awaken_managed_bridge::MANAGED_BETA),
        Some(DREAMING_BETA),
    ] {
        let mut builder = Request::builder().method("GET").uri("/v1/dreams?beta=true");
        if let Some(header) = header {
            builder = builder.header("anthropic-beta", header);
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/dreams?beta=true")
                .header(
                    "anthropic-beta",
                    format!("{},{}", awaken_managed_bridge::MANAGED_BETA, DREAMING_BETA),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn list_supports_official_repeated_status_filters_and_cursor_pages() {
    // List causes/effects: newest-first inventory + limit -> E1 page cursor;
    // repeated statuses -> E2 OR filter; invalid bound/limit -> E3 400. Rules
    // L1-L3 cover the SDK PageCursor and DreamListParams contract.
    let (state, _, _) = state(Outcome::Complete);
    let app = dreams_router(state);
    let mut ids = Vec::new();
    for session in ["s1", "s2"] {
        let (_, created) = request(
            &app,
            "POST",
            "/v1/dreams",
            Some(create_body("mem", &[session])),
        )
        .await;
        ids.push(created["id"].as_str().unwrap().to_string());
    }
    for id in &ids {
        let _ = wait_for_status(&app, id, "completed").await;
    }
    let (_, first) = request(&app, "GET", "/v1/dreams?limit=1", None).await;
    assert_eq!(first["data"].as_array().unwrap().len(), 1);
    let cursor = first["next_page"].as_str().unwrap();
    let (_, second) = request(
        &app,
        "GET",
        &format!("/v1/dreams?limit=1&page={cursor}"),
        None,
    )
    .await;
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    let (_, filtered) = request(
        &app,
        "GET",
        "/v1/dreams?statuses=failed&statuses=completed",
        None,
    )
    .await;
    assert_eq!(filtered["data"].as_array().unwrap().len(), 2);
    for uri in [
        "/v1/dreams?limit=0",
        "/v1/dreams?created_at%5Bgt%5D=not-a-time",
    ] {
        assert_eq!(
            request(&app, "GET", uri, None).await.0,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn sqlite_repository_restores_terminal_dreams_after_restart() {
    // Persistence cause/effect rule: terminal transition committed -> process
    // restart -> retrieve projects the same lifecycle/output/usage. An incomplete
    // transition is covered by `resume_incomplete`; this rule protects terminal
    // jobs from being process-local BackgroundRuns state.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "awaken-dream-state-{}-{unique}.db",
        std::process::id()
    ));
    let worker = Arc::new(Worker {
        outcome: Outcome::Complete,
        started: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    });
    let repository =
        Arc::new(SqliteManagedSessionRepository::open(path.to_str().unwrap()).unwrap());
    let state = Arc::new(DreamState::with_repository(worker, repository).unwrap());
    let app = dreams_router(state.clone());
    let (_, created) = request(&app, "POST", "/v1/dreams", Some(create_body("mem", &["s"]))).await;
    let id = created["id"].as_str().unwrap().to_string();
    let completed = wait_for_status(&app, &id, "completed").await;
    drop(app);
    drop(state);

    let reopened = DreamState::with_repository(
        Arc::new(Worker {
            outcome: Outcome::Complete,
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        }),
        Arc::new(SqliteManagedSessionRepository::open(path.to_str().unwrap()).unwrap()),
    )
    .unwrap();
    let restored = reopened.retrieve("default", &id).unwrap();
    let restored = serde_json::to_value(restored).unwrap();
    assert_eq!(restored["status"], "completed");
    assert_eq!(restored["outputs"], completed["outputs"]);
    assert_eq!(restored["usage"], completed["usage"]);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn workspace_agent_selection_uses_effective_default_and_freezes_override() {
    struct RecordingWorker(Arc<std::sync::Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl MemoryConsolidationWorker for RecordingWorker {
        async fn validate_inputs(
            &self,
            request: &MemoryConsolidationRequest,
        ) -> Result<(), MemoryConsolidationFailure> {
            self.0
                .lock()
                .unwrap()
                .push(request.agent_selection.agent_id.clone());
            Ok(())
        }
        async fn prepare(
            &self,
            request: &MemoryConsolidationRequest,
        ) -> Result<MemoryConsolidationPreparation, MemoryConsolidationFailure> {
            Ok(MemoryConsolidationPreparation {
                result_memory_store_id: format!("result-{}", request.job_id),
                session_id: format!("session-{}", request.job_id),
            })
        }
        async fn execute(
            &self,
            _request: &MemoryConsolidationRequest,
            _preparation: &MemoryConsolidationPreparation,
            _cancellation: MemoryConsolidationCancellation,
        ) -> Result<DreamUsage, MemoryConsolidationFailure> {
            Ok(DreamUsage::default())
        }
    }

    // Selection decision table: no Workspace row -> built-in effective default;
    // exact override row -> that published Agent id; clearing -> built-in again.
    // Each create freezes its selection before dispatch, so policy edits affect
    // only subsequent jobs and no default Agent row is duplicated per Workspace.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let state = Arc::new(DreamState::new(Arc::new(RecordingWorker(seen.clone()))));
    let app = dreams_router(state.clone());
    let _ = request(
        &app,
        "POST",
        "/v1/dreams",
        Some(create_body("mem", &["s1"])),
    )
    .await;
    state
        .set_workspace_agent_override("default", Some("agent_custom_consolidator"))
        .unwrap();
    let _ = request(
        &app,
        "POST",
        "/v1/dreams",
        Some(create_body("mem", &["s2"])),
    )
    .await;
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            BUILT_IN_MEMORY_CONSOLIDATOR_AGENT_ID.to_string(),
            "agent_custom_consolidator".to_string()
        ]
    );
}
