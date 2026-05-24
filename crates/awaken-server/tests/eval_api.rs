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

use awaken_contract::config_record::{ConfigRecord, RecordMeta};
use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::storage::StorageError;
use awaken_eval::test_support::UnusedExecutor;
use awaken_eval::{
    DATASETS_NAMESPACE, DatasetSpec, EvalRun, EvalRunExecutionMode, EvalRunItem, EvalRunStore,
    FileEvalRunStore, Fixture, MatrixCell,
};
use awaken_ext_observability::trace_store::{TraceStore, file::FileTraceStore};
use awaken_ext_observability::{DelegationSpan, GenAISpan, MetricsEvent, SpanContext};
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
    config_store: Arc<dyn ConfigStore>,
    trace_store: Arc<FileTraceStore>,
    eval_run_store: Arc<FileEvalRunStore>,
    /// Root passed to `FileEvalRunStore::new`. Tests that need to seed a
    /// corrupt on-disk run (e.g. one with duplicate item keys that the
    /// store's `write()` would normally reject) write the JSON file
    /// straight to `{root}/eval_runs/{yyyy-mm}/{run_id}.json`.
    eval_run_root: std::path::PathBuf,
}

async fn build_test_app_without_run_store() -> axum::Router {
    // Variant used by persist+no-store regression test: state has no
    // EvalRunStore attached so the online handler must refuse
    // persist=true BEFORE any provider call burns tokens.
    let thread_store = Arc::new(InMemoryStore::new());
    let config_store = Arc::new(InMemoryStore::new());
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
        config_store as Arc<dyn awaken_contract::contract::config_store::ConfigStore>,
    )
    .with_admin_api_config(AdminApiConfig {
        expose_config_routes: true,
        ..AdminApiConfig::default()
    })
    .with_admin_api_bearer_token(BEARER);
    build_router(&state).with_state(state)
}

async fn build_test_app() -> TestApp {
    build_test_app_with_config_store(Arc::new(InMemoryStore::new())).await
}

async fn build_test_app_with_config_store(config_store: Arc<dyn ConfigStore>) -> TestApp {
    let thread_store = Arc::new(InMemoryStore::new());
    let trace_store = Arc::new(FileTraceStore::new(temp_dir("eval-trace")).unwrap());
    let eval_run_root = temp_dir("eval-runs");
    let eval_run_store = Arc::new(FileEvalRunStore::new(eval_run_root.clone()).unwrap());

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
    .with_config_store(config_store.clone())
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
        config_store,
        trace_store,
        eval_run_store,
        eval_run_root,
    }
}

struct CasConflictConfigStore {
    inner: Arc<InMemoryStore>,
    conflict_id: String,
}

