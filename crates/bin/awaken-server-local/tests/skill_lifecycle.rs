//! End-to-end skill lifecycle through the *Managed Agents* protocol (ADR-0036).
//!
//! Drives the real kernel over the public `/v1/sessions...` wire and proves the
//! two-tool skill surface from **discover** to **use**:
//!
//!   offer   (SkillSpec → registry → `list_skills` + `Skill`, both catalog-free)
//!     → discover (model calls `list_skills`; the catalog comes back as data)
//!     → activate (model calls `Skill { skill }`; instructions returned)
//!     → use      (the loop continues; the model replies)
//!
//! Also proves discovery is not baked into the descriptors (they carry neither the
//! catalog nor the body — ADR-0036 D2/D7), and that the tools are not ambient when
//! no skill is offered.

use std::sync::Arc;

use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
};
use awaken_server_local::{SkillSpec};
use awaken_scenario_host::{build_router, build_router_with_skills};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

const SKILL_ID: &str = "deploy";
const SKILL_TOOL: &str = "Skill";
const LIST_TOOL: &str = "list_skills";
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

fn events_of<'a>(list: &'a serde_json::Value, ty: &str) -> Vec<&'a serde_json::Value> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == ty)
        .collect()
}

fn message_texts(list: &serde_json::Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["content"][0]["text"].as_str().map(str::to_string))
        .collect()
}

fn skill_spec() -> SkillSpec {
    SkillSpec::new(SKILL_ID, "deploy", "Run the deploy checklist", SKILL_BODY)
        .with_when_to_use("shipping a release")
}

// ── the model under test ─────────────────────────────────────────────────────

/// Step 0: assert both skill tools are advertised and carry no catalog/body, then
/// discover with `list_skills`. Step 1: activate with `Skill`. Step 2: reply.
/// Steps are told apart by how many tool results are already in the transcript.
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
        match tool_results {
            0 => {
                let has_list = request.tools.iter().any(|t| t.id == LIST_TOOL);
                let has_skill = request.tools.iter().any(|t| t.id == SKILL_TOOL);
                if !has_list || !has_skill {
                    return Ok(ChatResponse {
                        output: AssistantOutput::text("NO_SKILL_ADVERTISED"),
                        usage: None,
                        stop_reason: None,
                    });
                }
                // Discovery is a `list_skills` call, not the descriptor: neither
                // tool descriptor may carry the catalog (id) or the body.
                let leaked = request.tools.iter().any(|t| {
                    t.description.contains(SKILL_ID) || t.description.contains(SKILL_BODY)
                });
                if leaked {
                    return Ok(ChatResponse {
                        output: AssistantOutput::text("CATALOG_IN_DESCRIPTOR"),
                        usage: None,
                        stop_reason: None,
                    });
                }
                Ok(ChatResponse {
                    output: AssistantOutput::from_tool_calls(vec![ToolCall {
                        call_id: "l1".into(),
                        tool_id: LIST_TOOL.into(),
                        arguments: serde_json::json!({}),
                    }]),
                    usage: None,
                    stop_reason: None,
                })
            }
            1 => Ok(ChatResponse {
                output: AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "s1".into(),
                    tool_id: SKILL_TOOL.into(),
                    arguments: serde_json::json!({ "skill": SKILL_ID }),
                }]),
                usage: None,
                stop_reason: None,
            }),
            _ => Ok(ChatResponse {
                output: AssistantOutput::text("USED_SKILL"),
                usage: None,
                stop_reason: None,
            }),
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn offered_skill_is_discovered_activated_and_used() {
    let app = build_router_with_skills(Arc::new(SkillUserModel), "scripted", vec![skill_spec()]);
    let id = create_session(&app).await;

    let list = send_message(&app, &id, "please deploy").await;
    let texts = message_texts(&list);

    // descriptors stayed catalog-free (else the model would have flagged it).
    assert!(!texts.contains(&"CATALOG_IN_DESCRIPTOR".to_string()));
    assert!(!texts.contains(&"NO_SKILL_ADVERTISED".to_string()));

    // discovered + activated: both tools were invoked, in order.
    let tool_uses: Vec<String> = events_of(&list, "agent.tool_use")
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        tool_uses.contains(&LIST_TOOL.to_string()),
        "list_skills invoked: {tool_uses:?}"
    );
    assert!(
        tool_uses.contains(&SKILL_TOOL.to_string()),
        "Skill invoked: {tool_uses:?}"
    );

    // the catalog came back from `list_skills` as data (id present, body absent);
    // the activation returned the body.
    let results: Vec<String> = events_of(&list, "agent.tool_result")
        .iter()
        .map(|e| e["content"][0]["text"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        results
            .iter()
            .any(|r| r.contains(SKILL_ID) && !r.contains(SKILL_BODY)),
        "list_skills returned the catalog (metadata, no body): {results:?}"
    );
    assert!(
        results.iter().any(|r| r.contains(SKILL_BODY)),
        "Skill returned the instructions body: {results:?}"
    );

    // used: the loop continued and the model replied; ended cleanly.
    assert!(texts.contains(&"USED_SKILL".to_string()));
    let idle = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(idle["type"], "session.status_idle");
    assert_eq!(idle["stop_reason"]["type"], "end_turn");
}

#[tokio::test]
async fn skill_tools_are_not_ambient_without_offering_a_skill() {
    // No skill offered: neither `list_skills` nor `Skill` is advertised.
    let app = build_router(Arc::new(SkillUserModel), "scripted");
    let id = create_session(&app).await;

    let list = send_message(&app, &id, "please deploy").await;

    assert!(
        events_of(&list, "agent.tool_use").is_empty(),
        "no skill tools without an offered skill"
    );
    assert!(
        message_texts(&list).contains(&"NO_SKILL_ADVERTISED".to_string()),
        "the skill tools appear only because a skill was offered (not ambient)"
    );
}
