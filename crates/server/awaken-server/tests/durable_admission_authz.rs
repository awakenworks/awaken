//! Admission-time safety + trust-boundary pins for the durable-ingress HTTP
//! surface (`/v1/durable/threads/{thread}/...`), driven through the *real*
//! run-serving router (`build_router` → `mount`) via `oneshot`.
//!
//! Three admission concerns, all asserted against current behavior:
//!
//! 1. **Poison payload** — a malformed durable `submit_background` / `cancel`
//!    (invalid JSON, missing required field, wrong type) is rejected with a clean
//!    `4xx` at admission and enqueues **no** dispatch row: the run is refused
//!    BEFORE it can reach the queue, never a `5xx`/panic and never a poisoned run
//!    that fails later at drive time.
//!
//! 2. **Oversized payload** — the router carries axum's default body limit (2 MiB);
//!    an over-limit body is rejected cleanly (`413`), not accepted unboundedly.
//!
//! 3. **Authz / trust boundary** — the run-serving router (managed `/v1/sessions`,
//!    ai-sdk, a2a, AND the durable-ingress verbs) is the local-trust plane and
//!    carries **no** bearer auth: IAM lives on the *separate* management/config
//!    plane (`build_secured_management_router`, exercised by `management_authz`).
//!    These tests PIN that boundary: the durable routes share the exact same open
//!    posture as their managed siblings, so a durable route is not bypassing auth
//!    that a sibling enforces (no sibling on this plane enforces any). An
//!    unauthenticated durable submit/cancel is admitted (and then judged purely on
//!    payload), identically to an unauthenticated `/v1/sessions` create.
//!
//! The durable pool is engaged process-wide for this test binary via
//! `AWAKEN_INGRESS=durable` + an on-disk `AWAKEN_STORAGE_DIR` (the sqlite dispatch
//! queue), mirroring how the durable e2e drivers configure the server. Env is set
//! exactly once, race-free, through a `LazyLock` every test touches before it
//! builds a router (the `LazyLock` `Once` synchronizes the single write
//! happens-before every later env read).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;

use awaken_scenario_host::{EchoModel, build_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tower::ServiceExt;

/// The durable dispatch queue is one process-shared sqlite file and the
/// `/dispatches` observation endpoint reports it globally (all threads), so a
/// concurrent valid submit in one test would inflate another's count. Serialize
/// every test in this binary through one gate: within a gated window no *other*
/// test submits, so a queue-count delta is attributable to the window's own
/// operations alone.
static GATE: Mutex<()> = Mutex::const_new(());

/// Engage durable ingress for this test binary's process, backed by an on-disk
/// sqlite dispatch queue (not the volatile in-memory footgun). Runs its body
/// exactly once; every test dereferences it before building a router, so the two
/// `set_var` writes happen-before every later env read (no data race).
static DURABLE: LazyLock<PathBuf> = LazyLock::new(|| {
    let dir = std::env::temp_dir().join(format!("awaken-durable-admission-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create durable storage dir");
    // SAFETY: the LazyLock `Once` runs this closure a single time with no other
    // thread able to observe the vars until it returns; after that the values are
    // constant, so all subsequent reads are synchronized-after these writes.
    unsafe {
        std::env::set_var("AWAKEN_INGRESS", "durable");
        std::env::set_var("AWAKEN_STORAGE_DIR", &dir);
    }
    dir
});

fn app() -> Router {
    // Touch the durable init before the router reads any of its env.
    let _ = &*DURABLE;
    build_router(Arc::new(EchoModel), "echo")
}

/// Send a request with an explicit raw body + optional Authorization header and
/// return `(status, parsed-json-or-null)`.
async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    content_type: Option<&str>,
    auth: Option<&str>,
    body: Body,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(ct) = content_type {
        b = b.header("content-type", ct);
    }
    if let Some(token) = auth {
        b = b.header("authorization", format!("Bearer {token}"));
    }
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

/// A durable JSON POST with an explicit body string (so we can send malformed
/// JSON that would not round-trip through `serde_json`).
async fn post_raw(app: &Router, uri: &str, raw: &str) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        uri,
        Some("application/json"),
        None,
        Body::from(raw.to_string()),
    )
    .await
}

