//! End-to-end skill lifecycle through the *Managed Agents* protocol (ADR-0035).
//!
//! Drives the real kernel over the public `/v1/sessions...` wire and proves the
//! whole path from **provision** to **use**:
//!
//!   provision (SkillMount → SandboxProvider → Environment: skill tool + descriptor)
//!     → advertise (descriptor reaches the model's tool list)
//!     → call    (model invokes `skill__<id>`; allowed, not parked)
//!     → deliver (runtime executes the skill RawTool, returns the body)
//!     → use     (the loop continues; the model replies)
//!
//! A control case with no provisioning proves the skill is *not* ambient: it
//! appears only because it was provisioned.

use std::sync::Arc;

use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
};
use awaken_server_local::{
    SkillMount, build_router, build_router_with_skills, content_fingerprint,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

const SKILL_ID: &str = "deploy";
const SKILL_TOOL: &str = "skill__deploy";
const SKILL_BODY: &str = "# Deploy checklist\nSTEP-ALPHA: run migrations before shipping.";

// ── managed-protocol harness (the same wire the SDK speaks) ──────────────────

async fn json_call(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(serde_json::to_vec(&body).unwrap())
        })
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{method} {uri}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn event_types(list: &serde_json::Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

async fn create_session(app: &Router) -> String {
    let s = json_call(
        app,
        "POST",
        "/v1/sessions",
        serde_json::json!({ "agent": "assistant" }),
    )
    .await;
    s["id"].as_str().unwrap().to_string()
}

async fn send_message(app: &Router, session: &str, text: &str) -> serde_json::Value {
    json_call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }] }),
    )
    .await;
    json_call(
        app,
        "GET",
        &format!("/v1/sessions/{session}/events"),
        serde_json::Value::Null,
    )
    .await
}

fn event<'a>(list: &'a serde_json::Value, ty: &str) -> Option<&'a serde_json::Value> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == ty)
}

fn skill_mount() -> SkillMount {
    SkillMount {
        id: SKILL_ID.into(),
        version: 1,
        content_hash: content_fingerprint(SKILL_BODY.as_bytes()),
        name: "deploy".into(),
        description: "Run the deploy checklist".into(),
        body: SKILL_BODY.into(),
    }
}

// ── the model under test ─────────────────────────────────────────────────────

/// Turn 0: assert the provisioned skill is advertised, then call it (or report
/// its absence). Turn 1: having received the skill body, reply — proving the loop
/// continued after the skill executed.
struct SkillUserModel;

#[async_trait::async_trait]
impl LlmExecutor for SkillUserModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let tool_results = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count();
        if tool_results == 0 {
            // The provisioned skill must be visible in the model's tool list.
            if !request.tools.iter().any(|t| t.id == SKILL_TOOL) {
                return Ok(ChatResponse {
                    output: AssistantOutput::text("NO_SKILL_ADVERTISED"),
                    usage: None,
                });
            }
            return Ok(ChatResponse {
                output: AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "s1".into(),
                    tool_id: SKILL_TOOL.into(),
                    arguments: serde_json::json!({}),
                }]),
                usage: None,
            });
        }
        Ok(ChatResponse {
            output: AssistantOutput::text("USED_SKILL"),
            usage: None,
        })
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn provisioned_skill_is_advertised_called_and_used() {
    let app = build_router_with_skills(Arc::new(SkillUserModel), "scripted", vec![skill_mount()]);
    let id = create_session(&app).await;

    // One managed turn drives the whole loop: the skill is allowed (not parked),
    // so tool_use → tool_result → message → idle all land in one response.
    let list = send_message(&app, &id, "please deploy").await;
    let types = event_types(&list);

    // called: the model saw and invoked the provisioned skill
    let tool_use = event(&list, "agent.tool_use").expect("skill was called");
    assert_eq!(
        tool_use["name"], SKILL_TOOL,
        "the provisioned skill was invoked"
    );

    // delivered: the runtime executed the skill RawTool and returned the body
    let tool_result = event(&list, "agent.tool_result").expect("skill produced a result");
    assert_eq!(
        tool_result["content"][0]["text"], SKILL_BODY,
        "the provisioned skill body was delivered to the conversation"
    );

    // used: the loop continued and the model replied
    let message = event(&list, "agent.message").expect("model replied after the skill");
    assert_eq!(message["content"][0]["text"], "USED_SKILL");

    // allowed, not parked: the turn ended cleanly in one shot
    let idle = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(idle["type"], "session.status_idle");
    assert_eq!(idle["stop_reason"]["type"], "end_turn");
    assert!(
        !types.contains(&"NO_SKILL_ADVERTISED".to_string()),
        "sanity: the skill must have been advertised"
    );
}

#[tokio::test]
async fn skill_is_not_ambient_without_provisioning() {
    // Same model, but no skill provisioned: the model must not see `skill__deploy`.
    let app = build_router(Arc::new(SkillUserModel), "scripted");
    let id = create_session(&app).await;

    let list = send_message(&app, &id, "please deploy").await;

    // No tool call happened; the model reported the skill absent.
    assert!(
        event(&list, "agent.tool_use").is_none(),
        "no skill without provisioning"
    );
    let message = event(&list, "agent.message").expect("model replied");
    assert_eq!(
        message["content"][0]["text"], "NO_SKILL_ADVERTISED",
        "the skill appears only because it was provisioned (not ambient)"
    );
}
