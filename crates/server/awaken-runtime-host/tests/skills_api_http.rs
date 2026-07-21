//! The Skills API (`/v1/skills`, ADR-0036) end-to-end through its real axum router.
//! Two create paths coexist: the SDK multipart upload (a `SKILL.md` + supporting
//! files) and the legacy JSON `{id, content}` delivery. Both feed the runtime's
//! single durable Skill repository; definitions, versions, and binary bundles share
//! that one source of truth.
//!
//! The in-module unit test already covers the durable-only catalog-id fallback; this
//! binary drives the untested SDK surface: multipart create, list, retrieve, the
//! `versions` subresource (create / list / retrieve / content / delete), the legacy
//! JSON path's fail-closed 409 when no durable store is wired, and the error arms.

use std::sync::Arc;

use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult};
use awaken_runtime_host::{SharedHost, skills_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoLlm;
#[async_trait::async_trait]
impl LlmExecutor for NoLlm {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!("the skills API never infers")
    }
}

/// A router over a host backed by a durable skill store in a fresh temp dir, so the
/// SDK delivery (`store_put`) actually persists.
fn router_with_store() -> (Router, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "awaken-skillsapi-http-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let host =
        Arc::new(SharedHost::new(Arc::new(NoLlm), "test").with_skill_store(dir.join("store")));
    (skills_router(host), dir)
}
static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const BOUNDARY: &str = "X-SKILL-BOUNDARY";

fn multipart_skill(content: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"SKILL.md\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: text/markdown\r\n\r\n");
    body.extend_from_slice(content.as_bytes());
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn post_multipart(router: &Router, uri: &str, content: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(multipart_skill(content)))
        .unwrap();
    read(router.clone().oneshot(req).await.unwrap()).await
}

async fn post_json(router: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    read(router.clone().oneshot(req).await.unwrap()).await
}

