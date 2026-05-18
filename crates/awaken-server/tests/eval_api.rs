//! HTTP-level integration tests for ADR-0032 D6 + D1 + D7 routes:
//!
//!   /v1/eval/datasets          — list / create
//!   /v1/eval/datasets/:id      — get / put / delete
//!   /v1/eval/datasets/:id/items — curate from trace
//!   /v1/eval/runs              — list / start
//!   /v1/eval/runs/:id          — fetch (+ optional ?baseline= diff)
//!
//! Each test stands up a minimal `AppState` with in-memory
//! ConfigStore + file-backed TraceStore + file-backed EvalRunStore, then
//! drives the router via `tower::ServiceExt::oneshot`. The harness is
//! deliberately leaner than `config_api.rs`: eval CRUD doesn't touch the
//! agent runtime except for `POST /v1/eval/runs`, which uses the
//! bundled scripted-executor path that doesn't need a real provider.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_eval::{EvalRun, EvalRunItem, EvalRunStore, FileEvalRunStore, Fixture};
use awaken_ext_observability::trace_store::{TraceStore, file::FileTraceStore};
use awaken_ext_observability::{GenAISpan, MetricsEvent, SpanContext};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_server::app::{AdminApiConfig, AppState, ServerConfig};
use awaken_server::mailbox::{Mailbox, MailboxConfig};
use awaken_server::routes::build_router;
use awaken_stores::InMemoryStore;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

// ── Stub executor — fixtures use scripted upstream; this never fires. ─────

struct UnusedExecutor;

#[async_trait]
impl LlmExecutor for UnusedExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        Ok(StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }
    fn name(&self) -> &str {
        "unused"
    }
}

// ── Harness ───────────────────────────────────────────────────────────────

const BEARER: &str = "test-admin-token";

