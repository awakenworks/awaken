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
//! `mount`-based factory exposes and which drives one real Run against the host's
//! model — so the assistant text (or the tool the Run awaits on) is the observable
//! that proves the factory wired the intended model/host.

use awaken_scenario_host::{
    build_acp_sandboxed_router_with_deployment, build_config_router, build_custom_router,
    build_delegation_router, build_echo_router, build_vision_router, build_worker_router,
};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Drive one AI SDK Run (`POST /v1/ai-sdk/chat`) with a single user message and
/// collect the decoded UI-stream frames. This runs the host's model end to end and
/// projects the committed Run, so the frames are the factory's observable behavior.
async fn drive_run(app: axum::Router, user_text: &str) -> Vec<Value> {
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
    assert_eq!(response.status(), StatusCode::OK, "the Run is served");
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

/// The names of the tools the Run surfaced as an authoritative tool call
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
async fn echo_factory_drives_a_run_and_reflects_the_user() {
    // Causes: C1 the default factory receives one AI SDK User input. Effects:
    // E1 the shared mount drives one Run and returns the EchoModel reply.
    // Constraints/invariants: the factory selects the existing shared mount and
    // EchoModel; it owns no parallel Run or response path. Decision rule F1:
    // C1 -> E1 with the exact reflected text.
    let frames = drive_run(build_echo_router(), "probe-42").await;
    assert_eq!(assistant_text(&frames), "Echo: probe-42");
}

#[tokio::test]
async fn vision_factory_wires_the_media_reporting_model() {
    // Causes: C1 the vision factory receives a text-only Run. Effects: E1 the
    // VisionProbeModel reports the text and explicit absence of media.
    // Constraints/invariants: the factory must select VisionProbeModel through
    // the shared mount, never substitute the echo fixture. Decision rule F2:
    // C1 -> E1=`saw no media; text: what color`.
    let frames = drive_run(build_vision_router(), "what color").await;
    assert_eq!(assistant_text(&frames), "saw no media; text: what color");
}

#[tokio::test]
async fn custom_factory_exposes_the_client_executed_submit_answer_tool() {
    // Causes: C1 the custom factory receives a first-Step User input and declares
    // `submit_answer` as client-executed. Effects: E1 CustomToolModel emits that
    // authoritative tool call and the Run awaits instead of answering with text.
    // Constraints/invariants: `with_client_tools` is the sole execution-policy
    // source; the factory cannot host-execute or synthesize the result. Decision
    // rule F3: C1 -> E1 with one surfaced `submit_answer` call.
    let frames = drive_run(build_custom_router(), "solve it").await;
    assert!(
        tool_names(&frames).iter().any(|n| n == "submit_answer"),
        "the Run awaiting on the client-executed submit_answer tool: {frames:?}"
    );
}

#[tokio::test]
async fn delegation_factory_wires_the_fixed_managed_coordination_surface() {
    // Causes: C1 the factory installs a frozen researcher roster and C2 Managed
    // projection replaces authored delegation with the fixed coordination tools.
    // Effects: E1 list_agents precedes E2 send_to_agent and E3 the coordinator
    // ends on an admission receipt, never the child payload. Decision table:
    // R1(C1+C2)->E1+E2+E3; missing roster/tool wiring cannot produce all three.
    // Child settlement is asynchronous and belongs to the real-process Managed
    // E2E; this in-process factory pin intentionally stops at the admission seam.
    // Constraints/invariants: the frozen roster and fixed coordination tools are
    // the only topology inputs; this factory must not turn admission into a
    // synchronous child-result path.
    let frames = drive_run(build_delegation_router(), "go research this").await;
    assert_eq!(tool_names(&frames), ["list_agents", "send_to_agent"]);
    let reply = assistant_text(&frames);
    assert!(reply.starts_with("coordination accepted: "), "{reply}");
    assert!(!reply.contains("researched: 42"), "{reply}");
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
    // Extracting its startup into `acp_scenarios` introduces no new branch or
    // effect, so no new decision table applies; this end-to-end route observation
    // is the regression coverage for the structural move.
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = awaken_runtime_host::SandboxTier::Local;
    assert_eq!(
        get_status(
            build_acp_sandboxed_router_with_deployment(deployment).await,
            "/v1/environments",
        )
        .await,
        StatusCode::OK,
    );
}

// Multi-thread matches the production host; admin-assistant publication resolves
// through the scenario's async catalog adapter.
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
