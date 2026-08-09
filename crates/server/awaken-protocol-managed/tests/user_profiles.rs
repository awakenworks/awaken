//! The user-profiles front door over HTTP: create/retrieve/update/list plus the
//! enrollment_url action, including the metadata-merge (empty-string removes) and
//! relationship-default semantics the SDK documents.

use std::sync::Arc;

use awaken_data_subject_application::DataSubjectApplication;
use awaken_data_subject_store::InMemoryDataSubjectRepo;
use awaken_protocol_managed::{
    MANAGED_BETA, USER_PROFILES_BETA, enforce_managed_beta, user_profiles_router,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> Router {
    let repo = Arc::new(InMemoryDataSubjectRepo::new());
    let application = Arc::new(
        DataSubjectApplication::new(repo, b"test-user-profile-enrollment-key!!".to_vec())
            .expect("valid test enrollment key"),
    );
    user_profiles_router(application, "org_test".into())
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn user_profile_crud_and_metadata_merge() {
    // Cause/effect graph: C1 operation={create,retrieve,update,list,enroll}; C2
    // profile={present,missing}; C3 patch={metadata delete/upsert, relationship,
    // name,trust grant}; C4 enrollment token={opaque signed}. Effects: E1 one
    // application-backed aggregate is projected consistently; E2 mixed patch
    // preserves untouched facts; E3 missing reads/actions are 404; E4 enrollment
    // returns an opaque signed URL. Decision rules R1 present+C1+C3 -> E1+E2;
    // R2 missing+C1 -> E3; R3 present+enroll+C4 -> E4.
    let app = app();

    // Create — relationship defaults to `external`, trust_grants is present+empty.
    let (s, p) = call(
        &app,
        "POST",
        "/v1/user_profiles",
        Some(json!({ "external_id": "u-1", "metadata": { "a": "1", "keep": "x" } })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(p["type"], "user_profile");
    assert_eq!(p["relationship"], "external");
    assert_eq!(p["external_id"], "u-1");
    assert!(p["trust_grants"].is_object());
    let id = p["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("uprof_"));

    // Retrieve.
    let (s, got) = call(&app, "GET", &format!("/v1/user_profiles/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["id"], id);

    // Update — replace name + relationship, merge metadata (delete `a` via "",
    // add `b`, keep `keep`).
    let (s, up) = call(
        &app,
        "POST",
        &format!("/v1/user_profiles/{id}"),
        Some(json!({
            "name": "Acme",
            "relationship": "resold",
            "metadata": { "a": "", "b": "2" },
            "trust_grants": { "calendar": { "status": "pending" } }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(up["name"], "Acme");
    assert_eq!(up["relationship"], "resold");
    assert_eq!(up["trust_grants"]["calendar"]["status"], "pending");
    assert_eq!(up["metadata"]["b"], "2");
    assert_eq!(up["metadata"]["keep"], "x");
    assert!(
        up["metadata"].get("a").is_none(),
        "empty-string value removes the key"
    );

    // List — one full page, our profile present.
    let (s, page) = call(&app, "GET", "/v1/user_profiles", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page["has_more"], false);
    assert!(page["next_page"].is_null());
    let ids: Vec<&str> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![id.as_str()]);

    // Enrollment URL.
    let (s, enr) = call(
        &app,
        "POST",
        &format!("/v1/user_profiles/{id}/enrollment_url"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(enr["type"], "enrollment_url");
    assert!(enr["url"].as_str().unwrap().starts_with("/enroll/"));
    assert!(
        !enr["url"].as_str().unwrap().contains(&id),
        "signed enrollment state is opaque on the URL"
    );
    assert!(enr["expires_at"].is_string());

    // Unknown ids 404 on retrieve/update/enrollment.
    for (method, path) in [
        ("GET", "/v1/user_profiles/uprof_missing".to_string()),
        (
            "POST",
            "/v1/user_profiles/uprof_missing/enrollment_url".to_string(),
        ),
    ] {
        let (s, _) = call(&app, method, &path, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{method} {path}");
    }
}

#[tokio::test]
async fn over_long_fields_are_rejected() {
    // Decision rule L1: name/external_id length >255 -> 400 before application
    // mutation. The adjacent CRUD rule owns the <=255 success combinations.
    let app = app();
    let (s, _) = call(
        &app,
        "POST",
        "/v1/user_profiles",
        Some(json!({ "name": "x".repeat(256) })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn user_profiles_paginate_by_anthropic_page_cursor() {
    // Cause/effect table: C1 rows=3; C2 limit=2; C3 cursor={absent,page1-last};
    // E1 first page has two rows+cursor; E2 resume has one non-overlapping row;
    // E3 terminal next_page=null. R1 C3=absent -> E1; R2 C3=cursor -> E2+E3.
    let app = app();
    for i in 0..3 {
        let (s, _) = call(
            &app,
            "POST",
            "/v1/user_profiles",
            Some(json!({ "external_id": format!("u-{i}") })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    // First page of 2 → more remain, next_page names the last row of the page.
    let (_, p1) = call(&app, "GET", "/v1/user_profiles?limit=2", None).await;
    let ids1: Vec<&str> = p1["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids1.len(), 2);
    assert_eq!(p1["has_more"], true);
    assert_eq!(p1["next_page"], ids1[1]);

    // RunResume with `?page=<next_page>` → the remaining row, terminal (next_page null).
    let cursor = p1["next_page"].as_str().unwrap();
    let (_, p2) = call(
        &app,
        "GET",
        &format!("/v1/user_profiles?page={cursor}"),
        None,
    )
    .await;
    let ids2: Vec<&str> = p2["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids2.len(), 1);
    assert_ne!(ids2[0], ids1[1], "no overlap with the first page");
    assert!(
        p2["next_page"].is_null(),
        "the last page has no continuation cursor"
    );
    assert_eq!(p2["has_more"], false);
}

#[tokio::test]
async fn user_profiles_require_their_own_beta_family() {
    // Causes: C1 User Profiles path; C2 header={absent,managed,user-profiles}.
    // Effects: E1 absent/wrong family -> 400; E2 exact User Profiles beta reaches
    // the handler. Constraint: Managed Agents beta must not authorize its sibling
    // API. Rules B1 C2=absent -> E1; B2 C2=managed -> E1; B3 C2=profile -> E2.
    let app = app().layer(axum::middleware::from_fn(enforce_managed_beta));
    for (header, expected) in [
        (None, StatusCode::BAD_REQUEST),
        (Some(MANAGED_BETA), StatusCode::BAD_REQUEST),
        (Some(USER_PROFILES_BETA), StatusCode::OK),
    ] {
        let mut request = Request::get("/v1/user_profiles");
        if let Some(header) = header {
            request = request.header("anthropic-beta", header);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "header={header:?}");
    }
}
