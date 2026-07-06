//! Deployment-form-independent e2e for the open standalone: it drives the
//! assembled, guarded session surface over the built-in `HelloModel` — no
//! external provider, no durable store, no environment. The same assertions hold
//! wherever this router is mounted, which is the point: enforcement and the agent
//! loop are properties of the open runtime, not of a deployment.

use std::sync::Arc;

use awaken_standalone::{HelloModel, Standalone, banner, boot, build};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

/// One request through a freshly-built standalone (no shared session state),
/// optionally bearing the seeded api key. Returns the HTTP status only.
async fn call(method: &str, path: &str, with_key: bool) -> StatusCode {
    let standalone = build(Arc::new(HelloModel));
    let mut builder = Request::builder().method(method).uri(path);
    if with_key {
        builder = builder.header("authorization", format!("Bearer {}", standalone.api_token));
    }
    let request = builder.body(Body::empty()).expect("request");
    standalone
        .router
        .oneshot(request)
        .await
        .expect("router call")
        .status()
}

/// One request against a shared router (clone per call — `Router` is cheap to
/// clone, and cloning preserves the shared session state), with an optional JSON
/// body and bearer key. Returns `(status, parsed-json)`.
async fn request(
    router: &axum::Router,
    method: &str,
    path: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {key}"));
    let request = match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&value).expect("body")))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    };
    let response = router.clone().oneshot(request).await.expect("router call");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body bytes");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

// --- enforcement (auth is required on every axis) ---------------------------

