//! The user-profiles front door over HTTP: create/retrieve/update/list plus the
//! enrollment_url action, including the metadata-merge (empty-string removes) and
//! relationship-default semantics the SDK documents.

use std::sync::Arc;

use awaken_protocol_managed::{UserProfileState, user_profiles_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> Router {
    user_profiles_router(Arc::new(UserProfileState::new()))
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
            "metadata": { "a": "", "b": "2" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(up["name"], "Acme");
    assert_eq!(up["relationship"], "resold");
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
    assert!(enr["url"].as_str().unwrap().contains(&id));
    assert!(enr["expires_at"].is_string());

    // Unknown ids 404 on retrieve/update/enrollment.
    for (method, path) in [
        ("GET", format!("/v1/user_profiles/uprof_missing")),
        (
            "POST",
            format!("/v1/user_profiles/uprof_missing/enrollment_url"),
        ),
    ] {
        let (s, _) = call(&app, method, &path, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{method} {path}");
    }
}

#[tokio::test]
async fn over_long_fields_are_rejected() {
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