async fn post_json(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    post_raw(app, uri, &serde_json::to_string(&body).unwrap()).await
}

/// The number of dispatch rows currently queued for `thread` (via the durable-ops
/// observation endpoint). Zero for a thread that never had a valid submit.
async fn dispatch_count(app: &Router, thread: &str) -> usize {
    let (status, body) = send(
        app,
        "GET",
        &format!("/v1/durable/threads/{thread}/dispatches"),
        None,
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "dispatches observation must be reachable in durable mode: {body}"
    );
    body["dispatches"]
        .as_array()
        .map(|rows| rows.len())
        .unwrap_or(0)
}

/// Concern 1: a poison durable `submit_background` (invalid JSON, missing `text`,
/// wrong `text` type) is rejected with a clean 4xx at admission and enqueues NO
/// dispatch row — the run never reaches the queue to fail later at drive time.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_durable_submit_is_rejected_4xx_and_enqueues_nothing() {
    let _gate = GATE.lock().await;
    let app = app();
    let thread = "admission-malformed-submit";
    let uri = format!("/v1/durable/threads/{thread}/submit_background");

    // Snapshot the queue size before any malformed traffic. Under the gate no
    // other test submits, so the only thing that can grow this is our own calls.
    let before = dispatch_count(&app, thread).await;

    // (a) Structurally invalid JSON — the extractor rejects before the handler.
    let (status, _) = post_raw(&app, &uri, "{ this is not json ").await;
    assert!(
        status.is_client_error(),
        "invalid JSON must be a clean 4xx, got {status}"
    );
    assert!(!status.is_server_error(), "must never 5xx on poison JSON");

    // (b) Valid JSON but the required `text` field is missing → handler 400.
    let (status, body) = post_json(&app, &uri, json!({ "agent": "assistant" })).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "missing `text` must be 400 at admission: {body}"
    );
    assert!(body["error"].is_string(), "clean error envelope: {body}");

    // (c) Wrong type — `text` is a number, not a string → handler 400.
    let (status, body) = post_json(&app, &uri, json!({ "text": 12345 })).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "wrong-typed `text` must be 400 at admission: {body}"
    );

    // (d) Empty body with the JSON content-type → still a clean 4xx, not a 5xx.
    let (status, _) = post_raw(&app, &uri, "").await;
    assert!(
        status.is_client_error() && !status.is_server_error(),
        "empty body must be a clean 4xx, got {status}"
    );

    // The admission rejections enqueued nothing: the queue did not grow (the
    // background pool may only drain/settle pre-existing rows, never add one, so
    // "no new enqueue" is exactly `after <= before`).
    let after = dispatch_count(&app, thread).await;
    assert!(
        after <= before,
        "a rejected malformed submit must NOT create a dispatch row (before={before}, after={after})"
    );
}

