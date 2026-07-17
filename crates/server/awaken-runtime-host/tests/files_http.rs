//! The Files API (`/v1/files`, ADR-0038) end-to-end through its real axum router.
//! Uploads land in the host's content-addressed blob store (BLAKE3 id), so an
//! upload is idempotent (equal bytes → same id) and downloadable/deletable by that
//! id. Covers upload / get-metadata / download / delete, the idempotency contract,
//! and the fail-closed arms (no `file` part, unknown id, empty scope).

use std::sync::Arc;

use awaken_managed_routers::files_router;
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult};
use awaken_runtime_host::SharedHost;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

struct NoLlm;
#[async_trait::async_trait]
impl LlmExecutor for NoLlm {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!("the files API never infers")
    }
}

fn router() -> Router {
    let host = Arc::new(SharedHost::new(Arc::new(NoLlm), "test"));
    files_router(host)
}

const BOUNDARY: &str = "X-AWAKEN-BOUNDARY";

/// A `multipart/form-data` body with a single `file` part.
fn multipart_file(filename: &str, content: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn upload(router: &Router, filename: &str, content: &[u8]) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/files")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(multipart_file(filename, content)))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get(router: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let resp = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn delete(router: &Router, uri: &str) -> (StatusCode, Value) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn upload_download_metadata_and_delete_roundtrip() {
    let router = router();

    let (status, meta) = upload(&router, "hello.txt", b"hello world").await;
    assert_eq!(status, StatusCode::OK, "{meta}");
    let id = meta["id"].as_str().unwrap().to_string();
    assert_eq!(meta["type"], "file");
    assert_eq!(meta["filename"], "hello.txt");
    assert_eq!(meta["size_bytes"], 11);
    assert_eq!(meta["downloadable"], true);

    // Metadata by id.
    let (status, body) = get(&router, &format!("/v1/files/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    let meta: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(meta["size_bytes"], 11);

    // Download returns the raw bytes.
    let (status, bytes) = get(&router, &format!("/v1/files/{id}/content")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"hello world");

    // Delete → receipt; a second delete is a clean 404 (the blob is gone).
    let (status, receipt) = delete(&router, &format!("/v1/files/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["type"], "file_deleted");
    let (status, _) = delete(&router, &format!("/v1/files/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upload_is_content_addressed_and_idempotent() {
    let router = router();
    // Equal bytes → same id regardless of filename (content-addressed store).
    let (_, a) = upload(&router, "a.bin", b"same bytes").await;
    let (_, b) = upload(&router, "b.bin", b"same bytes").await;
    assert_eq!(a["id"], b["id"], "equal bytes mint the same id");
    // Different bytes → different id.
    let (_, c) = upload(&router, "c.bin", b"other bytes").await;
    assert_ne!(a["id"], c["id"]);
}

#[tokio::test]
async fn error_arms_are_fail_closed() {
    let router = router();

    // A multipart with no `file` part is a 400.
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nagent\r\n--{BOUNDARY}--\r\n"
    );
    let req = Request::builder()
        .method("POST")
        .uri("/v1/files")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Unknown id: metadata 404, download 404.
    let (status, _) = get(&router, "/v1/files/file_missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&router, "/v1/files/file_missing/content").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_without_scope_is_empty() {
    let router = router();
    // This server scopes files to a session, so a global list (no `scope_id`) is empty.
    let (status, body) = get(&router, "/v1/files").await;
    assert_eq!(status, StatusCode::OK);
    let list: Value = serde_json::from_slice(&body).unwrap();
    assert!(list["data"].as_array().unwrap().is_empty(), "{list}");
    assert_eq!(list["has_more"], false);

    // A scope_id for an unknown session harvests nothing and lists nothing (no panic).
    let (status, body) = get(&router, "/v1/files?scope_id=no-such-session").await;
    assert_eq!(status, StatusCode::OK);
    let list: Value = serde_json::from_slice(&body).unwrap();
    assert!(list["data"].as_array().unwrap().is_empty(), "{list}");
}