async fn get(router: &Router, uri: &str) -> (StatusCode, String) {
    let resp = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn get_bytes(router: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 8 << 20)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn delete(router: &Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    read(router.clone().oneshot(req).await.unwrap()).await
}

async fn read(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

const SKILL_V1: &str = "---\nname: Greeter\ndescription: says hi\n---\nsay hello";
const SKILL_V2: &str = "---\nname: Greeter\ndescription: says hi\n---\nsay HELLO LOUDER";

#[tokio::test]
async fn multipart_bundle_preserves_binary_support_files() {
    let (router, dir) = router_with_store();
    let binary = vec![0, 159, 146, 150, 255];
    let mut body = multipart_skill(SKILL_V1);
    let closing = format!("--{BOUNDARY}--\r\n").into_bytes();
    body.truncate(body.len() - closing.len());
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"assets/data.bin\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(&binary);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(&closing);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/skills")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, created) = read(response).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["id"].as_str().unwrap();
    let (status, got) = get_bytes(
        &router,
        &format!("/v1/skills/{id}/versions/1/files/assets/data.bin"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, binary);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn sdk_multipart_create_list_retrieve_and_version_lifecycle() {
    let (router, dir) = router_with_store();

    // Multipart create (SDK path) registers a v1 and returns the tagged catalog id.
    let (status, created) = post_multipart(&router, "/v1/skills", SKILL_V1).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["type"], "skill");
    assert_eq!(created["latest_version"], "1");
    assert_eq!(created["source"], "api");

    // List surfaces it.
    let (status, list) = get(&router, "/v1/skills").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list.contains(&id),
        "list surfaces the created skill: {list}"
    );

    // Retrieve by id.
    let (status, got) = read(
        router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/skills/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["id"], id.as_str());

    // Add a version (SDK multipart).
    let (status, v2) =
        post_multipart(&router, &format!("/v1/skills/{id}/versions"), SKILL_V2).await;
    assert_eq!(status, StatusCode::OK, "{v2}");
    assert_eq!(v2["type"], "skill_version");
    let vid = v2["id"].as_str().unwrap().to_string();

    // List versions → two rows.
    let (status, versions) = read(
        router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/skills/{id}/versions"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(versions["data"].as_array().unwrap().len(), 2, "{versions}");

    // `latest` content downloads the newest version's SKILL.md.
    let (status, content) = get(&router, &format!("/v1/skills/{id}/versions/latest/content")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content.contains("LOUDER"), "latest content: {content}");

    // Retrieve a specific version by its id.
    let (status, one) = get(&router, &format!("/v1/skills/{id}/versions/{vid}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(one.contains(&vid));

    // Delete the newer version → receipt; the skill survives.
    let (status, receipt) = delete(&router, &format!("/v1/skills/{id}/versions/{vid}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["type"], "skill_version_deleted");

    // Delete the skill → receipt.
    let (status, receipt) = delete(&router, &format!("/v1/skills/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["type"], "skill_deleted");

    let (status, _) = get(&router, &format!("/v1/skills/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, list) = get(&router, "/v1/skills").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !list.contains(&id),
        "deleted skill must not be re-projected"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn legacy_json_create_fails_closed_without_a_durable_store() {
    // No `with_skill_store`: the host has no durable skill catalog, so the legacy
    // `{id, content}` delivery has nowhere to land → 409 (fail closed, no silent drop).
    let host = Arc::new(SharedHost::new(Arc::new(NoLlm), "test"));
    let router = skills_router(host);
    let (status, v) = post_json(
        &router,
        "/v1/skills",
        json!({ "id": "greeter", "content": SKILL_V1 }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
}

#[tokio::test]
async fn legacy_json_create_delivers_with_a_durable_store() {
    let (router, dir) = router_with_store();
    let (status, v) = post_json(
        &router,
        "/v1/skills",
        json!({ "id": "greeter", "content": SKILL_V1 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["type"], "skill");
    // The delivered id retrieves.
    let stored_id = v["id"].as_str().unwrap().to_string();
    let (status, _) = get(&router, &format!("/v1/skills/{stored_id}")).await;
    assert_eq!(status, StatusCode::OK);
    let _ = std::fs::remove_dir_all(&dir);
}

// Both `POST /v1/skills` create paths now agree on the no-durable-store case: they
// FAIL CLOSED (409). The SDK multipart path checks `store_put`'s `None` just like
// the legacy JSON path, so a store-less host never reports success for a skill it
// neither delivered on a thread nor persisted across a restart — upholding the
// module's "BOTH feed the durable catalog … survives a restart" contract.
#[tokio::test]
async fn sdk_multipart_create_fails_closed_without_a_durable_store() {
    let host = Arc::new(SharedHost::new(Arc::new(NoLlm), "test"));
    let router = skills_router(host);
    // Multipart fails closed (409) when nothing durable backs it…
    let (status, created) = post_multipart(&router, "/v1/skills", SKILL_V1).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "SDK create must fail closed with no durable store: {created}"
    );
    // …matching the legacy JSON path, which 409s on the very same host.
    let (json_status, _) = post_json(
        &router,
        "/v1/skills",
        json!({ "id": "greeter", "content": SKILL_V1 }),
    )
    .await;
    assert_eq!(
        json_status,
        StatusCode::CONFLICT,
        "the sibling JSON path fails CLOSED on the same store-less host"
    );
}

#[tokio::test]
async fn error_arms_are_fail_closed() {
    let (router, dir) = router_with_store();

    // Retrieve / delete an unknown skill → 404.
    let (status, _) = get(&router, "/v1/skills/skill_missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = delete(&router, "/v1/skills/skill_missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A version create against an unknown skill → 404.
    let (status, _) = post_multipart(&router, "/v1/skills/skill_missing/versions", SKILL_V1).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A multipart create with no file part → 400 (no SKILL.md).
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"display_title\"\r\n\r\nX\r\n--{BOUNDARY}--\r\n"
    );
    let req = Request::builder()
        .method("POST")
        .uri("/v1/skills")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap();
    let (status, _) = read(router.clone().oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A version retrieve/content on an unknown version → 404.
    let (status, created) = post_multipart(&router, "/v1/skills", SKILL_V1).await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap().to_string();
    let (status, _) = get(&router, &format!("/v1/skills/{id}/versions/999")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&router, &format!("/v1/skills/{id}/versions/999/content")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&dir);
}
