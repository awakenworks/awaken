//! The Files API (`/v1/files`, ADR-0038) end-to-end through its real axum router.
//! Public Files identities and metadata live in the FileCatalog while equal bytes
//! deduplicate privately in the BLAKE3 FileStore. The comments beside each test
//! preserve the cause/effect decision rules for this compatibility boundary.

use awaken_protocol_managed::files_router;
use awaken_resource_contract::FileRecord;
use awaken_tenancy::WorkspaceScope;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

mod support;

fn router() -> Router {
    files_router(support::resources::ephemeral_resources().files())
}

const BOUNDARY: &str = "X-AWAKEN-BOUNDARY";

fn in_test_workspace(mut request: Request<Body>) -> Request<Body> {
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    request
}

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
    let req = in_test_workspace(
        Request::builder()
            .method("POST")
            .uri("/v1/files")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(multipart_file(filename, content)))
            .unwrap(),
    );
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
        .oneshot(in_test_workspace(
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
        ))
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
        .oneshot(in_test_workspace(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        ))
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
    // Causes: C1 valid upload, C2 catalog/lifecycle available, C3 uploaded File
    // (not Agent output). Effects: E1 tagged opaque identity + real metadata,
    // E2 metadata/list visible, E3 content download denied, E4 delete makes all
    // subsequent logical reads 404. Rule R1 = C1∧C2∧C3 → E1..E4.
    let router = router();

    let (status, meta) = upload(&router, "hello.txt", b"hello world").await;
    assert_eq!(status, StatusCode::OK, "{meta}");
    let id = meta["id"].as_str().unwrap().to_string();
    assert_eq!(meta["type"], "file");
    assert_eq!(meta["filename"], "hello.txt");
    assert_eq!(meta["size_bytes"], 11);
    assert_eq!(meta["mime_type"], "application/octet-stream");
    assert_eq!(meta["downloadable"], false);
    assert_eq!(meta["purpose"], "input");
    assert!(meta["session_id"].is_null());
    assert!(meta["logical_path"].is_null());
    assert!(id.starts_with("file_"), "{id}");
    assert_ne!(meta["created_at"], "1970-01-01T00:00:00Z");

    // Metadata by id.
    let (status, body) = get(&router, &format!("/v1/files/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    let meta: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(meta["size_bytes"], 11);

    // Anthropic upload Files are inputs, not downloadable Agent artifacts.
    let (status, bytes) = get(&router, &format!("/v1/files/{id}/content")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{:?}", bytes);

    // Delete → receipt; a second delete is a clean 404 (the blob is gone).
    let (status, receipt) = delete(&router, &format!("/v1/files/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["type"], "file_deleted");
    let (status, _) = delete(&router, &format!("/v1/files/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upload_requires_complete_resource_lifecycle_authority() {
    // Rule R2: Resource authorities require a lifecycle owner before the
    // canonical ResourcesApplication can exist. This is compile-time structural
    // coverage; the HTTP adapter accepts only the already-complete File service.
    fn requires_file_service(
        _: std::sync::Arc<dyn awaken_resource_contract::FileApplicationService>,
    ) {
    }
    requires_file_service(support::resources::ephemeral_resources().files());
}

#[tokio::test]
async fn equal_upload_bytes_deduplicate_privately_but_keep_distinct_public_files() {
    // Rule R3: equal bytes + two upload operations → two logical File ids and
    // metadata records (E6), while physical dedup remains an unexposed store fact.
    let router = router();
    let (_, a) = upload(&router, "a.bin", b"same bytes").await;
    let (_, b) = upload(&router, "b.bin", b"same bytes").await;
    assert_ne!(a["id"], b["id"], "each upload is a logical File");
    let (_, c) = upload(&router, "c.bin", b"other bytes").await;
    assert_ne!(a["id"], c["id"]);
    assert_eq!(
        delete(&router, &format!("/v1/files/{}", a["id"].as_str().unwrap()))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        delete(&router, &format!("/v1/files/{}", b["id"].as_str().unwrap()))
            .await
            .0,
        StatusCode::OK,
        "two logical Files sharing one blob have independent delete intents"
    );
}

#[tokio::test]
async fn error_arms_are_fail_closed() {
    // Rules R4/R5: missing file part or unknown opaque id → 400/404 with no
    // catalog mutation or cross-resource fallback.
    let router = router();

    // A multipart with no `file` part is a 400.
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nagent\r\n--{BOUNDARY}--\r\n"
    );
    let req = in_test_workspace(
        Request::builder()
            .method("POST")
            .uri("/v1/files")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .unwrap(),
    );
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Unknown id: metadata 404, download 404.
    let (status, _) = get(&router, "/v1/files/file_missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&router, "/v1/files/file_missing/content").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn global_and_scoped_lists_are_pure_catalog_queries() {
    // Rule R6: active uploaded File + no scope → global Files list contains it.
    // Rule R7: unknown scope → empty page; listing never scans a Sandbox.
    let router = router();
    let (_, uploaded) = upload(&router, "listed.txt", b"listed").await;
    let (status, body) = get(&router, "/v1/files").await;
    assert_eq!(status, StatusCode::OK);
    let list: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(list["data"].as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list["data"][0]["id"], uploaded["id"]);
    assert_eq!(list["has_more"], false);
    assert_eq!(list["first_id"], uploaded["id"]);
    assert_eq!(list["last_id"], uploaded["id"]);

    // A scope_id for an unknown session harvests nothing and lists nothing (no panic).
    let (status, body) = get(&router, "/v1/files?scope_id=no-such-session").await;
    assert_eq!(status, StatusCode::OK);
    let list: Value = serde_json::from_slice(&body).unwrap();
    assert!(list["data"].as_array().unwrap().is_empty(), "{list}");
}

#[tokio::test]
async fn harvested_output_is_scoped_downloadable_and_independent_of_live_session_state() {
    // Rule R8: downloadable output record + durable blob + Session no longer
    // registered → scope query and content download still succeed. This pins the
    // File-over-Session lifecycle edge without relying on a GET-time harvest.
    let resources = support::resources::ephemeral_resources();
    let authorities = resources.authorities();
    let blob_id = authorities
        .file_store()
        .put(b"finished report")
        .await
        .unwrap();
    let record = FileRecord {
        id: "file_output".into(),
        workspace_id: "test".into(),
        blob_id,
        filename: "report.txt".into(),
        mime_type: "text/plain".into(),
        size_bytes: 15,
        created_at: "2026-01-01T00:00:00Z".into(),
        downloadable: true,
        scope_id: Some("deleted-session".into()),
        logical_path: Some("report.txt".into()),
        harvest_key: Some(awaken_resource_contract::harvest_idempotency_key(
            "deleted-session",
            "report.txt",
            "hash",
        )),
        deleted: false,
    };
    authorities
        .file_catalog()
        .create_file(record)
        .await
        .unwrap();
    let router = files_router(resources.files());

    let (status, body) = get(&router, "/v1/files?scope_id=deleted-session").await;
    assert_eq!(status, StatusCode::OK);
    let list: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(list["data"][0]["id"], "file_output");
    assert_eq!(list["data"][0]["downloadable"], true);
    assert_eq!(list["data"][0]["purpose"], "artifact");
    assert_eq!(list["data"][0]["session_id"], "deleted-session");
    assert_eq!(list["data"][0]["logical_path"], "report.txt");
    let (status, body) = get(&router, "/v1/files?purpose=artifact").await;
    assert_eq!(status, StatusCode::OK);
    let artifacts: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(artifacts["data"].as_array().unwrap().len(), 1);
    let (status, body) = get(&router, "/v1/files?purpose=input").await;
    assert_eq!(status, StatusCode::OK);
    let inputs: Value = serde_json::from_slice(&body).unwrap();
    assert!(inputs["data"].as_array().unwrap().is_empty());
    assert_eq!(
        get(&router, "/v1/files?purpose=unknown").await.0,
        StatusCode::BAD_REQUEST
    );
    let (status, bytes) = get(&router, "/v1/files/file_output/content").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"finished report");
}

#[tokio::test]
async fn list_pagination_validates_and_advances_cursors() {
    // Causes: C1 three active records, C2 limit=2, C3 valid after cursor.
    // Effects: first rule has_more=true and boundaries; second rule returns the
    // remaining record. Invalid/conflicting cursor causes map to 400 (R8-R10).
    let router = router();
    for name in ["one.txt", "two.txt", "three.txt"] {
        assert_eq!(
            upload(&router, name, name.as_bytes()).await.0,
            StatusCode::OK
        );
    }
    let (status, body) = get(&router, "/v1/files?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    let first: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(first["data"].as_array().unwrap().len(), 2);
    assert_eq!(first["has_more"], true);
    let cursor = first["last_id"].as_str().unwrap();
    let (status, body) = get(&router, &format!("/v1/files?limit=2&after_id={cursor}")).await;
    assert_eq!(status, StatusCode::OK);
    let second: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    assert_eq!(second["has_more"], false);

    assert_eq!(
        get(&router, "/v1/files?limit=0").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&router, "/v1/files?before_id=x&after_id=y").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&router, "/v1/files?after_id=file_missing").await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn filename_validation_rejects_unsafe_and_overlong_names() {
    // Rule R11: forbidden path/control character or >255 Unicode scalar values
    // → 400 before blob/catalog creation. Boundary 255 remains accepted.
    let router = router();
    assert_eq!(
        upload(&router, "../escape.txt", b"x").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        upload(&router, &"a".repeat(256), b"x").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        upload(&router, &"a".repeat(255), b"x").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn workspace_capacity_is_checked_before_accepting_more_bytes() {
    // Rule R12: active logical bytes at the 500 GiB Workspace boundary + a
    // nonempty upload → invalid request before the new blob/File is created.
    // A synthetic catalog record tests the boundary without allocating 500 GiB.
    let resources = support::resources::ephemeral_resources();
    resources
        .authorities()
        .file_catalog()
        .create_file(FileRecord {
            id: "file_capacity".into(),
            workspace_id: "test".into(),
            blob_id: "blob-capacity".into(),
            filename: "capacity.bin".into(),
            mime_type: "application/octet-stream".into(),
            size_bytes: 500 * 1024 * 1024 * 1024,
            created_at: "2026-01-01T00:00:00Z".into(),
            downloadable: false,
            scope_id: None,
            logical_path: None,
            harvest_key: None,
            deleted: false,
        })
        .await
        .unwrap();
    let error = resources
        .files()
        .create_uploaded_file("test", "one.txt".into(), "text/plain".into(), b"1")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Workspace files exceed"));
}

#[tokio::test]
async fn file_routes_require_a_preselected_workspace() {
    // Rule R13: no trusted Workspace extension → fail closed at the tenancy
    // extractor before catalog access.
    let router = router();
    for request in [
        Request::builder()
            .uri("/v1/files")
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .uri("/v1/files/file_unknown")
            .body(Body::empty())
            .unwrap(),
    ] {
        assert_eq!(
            router.clone().oneshot(request).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }
}
