//! The user-profiles front door over HTTP: create/retrieve/update/list plus the
//! enrollment_url action, including the metadata-merge (empty-string removes) and
//! relationship-default semantics the SDK documents.
mod support;

use std::sync::Arc;

use awaken_data_subject_application::DataSubjectApplication;
use awaken_data_subject_store::InMemoryDataSubjectRepo;
use awaken_protocol_managed::{
    MANAGED_BETA, ManagedCapability, USER_PROFILES_BETA, enforce_managed_beta, user_profiles_router,
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
        DataSubjectApplication::new(
            repo,
            b"test-user-profile-enrollment-key!!".to_vec(),
            awaken_time::system_clock(),
        )
        .expect("valid test enrollment key"),
    );
    user_profiles_router(application, "org_test".into())
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> support::Call<serde_json::Value> {
    call_with_beta(app, method, uri, USER_PROFILES_BETA, body).await
}

async fn call_with_beta(
    app: &Router,
    method: &str,
    uri: &str,
    beta: &str,
    body: Option<Value>,
) -> support::Call<serde_json::Value> {
    let mut b = Request::builder().method(method).uri(uri);
    b = b.header("anthropic-beta", beta);
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
    support::Call::new(status, value)
}

#[tokio::test]
async fn beta_capability_selects_one_exact_user_profile_projection() {
    // Causal graph: one durable profile -> capability selector -> one wire
    // vocabulary. The selector, not SDK/User-Agent metadata, is the only cause.
    //
    // Decision table:
    // | selector | access_type request | response fields | effect |
    // | 03-24    | absent              | relationship    | accept |
    // | 03-24    | present             | n/a             | reject before write |
    // | 08-18    | present             | access_type, no legacy field | accept |
    // | both     | any                 | n/a             | reject as ambiguous |
    //
    // Cross-projection invariant: reading the current-created aggregate through
    // the legacy capability omits only the new vocabulary; identity and legacy
    // relationship remain stable.
    let app = app();
    let current = ManagedCapability::UserProfilesCurrent.beta();
    let created = call_with_beta(
        &app,
        "POST",
        "/v1/user_profiles",
        current,
        Some(json!({
            "access_type": "passthrough",
            "name": "Resold",
            "external_user_onboarded_at": "2026-09-03T01:02:03Z"
        })),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(created["access_type"], "passthrough");
    assert!(created.get("relationship").is_none());
    assert_eq!(
        created["external_user_onboarded_at"],
        "2026-09-03T01:02:03Z"
    );
    let id = created["id"].as_str().unwrap();

    let legacy = call(&app, "GET", &format!("/v1/user_profiles/{id}"), None)
        .await
        .expect(StatusCode::OK);
    assert!(legacy.get("access_type").is_none());
    assert!(legacy.get("external_user_onboarded_at").is_none());
    assert_eq!(legacy["relationship"], "resold");

    let alpha = call_with_beta(
        &app,
        "POST",
        "/v1/user_profiles",
        current,
        Some(json!({ "access_type": "application", "name": "Alpha" })),
    )
    .await
    .expect(StatusCode::OK);
    let by_name = call_with_beta(
        &app,
        "GET",
        "/v1/user_profiles?order_by=name&order=asc",
        current,
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(by_name["data"][0]["id"], alpha["id"]);

    let _ = call_with_beta(
        &app,
        "POST",
        "/v1/user_profiles",
        current,
        Some(json!({ "external_user_onboarded_at": "not-a-timestamp" })),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "invalid timestamp is pre-write");

    let _ = call(
        &app,
        "POST",
        "/v1/user_profiles",
        Some(json!({ "access_type": "application" })),
    )
    .await
    .expect(StatusCode::BAD_REQUEST);

    let combined = format!("{USER_PROFILES_BETA}, {current}");
    let _ = call_with_beta(
        &app,
        "GET",
        &format!("/v1/user_profiles/{id}"),
        &combined,
        None,
    )
    .await
    .expect(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn external_user_details_exist_only_under_the_2026_09_04_selector() {
    // Decision table (selector x external_user_details in request -> effect):
    // | 03-24 | present | reject before write            |
    // | 08-18 | present | reject before write            |
    // | 09-04 | present | accept; response states every field, null when unsaid |
    // | 09-04 | absent  | accept; response carries the record (all null) |
    // | 08-18 | read    | field absent from the response |
    // | 09-04 | patch   | unnamed fields kept, null clears, value replaces |
    let app = app();
    let current = ManagedCapability::UserProfilesCurrent.beta();
    let external = ManagedCapability::UserProfilesExternal.beta();
    let details =
        json!({ "external_user_details": { "account_status": "active", "country": "DE" } });
    for (selector, rule) in [(USER_PROFILES_BETA, "03-24"), (current, "08-18")] {
        let body = call_with_beta(
            &app,
            "POST",
            "/v1/user_profiles",
            selector,
            Some(details.clone()),
        )
        .await
        .expect_for(StatusCode::BAD_REQUEST, rule);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("user-profiles-2026-09-04"),
            "{rule}: the refusal names the selector: {body}"
        );
    }
    let created = call_with_beta(
        &app,
        "POST",
        "/v1/user_profiles",
        external,
        Some(json!({
            "access_type": "application",
            "external_user_details": {
                "account_status": "active",
                "country": "DE",
                "onboarded_at": "2026-09-03T01:02:03Z"
            }
        })),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        created["access_type"], "application",
        "09-04 keeps the current vocabulary"
    );
    assert_eq!(
        created["external_user_details"],
        json!({
            "account_status": "active",
            "country": "DE",
            "email_hash": null,
            "entity_type": null,
            "name_hash": null,
            "onboarded_at": "2026-09-03T01:02:03Z",
            "reference_id": null
        }),
        "every field is present, null until stated"
    );
    let id = created["id"].as_str().unwrap();
    let read_current = call_with_beta(
        &app,
        "GET",
        &format!("/v1/user_profiles/{id}"),
        current,
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert!(
        read_current.get("external_user_details").is_none(),
        "08-18 omits the newer vocabulary"
    );
    let bare = call_with_beta(&app, "POST", "/v1/user_profiles", external, Some(json!({})))
        .await
        .expect(StatusCode::OK);
    assert_eq!(
        bare["external_user_details"]["country"],
        Value::Null,
        "unstated record is all null"
    );
    let patched = call_with_beta(
        &app,
        "POST",
        &format!("/v1/user_profiles/{id}"),
        external,
        Some(json!({ "external_user_details": { "country": null, "entity_type": "business" } })),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        patched["external_user_details"]["account_status"], "active",
        "unnamed kept"
    );
    assert_eq!(
        patched["external_user_details"]["country"],
        Value::Null,
        "null clears"
    );
    assert_eq!(
        patched["external_user_details"]["entity_type"], "business",
        "value replaces"
    );
    let _ = call_with_beta(
        &app,
        "POST",
        "/v1/user_profiles",
        external,
        Some(json!({ "external_user_details": { "country": "Germany" } })),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "country must be an alpha-2 code");
    // The official client sends its default 08-18 selector beside the 09-04
    // opt-in on one header: the current family is cumulative and the newest
    // selector decides; the legacy selector stays exclusive.
    let cumulative = format!("{current}, {external}");
    let read = call_with_beta(
        &app,
        "GET",
        &format!("/v1/user_profiles/{id}"),
        &cumulative,
        None,
    )
    .await
    .expect_for(
        StatusCode::OK,
        "08-18 + 09-04 selects the external projection",
    );
    assert_eq!(read["external_user_details"]["entity_type"], "business");
    let exclusive = format!("{USER_PROFILES_BETA}, {external}");
    let _ = call_with_beta(
        &app,
        "GET",
        &format!("/v1/user_profiles/{id}"),
        &exclusive,
        None,
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "03-24 + 09-04 is refused");
}

// Test design: malformed_user_profile_query_uses_anthropic_error_envelope
// Cause/effect graph: malformed public query -> shared ManagedQuery boundary ->
// SDK-decodable invalid_request_error; the profile application is never called.
// Decision table: order={asc,desc} -> typed list input; order={unknown} -> 400
// JSON error envelope with the rejected field in the diagnostic.
#[tokio::test]
async fn malformed_user_profile_query_uses_anthropic_error_envelope() {
    let body = call(&app(), "GET", "/v1/user_profiles?order=newest", None)
        .await
        .expect(StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("order"))
    );
    let body = call(&app(), "GET", "/v1/user_profiles?order_by=updated_at", None)
        .await
        .expect(StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

// Test design: user_profile_crud_and_metadata_merge
// Cause/effect graph: one UserProfile aggregate owns create/retrieve/update/list/enrollment and metadata merge semantics.
// Decision table: omitted=preserve; null/empty removes where specified; conflicting access vocabulary=400; unknown=404.
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
    let p = call(
        &app,
        "POST",
        "/v1/user_profiles",
        Some(json!({ "external_id": "u-1", "metadata": { "a": "1", "keep": "x" } })),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(p["type"], "user_profile");
    assert_eq!(p["relationship"], "external");
    assert_eq!(p["external_id"], "u-1");
    assert!(p["trust_grants"].is_object());
    let id = p["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("uprof_"));

    // Retrieve.
    let got = call(&app, "GET", &format!("/v1/user_profiles/{id}"), None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(got["id"], id);

    // Update — replace name + relationship, merge metadata (delete `a` via "",
    // add `b`, keep `keep`).
    let up = call(
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
    .await
    .expect(StatusCode::OK);
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
    let page = call(&app, "GET", "/v1/user_profiles", None)
        .await
        .expect(StatusCode::OK);
    assert!(page.get("has_more").is_none());
    assert!(page["next_page"].is_null());
    let ids: Vec<&str> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![id.as_str()]);

    // Enrollment URL.
    let enr = call(
        &app,
        "POST",
        &format!("/v1/user_profiles/{id}/enrollment_url"),
        None,
    )
    .await
    .expect(StatusCode::OK);
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
        let _ = call(&app, method, &path, None)
            .await
            .expect_for(StatusCode::NOT_FOUND, "expected status");
    }
}

#[tokio::test]
async fn over_long_fields_are_rejected() {
    // Decision rule L1: name/external_id length >255 -> 400 before application
    // mutation. The adjacent CRUD rule owns the <=255 success combinations.
    let app = app();
    let _ = call(
        &app,
        "POST",
        "/v1/user_profiles",
        Some(json!({ "name": "x".repeat(256) })),
    )
    .await
    .expect(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn user_profiles_paginate_by_anthropic_page_cursor() {
    // Cause/effect table: C1 rows=3; C2 limit=2; C3 cursor={absent,page1-last};
    // E1 first page has two rows+cursor; E2 resume has one non-overlapping row;
    // E3 terminal next_page=null. R1 C3=absent -> E1; R2 C3=cursor -> E2+E3.
    let app = app();
    for i in 0..3 {
        let _ = call(
            &app,
            "POST",
            "/v1/user_profiles",
            Some(json!({ "external_id": format!("u-{i}") })),
        )
        .await
        .expect(StatusCode::OK);
    }

    // First page of 2 → more remain, next_page names the last row of the page.
    let p1 = call(&app, "GET", "/v1/user_profiles?limit=2", None)
        .await
        .expect(StatusCode::OK);
    let ids1: Vec<&str> = p1["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids1.len(), 2);
    assert!(p1.get("has_more").is_none());
    assert_eq!(p1["next_page"], ids1[1]);

    // RunResume with `?page=<next_page>` → the remaining row, terminal (next_page null).
    let cursor = p1["next_page"].as_str().unwrap();
    let p2 = call(
        &app,
        "GET",
        &format!("/v1/user_profiles?page={cursor}"),
        None,
    )
    .await
    .expect(StatusCode::OK);
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
    assert!(p2.get("has_more").is_none());
}

#[tokio::test]
async fn user_profiles_require_their_own_beta_family() {
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Causes: C1 User Profiles path; C2 header={absent,managed,pinned,latest}.
    // Effects: E1 absent/wrong family -> 400; E2 either SDK-generated User
    // Profiles beta reaches the one handler. Constraint: Managed Agents beta
    // must not authorize its sibling API.
    //
    // Decision table:
    // | Rule | header | effect |
    // | B1 | absent | 400 |
    // | B2 | managed-agents | 400 |
    // | B3 | SDK 0.117.1 user-profiles-2026-03-24 | handler |
    // | B4 | current SDK user-profiles-2026-08-18 | handler |
    let app = app().layer(axum::middleware::from_fn(enforce_managed_beta));
    for (header, expected) in [
        (None, StatusCode::BAD_REQUEST),
        (Some(MANAGED_BETA), StatusCode::BAD_REQUEST),
        (Some(USER_PROFILES_BETA), StatusCode::OK),
        (
            Some(ManagedCapability::UserProfilesCurrent.beta()),
            StatusCode::OK,
        ),
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
