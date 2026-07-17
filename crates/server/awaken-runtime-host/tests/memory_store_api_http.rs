//! The memory-store API (`/v1/memory_stores`, ADR-0038/0053) end-to-end through its
//! real axum router, driven over `tower::ServiceExt::oneshot`. Covers the store
//! aggregate (create / retrieve / list / update / delete / archive), the `memories`
//! subresource (create / list / retrieve / update with a `content_sha256`
//! precondition / delete), and the `memory_versions` log (list / retrieve / redact),
//! plus the fail-closed error arms (unknown store, unknown memory, bad path).
//!
//! The router is built over a real `SharedHost` whose content backends are the
//! ephemeral in-memory blob store + in-memory `SqliteMemoryFs` — the same code paths
//! a durable deployment runs, only the storage root differs.

use std::sync::Arc;

use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult};
use awaken_runtime_host::{SharedHost, memory_stores_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

/// The memory-store map never calls the model.
struct NoLlm;
#[async_trait::async_trait]
impl LlmExecutor for NoLlm {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!("the memory-store API never infers")
    }
}

fn router() -> Router {
    let host = Arc::new(SharedHost::new(Arc::new(NoLlm), "test"));
    memory_stores_router(host)
}

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&v).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Create a store over HTTP and return its minted id.
async fn create_store(router: &Router, name: &str) -> String {
    let (status, v) = call(
        router,
        "POST",
        "/v1/memory_stores",
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create store: {v}");
    v["id"].as_str().expect("minted store id").to_string()
}

#[tokio::test]
async fn store_lifecycle_create_get_list_update_archive_delete() {
    let router = router();

    // Create with the SDK body (name/description/metadata) mints a durable id.
    let (status, created) = call(
        &router,
        "POST",
        "/v1/memory_stores",
        Some(json!({ "name": "notes", "description": "my notes", "metadata": { "team": "core" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["type"], "memory_store");
    assert_eq!(created["name"], "notes");
    assert_eq!(created["metadata"]["team"], "core");
    assert_eq!(created["archived_at"], Value::Null);

    // Retrieve carries the SDK object PLUS the legacy mount-blob fields.
    let (status, got) = call(&router, "GET", &format!("/v1/memory_stores/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["name"], "notes");
    assert_eq!(got["size_bytes"], 0, "a fresh store's blob is empty");
    assert!(got.get("content").is_some(), "legacy content field present");

    // List surfaces the non-archived store.
    let (status, list) = call(&router, "GET", "/v1/memory_stores", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == id.as_str()),
        "list surfaces the store: {list}"
    );

    // Update: patch description (empty clears) + metadata (string upsert, null delete).
    let (status, updated) = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{id}"),
        Some(json!({ "description": "updated", "metadata": { "team": null, "owner": "alice" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["description"], "updated");
    assert_eq!(updated["metadata"]["owner"], "alice");
    assert!(
        updated["metadata"].get("team").is_none(),
        "null metadata value deletes the key: {updated}"
    );

    // Archive stamps archived_at and drops the store from the listing.
    let (status, archived) = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{id}/archive"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(archived["archived_at"], Value::Null);
    let (_, list) = call(&router, "GET", "/v1/memory_stores", None).await;
    assert!(
        !list["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == id.as_str()),
        "an archived store is not listed: {list}"
    );

    // Delete a fresh store is a soft archive → receipt.
    let id2 = create_store(&router, "trash").await;
    let (status, receipt) =
        call(&router, "DELETE", &format!("/v1/memory_stores/{id2}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["type"], "memory_store_deleted");
    assert_eq!(receipt["id"], id2.as_str());
}

#[tokio::test]
async fn unknown_store_is_fail_closed_on_every_verb() {
    let router = router();
    let missing = "memstore_does_not_exist";

    // GET on a store with neither a blob nor a registry row → 404.
    let (status, v) = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{missing}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");

    // update / delete / archive on an unknown identity all 404.
    for (method, suffix, body) in [
        ("POST", "", Some(json!({ "description": "x" }))),
        ("DELETE", "", None),
        ("POST", "/archive", None),
    ] {
        let (status, _) = call(
            &router,
            method,
            &format!("/v1/memory_stores/{missing}{suffix}"),
            body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {suffix} on unknown store"
        );
    }
}

#[tokio::test]
async fn memory_crud_with_precondition_and_version_log() {
    let router = router();
    let store = create_store(&router, "mem-host").await;

    // Create a memory (path-addressed head in the durable store).
    let (status, mem) = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories"),
        Some(json!({ "path": "/a.md", "content": "hello" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{mem}");
    let mid = mem["id"].as_str().unwrap().to_string();
    let sha = mem["content_sha256"].as_str().unwrap().to_string();
    assert_eq!(mem["path"], "/a.md");
    assert_eq!(mem["content"], "hello");
    assert_eq!(mem["content_size_bytes"], 5);

    // List (full view) returns the head content; basic view elides it.
    let (status, full) = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(full["data"][0]["content"], "hello");
    let (_, basic) = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories?view=basic"),
        None,
    )
    .await;
    assert_eq!(
        basic["data"][0]["content"],
        Value::Null,
        "basic view elides content"
    );

    // Retrieve by id.
    let (status, got) = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["content"], "hello");

    // A stale content_sha256 precondition is a 409 CAS failure.
    let (status, conflict) = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        Some(json!({ "content": "new", "precondition": { "content_sha256": "0".repeat(64) } })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(
        conflict["error"]["type"],
        "memory_precondition_failed_error"
    );

    // The matching precondition (the live sha) succeeds and bumps the version id.
    let (status, updated) = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        Some(json!({ "content": "world", "precondition": { "content_sha256": sha } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["content"], "world");

    // The version log now carries a `created` then a `modified` row.
    let (status, versions) = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ops: Vec<&str> = versions["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["operation"].as_str().unwrap())
        .collect();
    assert_eq!(
        ops,
        vec!["created", "modified"],
        "version history: {versions}"
    );
    let first_vid = versions["data"][0]["id"].as_str().unwrap().to_string();

    // Retrieve a single version.
    let (status, ver) = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions/{first_vid}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ver["operation"], "created");
    assert_eq!(ver["content"], "hello");

    // Redact a version: stamp redacted_at + drop content.
    let (status, redacted) = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memory_versions/{first_vid}/redact"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(redacted["redacted_at"], Value::Null);
    assert_eq!(redacted["content"], Value::Null, "redaction drops content");

    // Delete the memory → receipt + a `deleted` version row.
    let (status, receipt) = call(
        &router,
        "DELETE",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["type"], "memory_deleted");
    let (_, after) = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        None,
    )
    .await;
    // The head is gone: retrieve by id is now 404.
    let (status, _) = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memories/{mid}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "deleted memory: {after}");
}

#[tokio::test]
async fn memory_error_arms_are_fail_closed() {
    let router = router();
    let store = create_store(&router, "errs").await;

    // A create without a `path` is a 400.
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memories"),
        Some(json!({ "content": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A create against an UNKNOWN store is 404 (the mount-blob existence check).
    let (status, _) = call(
        &router,
        "POST",
        "/v1/memory_stores/memstore_missing/memories",
        Some(json!({ "path": "/a.md", "content": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Retrieve / update / delete an unknown memory id in a real store → 404.
    for method in ["GET", "POST", "DELETE"] {
        let body = (method == "POST").then(|| json!({ "content": "x" }));
        let (status, _) = call(
            &router,
            method,
            &format!("/v1/memory_stores/{store}/memories/mem_missing"),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} unknown memory");
    }

    // Version endpoints on an unknown store → 404; an unknown version id in a real
    // store → 404.
    let (status, _) = call(
        &router,
        "GET",
        "/v1/memory_stores/memstore_missing/memory_versions",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &router,
        "GET",
        &format!("/v1/memory_stores/{store}/memory_versions/memver_nope"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/memory_stores/{store}/memory_versions/memver_nope/redact"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
