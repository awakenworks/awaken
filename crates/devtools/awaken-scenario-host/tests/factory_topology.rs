//! Factory self-tests: prove each `build_*_router` scenario factory assembles the
//! topology its NAME promises, by DRIVING the constructed router in-process (tower
//! `oneshot`) and asserting the runtime it wired — not by re-reading its source.
//!
//! Harness trustworthiness (this is the whole point): a miswired factory otherwise
//! surfaces only as a downstream `e2e/*.mjs` failure driving a real binary, hard to
//! localize. These tests are network-free and deterministic — the scenario models
//! run in-process (no `AWAKEN_MODEL_SOURCE`), so a drift in a factory's assembly is
//! caught locally. They match `models.rs`'s style: one behavior per test, named for
//! the claim it pins, driven through the real router the mode builds.
//!
//! The seam exercised is the AI SDK adapter (`POST /v1/ai-sdk/chat`), which every
//! `mount`-based factory exposes and which drives one real turn against the host's
//! model — so the assistant text (or the tool the turn parks on) is the observable
//! that proves the factory wired the intended model/host.

use awaken_scenario_host::{
    build_acp_sandboxed_router, build_config_router, build_custom_router, build_delegation_router,
    build_echo_router, build_vision_router, build_worker_router,
};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Drive one AI SDK turn (`POST /v1/ai-sdk/chat`) with a single user message and
/// collect the decoded UI-stream frames. This runs the host's model end to end and
/// projects the committed turn, so the frames are the factory's observable behavior.
async fn drive_turn(app: axum::Router, user_text: &str) -> Vec<Value> {
    let body = json!({
        "messages": [{ "role": "user", "parts": [{ "type": "text", "text": user_text }] }]
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "the turn is served");
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .collect()
}

/// The assistant's committed text, reconstructed from the ordered `text-delta`
/// frames (the AI SDK projects the committed assistant message as text deltas).
fn assistant_text(frames: &[Value]) -> String {
    frames
        .iter()
        .filter(|f| f["type"] == "text-delta")
        .filter_map(|f| f["delta"].as_str())
        .collect()
}

/// The names of the tools the turn surfaced as an authoritative tool call
/// (`tool-input-available`), so a client-executed-tool factory is provable.
fn tool_names(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .filter(|f| f["type"] == "tool-input-available")
        .filter_map(|f| f["toolName"].as_str().map(str::to_string))
        .collect()
}

/// `GET`, returning the status only — used to prove a differential route exists on
/// a special factory but is absent (404) on the plain echo mount.
async fn get_status(app: axum::Router, uri: &str) -> StatusCode {
    app.oneshot(
        Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

#[tokio::test]
async fn echo_factory_drives_a_turn_and_reflects_the_user() {
    // The baseline: the default mode wires the `EchoModel` over the shared mount, so
    // a turn reflects the user's text — proving the AI SDK seam + host are assembled.
    let frames = drive_turn(build_echo_router(), "probe-42").await;
    assert_eq!(assistant_text(&frames), "Echo: probe-42");
}

#[tokio::test]
async fn vision_factory_wires_the_media_reporting_model() {
    // `build_vision_router` must wire the `VisionProbeModel` (not the echo model):
    // its reply names the media it saw. A text-only turn reports "saw no media",
    // which the echo model would never say — so this pins the model the factory chose.
    let frames = drive_turn(build_vision_router(), "what color").await;
    assert_eq!(assistant_text(&frames), "saw no media; text: what color");
}

#[tokio::test]
async fn custom_factory_exposes_the_client_executed_submit_answer_tool() {
    // The custom-tool factory declares `submit_answer` as a client-executed tool and
    // drives the `CustomToolModel`, which calls it on the first turn. The turn must
    // therefore PARK on a `submit_answer` tool call (surfaced authoritatively), not
    // answer with text — proving `with_client_tools` reached the run.
    let frames = drive_turn(build_custom_router(), "solve it").await;
    assert!(
        tool_names(&frames).iter().any(|n| n == "submit_answer"),
        "the turn parked on the client-executed submit_answer tool: {frames:?}"
    );
}

#[tokio::test]
async fn delegation_factory_routes_a_delegated_sub_run_to_completion() {
    // `build_delegation_router` wires the `agent_run` delegation tool + the
    // `researcher` roster and the `DelegatingModel`. A turn must delegate (server-side
    // sub-run), then report the delegate's answer — the whole delegate loop running
    // to a final text proves the delegation topology (roster + tool), not just a model.
    let frames = drive_turn(build_delegation_router(), "go research this").await;
    assert_eq!(assistant_text(&frames), "delegate said: researched: 42");
}

#[tokio::test]
async fn worker_factory_mounts_the_environments_surface_absent_from_a_plain_mount() {
    // The worker factory merges `/v1/environments` (self-hosted work dispatch) onto
    // the mount; the plain echo mount has no such route. The differential (200 here,
    // 404 there) proves the environments merge — the topology the mode's name claims.
    assert_eq!(
        get_status(build_worker_router(), "/v1/environments").await,
        StatusCode::OK,
        "the worker factory mounts /v1/environments"
    );
    assert_eq!(
        get_status(build_echo_router(), "/v1/environments").await,
        StatusCode::NOT_FOUND,
        "the plain echo mount does not"
    );
}

#[tokio::test]
async fn acp_sandboxed_factory_also_mounts_the_environments_surface() {
    // The sandboxed-ACP factory shares the environments merge (a session's networking
    // policy must reach the sandbox launch), so `/v1/environments` is present here too.
    assert_eq!(
        get_status(build_acp_sandboxed_router(), "/v1/environments").await,
        StatusCode::OK,
    );
}

// Multi-thread: `build_config_router` seeds the admin assistant, whose publish
// resolves a model through `CatalogModelResolver`, which bridges the async catalog
// snapshot via `block_in_place` (valid only on a multi-thread runtime).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_factory_mounts_the_config_agents_plane_absent_from_a_plain_mount() {
    // `build_config_router` merges the config data plane (`/v1/config/agents`) onto the
    // mount and seeds the admin assistant; the plain echo mount has no config plane.
    // The differential proves the config-plane merge (topology), and the 200 proves the
    // in-memory SQLite config store + seeding assembled without error.
    assert_eq!(
        get_status(build_config_router().await, "/v1/config/agents").await,
        StatusCode::OK,
        "the config factory mounts /v1/config/agents"
    );
    assert_eq!(
        get_status(build_echo_router(), "/v1/config/agents").await,
        StatusCode::NOT_FOUND,
        "the plain echo mount does not",
    );
}