/// Concern 1 (cancel verb): a poison durable `cancel` is likewise a clean 4xx —
/// missing `run_id`, wrong type, and an unknown `run_id` all fail closed with 400,
/// never a 5xx/panic and never a silent success.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_durable_cancel_is_rejected_4xx() {
    let _gate = GATE.lock().await;
    let app = app();
    let thread = "admission-malformed-cancel";
    let uri = format!("/v1/durable/threads/{thread}/cancel");

    // Missing `run_id` → 400 at admission.
    let (status, body) = post_json(&app, &uri, json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "missing run_id: {body}");

    // Wrong type → 400.
    let (status, _) = post_json(&app, &uri, json!({ "run_id": 7 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Invalid JSON → clean 4xx (extractor), never a 5xx.
    let (status, _) = post_raw(&app, &uri, "}{").await;
    assert!(status.is_client_error() && !status.is_server_error());

    // A well-formed cancel for an unknown run id fails CLOSED (400), never a
    // silent 200 and never a 5xx.
    let (status, body) = post_json(&app, &uri, json!({ "run_id": "run-does-not-exist" })).await;
    assert!(
        status.is_client_error() && !status.is_server_error(),
        "unknown run_id must fail closed with a 4xx, got {status}: {body}"
    );
}

/// Concern 2: the run-serving router carries axum's default 2 MiB body limit — an
/// over-limit durable submit is rejected cleanly (413), not accepted unboundedly
/// (no OOM, no 5xx). This pins that a size guard EXISTS on the durable path.
#[tokio::test(flavor = "multi_thread")]
async fn oversized_durable_submit_is_rejected_by_the_body_limit() {
    let _gate = GATE.lock().await;
    let app = app();
    let thread = "admission-oversized";
    let uri = format!("/v1/durable/threads/{thread}/submit_background");

    let before = dispatch_count(&app, thread).await;

    // A ~3 MiB JSON body, over axum's 2 MiB default limit.
    let big = "a".repeat(3 * 1024 * 1024);
    let raw = serde_json::to_string(&json!({ "text": big })).unwrap();
    let (status, _) = post_raw(&app, &uri, &raw).await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "an over-limit body must be rejected with 413, got {status}"
    );

    // And it enqueued nothing (the queue did not grow).
    let after = dispatch_count(&app, thread).await;
    assert!(
        after <= before,
        "an over-limit submit must NOT create a dispatch row (before={before}, after={after})"
    );
}

/// Concern 3 (trust boundary): the durable-ingress plane carries NO bearer auth —
/// exactly like its managed siblings on the same run-serving router. This test
/// PINS that boundary so a regression that (a) starts rejecting local durable
/// callers or (b) silently diverges the durable routes' auth posture from the
/// siblings' is caught. An unauthenticated durable submit/cancel is admitted and
/// judged purely on payload — never answered with 401/403.
#[tokio::test(flavor = "multi_thread")]
async fn durable_plane_shares_its_managed_siblings_open_local_trust_posture() {
    let _gate = GATE.lock().await;
    let app = app();

    // Sibling baseline: an unauthenticated managed session create is admitted
    // (this plane is open by design — IAM is the separate management router).
    let (status, body) = post_json(&app, "/v1/sessions", json!({ "agent": "assistant" })).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the managed sibling is open (no bearer auth on the run-serving plane): {body}"
    );

    // Durable submit WITHOUT any Authorization header is NOT an auth rejection: a
    // valid payload is accepted (200) and an invalid one is a payload 400 — never
    // 401/403. Same open posture as the sibling above.
    let thread = "trust-boundary-thread";
    let (status, body) = post_json(
        &app,
        &format!("/v1/durable/threads/{thread}/submit_background"),
        json!({ "text": "hello from an unauthenticated local caller" }),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "durable submit is not authed"
    );
    assert_ne!(
        status,
        StatusCode::FORBIDDEN,
        "durable submit is not authed"
    );
    assert_eq!(
        status,
        StatusCode::OK,
        "an unauthenticated valid durable submit is admitted on the local-trust plane: {body}"
    );
    assert_eq!(body["queued"], json!(true), "the run was queued: {body}");

    // Durable cancel WITHOUT auth is likewise never an auth rejection (it fails on
    // payload/state instead — here an unknown run id fails closed with a 4xx).
    let (status, _) = post_json(
        &app,
        &format!("/v1/durable/threads/{thread}/cancel"),
        json!({ "run_id": "run-unknown" }),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "durable cancel is not authed"
    );
    assert_ne!(
        status,
        StatusCode::FORBIDDEN,
        "durable cancel is not authed"
    );

    // A garbage bearer token is ignored (not honored, not rejected) — the plane
    // simply does not consult Authorization, confirming the trust boundary is the
    // network/loopback, not a token.
    let (status, _) = send(
        &app,
        "POST",
        &format!("/v1/durable/threads/{thread}/cancel"),
        Some("application/json"),
        Some("totally-bogus-token"),
        Body::from(json!({ "run_id": "run-unknown" }).to_string()),
    )
    .await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
    assert_ne!(status, StatusCode::FORBIDDEN);
}