#[tokio::test]
async fn the_bare_session_surface_requires_a_credential() {
    assert_eq!(
        call("POST", "/v1/sessions", false).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_project_session_surface_requires_a_credential() {
    assert_eq!(
        call("POST", "/projects/local/v1/sessions", false).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_read_on_the_bare_surface_also_requires_a_credential() {
    assert_eq!(
        call("GET", "/v1/sessions/sesn_1", false).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn an_unauthored_project_is_not_found() {
    assert_eq!(
        call("POST", "/projects/ghost/v1/sessions", true).await,
        StatusCode::NOT_FOUND
    );
}

// --- the agent loop runs end-to-end, on both addressing modes ---------------

/// Create a session, send one user message, and return the agent's reply texts.
async fn converse(router: &axum::Router, key: &str, prefix: &str) -> Vec<String> {
    let (status, session) = request(
        router,
        "POST",
        &format!("{prefix}/v1/sessions"),
        key,
        Some(json!({ "agent": "assistant" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create session: {session}");
    let id = session["id"].as_str().expect("session id").to_string();

    let (status, _) = request(
        router,
        "POST",
        &format!("{prefix}/v1/sessions/{id}/events"),
        key,
        Some(json!({
            "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "send message");

    let (status, list) = request(
        router,
        "GET",
        &format!("{prefix}/v1/sessions/{id}/events"),
        key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list events");
    list["data"]
        .as_array()
        .expect("events data")
        .iter()
        .filter(|event| event["type"] == "agent.message")
        .map(|event| {
            event["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn a_full_agent_turn_runs_on_the_bare_surface() {
    let Standalone {
        router, api_token, ..
    } = build(Arc::new(HelloModel));
    let replies = converse(&router, &api_token, "").await;
    assert!(
        replies
            .iter()
            .any(|r| r.contains("Hello from awaken-standalone")),
        "the agent replied: {replies:?}"
    );
}

#[tokio::test]
async fn a_full_agent_turn_runs_under_the_project_prefix() {
    let Standalone {
        router, api_token, ..
    } = build(Arc::new(HelloModel));
    let replies = converse(&router, &api_token, "/projects/local").await;
    assert!(
        replies
            .iter()
            .any(|r| r.contains("Hello from awaken-standalone")),
        "the agent replied under the project prefix: {replies:?}"
    );
}

#[tokio::test]
async fn a_created_session_round_trips_through_retrieve() {
    let Standalone {
        router, api_token, ..
    } = build(Arc::new(HelloModel));
    let (status, session) = request(
        &router,
        "POST",
        "/v1/sessions",
        &api_token,
        Some(json!({ "agent": "assistant" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = session["id"].as_str().expect("session id").to_string();

    let (status, retrieved) = request(
        &router,
        "GET",
        &format!("/v1/sessions/{id}"),
        &api_token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retrieved["id"].as_str(), Some(id.as_str()));
}

#[tokio::test]
async fn multiple_turns_accumulate_agent_replies() {
    let Standalone {
        router, api_token, ..
    } = build(Arc::new(HelloModel));
    let (_, session) = request(
        &router,
        "POST",
        "/v1/sessions",
        &api_token,
        Some(json!({ "agent": "assistant" })),
    )
    .await;
    let id = session["id"].as_str().expect("session id").to_string();

    for text in ["first", "second"] {
        let (status, _) = request(
            &router,
            "POST",
            &format!("/v1/sessions/{id}/events"),
            &api_token,
            Some(json!({
                "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, list) = request(
        &router,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        &api_token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let replies = list["data"]
        .as_array()
        .expect("events")
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .count();
    assert!(
        replies >= 2,
        "two turns yield at least two agent replies, got {replies}"
    );
}

// --- boot + banner (the single-machine hand-off) ----------------------------

#[test]
fn boot_seeds_two_distinct_keys() {
    let standalone = boot();
    assert!(standalone.admin_token.starts_with("sk-ant-"));
    assert!(standalone.api_token.starts_with("sk-ant-"));
    assert_ne!(standalone.admin_token, standalone.api_token);
}

#[test]
fn the_banner_names_the_keys_and_the_addressing() {
    let standalone = boot();
    let text = banner(&standalone, "127.0.0.1:9999");
    assert!(text.contains(&standalone.admin_token));
    assert!(text.contains(&standalone.api_token));
    assert!(text.contains("/projects/local/v1/sessions"));
    assert!(text.contains("127.0.0.1:9999"));
}

// --- run() serves over a real TCP socket (the binary's serve path) ----------

#[tokio::test]
async fn run_binds_serves_over_tcp_and_enforces_auth() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (addr_tx, addr_rx) = tokio::sync::oneshot::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let mut addr_tx = Some(addr_tx);

    // Serve on an ephemeral port until we signal shutdown; report the bound addr.
    let server = tokio::spawn(async move {
        awaken_standalone::run(
            "127.0.0.1:0",
            async {
                let _ = stop_rx.await;
            },
            move |standalone, local| {
                // `on_bound` sees the seeded keys + the bound address.
                assert!(standalone.api_token.starts_with("sk-ant-"));
                let _ = addr_tx.take().expect("bound once").send(local);
            },
        )
        .await
    });

    let local = addr_rx.await.expect("bound address");

    // A raw HTTP/1.1 request over the real socket — no client dep. Unauthenticated,
    // so the guard answers 401 before any handler, proving the served router
    // enforces auth end to end.
    let mut stream = tokio::net::TcpStream::connect(local)
        .await
        .expect("connect");
    stream
        .write_all(b"GET /v1/sessions/x HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("write request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 401"),
        "expected 401 status line, got: {:?}",
        text.lines().next()
    );

    let _ = stop_tx.send(());
    server.await.expect("join server").expect("serve ok");
}

#[test]
fn print_banner_does_not_panic() {
    let standalone = boot();
    awaken_standalone::print_banner(&standalone, "127.0.0.1:1".parse().expect("addr"));
}

#[tokio::test]
async fn run_surfaces_a_bind_error_instead_of_panicking() {
    // An unbindable address is an `Err`, not a panic — the binary maps it to a
    // clean exit message.
    let result =
        awaken_standalone::run("this is not an address", std::future::pending(), |_, _| {}).await;
    assert!(result.is_err());
}
