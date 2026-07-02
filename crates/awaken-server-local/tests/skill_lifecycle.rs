//! End-to-end skill lifecycle through the *Managed Agents* protocol (ADR-0036).
//!
//! Drives the real kernel over the public `/v1/sessions...` wire and proves the
//! whole path from **offer** to **use** through the single `Skill` tool:
//!
//!   offer   (SkillSpec → registry → the one `Skill` tool + catalog descriptor)
//!     → advertise (the `Skill` tool is in the model's list; its description lists
//!                  the skill by id — never a per-skill tool)
//!     → call    (model invokes `Skill { skill: "deploy" }`; allowed, not parked)
//!     → deliver (runtime executes the `Skill` RawTool, returns the instructions)
//!     → use     (the loop continues; the model replies)
//!
//! A control case with no skills proves the `Skill` tool is *not* ambient: it
//! appears only because a skill was offered.

use std::sync::Arc;

use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
};
use awaken_server_local::{SkillSpec, build_router, build_router_with_skills};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

const SKILL_ID: &str = "deploy";
const SKILL_TOOL: &str = "Skill";
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

fn skill_spec() -> SkillSpec {
    SkillSpec::new(SKILL_ID, "deploy", "Run the deploy checklist", SKILL_BODY)
        .with_when_to_use("shipping a release")
}

// ── the model under test ─────────────────────────────────────────────────────

/// Turn 0: assert the single `Skill` tool is advertised and its catalog lists the
/// offered skill, then activate it (or report its absence). Turn 1: having
/// received the skill instructions, reply — proving the loop continued.
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
            // The one `Skill` tool must be advertised, and its description (the
            // catalog) must name the offered skill by id. There must be no
            // per-skill tool: no tool id other than `Skill` is skill-derived.
            let skill_tool = request.tools.iter().find(|t| t.id == SKILL_TOOL);
            let advertised = skill_tool
                .map(|t| t.description.contains(SKILL_ID))
                .unwrap_or(false);
            let leaked_per_skill = request
                .tools
                .iter()
                .any(|t| t.id != SKILL_TOOL && t.id.to_ascii_lowercase().starts_with("skill"));
            if !advertised || leaked_per_skill {
                return Ok(ChatResponse {
                    output: AssistantOutput::text("NO_SKILL_ADVERTISED"),
                    usage: None,
                });
            }
            return Ok(ChatResponse {
                output: AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "s1".into(),
                    tool_id: SKILL_TOOL.into(),
                    arguments: serde_json::json!({ "skill": SKILL_ID }),
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
async fn offered_skill_is_advertised_activated_and_used() {
    let app = build_router_with_skills(Arc::new(SkillUserModel), "scripted", vec![skill_spec()]);
    let id = create_session(&app).await;

    // One managed turn drives the whole loop: the skill activation is allowed (not
    // parked), so tool_use → tool_result → message → idle all land in one response.
    let list = send_message(&app, &id, "please deploy").await;
    let types = event_types(&list);

    // called: the model saw the single `Skill` tool and activated the skill.
    let tool_use = event(&list, "agent.tool_use").expect("skill was activated");
    assert_eq!(
        tool_use["name"], SKILL_TOOL,
        "the single Skill tool was invoked"
    );

    // delivered: the runtime executed the `Skill` RawTool and returned the body.
    let tool_result = event(&list, "agent.tool_result").expect("skill produced a result");
    let delivered = tool_result["content"][0]["text"].as_str().unwrap();
    assert!(
        delivered.contains(SKILL_BODY),
        "the activated skill's instructions were delivered: {delivered:?}"
    );

    // used: the loop continued and the model replied.
    let message = event(&list, "agent.message").expect("model replied after the skill");
    assert_eq!(message["content"][0]["text"], "USED_SKILL");

    // allowed, not parked: the turn ended cleanly in one shot.
    let idle = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(idle["type"], "session.status_idle");
    assert_eq!(idle["stop_reason"]["type"], "end_turn");
    assert!(
        !types.contains(&"NO_SKILL_ADVERTISED".to_string()),
        "sanity: the skill must have been advertised"
    );
}

#[tokio::test]
async fn skill_tool_is_not_ambient_without_offering_a_skill() {
    // Same model, but no skill offered: the `Skill` tool must not be advertised.
    let app = build_router(Arc::new(SkillUserModel), "scripted");
    let id = create_session(&app).await;

    let list = send_message(&app, &id, "please deploy").await;

    // No tool call happened; the model reported the skill absent.
    assert!(
        event(&list, "agent.tool_use").is_none(),
        "no Skill tool without an offered skill"
    );
    let message = event(&list, "agent.message").expect("model replied");
    assert_eq!(
        message["content"][0]["text"], "NO_SKILL_ADVERTISED",
        "the Skill tool appears only because a skill was offered (not ambient)"
    );
}