fn temp_dir(prefix: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("awaken-{prefix}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct TestApp {
    router: axum::Router,
    trace_store: Arc<FileTraceStore>,
    eval_run_store: Arc<FileEvalRunStore>,
}

async fn build_test_app() -> TestApp {
    let thread_store = Arc::new(InMemoryStore::new());
    let config_store = Arc::new(InMemoryStore::new());
    let trace_store = Arc::new(FileTraceStore::new(temp_dir("eval-trace")).unwrap());
    let eval_run_store = Arc::new(FileEvalRunStore::new(temp_dir("eval-runs")).unwrap());

    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(UnusedExecutor))
            .with_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );
    let resolver = runtime.resolver_arc();
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "eval-test".into(),
        MailboxConfig::default(),
    ));

    let state = AppState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        ServerConfig {
            address: "127.0.0.1:0".to_string(),
            ..ServerConfig::default()
        },
    )
    .with_config_store(
        config_store.clone() as Arc<dyn awaken_contract::contract::config_store::ConfigStore>
    )
    .with_trace_store(trace_store.clone() as Arc<dyn TraceStore>)
    .with_eval_run_store(eval_run_store.clone() as Arc<dyn EvalRunStore>)
    .with_admin_api_config(AdminApiConfig {
        expose_config_routes: true,
        expose_trace_routes: true,
        ..AdminApiConfig::default()
    })
    .with_admin_api_bearer_token(BEARER);

    TestApp {
        router: build_router(&state).with_state(state),
        trace_store,
        eval_run_store,
    }
}

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {BEARER}"));
    let req = if let Some(b) = body {
        builder = builder.header("Content-Type", "application/json");
        builder
            .body(Body::from(serde_json::to_vec(&b).unwrap()))
            .unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

fn sample_fixture(id: &str) -> Fixture {
    serde_json::from_value(json!({
        "id": id,
        "user_input": "what is six times seven",
        "provider_script": [
            {"kind": "chat_response", "content": "42", "tokens": {"total_tokens": 5}}
        ],
        "expect": { "final_answer_contains": ["42"] }
    }))
    .unwrap()
}

// ── Dataset CRUD ──────────────────────────────────────────────────────────

#[tokio::test]
async fn dataset_create_get_list_delete_round_trip() {
    let app = build_test_app().await;

    // Create.
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-A",
            "spec": { "description": "smoke", "fixtures": [sample_fixture("alpha")] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["meta"]["revision"], 0);

    // Get.
    let (status, body) = request(&app.router, "GET", "/v1/eval/datasets/DS-A", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["spec"]["description"], "smoke");
    assert_eq!(body["spec"]["fixtures"].as_array().unwrap().len(), 1);

    // List.
    let (status, body) = request(&app.router, "GET", "/v1/eval/datasets", None).await;
    assert_eq!(status, StatusCode::OK);
    let datasets = body["datasets"].as_array().unwrap();
    assert_eq!(datasets.len(), 1);
    assert_eq!(datasets[0]["id"], "DS-A");
    assert_eq!(datasets[0]["fixture_count"], 1);

    // Delete (idempotent).
    let (status, _) = request(&app.router, "DELETE", "/v1/eval/datasets/DS-A", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app.router, "DELETE", "/v1/eval/datasets/DS-A", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "delete is idempotent");
}

#[tokio::test]
async fn dataset_create_conflicts_on_duplicate_id() {
    let app = build_test_app().await;
    let body = json!({
        "id": "DS-DUP",
        "spec": { "fixtures": [sample_fixture("a")] }
    });
    let (status, _) = request(&app.router, "POST", "/v1/eval/datasets", Some(body.clone())).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = request(&app.router, "POST", "/v1/eval/datasets", Some(body)).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
}

#[tokio::test]
async fn dataset_put_with_stale_revision_returns_409() {
    let app = build_test_app().await;
    let initial = json!({
        "id": "DS-REV",
        "spec": { "fixtures": [sample_fixture("a")] }
    });
    let (status, _) = request(&app.router, "POST", "/v1/eval/datasets", Some(initial)).await;
    assert_eq!(status, StatusCode::CREATED);

    // PUT with revision=0 (matches the freshly-created record).
    let put_body = json!({
        "expected_revision": 0,
        "spec": { "fixtures": [sample_fixture("a"), sample_fixture("b")] }
    });
    let (status, body) = request(
        &app.router,
        "PUT",
        "/v1/eval/datasets/DS-REV",
        Some(put_body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["meta"]["revision"], 1);

    // Repeat PUT with the now-stale revision=0 — must 409.
    let (status, body) = request(
        &app.router,
        "PUT",
        "/v1/eval/datasets/DS-REV",
        Some(put_body),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
}

#[tokio::test]
async fn dataset_get_returns_404_for_unknown_id() {
    let app = build_test_app().await;
    let (status, _) = request(&app.router, "GET", "/v1/eval/datasets/ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Curate from trace ─────────────────────────────────────────────────────

fn captured_inference_span(run_id: &str, text: &str, with_user: bool) -> GenAISpan {
    let request_messages = if with_user {
        Some(json!([
            {"role": "user", "content": [{"type": "text", "text": "auto prompt"}]}
        ]))
    } else {
        None
    };
    GenAISpan {
        context: SpanContext {
            run_id: run_id.into(),
            agent_id: "default".into(),
            ..Default::default()
        },
        step_index: Some(0),
        model: "claude-opus-4-7".into(),
        provider: "anthropic".into(),
        operation: "chat".into(),
        response_model: None,
        response_id: None,
        finish_reasons: vec!["end_turn".into()],
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(10),
        output_tokens: Some(4),
        total_tokens: Some(14),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: vec![],
        duration_ms: 1,
        started_at_ms: 0,
        ended_at_ms: 0,
        response_content: Some(json!([{"type": "text", "text": text}])),
        response_tool_calls: None,
        request_messages,
    }
}

#[tokio::test]
async fn curate_items_appends_fixture_recovered_from_trace() {
    let app = build_test_app().await;

    // Seed a trace whose first span captured the user prompt — the
    // server must recover user_input without operator help.
    let run_id = "01HXCUR0000000000000000001";
    app.trace_store
        .append(
            run_id,
            &MetricsEvent::Inference(captured_inference_span(run_id, "the answer is 42", true)),
        )
        .unwrap();

    // Empty dataset to receive the curated fixture.
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-CUR", "spec": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Curate.
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-CUR/items",
        Some(json!({ "from_run_id": run_id })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["spec"]["fixtures"].as_array().unwrap().len(), 1);
    let added = &body["spec"]["fixtures"][0];
    assert_eq!(added["id"], run_id);
    assert_eq!(added["user_input"], "auto prompt");
    assert_eq!(added["source_run_id"], run_id);
}

#[tokio::test]
async fn curate_items_400s_when_trace_lacks_user_and_body_lacks_input() {
    let app = build_test_app().await;

    let run_id = "01HXCUR0000000000000000002";
    app.trace_store
        .append(
            run_id,
            &MetricsEvent::Inference(captured_inference_span(run_id, "ok", false)),
        )
        .unwrap();

    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-CUR2", "spec": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-CUR2/items",
        Some(json!({ "from_run_id": run_id })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("user_input"),
        "body: {body}"
    );
}

// ── Eval runs ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn start_eval_run_drives_dataset_and_persists() {
    let app = build_test_app().await;

    // Seed a dataset that the run will exercise.
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-RUN",
            "spec": {
                "fixtures": [sample_fixture("alpha"), sample_fixture("beta")]
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({ "dataset_id": "DS-RUN" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let run = &body["run"];
    assert_eq!(run["dataset_id"], "DS-RUN");
    let items = run["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    for item in items {
        assert!(item["report"]["passed"].as_bool().unwrap());
        // Tee sink wired in the harness — trace_run_id must be present.
        assert!(item["trace_run_id"].is_string());
    }
    // No baseline requested → no diff.
    assert!(body["diff"].is_null());
}

#[tokio::test]
async fn start_eval_run_400s_for_empty_dataset() {
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-EMPTY", "spec": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({ "dataset_id": "DS-EMPTY" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("no fixtures to replay"),
        "body: {body}"
    );
}

#[tokio::test]
async fn get_eval_run_with_baseline_surfaces_diff() {
    let app = build_test_app().await;

    // Pre-seed two runs directly via EvalRunStore so we don't have to
    // double-replay through the route (already covered above) and can
    // craft a guaranteed difference between them.
    let store = app.eval_run_store.clone();
    let baseline = baseline_run("BASE-001");
    let new = new_run_with_drift("NEW-001");
    store.write(&baseline).unwrap();
    store.write(&new).unwrap();

    let (status, body) = request(
        &app.router,
        "GET",
        "/v1/eval/runs/NEW-001?baseline=BASE-001",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let diff = &body["diff"];
    assert!(diff.is_object(), "diff present");
    // At least one drift/regression entry from the seeded difference.
    let entries = diff["entries"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e["kind"] == "drift" || e["kind"] == "regression"),
        "expected a drift or regression; got {entries:?}"
    );
}

#[tokio::test]
async fn get_eval_run_with_unknown_baseline_returns_404() {
    let app = build_test_app().await;
    let run = baseline_run("LONELY");
    app.eval_run_store.write(&run).unwrap();
    let (status, _) = request(
        &app.router,
        "GET",
        "/v1/eval/runs/LONELY?baseline=ghost",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Atomic fixture append (POST /v1/eval/datasets/:id/fixtures) ──────────

#[tokio::test]
async fn append_fixture_adds_to_existing_dataset_and_bumps_revision() {
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-APPEND",
            "spec": { "fixtures": [sample_fixture("alpha")] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-APPEND/fixtures",
        Some(json!({
            "fixture": sample_fixture("beta"),
            "expected_revision": 0
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["meta"]["revision"], 1);
    let names: Vec<&str> = body["spec"]["fixtures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha", "beta"]);
}

#[tokio::test]
async fn append_fixture_409s_on_stale_revision() {
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-STALE",
            "spec": { "fixtures": [sample_fixture("a")] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-STALE/fixtures",
        Some(json!({
            "fixture": sample_fixture("b"),
            "expected_revision": 99
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
}

#[tokio::test]
async fn append_fixture_409s_on_duplicate_id() {
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-DUP-FX",
            "spec": { "fixtures": [sample_fixture("alpha")] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-DUP-FX/fixtures",
        Some(json!({
            "fixture": sample_fixture("alpha"),
            "expected_revision": 0
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("already has fixture"),
        "body: {body}"
    );
}

// ── Dataset run matrix-mode validation ────────────────────────────────────

#[tokio::test]
async fn start_eval_run_with_models_404s_on_unknown_model() {
    // Dataset has fixtures (scripted) but the matrix references an
    // unregistered model — fast-fail with 404 before any cell runs.
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-MATRIX",
            "spec": { "fixtures": [sample_fixture("alpha")] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-MATRIX",
            "models": ["unknown-model"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown-model"),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_caps_total_cells() {
    // 50 fixtures × 3 models = 150 cells exceeds MAX_CELLS_PER_SYNC_RUN (100).
    let app = build_test_app().await;
    let fixtures: Vec<_> = (0..50).map(|i| sample_fixture(&format!("f{i}"))).collect();
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-BIG", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-BIG",
            "models": ["m1", "m2", "m3"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("expands to 150 cells"),
        "body: {body}"
    );
}

// ── Online eval (POST /v1/eval/online) — validation paths ────────────────
//
// The happy path (cell execution against a real provider) is unit-tested
// in awaken-eval's runtime_replayer Live mode; the integration tests
// here cover the server-side validation and registry-lookup branches
// that don't require a live LLM.

#[tokio::test]
async fn online_eval_400s_on_empty_models() {
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({ "user_input": "test", "models": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("models"),
        "body: {body}"
    );
}

#[tokio::test]
async fn online_eval_400s_on_too_many_models() {
    // MAX_CELLS_PER_SYNC_ONLINE = 10; 11 must be rejected up-front
    // before any provider lookup or token spend.
    let app = build_test_app().await;
    let models: Vec<String> = (0..11).map(|i| format!("m{i}")).collect();
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({ "user_input": "test", "models": models })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("exceed sync online cap"),
        "body: {body}"
    );
}

#[tokio::test]
async fn online_eval_404s_on_unknown_model() {
    // No model bindings registered in this TestApp's config_store —
    // the resolver must surface a NotFound with the missing id.
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({ "user_input": "test", "models": ["missing-model"] })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("missing-model"),
        "body: {body}"
    );
}

// ── Auth ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn eval_routes_require_admin_bearer() {
    let app = build_test_app().await;
    // Same `request` helper but skip the Authorization header.
    let req = Request::builder()
        .method("GET")
        .uri("/v1/eval/datasets")
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── Helpers for the diff test ─────────────────────────────────────────────

fn baseline_run(id: &str) -> EvalRun {
    EvalRun {
        id: id.into(),
        dataset_id: "DS-DIFF".into(),
        dataset_revision: 1,
        items: vec![item("alpha", true, "good answer")],
        started_at_secs: 1_700_000_000,
        ended_at_secs: 1_700_000_001,
    }
}

fn new_run_with_drift(id: &str) -> EvalRun {
    // Same fixture id, different final_text → drift (both still pass).
    EvalRun {
        id: id.into(),
        dataset_id: "DS-DIFF".into(),
        dataset_revision: 1,
        items: vec![item("alpha", true, "different answer")],
        started_at_secs: 1_700_000_100,
        ended_at_secs: 1_700_000_101,
    }
}

fn item(fixture_id: &str, passed: bool, final_text: &str) -> EvalRunItem {
    use awaken_eval::ReplayReport;
    EvalRunItem {
        fixture_id: fixture_id.into(),
        cell: None,
        report: ReplayReport {
            fixture_id: fixture_id.into(),
            passed,
            failures: vec![],
            final_text: final_text.into(),
            inference_count: 1,
            tool_count: 0,
            tool_failures: 0,
            total_input_tokens: 1,
            total_output_tokens: 1,
            total_tokens: 2,
            session_duration_ms: 1,
            elapsed_ms: 0,
            tool_calls_by_agent: vec![],
            error_type: None,
            inference_error_count: 0,
            runtime_failure: None,
        },
        trace_run_id: None,
    }
}
