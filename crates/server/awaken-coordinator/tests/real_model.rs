//! Real-model conformance: drive the A2A adapter over a **live** Anthropic-compatible
//! endpoint (e.g. Kimi's `https://api.kimi.com/coding/v1/`) through the genai
//! provider, and assert a real `message:send` returns a completed `Task` with the
//! model's reply.
//!
//! Gated with `#[ignore]` because it needs network + credentials. Run explicitly:
//!
//! ```sh
//! KIMI_API_KEY=sk-... cargo test -p awaken-coordinator --test real_model -- --ignored
//! ```
//!
//! Env: `KIMI_API_KEY` (required), `KIMI_BASE_URL` (default the Kimi coding
//! endpoint), `KIMI_MODEL` (default `kimi-for-coding`). Any Anthropic
//! Messages-API gateway works by overriding the base URL and model.

use std::sync::Arc;

use awaken_provider_genai::GenaiExecutor;
use awaken_scenario_host::build_router;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn a2a_message_send_over_a_live_kimi_model() {
    // Causes: the fixtures below establish `a2a message send over a live kimi model` with the
    // concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Coverage rationale: `a2a message send over a live kimi model` is one independent branch
    // selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let key = std::env::var("KIMI_API_KEY")
        .expect("set KIMI_API_KEY (the Kimi/Anthropic-compatible key) to run this test");
    let base = std::env::var("KIMI_BASE_URL")
        .unwrap_or_else(|_| "https://api.kimi.com/coding/v1/".to_string());
    let model = std::env::var("KIMI_MODEL").unwrap_or_else(|_| "kimi-for-coding".to_string());

    let executor = GenaiExecutor::anthropic_compatible(base, key);
    let app = build_router(Arc::new(executor), model);

    // One A2A Run against the live model. The prompt steers a short, tool-free
    // reply so the assertion is stable across models.
    let (status, body) = call(
        &app,
        "POST",
        "/v1/a2a/message:send",
        json!({ "message": {
            "messageId": "m1",
            "contextId": "kimi-real-1",
            "role": "ROLE_USER",
            "parts": [{ "text": "Reply with exactly the word PONG and nothing else. Do not use any tools." }]
        }}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "live message:send failed: {body}");
    let task = serde_json::from_str::<Value>(&body).expect("task json")["task"].clone();
    assert_eq!(
        task["status"]["state"], "completed",
        "the live Run should complete: {body}"
    );

    let reply = task["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !reply.trim().is_empty(),
        "the model returned an empty reply: {body}"
    );
    assert!(
        reply.to_uppercase().contains("PONG"),
        "the model reply should contain PONG, got: {reply:?}"
    );
    eprintln!("live kimi reply: {reply:?}");
}