impl CasConflictConfigStore {
    fn new(conflict_id: &str) -> Self {
        Self {
            inner: Arc::new(InMemoryStore::new()),
            conflict_id: conflict_id.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl ConfigStore for CasConflictConfigStore {
    async fn get(&self, namespace: &str, id: &str) -> Result<Option<Value>, StorageError> {
        self.inner.get(namespace, id).await
    }

    async fn list(
        &self,
        namespace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, Value)>, StorageError> {
        self.inner.list(namespace, offset, limit).await
    }

    async fn put(&self, namespace: &str, id: &str, value: &Value) -> Result<(), StorageError> {
        self.inner.put(namespace, id, value).await
    }

    async fn delete(&self, namespace: &str, id: &str) -> Result<(), StorageError> {
        self.inner.delete(namespace, id).await
    }

    async fn put_if_absent(
        &self,
        namespace: &str,
        id: &str,
        value: &Value,
    ) -> Result<(), StorageError> {
        self.inner.put_if_absent(namespace, id, value).await
    }

    async fn put_if_revision(
        &self,
        namespace: &str,
        id: &str,
        value: &Value,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        if namespace == DATASETS_NAMESPACE && id == self.conflict_id {
            return Err(StorageError::VersionConflict {
                expected: expected_revision,
                actual: expected_revision.saturating_add(1),
            });
        }
        self.inner
            .put_if_revision(namespace, id, value, expected_revision)
            .await
    }
}

/// Test-only backdoor: write a hand-crafted `EvalRun` JSON file straight
/// into the `FileEvalRunStore` shard layout, bypassing
/// `EvalRunStore::write`. Needed for regression tests that exercise
/// "what if a corrupt run is already on disk" scenarios — the normal
/// `write()` path now rejects duplicate-key runs, so the only way to
/// stage one is to drop the file in by hand.
fn seed_corrupt_eval_run(root: &std::path::Path, run: &EvalRun) {
    let (year, month) = {
        // Mirror FileEvalRunStore's shard layout: yyyy-mm derived from
        // started_at_secs, UTC. Use chrono via time so we don't pull in
        // a new test dep — the started_at is fully under test control
        // and we can hard-code the shard if we generate it ourselves.
        use chrono::{TimeZone, Utc};
        let dt = Utc.timestamp_opt(run.started_at_secs as i64, 0).unwrap();
        (dt.format("%Y").to_string(), dt.format("%m").to_string())
    };
    let shard = root.join("eval_runs").join(format!("{year}-{month}"));
    std::fs::create_dir_all(&shard).unwrap();
    let path = shard.join(format!("{}.json", run.id));
    let bytes = serde_json::to_vec(run).unwrap();
    std::fs::write(&path, bytes).unwrap();
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

async fn request_bytes(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
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
    (status, bytes.to_vec())
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

fn seed_indexed_trace(
    trace_store: &FileTraceStore,
    id: &str,
    text: &str,
    with_user: bool,
    started_secs: u64,
) {
    use awaken_ext_observability::trace_store::RunSummary;
    trace_store
        .append(
            id,
            &MetricsEvent::Inference(captured_inference_span(id, text, with_user)),
        )
        .unwrap();
    trace_store
        .write_index_for_run(
            id,
            &RunSummary {
                run_id: id.into(),
                agent_id: "default".into(),
                started_at: UNIX_EPOCH + std::time::Duration::from_secs(started_secs),
                ended_at: None,
                prompt_ids: vec![],
                experiment_id: None,
                variant_name: None,
                final_status: None,
                judge_score: None,
            },
        )
        .unwrap();
}

fn prune_all_unreferenced_traces(trace_store: &FileTraceStore) -> u64 {
    trace_store
        .prune(
            UNIX_EPOCH + std::time::Duration::from_secs(4_000_000_000),
            &std::collections::HashSet::new(),
        )
        .unwrap()
}

async fn seed_dataset_record(app: &TestApp, id: &str, spec: DatasetSpec) {
    let record = ConfigRecord {
        spec,
        meta: RecordMeta::new_user(),
    };
    let value = record.to_value().unwrap();
    app.config_store
        .put(DATASETS_NAMESPACE, id, &value)
        .await
        .unwrap();
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

/// `DELETE ?expected_revision=N` is a compare-and-swap. The trace →
/// fixture rollback relies on it: if a concurrent operator appended a
/// fixture between the inline create and the failed curate, the revision
/// has moved and the guarded delete must reject (409) rather than wipe
/// their work. An unguarded delete (no query param) still removes
/// unconditionally.
#[tokio::test]
async fn delete_dataset_guarded_by_expected_revision() {
    let app = build_test_app().await;

    // Inline-created dataset starts at revision 0 (mirrors the rollback
    // path: SaveTraceAsFixtureModal creates an empty dataset, captures
    // its revision, then tries to curate).
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-GUARD", "spec": { "fixtures": [] } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["meta"]["revision"], 0);

    // Simulate the concurrent write that the rollback must not destroy:
    // another operator appends a fixture, bumping the revision to 1.
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-GUARD/fixtures",
        Some(json!({ "fixture": sample_fixture("concurrent"), "expected_revision": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Rollback deletes against the *stale* revision it captured at create
    // time (0). The dataset is now at revision 1 → 409, data preserved.
    let (status, _) = request(
        &app.router,
        "DELETE",
        "/v1/eval/datasets/DS-GUARD?expected_revision=0",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "stale revision must not delete"
    );
    let (status, body) = request(&app.router, "GET", "/v1/eval/datasets/DS-GUARD", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "dataset survived the guarded delete"
    );
    assert_eq!(body["spec"]["fixtures"].as_array().unwrap().len(), 1);

    // Guarded delete against the current revision (1) succeeds.
    let (status, _) = request(
        &app.router,
        "DELETE",
        "/v1/eval/datasets/DS-GUARD?expected_revision=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app.router, "GET", "/v1/eval/datasets/DS-GUARD", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dataset_create_400s_on_duplicate_fixture_id() {
    // Duplicate fixture ids would silently overwrite each other inside
    // the diff map (`BTreeMap` keyed by fixture_id) and produce a result
    // whose meaning depends on Vec ordering. Reject up front.
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-DUPFX",
            "spec": { "fixtures": [sample_fixture("twin"), sample_fixture("twin")] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("duplicate fixture id"),
        "body: {body}"
    );
}

#[tokio::test]
async fn dataset_create_400s_on_invalid_min_judge_score() {
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-BAD-JUDGE-THRESHOLD",
            "spec": {
                "fixtures": [{
                    "id": "bad-threshold",
                    "user_input": "grade this",
                    "provider_script": [
                        {"kind": "chat_response", "content": "ok"}
                    ],
                    "expect": { "min_judge_score": 1.5 }
                }]
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap_or("");
    assert!(err.contains("min_judge_score"), "body: {body}");
    assert!(err.contains("[0.0, 1.0]"), "body: {body}");
}

#[tokio::test]
async fn dataset_put_400s_on_duplicate_fixture_id() {
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-DUPPUT", "spec": { "fixtures": [sample_fixture("a")] } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = request(
        &app.router,
        "PUT",
        "/v1/eval/datasets/DS-DUPPUT",
        Some(json!({
            "expected_revision": 0,
            "spec": { "fixtures": [sample_fixture("twin"), sample_fixture("twin")] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("duplicate fixture id"),
        "body: {body}"
    );
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

fn unsupported_provider_script_span(run_id: &str) -> GenAISpan {
    let mut span = captured_inference_span(run_id, "", true);
    span.finish_reasons = vec!["tool_use".into()];
    span.response_content = None;
    span.response_tool_calls = Some(json!([
        {"id": "call-1", "name": "search", "arguments": {"q": "alpha"}},
        {"id": "call-2", "name": "write", "arguments": {"text": "beta"}}
    ]));
    span
}

fn delegation_span(parent_run_id: &str, child_run_id: &str) -> DelegationSpan {
    DelegationSpan {
        context: SpanContext {
            run_id: parent_run_id.into(),
            agent_id: "default".into(),
            ..Default::default()
        },
        parent_run_id: parent_run_id.into(),
        child_run_id: Some(child_run_id.into()),
        target_agent_id: "researcher".into(),
        tool_call_id: "call-subagent".into(),
        duration_ms: Some(7),
        success: true,
        error_message: None,
        timestamp_ms: 1,
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
        Some(json!({
            "from_run_id": run_id,
            "expected": { "final_answer_contains": ["42"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["spec"]["fixtures"].as_array().unwrap().len(), 1);
    let added = &body["spec"]["fixtures"][0];
    assert_eq!(added["id"], run_id);
    assert_eq!(added["user_input"], "auto prompt");
    assert_eq!(added["source_run_id"], run_id);
    assert_eq!(added["expect"]["final_answer_contains"][0], "42");

    let removed = app
        .trace_store
        .prune(
            UNIX_EPOCH + std::time::Duration::from_secs(4_000_000_000),
            &std::collections::HashSet::new(),
        )
        .unwrap();
    assert_eq!(removed, 0, "curated source trace must be pinned");
    assert!(
        !app.trace_store.read(run_id).unwrap().is_empty(),
        "source trace should survive retention after curation"
    );
}

#[tokio::test]
async fn trace_to_dataset_to_eval_round_trips_with_subagent_trace() {
    let app = build_test_app().await;
    let parent_run_id = "01HXE2E000000000000000001";
    let child_run_id = "01HXE2E000000000000000002";

    app.trace_store
        .append(
            parent_run_id,
            &MetricsEvent::Delegation(delegation_span(parent_run_id, child_run_id)),
        )
        .unwrap();
    app.trace_store
        .append(
            parent_run_id,
            &MetricsEvent::Inference(captured_inference_span(
                parent_run_id,
                "sub-agent found answer 42",
                true,
            )),
        )
        .unwrap();
    app.trace_store
        .append(
            child_run_id,
            &MetricsEvent::Inference(captured_inference_span(
                child_run_id,
                "child research result",
                true,
            )),
        )
        .unwrap();

    let (status, bytes) = request_bytes(
        &app.router,
        "GET",
        &format!("/v1/traces/{parent_run_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let trace_body = String::from_utf8(bytes).unwrap();
    assert!(trace_body.contains("\"type\":\"delegation\""));
    assert!(trace_body.contains(child_run_id));
    assert!(trace_body.contains("sub-agent found answer 42"));

    let (status, bytes) = request_bytes(
        &app.router,
        "GET",
        &format!("/v1/traces/{child_run_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8(bytes)
            .unwrap()
            .contains("child research result")
    );

    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-E2E-SUB", "spec": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, dataset) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-E2E-SUB/items",
        Some(json!({
            "from_run_id": parent_run_id,
            "expected": { "final_answer_contains": ["42"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {dataset}");
    let fixture = &dataset["spec"]["fixtures"][0];
    assert_eq!(fixture["source_run_id"], parent_run_id);
    assert_eq!(fixture["source_model_id"], "claude-opus-4-7");
    assert_eq!(fixture["user_input"], "auto prompt");
    assert_eq!(
        fixture["provider_script"][0]["content"],
        "sub-agent found answer 42"
    );

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-E2E-SUB",
            "mode": "scripted",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let item = &body["run"]["items"][0];
    assert!(item["report"]["passed"].as_bool().unwrap());
    assert_eq!(item["report"]["final_text"], "sub-agent found answer 42");
    assert!(
        item["trace_run_id"].is_string(),
        "eval item should link to replay trace: {item}"
    );
}

#[tokio::test]
async fn curate_items_cas_failure_does_not_pin_trace() {
    let app =
        build_test_app_with_config_store(Arc::new(CasConflictConfigStore::new("DS-CUR-CAS"))).await;
    let run_id = "01HXCUR0000000000000000CAS";
    app.trace_store
        .append(
            run_id,
            &MetricsEvent::Inference(captured_inference_span(run_id, "the answer is 42", true)),
        )
        .unwrap();
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-CUR-CAS", "spec": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-CUR-CAS/items",
        Some(json!({
            "from_run_id": run_id,
            "expected": { "final_answer_contains": ["42"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("revision conflict"),
        "body: {body}"
    );
    assert_eq!(
        prune_all_unreferenced_traces(app.trace_store.as_ref()),
        1,
        "failed dataset CAS must not create trace retention references"
    );
}

#[tokio::test]
async fn parallel_tool_trace_curates_live_only_and_scripted_eval_fails_closed() {
    // The primary curation value for Live eval is the captured user
    // prompt + expectations. `provider_script` is an optional scripted
    // snapshot; when its schema cannot represent the trace, the server
    // must not reject an otherwise useful real-agent fixture.
    let app = build_test_app().await;
    let run_id = "01HXCUR0000000000000000004";
    app.trace_store
        .append(
            run_id,
            &MetricsEvent::Inference(unsupported_provider_script_span(run_id)),
        )
        .unwrap();

    let (status, bytes) =
        request_bytes(&app.router, "GET", &format!("/v1/traces/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let trace_body = String::from_utf8(bytes).unwrap();
    assert!(trace_body.contains("\"name\":\"search\""));
    assert!(trace_body.contains("\"name\":\"write\""));

    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-CUR-LIVE", "spec": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-CUR-LIVE/items",
        Some(json!({
            "from_run_id": run_id,
            "expected": { "final_answer_contains": ["answer"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let added = &body["spec"]["fixtures"][0];
    assert_eq!(added["user_input"], "auto prompt");
    assert!(added["provider_script"].is_null());
    assert!(
        added["provider_script_error"]
            .as_str()
            .unwrap_or("")
            .contains("provider_script currently supports one tool call"),
        "fixture: {added}"
    );

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-CUR-LIVE",
            "mode": "scripted",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let report = &body["run"]["items"][0]["report"];
    assert!(!report["passed"].as_bool().unwrap());
    assert_eq!(report["runtime_failure"]["kind"], "runtime_error");
    assert!(
        report["runtime_failure"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("no replayable provider_script"),
        "report: {report}"
    );
}

#[tokio::test]
async fn curate_items_require_mode_rejects_unsupported_provider_script() {
    let app = build_test_app().await;
    let run_id = "01HXCUR0000000000000000005";
    app.trace_store
        .append(
            run_id,
            &MetricsEvent::Inference(unsupported_provider_script_span(run_id)),
        )
        .unwrap();

    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-CUR-REQ", "spec": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-CUR-REQ/items",
        Some(json!({
            "from_run_id": run_id,
            "provider_script_mode": "require",
            "expected": { "final_answer_contains": ["answer"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("provider_script currently supports one tool call"),
        "body: {body}"
    );
}

#[tokio::test]
async fn curate_items_400s_on_empty_expected() {
    let app = build_test_app().await;
    let run_id = "01HXCUR0000000000000000003";
    app.trace_store
        .append(
            run_id,
            &MetricsEvent::Inference(captured_inference_span(run_id, "ok", true)),
        )
        .unwrap();

    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-CUR3", "spec": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-CUR3/items",
        Some(json!({ "from_run_id": run_id, "expected": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("at least one expectation"),
        "body: {body}"
    );
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
        Some(json!({
            "from_run_id": run_id,
            "expected": { "final_answer_contains": ["ok"] },
        })),
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
    assert_eq!(run["execution_mode"], "scripted");
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
async fn start_eval_run_accepts_explicit_scripted_mode() {
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-RUN-SCRIPTED",
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
            "dataset_id": "DS-RUN-SCRIPTED",
            "mode": "scripted",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["run"]["execution_mode"], "scripted");
    assert_eq!(body["run"]["items"].as_array().unwrap().len(), 1);
    assert!(body["run"]["items"][0]["cell"].is_null());
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
async fn get_eval_run_diff_keys_cell_less_samples_by_sample_index() {
    let app = build_test_app().await;
    let store = app.eval_run_store.clone();

    let mut baseline = baseline_run("BASE-SAMPLES");
    baseline.items = vec![
        {
            let mut it = item("alpha", true, "same");
            it.sample_index = Some(0);
            it
        },
        {
            let mut it = item("alpha", true, "old");
            it.sample_index = Some(1);
            it
        },
    ];
    let mut new = baseline_run("NEW-SAMPLES");
    new.items = vec![
        {
            let mut it = item("alpha", true, "same");
            it.sample_index = Some(0);
            it
        },
        {
            let mut it = item("alpha", false, "bad");
            it.sample_index = Some(1);
            it
        },
    ];
    store.write(&baseline).unwrap();
    store.write(&new).unwrap();

    let (status, body) = request(
        &app.router,
        "GET",
        "/v1/eval/runs/NEW-SAMPLES?baseline=BASE-SAMPLES",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let entries = body["diff"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "body: {body}");
    assert!(
        entries
            .iter()
            .any(|e| e["sample_index"] == 0 && e["kind"] == "unchanged"),
        "sample 0 should pair independently: {body}"
    );
    assert!(
        entries
            .iter()
            .any(|e| e["sample_index"] == 1 && e["kind"] == "regression"),
        "sample 1 should pair independently: {body}"
    );
}

#[tokio::test]
async fn get_eval_run_diff_400s_on_sample_count_mismatch() {
    let app = build_test_app().await;
    let store = app.eval_run_store.clone();
    let cell = MatrixCell {
        model_id: Some("m1".into()),
    };

    let mut baseline = baseline_run("BASE-SAMPLE-DIFF");
    baseline.execution_mode = EvalRunExecutionMode::Live;
    baseline.items[0].cell = Some(cell.clone());

    let mut new = baseline_run("NEW-SAMPLE-DIFF");
    new.execution_mode = EvalRunExecutionMode::Live;
    new.items = (0..2)
        .map(|sample| {
            let mut it = item("alpha", true, &format!("sample {sample}"));
            it.cell = Some(cell.clone());
            it.sample_index = Some(sample);
            it
        })
        .collect();

    store.write(&baseline).unwrap();
    store.write(&new).unwrap();
    let (status, body) = request(
        &app.router,
        "GET",
        "/v1/eval/runs/NEW-SAMPLE-DIFF?baseline=BASE-SAMPLE-DIFF",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("different sample counts"),
        "body: {body}"
    );
}

#[tokio::test]
async fn get_eval_run_baseline_400s_on_adhoc_run() {
    // Ad-hoc online runs all carry dataset_id="_adhoc" + revision 0.
    // Without an explicit guard, two unrelated _adhoc runs would pass
    // the dataset-revision schema check and produce a meaningless diff.
    let app = build_test_app().await;
    let mut adhoc_a = baseline_run("ADHOC-A");
    let mut adhoc_b = baseline_run("ADHOC-B");
    adhoc_a.dataset_id = "_adhoc".into();
    adhoc_a.dataset_revision = 0;
    adhoc_b.dataset_id = "_adhoc".into();
    adhoc_b.dataset_revision = 0;
    app.eval_run_store.write(&adhoc_a).unwrap();
    app.eval_run_store.write(&adhoc_b).unwrap();
    let (status, body) = request(
        &app.router,
        "GET",
        "/v1/eval/runs/ADHOC-B?baseline=ADHOC-A",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("ad-hoc"),
        "body: {body}"
    );
}

#[tokio::test]
async fn get_eval_run_dirty_historical_run_500s_without_diff_context() {
    // Reading an already-corrupt stored run directly is a store
    // corruption signal, not a bad diff selection. The diff routes map
    // the same duplicate-key shape to 400 because the caller selected a
    // non-diffable current/baseline pair; a plain GET has no such
    // request context and should stay fail-loud as 500.
    let app = build_test_app().await;
    let mut dirty = baseline_run("DIRTY-NODIFF");
    let dup = dirty.items[0].clone();
    dirty.items.push(dup);
    seed_corrupt_eval_run(&app.eval_run_root, &dirty);

    let (status, body) = request(&app.router, "GET", "/v1/eval/runs/DIRTY-NODIFF", None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("duplicate eval-run item key"),
        "body: {body}"
    );
}

#[tokio::test]
async fn get_eval_run_diff_400s_when_selected_current_has_duplicate_item_keys() {
    // Diff must refuse to silently collapse duplicate (fixture_id, cell,
    // sample_index) keys via the BTreeMap pairing. A run that managed
    // to land two items with the same key (e.g. a future store impl
    // with weaker write-once guarantees) should surface a structured
    // error from /v1/eval/runs/:id?baseline=, not produce an
    // order-dependent diff.
    let app = build_test_app().await;
    let mut baseline = baseline_run("BASE-DUP");
    let mut newer = baseline_run("NEW-DUP");
    // Inject a duplicate item into the new run.
    let dup = newer.items[0].clone();
    newer.items.push(dup);
    baseline.dataset_id = newer.dataset_id.clone();
    baseline.dataset_revision = newer.dataset_revision;
    app.eval_run_store.write(&baseline).unwrap();
    // `FileEvalRunStore::write` now rejects duplicate-key runs at the
    // store boundary, so the only way to stage the corrupt-newer
    // scenario this test exercises is to drop the JSON in by hand —
    // exactly the "future store impl with weaker guarantees" the diff
    // guard is supposed to catch.
    seed_corrupt_eval_run(&app.eval_run_root, &newer);
    let (status, body) = request(
        &app.router,
        "GET",
        "/v1/eval/runs/NEW-DUP?baseline=BASE-DUP",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("duplicate"),
        "body: {body}"
    );
}

#[tokio::test]
async fn get_eval_run_diff_400s_when_selected_baseline_has_duplicate_item_keys() {
    // Symmetric to the corrupt-current case above: selecting a baseline
    // that cannot be diffed is a bad diff request, not an internal
    // failure. The normal store write path rejects this shape, so seed
    // it directly to model already-corrupt on-disk state.
    let app = build_test_app().await;
    let mut baseline = baseline_run("BASE-DUP");
    let dup = baseline.items[0].clone();
    baseline.items.push(dup);
    let newer = new_run_with_drift("NEW-GOOD");
    app.eval_run_store.write(&newer).unwrap();
    seed_corrupt_eval_run(&app.eval_run_root, &baseline);

    let (status, body) = request(
        &app.router,
        "GET",
        "/v1/eval/runs/NEW-GOOD?baseline=BASE-DUP",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("duplicate eval-run item key"),
        "body: {body}"
    );
}

#[tokio::test]
async fn get_eval_run_baseline_400s_on_dataset_id_mismatch() {
    // Baseline run is for DS-DIFF; new run is for DS-OTHER. The
    // diff is meaningless across dataset schemas, so the route must
    // reject rather than silently produce a misleading diff on
    // coincidentally-matching fixture ids.
    let app = build_test_app().await;
    let baseline = baseline_run("BASE-X");
    let mut other = baseline_run("NEW-X");
    other.dataset_id = "DS-OTHER".into();
    app.eval_run_store.write(&baseline).unwrap();
    app.eval_run_store.write(&other).unwrap();

    let (status, body) = request(
        &app.router,
        "GET",
        "/v1/eval/runs/NEW-X?baseline=BASE-X",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("across datasets"),
        "body: {body}"
    );
}

#[tokio::test]
async fn get_eval_run_baseline_400s_on_dataset_revision_mismatch() {
    // Same dataset_id, different revision — the fixture set behind
    // each revision may differ; reject rather than silently diff.
    let app = build_test_app().await;
    let baseline = baseline_run("BASE-R");
    let mut newer = baseline_run("NEW-R");
    newer.dataset_revision = 2;
    app.eval_run_store.write(&baseline).unwrap();
    app.eval_run_store.write(&newer).unwrap();

    let (status, body) = request(
        &app.router,
        "GET",
        "/v1/eval/runs/NEW-R?baseline=BASE-R",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("dataset revisions"),
        "body: {body}"
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
async fn start_eval_run_revalidates_dataset_fixture_ids_before_model_lookup() {
    // Directly seed a corrupt historical dataset: normal dataset CRUD would
    // reject duplicate fixture ids. start_eval_run must catch it before model
    // lookup/provider calls, so the response is a dataset 400 rather than a
    // missing-model 404 after partial preflight.
    let app = build_test_app().await;
    seed_dataset_record(
        &app,
        "DS-CORRUPT-DUP-FX",
        DatasetSpec {
            description: String::new(),
            fixtures: vec![sample_fixture("dup"), sample_fixture("dup")],
        },
    )
    .await;

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-CORRUPT-DUP-FX",
            "mode": "live",
            "models": ["missing-model"]
        })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("duplicate fixture id"),
        "body: {body}"
    );
    assert!(
        app.eval_run_store
            .list(&awaken_eval::EvalRunFilter::default())
            .unwrap()
            .is_empty(),
        "dirty dataset preflight must not persist a run"
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
            .contains("expands to 150 units"),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_on_zero_walltime() {
    // `Some(0)` is rejected explicitly (mirrors /v1/eval/online) so the
    // operator notices the typo instead of silently inheriting the 60s
    // default. Omitting the field still takes the default.
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-WALLTIME",
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
            "dataset_id": "DS-WALLTIME",
            "models": ["m1"],
            "max_walltime_secs": 0,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("max_walltime_secs"),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_when_scripted_sets_walltime() {
    // The field is Live-only. Accepting a non-zero value on scripted
    // mode would be a silent no-op, which makes API behaviour harder to
    // reason about than rejecting the misconfiguration.
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-SCRIPTED-WALLTIME",
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
            "dataset_id": "DS-SCRIPTED-WALLTIME",
            "mode": "scripted",
            "max_walltime_secs": 10,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("requires mode=\"live\""),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_when_scripted_sets_token_budget() {
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-SCRIPTED-TOKENS",
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
            "dataset_id": "DS-SCRIPTED-TOKENS",
            "mode": "scripted",
            "max_total_tokens": 10,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("max_total_tokens requires mode=\"live\""),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_on_zero_samples() {
    // `samples: 0` is rejected explicitly instead of silently coerced to
    // 1 — the operator who typed 0 most likely meant "off" (omit) or a
    // real number, and coercing hides the typo.
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-SAMPLES",
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
            "dataset_id": "DS-SAMPLES",
            "models": ["m1"],
            "samples": 0,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("samples"),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_when_scripted_passes_samples() {
    // `samples` is a Live-only request field (documented on
    // `StartRunRequest.samples` and listed in the PR summary). Scripted
    // replays are deterministic, so an explicit value is misconfiguration
    // and gets rejected before the numeric cap check. Omitting `samples`
    // still works in both modes.
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-SAMPLES-SCRIPTED",
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
            "dataset_id": "DS-SAMPLES-SCRIPTED",
            "mode": "scripted",
            "samples": 999999,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("samples requires mode=\"live\""),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_baseline_validated_before_replay() {
    // Bad baseline_run_id must fail BEFORE any provider call or run
    // persist. The whole point is symmetry with the persist+no-store
    // guard: typo / wrong-dataset / wrong-revision baselines should not
    // silently burn tokens and leave a polluting half-finished run in
    // the store.
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-PREFLIGHT",
            "spec": { "fixtures": [sample_fixture("alpha")] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Baseline that points at a non-existent run.
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-PREFLIGHT",
            "baseline_run_id": "nonexistent",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("baseline eval run not found"),
        "body: {body}"
    );
    // Store stays empty — the missing-baseline check ran before any
    // replay or persist.
    assert_eq!(
        app.eval_run_store.list(&Default::default()).unwrap().len(),
        0
    );
}

#[tokio::test]
async fn start_eval_run_shape_errors_surface_before_baseline_check() {
    // Regression: when a request is BOTH shape-malformed (e.g.
    // `mode=scripted` with a `models` axis) AND references a bad
    // baseline, the response should report the shape error — the caller
    // needs to fix the request itself before the baseline matters. An
    // earlier ordering ran the baseline preflight first, so the caller
    // saw "baseline not found" while the real bug was the body.
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-PRIORITY",
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
            "dataset_id": "DS-PRIORITY",
            "mode": "scripted",
            "models": ["any-model"],
            "baseline_run_id": "nonexistent",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("`models` is only valid with mode=\"live\""),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_baseline_rejects_wrong_dataset_before_replay() {
    // Baseline that points at a real run, but a different dataset, must
    // fail upfront — never persist the new run before learning the diff
    // request is malformed.
    let app = build_test_app().await;
    let other_baseline = EvalRun {
        id: "WRONG-DS".into(),
        dataset_id: "different-dataset".into(),
        dataset_revision: 0,
        execution_mode: EvalRunExecutionMode::Scripted,
        items: vec![],
        started_at_secs: 1_700_000_000,
        ended_at_secs: 1_700_000_001,
    };
    app.eval_run_store.write(&other_baseline).unwrap();

    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-MISMATCH",
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
            "dataset_id": "DS-MISMATCH",
            "baseline_run_id": "WRONG-DS",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("across datasets"),
        "body: {body}"
    );
    // Pre-existing baseline is the only run in the store.
    let runs = app.eval_run_store.list(&Default::default()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, "WRONG-DS");
}

#[tokio::test]
async fn start_eval_run_baseline_rejects_execution_mode_mismatch_before_replay() {
    // Scripted and Live runs have different semantics even when they
    // point at the same dataset revision: scripted measures replay
    // determinism; Live measures real provider/agent behaviour. Diffing
    // them would be a misleading apples-to-oranges regression gate.
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-MODE-MISMATCH",
            "spec": { "fixtures": [sample_fixture("alpha")] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let live_baseline = EvalRun {
        id: "LIVE-BASE".into(),
        dataset_id: "DS-MODE-MISMATCH".into(),
        dataset_revision: 0,
        execution_mode: EvalRunExecutionMode::Live,
        items: vec![],
        started_at_secs: 1_700_000_000,
        ended_at_secs: 1_700_000_001,
    };
    app.eval_run_store.write(&live_baseline).unwrap();

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-MODE-MISMATCH",
            "mode": "scripted",
            "baseline_run_id": "LIVE-BASE",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("execution modes"),
        "body: {body}"
    );

    let runs = app.eval_run_store.list(&Default::default()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, "LIVE-BASE");
    assert_eq!(runs[0].execution_mode, EvalRunExecutionMode::Live);
}

#[tokio::test]
async fn start_eval_run_baseline_with_duplicate_item_keys_rejected_before_replay() {
    // Regression: a baseline whose items collide on
    // (fixture_id, cell, sample_index) used to slip past the preflight
    // and only fail inside `compute_diff_from_baseline` — i.e. AFTER
    // live replay had burned provider tokens and the new run had been
    // persisted to the store. `load_and_validate_baseline` now runs the
    // duplicate-key check up front so the request fails fast and the
    // store stays untouched.
    let app = build_test_app().await;

    // Register a dataset (dataset_revision=0) the baseline can pair with.
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-DUP-BASE",
            "spec": { "fixtures": [sample_fixture("alpha")] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Hand-craft a baseline with two items sharing the same key. The
    // store-write paths now reject duplicate keys (see
    // `file_store_rejects_duplicate_item_keys`) so we have to drop
    // the JSON file in by hand to simulate a pre-existing corrupt
    // on-disk record — exactly the case the preflight guard exists for.
    let dup_baseline = EvalRun {
        id: "DUP-BASE".into(),
        dataset_id: "DS-DUP-BASE".into(),
        dataset_revision: 0,
        execution_mode: EvalRunExecutionMode::Scripted,
        items: vec![item("alpha", true, "first"), item("alpha", true, "second")],
        started_at_secs: 1_700_000_000,
        ended_at_secs: 1_700_000_001,
    };
    seed_corrupt_eval_run(&app.eval_run_root, &dup_baseline);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-DUP-BASE",
            "baseline_run_id": "DUP-BASE",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("duplicate eval-run item key"),
        "body: {body}"
    );
    // No valid run was persisted before the diff bailed. The
    // pre-existing duplicate-key baseline is still on disk, but
    // FileEvalRunStore::list deliberately skips historical dirty runs
    // so list endpoints don't expose invalid eval data.
    let runs = app.eval_run_store.list(&Default::default()).unwrap();
    assert!(runs.is_empty());
    assert!(matches!(
        app.eval_run_store.read("DUP-BASE").unwrap_err(),
        awaken_eval::EvalRunStoreError::DuplicateItemKeys(_, _)
    ));
}

#[tokio::test]
async fn start_eval_run_baseline_rejects_sample_count_mismatch_before_replay() {
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-SAMPLE-MISMATCH",
            "spec": { "fixtures": [sample_fixture("alpha")] }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let mut one_sample_baseline = EvalRun {
        id: "BASE-SAMPLE-1".into(),
        dataset_id: "DS-SAMPLE-MISMATCH".into(),
        dataset_revision: 0,
        execution_mode: EvalRunExecutionMode::Live,
        items: vec![item("alpha", true, "baseline")],
        started_at_secs: 1_700_000_000,
        ended_at_secs: 1_700_000_001,
    };
    one_sample_baseline.items[0].cell = Some(MatrixCell {
        model_id: Some("missing-model".into()),
    });
    app.eval_run_store.write(&one_sample_baseline).unwrap();

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-SAMPLE-MISMATCH",
            "mode": "live",
            "models": ["missing-model"],
            "samples": 2,
            "baseline_run_id": "BASE-SAMPLE-1"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("different sample counts"),
        "body: {body}"
    );
    let runs = app.eval_run_store.list(&Default::default()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, "BASE-SAMPLE-1");
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

#[tokio::test]
async fn online_eval_503s_on_persist_without_run_store() {
    // Persist=true with no EvalRunStore wired must fail BEFORE any
    // model resolution / provider call — otherwise we burn tokens on a
    // request the persistence layer is known to reject.
    let app = build_test_app_without_run_store().await;
    let (status, body) = request(
        &app,
        "POST",
        "/v1/eval/online",
        Some(json!({
            "user_input": "test",
            "models": ["missing-model"],
            "persist": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("EvalRunStore is required"),
        "body: {body}"
    );
}

#[tokio::test]
async fn online_eval_400s_on_zero_walltime() {
    // max_walltime_secs=0 would time out every cell immediately — the
    // request body accepts it but the handler must reject up front, not
    // race the timeout against the first scheduled task.
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({
            "user_input": "test",
            "models": ["missing-model"],
            "max_walltime_secs": 0,
            "persist": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("max_walltime_secs"),
        "body: {body}"
    );
}

#[tokio::test]
async fn online_eval_400s_on_zero_samples() {
    // `samples: 0` would silently coerce to 1 under `unwrap_or(1).max(1)`
    // — the explicit Some(0) guard rejects it instead so the operator
    // notices the typo. Mirrors /v1/eval/runs same-named guard.
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({
            "user_input": "test",
            "models": ["missing-model"],
            "samples": 0,
            "persist": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("samples"),
        "body: {body}"
    );
}

#[tokio::test]
async fn online_eval_404s_on_unknown_agent_id() {
    // `agent_id` resolution runs BEFORE per-cell model resolution so a
    // typo'd agent surfaces a 404 immediately, with the missing id in
    // the body — operators don't get an opaque 500 after token spend.
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({
            "user_input": "test",
            "models": ["missing-model"],
            "agent_id": "missing-agent",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("missing-agent"),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_404s_on_unknown_agent_id() {
    // Same wiring on the dataset run path — agent lookup runs before
    // model resolution so a typo'd agent fails before the matrix even
    // starts.
    let app = build_test_app().await;
    let fixtures = vec![sample_fixture("f1")];
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-AGT", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-AGT",
            "models": ["missing-model"],
            "agent_id": "missing-agent",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("missing-agent"),
        "body: {body}"
    );
}

// ── Flakiness sampling (samples=N per cell) — validation paths ───────────

#[tokio::test]
async fn start_eval_run_400s_when_samples_above_cap() {
    let app = build_test_app().await;
    let fixtures = vec![sample_fixture("f1")];
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-S", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-S",
            "models": ["m1"],
            "samples": 50,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("samples=50"),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_when_samples_without_models() {
    let app = build_test_app().await;
    let fixtures = vec![sample_fixture("f1")];
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-S2", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-S2",
            "samples": 3,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("deterministic"),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_on_duplicate_models() {
    // Duplicate model ids would spawn the same matrix cell twice and
    // generate duplicate (fixture_id, cell, sample_index) keys that
    // diff_eval_items would silently collapse. Reject at the entry point.
    let app = build_test_app().await;
    let fixtures = vec![sample_fixture("f1")];
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-DUPM", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-DUPM",
            "models": ["m1", "m1"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("duplicate model"),
        "body: {body}"
    );
}

#[tokio::test]
async fn online_eval_400s_on_duplicate_models() {
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({
            "user_input": "p",
            "models": ["m1", "m1"],
            "persist": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("duplicate model"),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_when_scripted_with_agent_id() {
    // agent_id only makes sense in Live (matrix) mode — scripted runs
    // use the fixture's provider_script + a fixed stub agent. Rather
    // than silently ignore agent_id on a scripted request, reject it
    // so the operator isn't misled.
    let app = build_test_app().await;
    let fixtures = vec![sample_fixture("f1")];
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-SAID", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-SAID",
            "agent_id": "some-agent",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("agent_id requires mode=\"live\""),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_when_live_mode_omits_models() {
    let app = build_test_app().await;
    let fixtures = vec![sample_fixture("f1")];
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-LIVE-NOMODELS", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({ "dataset_id": "DS-LIVE-NOMODELS", "mode": "live" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("mode=\"live\" requires"),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_when_scripted_mode_has_models() {
    let app = build_test_app().await;
    let fixtures = vec![sample_fixture("f1")];
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-SCRIPTED-MODELS", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-SCRIPTED-MODELS",
            "mode": "scripted",
            "models": ["m1"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("only valid with mode=\"live\""),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_when_models_supplied_but_empty() {
    // `models: []` would otherwise pass `body.models.is_some()`, expand
    // into a 1-cell default with `model_id: None`, and panic inside
    // `run_matrix_cells` on the "matrix expansion always sets model_id"
    // expect. Reject the empty array up front.
    let app = build_test_app().await;
    let fixtures = vec![sample_fixture("f1")];
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-EM", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({ "dataset_id": "DS-EM", "models": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("non-empty"),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_when_samples_blow_total_units() {
    // 25 fixtures × 2 models × 3 samples = 150 > MAX_CELLS_PER_SYNC_RUN (100).
    let app = build_test_app().await;
    let fixtures: Vec<_> = (0..25).map(|i| sample_fixture(&format!("f{i}"))).collect();
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-S3", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-S3",
            "models": ["m1", "m2"],
            "samples": 3,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("150 units"),
        "body: {body}"
    );
}

#[tokio::test]
async fn online_eval_400s_on_samples_above_cap() {
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({ "user_input": "test", "models": ["m"], "samples": 50 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("samples=50"),
        "body: {body}"
    );
}

#[tokio::test]
async fn online_eval_400s_when_total_units_blow_cap() {
    // 4 models × 3 samples = 12 > MAX_CELLS_PER_SYNC_ONLINE (10).
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({
            "user_input": "test",
            "models": ["m1", "m2", "m3", "m4"],
            "samples": 3,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("12 units"),
        "body: {body}"
    );
}

// ── LLM-as-judge — validation paths ──────────────────────────────────────

#[tokio::test]
async fn start_eval_run_400s_when_judge_without_models() {
    let app = build_test_app().await;
    let fixtures = vec![sample_fixture("f1")];
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-J", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-J",
            "judge": { "model_id": "some-judge" },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("judge"),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_when_min_judge_score_has_no_live_judge() {
    let app = build_test_app().await;
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-JUDGE-REQ",
            "spec": {
                "fixtures": [{
                    "id": "needs-judge",
                    "user_input": "grade this qualitatively",
                    "provider_script": [
                        {"kind": "chat_response", "content": "ok"}
                    ],
                    "expect": { "min_judge_score": 0.7 }
                }]
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({ "dataset_id": "DS-JUDGE-REQ" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("mode=\"live\""),
        "body: {body}"
    );

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-JUDGE-REQ",
            "mode": "live",
            "models": ["missing-model"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("provide `judge`"),
        "body: {body}"
    );

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-JUDGE-REQ",
            "mode": "live",
            "models": ["missing-model"],
            "judge": { "model_id": "missing-judge" },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("judge.rubric"),
        "body: {body}"
    );

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-JUDGE-REQ",
            "mode": "live",
            "models": ["missing-model"],
            "judge": { "model_id": "missing-judge", "rubric": "   " },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("judge.rubric"),
        "body: {body}"
    );
}

#[tokio::test]
async fn start_eval_run_400s_on_historical_invalid_min_judge_score() {
    let app = build_test_app().await;
    let mut fixture = sample_fixture("bad-threshold");
    fixture.expect.min_judge_score = Some(-0.2);
    seed_dataset_record(
        &app,
        "DS-CORRUPT-JUDGE-THRESHOLD",
        DatasetSpec {
            description: String::new(),
            fixtures: vec![fixture],
        },
    )
    .await;

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-CORRUPT-JUDGE-THRESHOLD",
            "mode": "live",
            "models": ["missing-model"],
            "judge": { "model_id": "missing-judge", "rubric": "grade correctness" },
        })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap_or("");
    assert!(err.contains("min_judge_score"), "body: {body}");
    assert!(err.contains("[0.0, 1.0]"), "body: {body}");
}

#[tokio::test]
async fn start_eval_run_404s_on_unknown_judge_model() {
    let app = build_test_app().await;
    let fixtures = vec![sample_fixture("f1")];
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-J2", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-J2",
            "models": ["replay-model"],
            "judge": { "model_id": "missing-judge" },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("missing-judge"),
        "body: {body}"
    );
}

#[tokio::test]
async fn online_eval_400s_when_min_judge_score_has_no_judge() {
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({
            "user_input": "test",
            "models": ["missing-model"],
            "persist": false,
            "expectations": { "min_judge_score": 0.8 },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("provide `judge`"),
        "body: {body}"
    );

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({
            "user_input": "test",
            "models": ["missing-model"],
            "persist": false,
            "expectations": { "min_judge_score": 0.8 },
            "judge": { "model_id": "missing-judge" },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("judge.rubric"),
        "body: {body}"
    );
}

#[tokio::test]
async fn online_eval_400s_on_invalid_min_judge_score() {
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({
            "user_input": "test",
            "models": ["missing-model"],
            "persist": false,
            "expectations": { "min_judge_score": 1.2 },
            "judge": { "model_id": "missing-judge", "rubric": "grade correctness" },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap_or("");
    assert!(err.contains("min_judge_score"), "body: {body}");
    assert!(err.contains("[0.0, 1.0]"), "body: {body}");
}

#[tokio::test]
async fn online_eval_404s_on_unknown_judge_model() {
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({
            "user_input": "test",
            "models": ["m"],
            "judge": { "model_id": "missing-judge" },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let err = body["error"].as_str().unwrap_or("");
    assert!(
        err.contains("missing-judge") || err.contains("m"),
        "body: {body}"
    );
}

// ── Import from prod traces (POST /v1/eval/datasets/:id/import-traces) ──

#[tokio::test]
async fn import_traces_appends_curatable_traces_and_skips_existing() {
    let app = build_test_app().await;
    // Seed two traces with content capture + write indices so list()
    // returns them.
    use awaken_ext_observability::trace_store::RunSummary;
    use std::time::{Duration, UNIX_EPOCH};
    for (id, started) in [
        ("01HXIMP0000000000000000001", 1_700_000_100),
        ("01HXIMP0000000000000000002", 1_700_000_200),
    ] {
        app.trace_store
            .append(
                id,
                &MetricsEvent::Inference(captured_inference_span(id, "ok", true)),
            )
            .unwrap();
        let summary = RunSummary {
            run_id: id.into(),
            agent_id: "default".into(),
            started_at: UNIX_EPOCH + Duration::from_secs(started),
            ended_at: None,
            prompt_ids: vec![],
            experiment_id: None,
            variant_name: None,
            final_status: None,
            judge_score: None,
        };
        app.trace_store.write_index_for_run(id, &summary).unwrap();
    }

    // Empty dataset to receive imported fixtures.
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-IMP", "spec": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let rev = body["meta"]["revision"].as_u64().unwrap();

    // First import — two new fixtures land.
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-IMP/import-traces",
        Some(json!({
            "expected_revision": rev,
            "expected": { "final_answer_contains": ["ok"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["imported_count"], 2);
    assert_eq!(body["skipped_count"], 0);
    let new_rev = body["dataset_revision"].as_u64().unwrap();
    let (_, dataset) = request(&app.router, "GET", "/v1/eval/datasets/DS-IMP", None).await;
    let fixtures = dataset["spec"]["fixtures"].as_array().unwrap();
    assert_eq!(fixtures.len(), 2);
    assert_eq!(fixtures[0]["expect"]["final_answer_contains"][0], "ok");

    let removed = app
        .trace_store
        .prune(
            UNIX_EPOCH + Duration::from_secs(4_000_000_000),
            &std::collections::HashSet::new(),
        )
        .unwrap();
    assert_eq!(removed, 0, "imported source traces must be pinned");

    // Second import with same traces — all skipped (no clobber), no
    // revision bump.
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-IMP/import-traces",
        Some(json!({
            "expected_revision": new_rev,
            "expected": { "final_answer_contains": ["ok"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["imported_count"], 0);
    assert_eq!(body["skipped_count"], 2);
    assert_eq!(body["dataset_revision"], new_rev);
}

#[tokio::test]
async fn import_traces_imports_live_only_fixture_when_provider_script_is_unsupported() {
    let app = build_test_app().await;
    use awaken_ext_observability::trace_store::RunSummary;
    use std::time::{Duration, UNIX_EPOCH};

    let id = "01HXIMP0000000000000000003";
    app.trace_store
        .append(
            id,
            &MetricsEvent::Inference(unsupported_provider_script_span(id)),
        )
        .unwrap();
    app.trace_store
        .write_index_for_run(
            id,
            &RunSummary {
                run_id: id.into(),
                agent_id: "default".into(),
                started_at: UNIX_EPOCH + Duration::from_secs(1_700_000_250),
                ended_at: None,
                prompt_ids: vec![],
                experiment_id: None,
                variant_name: None,
                final_status: None,
                judge_score: None,
            },
        )
        .unwrap();

    let (_, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-IMP-LIVE", "spec": {} })),
    )
    .await;
    let rev = body["meta"]["revision"].as_u64().unwrap();

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-IMP-LIVE/import-traces",
        Some(json!({
            "expected_revision": rev,
            "expected": { "final_answer_contains": ["answer"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["imported_count"], 1);

    let (_, dataset) = request(&app.router, "GET", "/v1/eval/datasets/DS-IMP-LIVE", None).await;
    let fixture = &dataset["spec"]["fixtures"][0];
    assert_eq!(fixture["user_input"], "auto prompt");
    assert!(fixture["provider_script_error"].is_string());
    assert!(fixture["provider_script"].is_null());
}

#[tokio::test]
async fn import_traces_409s_on_stale_revision() {
    let app = build_test_app().await;
    let (_, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-IMP2", "spec": {} })),
    )
    .await;
    let rev = body["meta"]["revision"].as_u64().unwrap();
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-IMP2/import-traces",
        Some(json!({
            "expected_revision": rev + 99,
            "expected": { "final_answer_contains": ["ok"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("revision conflict"),
        "body: {body}"
    );
}

#[tokio::test]
async fn import_traces_cas_failure_does_not_pin_trace() {
    let app =
        build_test_app_with_config_store(Arc::new(CasConflictConfigStore::new("DS-IMP-CAS"))).await;
    let run_id = "01HXIMP0000000000000000CAS";
    seed_indexed_trace(
        app.trace_store.as_ref(),
        run_id,
        "the answer is 42",
        true,
        1_700_000_400,
    );
    let (_, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-IMP-CAS", "spec": {} })),
    )
    .await;
    let rev = body["meta"]["revision"].as_u64().unwrap();

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-IMP-CAS/import-traces",
        Some(json!({
            "expected_revision": rev,
            "expected": { "final_answer_contains": ["42"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("revision conflict"),
        "body: {body}"
    );
    assert_eq!(
        prune_all_unreferenced_traces(app.trace_store.as_ref()),
        1,
        "failed dataset CAS must not create trace retention references"
    );
}

#[tokio::test]
async fn import_traces_400s_when_trace_lacks_user_and_skip_disabled() {
    let app = build_test_app().await;
    use awaken_ext_observability::trace_store::RunSummary;
    use std::time::{Duration, UNIX_EPOCH};
    let id = "01HXIMP0000000000000000099";
    app.trace_store
        .append(
            id,
            &MetricsEvent::Inference(captured_inference_span(id, "ok", false)),
        )
        .unwrap();
    let summary = RunSummary {
        run_id: id.into(),
        agent_id: "default".into(),
        started_at: UNIX_EPOCH + Duration::from_secs(1_700_000_300),
        ended_at: None,
        prompt_ids: vec![],
        experiment_id: None,
        variant_name: None,
        final_status: None,
        judge_score: None,
    };
    app.trace_store.write_index_for_run(id, &summary).unwrap();

    let (_, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-IMP3", "spec": {} })),
    )
    .await;
    let rev = body["meta"]["revision"].as_u64().unwrap();

    // Default (skip_uncuratable=false) surfaces the missing user_input as 400.
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-IMP3/import-traces",
        Some(json!({
            "expected_revision": rev,
            "expected": { "final_answer_contains": ["ok"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("request_messages"),
        "body: {body}"
    );

    // With skip flag set, the same call returns 200 / imported=0.
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-IMP3/import-traces",
        Some(json!({
            "expected_revision": rev,
            "skip_uncuratable": true,
            "expected": { "final_answer_contains": ["ok"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["imported_count"], 0);
    assert_eq!(body["skipped_count"], 1);
}

// ── pass@k / pass^k aggregation (?aggregate=samples) ────────────────────

#[tokio::test]
async fn get_run_with_aggregate_samples_returns_pass_at_k_rollup() {
    let app = build_test_app().await;
    // 3 items for the same (fixture, cell) — 2 pass + 1 fail.
    let mut run = baseline_run("AGG-R");
    run.execution_mode = EvalRunExecutionMode::Live;
    run.items.clear();
    for (i, passed) in [(0u32, true), (1u32, false), (2u32, true)] {
        let mut report = item("alpha", passed, "x").report;
        report.passed = passed;
        run.items.push(EvalRunItem {
            fixture_id: "alpha".into(),
            cell: Some(awaken_eval::MatrixCell {
                model_id: Some("m1".into()),
            }),
            report,
            trace_run_id: None,
            sample_index: Some(i),
        });
    }
    app.eval_run_store.write(&run).unwrap();
    let (status, body) = request(
        &app.router,
        "GET",
        "/v1/eval/runs/AGG-R?aggregate=samples",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let aggs = body["aggregates"].as_array().unwrap();
    assert_eq!(aggs.len(), 1);
    let g = &aggs[0];
    assert_eq!(g["samples"], 3);
    assert_eq!(g["passed"], 2);
    assert_eq!(g["pass_at_k"], true);
    assert_eq!(g["pass_pow_k"], false);
}

#[tokio::test]
async fn get_run_default_omits_aggregates() {
    let app = build_test_app().await;
    let run = baseline_run("AGG-R2");
    app.eval_run_store.write(&run).unwrap();
    let (status, body) = request(&app.router, "GET", "/v1/eval/runs/AGG-R2", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("aggregates").is_none(),
        "default GET must not include aggregates field"
    );
}

#[tokio::test]
async fn get_run_rejects_unknown_aggregate_value() {
    // Unknown `?aggregate=` value is rejected by axum's Query
    // deserializer (the field is a typed enum, not a freeform string),
    // so the response is 400 with the framework's plain-text error
    // body — we just assert the status here.
    let app = build_test_app().await;
    let run = baseline_run("AGG-R3");
    app.eval_run_store.write(&run).unwrap();
    let (status, _) = request(
        &app.router,
        "GET",
        "/v1/eval/runs/AGG-R3?aggregate=tokens",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ── Dialogue importer (POST /v1/eval/datasets/:id/import-dialogue) ──────

#[tokio::test]
async fn import_dialogue_stitches_runs_into_multiturn_fixture() {
    let app = build_test_app().await;
    // Seed two captured runs to act as the two dialogue turns.
    for (id, text) in [
        ("01HXDLG0000000000000000001", "first answer"),
        ("01HXDLG0000000000000000002", "second answer"),
    ] {
        app.trace_store
            .append(
                id,
                &MetricsEvent::Inference(captured_inference_span(id, text, true)),
            )
            .unwrap();
    }
    // Empty dataset to receive the stitched dialogue.
    let (_, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-DLG", "spec": {} })),
    )
    .await;
    let rev = body["meta"]["revision"].as_u64().unwrap();

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-DLG/import-dialogue",
        Some(json!({
            "expected_revision": rev,
            "run_ids": [
                "01HXDLG0000000000000000001",
                "01HXDLG0000000000000000002",
            ],
            "fixture_id": "two-turn-dialogue",
            "expected": { "final_answer_contains": ["second"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["fixture_id"], "two-turn-dialogue");

    // Verify the stitched fixture has 1 turn 0 + 1 continued turn.
    let (_, body) = request(&app.router, "GET", "/v1/eval/datasets/DS-DLG", None).await;
    let fx = &body["spec"]["fixtures"][0];
    assert_eq!(fx["id"], "two-turn-dialogue");
    assert_eq!(fx["user_input"], "auto prompt");
    let continued = fx["continued_turns"].as_array().unwrap();
    assert_eq!(continued.len(), 1, "second run becomes one continued turn");
    assert_eq!(continued[0]["user_input"], "auto prompt");
    assert_eq!(fx["expect"]["final_answer_contains"][0], "second");

    let removed = app
        .trace_store
        .prune(
            UNIX_EPOCH + std::time::Duration::from_secs(4_000_000_000),
            &std::collections::HashSet::new(),
        )
        .unwrap();
    assert_eq!(removed, 0, "dialogue source traces must be pinned");
}

#[tokio::test]
async fn import_dialogue_cas_failure_does_not_pin_trace() {
    let app =
        build_test_app_with_config_store(Arc::new(CasConflictConfigStore::new("DS-DLG-CAS"))).await;
    let run_id = "01HXDLG0000000000000000CAS";
    app.trace_store
        .append(
            run_id,
            &MetricsEvent::Inference(captured_inference_span(run_id, "answer", true)),
        )
        .unwrap();
    let (_, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-DLG-CAS", "spec": {} })),
    )
    .await;
    let rev = body["meta"]["revision"].as_u64().unwrap();

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-DLG-CAS/import-dialogue",
        Some(json!({
            "expected_revision": rev,
            "run_ids": [run_id],
            "fixture_id": "dialogue",
            "expected": { "final_answer_contains": ["answer"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("revision conflict"),
        "body: {body}"
    );
    assert_eq!(
        prune_all_unreferenced_traces(app.trace_store.as_ref()),
        1,
        "failed dataset CAS must not create trace retention references"
    );
}

#[tokio::test]
async fn import_dialogue_400s_on_thread_id_mismatch() {
    // Two runs from different conversations would otherwise stitch into
    // a "dialogue" whose continuation is unrelated to the prior turn —
    // the resulting fixture would silently misrepresent the eval task.
    let app = build_test_app().await;
    for (id, thread) in [
        ("01HXDLG0000000000000000010", "thread-A"),
        ("01HXDLG0000000000000000011", "thread-B"),
    ] {
        let mut span = captured_inference_span(id, "answer", true);
        span.context.thread_id = thread.into();
        app.trace_store
            .append(id, &MetricsEvent::Inference(span))
            .unwrap();
    }
    let (_, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-DLG-MIX", "spec": {} })),
    )
    .await;
    let rev = body["meta"]["revision"].as_u64().unwrap();
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-DLG-MIX/import-dialogue",
        Some(json!({
            "expected_revision": rev,
            "run_ids": [
                "01HXDLG0000000000000000010",
                "01HXDLG0000000000000000011",
            ],
            "fixture_id": "mixed-threads",
            "expected": { "final_answer_contains": ["answer"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("thread_id="),
        "body: {body}"
    );
}

#[tokio::test]
async fn import_dialogue_400s_on_empty_run_ids() {
    let app = build_test_app().await;
    let (_, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-DLG2", "spec": {} })),
    )
    .await;
    let rev = body["meta"]["revision"].as_u64().unwrap();
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-DLG2/import-dialogue",
        Some(json!({
            "expected_revision": rev,
            "run_ids": [],
            "expected": { "final_answer_contains": ["answer"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap_or("").contains("non-empty"),
        "body: {body}"
    );
}

#[tokio::test]
async fn import_dialogue_409s_on_duplicate_fixture_id() {
    let app = build_test_app().await;
    let run_id = "01HXDLG0000000000000000099";
    app.trace_store
        .append(
            run_id,
            &MetricsEvent::Inference(captured_inference_span(run_id, "hi", true)),
        )
        .unwrap();
    // Dataset that already has a fixture with the would-be name.
    let (_, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({
            "id": "DS-DLG3",
            "spec": { "fixtures": [sample_fixture("already-here")] }
        })),
    )
    .await;
    let rev = body["meta"]["revision"].as_u64().unwrap();

    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets/DS-DLG3/import-dialogue",
        Some(json!({
            "expected_revision": rev,
            "run_ids": [run_id],
            "fixture_id": "already-here",
            "expected": { "final_answer_contains": ["hi"] },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("already-here"),
        "body: {body}"
    );
}

// ── Judge revise loop validation (revise_max_retries cap) ───────────────

#[tokio::test]
async fn start_eval_run_400s_when_revise_max_retries_above_cap() {
    let app = build_test_app().await;
    let fixtures = vec![sample_fixture("f1")];
    let (status, _) = request(
        &app.router,
        "POST",
        "/v1/eval/datasets",
        Some(json!({ "id": "DS-RV", "spec": { "fixtures": fixtures } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/runs",
        Some(json!({
            "dataset_id": "DS-RV",
            "models": ["m1"],
            "judge": {
                "model_id": "judge-model",
                "revise_max_retries": 99,
            },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("revise_max_retries=99"),
        "body: {body}"
    );
}

#[tokio::test]
async fn online_eval_400s_when_revise_max_retries_above_cap() {
    let app = build_test_app().await;
    let (status, body) = request(
        &app.router,
        "POST",
        "/v1/eval/online",
        Some(json!({
            "user_input": "hi",
            "models": ["m"],
            "judge": {
                "model_id": "judge-model",
                "revise_max_retries": 50,
            },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("revise_max_retries=50"),
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
        execution_mode: EvalRunExecutionMode::Scripted,
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
        execution_mode: EvalRunExecutionMode::Scripted,
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
            revision_count: 0,
            judge_score: None,
            judge_reasoning: None,
            cost_usd: None,
        },
        trace_run_id: None,
        sample_index: None,
    }
}
