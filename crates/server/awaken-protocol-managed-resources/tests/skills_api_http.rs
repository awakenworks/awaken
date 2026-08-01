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

use awaken_protocol_managed_resources::skills_router;
use awaken_tenancy::WorkspaceScope;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::io::Write;
use tower::ServiceExt;

mod support;

/// A router over the canonical Resources component backed by a durable Skill store, so the
/// SDK delivery (`store_put`) actually persists.
fn router_with_store() -> (Router, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "awaken-skillsapi-http-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let store = support::filesystem_skill_store(dir.join("store"));
    let resources = support::resources(store.clone());
    (skills_router(Some(store), resources.purge_scheduler()), dir)
}
static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const BOUNDARY: &str = "X-SKILL-BOUNDARY";

fn in_test_workspace(mut request: Request<Body>) -> Request<Body> {
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    request
}

fn multipart_files(files: &[(&str, &[u8])]) -> Vec<u8> {
    multipart_files_with_fields(files, &[])
}

fn multipart_files_with_fields(files: &[(&str, &[u8])], fields: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (path, content) in files {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"files\"; filename=\"{path}\"\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(content);
        body.extend_from_slice(b"\r\n");
    }
    for (name, value) in fields {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn multipart_skill(content: &str) -> Vec<u8> {
    multipart_files(&[("SKILL.md", content.as_bytes())])
}

async fn post_multipart(router: &Router, uri: &str, content: &str) -> (StatusCode, Value) {
    post_multipart_body(router, uri, multipart_skill(content), None).await
}

async fn post_multipart_body(
    router: &Router,
    uri: &str,
    body: Vec<u8>,
    if_match: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("POST").uri(uri).header(
        "content-type",
        format!("multipart/form-data; boundary={BOUNDARY}"),
    );
    if let Some(version) = if_match {
        builder = builder.header("if-match", version);
    }
    let req = in_test_workspace(builder.body(Body::from(body)).unwrap());
    read(router.clone().oneshot(req).await.unwrap()).await
}

fn skill_zip(name: &str, files: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    for (path, content, mode) in files {
        writer
            .start_file(
                format!("{name}/{path}"),
                zip::write::SimpleFileOptions::default().unix_permissions(*mode),
            )
            .unwrap();
        writer.write_all(content).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

async fn post_json(router: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = in_test_workspace(
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    );
    read(router.clone().oneshot(req).await.unwrap()).await
}

async fn get(router: &Router, uri: &str) -> (StatusCode, String) {
    let resp = router
        .clone()
        .oneshot(in_test_workspace(
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
        ))
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
        .oneshot(in_test_workspace(
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
        ))
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 8 << 20)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn delete(router: &Router, uri: &str) -> (StatusCode, Value) {
    let req = in_test_workspace(
        Request::builder()
            .method("DELETE")
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    );
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

// Cause/effect graph for canonical import:
// C1 input is one ZIP or path-preserving multipart directory; C2 exactly one
// bundle root contains UTF-8 SKILL.md; C3 support files include text, binary, and
// an executable script. Effects: E1 both inputs lose only the transport root,
// E2 every support byte remains addressable, E3 ZIP/script metadata survives in
// the immutable version, and E4 editor metadata can preserve that bit in a later
// complete upload. Decision rules R1=ZIP+C2+C3 -> E1/E2/E3,
// R2=directory+C2 -> E1/E2, and R3=files+matching executable_paths -> E4.
// These rules prove import adapters converge before the one SkillStore write
// instead of becoming parallel content models.
#[tokio::test]
async fn zip_and_directory_import_converge_on_one_canonical_bundle() {
    let (router, dir) = router_with_store();
    let zip_skill = b"---\nname: Zip Skill\ndescription: imported\n---\nUse scripts/run.sh";
    let zip = skill_zip(
        "zip-skill",
        &[
            ("SKILL.md", zip_skill, 0o644),
            ("references/api.md", b"reference", 0o644),
            ("scripts/run.sh", b"#!/bin/sh\necho ok\n", 0o755),
            ("assets/data.bin", &[0, 159, 255], 0o644),
        ],
    );
    let (status, created) = post_multipart_body(
        &router,
        "/v1/skills",
        multipart_files(&[("zip-skill.zip", &zip)]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let zip_id = created["id"].as_str().unwrap();
    let (status, version) = read(
        router
            .clone()
            .oneshot(in_test_workspace(
                Request::builder()
                    .uri(format!("/v1/skills/{zip_id}/versions/latest"))
                    .body(Body::empty())
                    .unwrap(),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{version}");
    assert_eq!(version["directory"], "zip-skill");
    let paths = version["files"].as_array().unwrap();
    assert!(paths.contains(&json!("SKILL.md")));
    assert!(paths.contains(&json!("references/api.md")));
    assert!(
        !paths
            .iter()
            .any(|path| path.as_str().unwrap().starts_with("zip-skill/"))
    );
    let executable = version["file_entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "scripts/run.sh")
        .unwrap();
    assert_eq!(executable["executable"], true);
    let (status, bytes) = get_bytes(
        &router,
        &format!("/v1/skills/{zip_id}/versions/latest/files/assets/data.bin"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, vec![0, 159, 255]);

    let edited = multipart_files_with_fields(
        &[
            ("SKILL.md", zip_skill),
            ("tools/helper", b"opaque executable"),
        ],
        &[("executable_paths", r#"["tools/helper"]"#)],
    );
    let (status, edited_version) = post_multipart_body(
        &router,
        &format!("/v1/skills/{zip_id}/versions"),
        edited,
        Some("1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited_version}");
    let helper = edited_version["file_entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "tools/helper")
        .unwrap();
    assert_eq!(helper["executable"], true);

    let directory_skill =
        b"---\nname: Directory Skill\ndescription: imported\n---\nRead references/api.md";
    let body = multipart_files(&[
        ("directory-skill/SKILL.md", directory_skill),
        ("directory-skill/references/api.md", b"directory reference"),
    ]);
    let (status, created) = post_multipart_body(&router, "/v1/skills", body, None).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["id"].as_str().unwrap();
    let (status, content) = get(
        &router,
        &format!("/v1/skills/{id}/versions/latest/files/references/api.md"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content, "directory reference");
    let _ = std::fs::remove_dir_all(dir);
}

// Invalid-import decision table: C1 unsafe traversal, C2 multiple transport
// roots, C3 missing root SKILL.md, C4 case-folded duplicate path, C5 ZIP mixed
// with ordinary files, C6 expanded entry above the per-file limit, C7 metadata
// naming an absent file, or C8 metadata attempting to override ZIP permissions
// each causes E1=400 and E2=no Skill definition. One-condition rules isolate every
// validator boundary; the final empty listing observes the no-write effect.
#[tokio::test]
async fn invalid_import_shapes_fail_before_the_skill_store_write() {
    let (router, dir) = router_with_store();
    let oversized = vec![b'x'; 2 * 1024 * 1024 + 1];
    let oversized_zip = skill_zip(
        "large",
        &[
            ("SKILL.md", b"# large", 0o644),
            ("assets/large.bin", &oversized, 0o644),
        ],
    );
    let cases = [
        multipart_files(&[("../SKILL.md", b"# traversal")]),
        multipart_files(&[("one/SKILL.md", b"# one"), ("two/ref.md", b"two")]),
        multipart_files(&[("only/references/readme.md", b"missing")]),
        multipart_files(&[("SKILL.md", b"# duplicate"), ("skill.md", b"collision")]),
        multipart_files(&[("bundle.zip", b"not zip"), ("SKILL.md", b"# mixed")]),
        multipart_files(&[("large.zip", &oversized_zip)]),
        multipart_files_with_fields(
            &[("SKILL.md", b"# absent executable")],
            &[("executable_paths", r#"["scripts/missing.sh"]"#)],
        ),
        multipart_files_with_fields(
            &[(
                "bundle.zip",
                &skill_zip("valid", &[("SKILL.md", b"# zip", 0o644)]),
            )],
            &[("executable_paths", r#"["SKILL.md"]"#)],
        ),
    ];
    for body in cases {
        let (status, _) = post_multipart_body(&router, "/v1/skills", body, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, listing) = get(&router, "/v1/skills").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&listing).unwrap()["data"],
        json!([])
    );
    let _ = std::fs::remove_dir_all(dir);
}

// Optimistic-publication decision table: C1 If-Match equals latest -> E1 append
// exactly one version; C2 If-Match is stale after that append -> E2 409 and E3 no
// extra version. This covers the browser draft race while the no-header SDK path
// remains covered by the ordinary lifecycle test below.
#[tokio::test]
async fn browser_publish_rejects_a_stale_base_version_without_appending() {
    let (router, dir) = router_with_store();
    let (status, created) = post_multipart(&router, "/v1/skills", SKILL_V1).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["id"].as_str().unwrap();
    let route = format!("/v1/skills/{id}/versions");
    let (status, _) =
        post_multipart_body(&router, &route, multipart_skill(SKILL_V2), Some("1")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, conflict) =
        post_multipart_body(&router, &route, multipart_skill(SKILL_V1), Some("1")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    let (status, versions) = read(
        router
            .clone()
            .oneshot(in_test_workspace(
                Request::builder().uri(route).body(Body::empty()).unwrap(),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(versions["data"].as_array().unwrap().len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}

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
        .oneshot(in_test_workspace(
            Request::builder()
                .method("POST")
                .uri("/v1/skills")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(body))
                .unwrap(),
        ))
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
    assert_eq!(created["source"], "custom");

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
            .oneshot(in_test_workspace(
                Request::builder()
                    .uri(format!("/v1/skills/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            ))
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
            .oneshot(in_test_workspace(
                Request::builder()
                    .uri(format!("/v1/skills/{id}/versions"))
                    .body(Body::empty())
                    .unwrap(),
            ))
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
    // No SkillStore port: the adapter has no durable skill catalog, so the legacy
    // `{id, content}` delivery has nowhere to land → 409 (fail closed, no silent drop).
    let resources = support::ephemeral_resources();
    let router = skills_router(None, resources.purge_scheduler());
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
    let resources = support::ephemeral_resources();
    let router = skills_router(None, resources.purge_scheduler());
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
    let req = in_test_workspace(
        Request::builder()
            .method("POST")
            .uri("/v1/skills")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .unwrap(),
    );
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

#[tokio::test]
async fn skill_routes_require_a_preselected_workspace() {
    let (router, dir) = router_with_store();
    for request in [
        Request::builder()
            .uri("/v1/skills")
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("POST")
            .uri("/v1/skills")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({ "id": "hidden", "content": SKILL_V1 })).unwrap(),
            ))
            .unwrap(),
    ] {
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    let (status, listing) = get(&router, "/v1/skills").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!listing.contains("hidden"));
    let _ = std::fs::remove_dir_all(dir);
}
