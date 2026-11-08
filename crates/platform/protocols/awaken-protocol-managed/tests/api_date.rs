//! API-date selection traverses the canonical Session HTTP router and store.
mod support;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    body: Value,
    date: &str,
    history_v2: bool,
) -> Value {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("anthropic-version", date);
    if history_v2 {
        request = request.header("x-awaken-session-time-version", "2");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(if body.is_null() {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{method} {path}");
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn session_api_date_is_consistent_across_crud_pages_threads_and_history_time() {
    // Causes: C1 legacy/current API date; C2 create/retrieve/update/list/thread/
    // archive response; C3 ordinary vs history-time v2. Effects: A1 current
    // snapshots always carry execution_identity; A2 legacy snapshots never gain
    // it; A3 history-time choice cannot override identity/date selection; A4
    // one canonical Session id and immutable Agent version survive every read.
    // Cross-product decision rows cover both dates and all response families;
    // exact source tests are independent of deployed IAM/provider acceptance.
    for date in ["2023-06-01", "2026-10-21"] {
        let state = support::shared_state(
            awaken_protocol_managed_testkit::CoordinatedRuntimeFake::default(),
        )
        .await;
        let app = support::edge_stamped(awaken_protocol_managed::router(state));
        let created = call(
            &app,
            "POST",
            "/v1/sessions",
            json!({"agent":"coder",
            "environment_id":awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID}),
            date,
            false,
        )
        .await;
        let id = created["id"].as_str().unwrap();
        let current = date == "2026-10-21";
        let check = |agent: &Value| {
            assert_eq!(agent.get("execution_identity").is_some(), current);
            if current {
                assert_eq!(
                    agent["execution_identity"],
                    json!({"type":"service_account"})
                );
            }
            assert_eq!(agent["id"], created["agent"]["id"]);
            assert_eq!(agent["version"], created["agent"]["version"]);
        };
        check(&created["agent"]);
        check(
            &call(
                &app,
                "POST",
                &format!("/v1/sessions/{id}"),
                json!({"title":"API date"}),
                date,
                false,
            )
            .await["agent"],
        );
        for history_v2 in [false, true] {
            check(
                &call(
                    &app,
                    "GET",
                    &format!("/v1/sessions/{id}"),
                    Value::Null,
                    date,
                    history_v2,
                )
                .await["agent"],
            );
            let listed = call(&app, "GET", "/v1/sessions", Value::Null, date, history_v2).await;
            assert_eq!(listed["data"][0]["id"], id);
            check(&listed["data"][0]["agent"]);
            let threads = call(
                &app,
                "GET",
                &format!("/v1/sessions/{id}/threads"),
                Value::Null,
                date,
                history_v2,
            )
            .await;
            check(&threads["data"][0]["agent"]);
            let tid = threads["data"][0]["id"].as_str().unwrap();
            check(
                &call(
                    &app,
                    "GET",
                    &format!("/v1/sessions/{id}/threads/{tid}"),
                    Value::Null,
                    date,
                    history_v2,
                )
                .await["agent"],
            );
        }
        check(
            &call(
                &app,
                "POST",
                &format!("/v1/sessions/{id}/archive"),
                json!({}),
                date,
                false,
            )
            .await["agent"],
        );
    }
}
