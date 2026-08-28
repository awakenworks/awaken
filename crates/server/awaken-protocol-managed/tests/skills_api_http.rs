//! The Skills API (`/v1/skills`, ADR-0036) end-to-end through its real axum router.
//! The SDK multipart upload (a `SKILL.md` + supporting files) feeds the runtime's
//! single durable Skill repository; definitions, versions, and binary bundles
//! share that one source of truth. Non-SDK JSON create bodies fail at the boundary.
//!
//! The in-module unit test already covers the durable-only catalog-id fallback; this
//! binary drives the untested SDK surface: multipart create, list, retrieve, the
//! `versions` subresource (create / list / retrieve / content / delete), strict
//! create content types, and the error arms.

use awaken_protocol_managed::skills_router;
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
fn router_with_store() -> (
    Router,
    std::sync::Arc<dyn awaken_resource_contract::SkillStore>,
    std::path::PathBuf,
) {
    let dir = std::env::temp_dir().join(format!(
        "awaken-skillsapi-http-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let store = support::resources::filesystem_skill_store(dir.join("store"));
    let resources = support::resources::resources(store.clone());
    (
        skills_router(Some(store.clone()), resources.purge_scheduler()),
        store,
        dir,
    )
}
static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const BOUNDARY: &str = "X-SKILL-BOUNDARY";

fn in_test_workspace(mut request: Request<Body>) -> Request<Body> {
    request
        .headers_mut()
        .insert("anthropic-beta", "skills-2025-10-02".parse().unwrap());
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

#[tokio::test]
async fn ga_skill_projection_and_query_only_selector_match_official_sdk() {
    // Cause/effect graph: C1 no beta selector, C2 GA display_name multipart,
    // C3 source=custom list, C4 complete version lifecycle, C5 the post-GA SDK
    // retains `beta=true` while omitting the Skills capability header. Effects:
    // E1 Skill uses display_name/latest_version_id/source object and no beta fields;
    // E2 list is PageCursor; E3 SkillVersion omits beta directory/version; E4 the
    // Beta-namespace-only archive/file endpoints remain reachable through the
    // query-only selector. Decision table: G1 C1+C2->E1; G2 C1+C3->E2;
    // G3 C1+C4->E3; G4 C5->E1; G5 C4+C5->E4. The create/retrieve/list/delete Skill
    // operations and complete Version lifecycle all execute without a Skills
    // capability header, over one SkillStore aggregate.
    let (router, _store, dir) = router_with_store();
    let body = multipart_files_with_fields(
        &[(
            "ga-skill/SKILL.md",
            b"---\nname: ga-skill\ndescription: ga\n---\n",
        )],
        &[("display_name", "GA Skill")],
    );
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/skills?beta=true")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    let (status, skill) = read(router.clone().oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "G1 {skill}");
    assert_eq!(skill["display_name"], "GA Skill", "G1/E1");
    assert_eq!(skill["source"]["type"], "custom", "G1/E1");
    assert!(skill["latest_version_id"].is_string(), "G1/E1");
    assert!(skill.get("display_title").is_none(), "G1/E1");
    let id = skill["id"].as_str().unwrap().to_owned();

    let mut request = Request::builder()
        .uri(format!("/v1/skills/{id}"))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    let (status, retrieved) = read(router.clone().oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "G1/retrieve {retrieved}");
    assert_eq!(retrieved["id"], id, "G1/retrieve");

    let mut request = Request::builder()
        .uri(format!("/v1/skills/{id}?beta=true"))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    let (status, change_point) = read(router.clone().oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "G4 {change_point}");
    assert_eq!(change_point["display_name"], "GA Skill", "G4/E1");
    assert_eq!(change_point["source"]["type"], "custom", "G4/E1");
    assert!(change_point["latest_version_id"].is_string(), "G4/E1");
    assert!(change_point.get("display_title").is_none(), "G4/E1");

    let mut request = Request::builder()
        .uri("/v1/skills?beta=true&source=custom")
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    let (status, list) = read(router.clone().oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "G2");
    assert!(list["next_page"].is_null(), "G2/E2");
    assert!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == id),
        "G2/E2"
    );

    let latest = skill["latest_version_id"].as_str().unwrap().to_owned();
    let mut request = Request::builder()
        .uri(format!("/v1/skills/{id}/versions/{latest}?beta=true"))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    let (status, version) = read(router.clone().oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "G3 {version}");
    assert!(version.get("directory").is_none(), "G3/E3");
    assert!(version.get("version").is_none(), "G3/E3");
    assert_eq!(version["name"], "ga-skill", "G3/E3");

    let mut request = Request::builder()
        .uri(format!(
            "/v1/skills/{id}/versions/{latest}/content?beta=true"
        ))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "G5/archive");
    assert_eq!(
        response.headers()["content-type"],
        "application/x-tar",
        "G5/E4"
    );

    let mut request = Request::builder()
        .uri(format!(
            "/v1/skills/{id}/versions/{latest}/files/SKILL.md?beta=true"
        ))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "G5/file");
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert!(
        bytes
            .windows(b"name: ga-skill".len())
            .any(|window| { window == b"name: ga-skill" }),
        "G5/E4"
    );

    let mut request = Request::builder()
        .uri(format!("/v1/skills/{id}/versions?beta=true"))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    let (status, versions) = read(router.clone().oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "G3/list {versions}");
    assert_eq!(versions["data"].as_array().unwrap().len(), 1, "G3/list");

    let next_body = multipart_files(&[(
        "ga-skill/SKILL.md",
        b"---\nname: ga-skill\ndescription: ga-v2\n---\n",
    )]);
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("/v1/skills/{id}/versions?beta=true"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(next_body))
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    let (status, created_version) = read(router.clone().oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "G3/create {created_version}");
    let created_version_id = created_version["id"].as_str().unwrap();

    let mut request = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/skills/{id}/versions/{created_version_id}?beta=true"
        ))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    let (status, deleted_version) = read(router.clone().oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "G3/delete {deleted_version}");
    assert_eq!(deleted_version["id"], created_version_id, "G3/delete");

    let mut request = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/skills/{id}?beta=true"))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    let (status, deleted_skill) = read(router.clone().oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "G1/delete {deleted_skill}");
    assert_eq!(deleted_skill["id"], id, "G1/delete");
    let _ = std::fs::remove_dir_all(dir);
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
    let (router, store, dir) = router_with_store();
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
    assert!(
        version.get("files").is_none(),
        "bundle internals stay off wire"
    );
    let stored_versions = store.list_versions("test", zip_id).await.unwrap();
    let stored = stored_versions.last().unwrap();
    assert!(stored.files.iter().any(|file| file.path == "SKILL.md"));
    assert!(
        stored
            .files
            .iter()
            .any(|file| file.path == "references/api.md")
    );
    assert!(
        stored
            .files
            .iter()
            .find(|file| file.path == "scripts/run.sh")
            .is_some_and(|file| file.executable)
    );
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
    assert!(edited_version.get("file_entries").is_none());
    let stored_versions = store.list_versions("test", zip_id).await.unwrap();
    assert!(
        stored_versions
            .last()
            .unwrap()
            .files
            .iter()
            .find(|file| file.path == "tools/helper")
            .is_some_and(|file| file.executable)
    );

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
    let (router, _store, dir) = router_with_store();
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
    let (router, _store, dir) = router_with_store();
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
    let (router, _store, dir) = router_with_store();
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
    let (router, _store, dir) = router_with_store();

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
async fn non_sdk_json_create_is_rejected_before_store_selection() {
    // Cause/effect rule: JSON content type is not the SDK multipart contract, so
    // admission returns 400 whether or not a durable store is configured. Store
    // availability must not revive a removed parallel create path.
    let resources = support::resources::ephemeral_resources();
    let router = skills_router(None, resources.purge_scheduler());
    let (status, v) = post_json(
        &router,
        "/v1/skills",
        json!({ "id": "greeter", "content": SKILL_V1 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
}

#[tokio::test]
async fn non_sdk_json_create_cannot_mutate_the_durable_store() {
    // Decision rule: valid-looking legacy payload + durable store -> 400 and an
    // empty official list. This proves deletion of the duplicate ingress rather
    // than merely hiding its response fields.
    let (router, _store, dir) = router_with_store();
    let (status, v) = post_json(
        &router,
        "/v1/skills",
        json!({ "id": "greeter", "content": SKILL_V1 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    let (status, list) = get(&router, "/v1/skills").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!list.contains("greeter"));
    let _ = std::fs::remove_dir_all(&dir);
}

// The one SDK create path fails closed when its durable owner is unavailable.
#[tokio::test]
async fn sdk_multipart_create_fails_closed_without_a_durable_store() {
    let resources = support::resources::ephemeral_resources();
    let router = skills_router(None, resources.purge_scheduler());
    // Multipart fails closed (409) when nothing durable backs it.
    let (status, created) = post_multipart(&router, "/v1/skills", SKILL_V1).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "SDK create must fail closed with no durable store: {created}"
    );
}

#[tokio::test]
async fn error_arms_are_fail_closed() {
    let (router, _store, dir) = router_with_store();

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
    let (router, _store, dir) = router_with_store();
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
