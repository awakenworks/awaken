//! End-to-end skill lifecycle through the *Managed Agents* protocol (ADR-0036).
//!
//! Drives the real kernel over the public `/v1/sessions...` wire and proves the
//! Anthropic-compatible filesystem Skill surface from **discover** to **use**:
//!
//!   offer   (Resource version + Agent binding → frozen `SKILL.md` projection)
//!     → discover (prompt carries metadata + path, never the body)
//!     → load     (model calls ordinary `read`; instructions returned)
//!     → use      (the loop continues; the model replies)
//!
//! Also proves semantic Skill tools are not exposed as a parallel path and that
//! no Skill discovery is ambient when none is offered.

mod support;

use std::sync::Arc;

use awaken_agent_contract::agent::message::Role;
use awaken_coordinator::SkillSpec;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_scenario_host::{build_router, build_router_with_managed_skills};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use support::wait_for_session_events;
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
        serde_json::json!({
            "agent": "assistant",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        }),
    )
    .await;
    s["id"].as_str().unwrap().to_string()
}

async fn send_message(app: &Router, session: &str, text: &str) -> serde_json::Value {
    let receipt = json_call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }] }),
    )
    .await;
    wait_for_session_events(
        app,
        session,
        Some(&receipt),
        "the skill lifecycle's terminal Session event",
        |events| {
            events
                .iter()
                .any(|event| event["type"] == "session.status_idle")
        },
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

/// Step 0: find the prompt-advertised SKILL.md path and load it with `read`.
/// Step 1: reply after the file body returns.
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
            .filter(|m| m.role == Role::Tool)
            .count();
        match tool_results {
            0 => {
                let system = request
                    .messages
                    .iter()
                    .filter(|message| message.role == Role::System)
                    .flat_map(|message| message.content.iter())
                    .filter_map(|block| match block {
                        awaken_agent_contract::agent::content::ContentBlock::Text { text } => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let skill_path = system.split('`').find(|part| part.ends_with("/SKILL.md"));
                if skill_path.is_none() {
                    return Ok(ChatResponse {
                        output: AssistantOutput::text("NO_SKILL_ADVERTISED"),
                        usage: None,
                        stop_reason: None,
                    });
                }
                if system.contains(SKILL_BODY) {
                    return Ok(ChatResponse {
                        output: AssistantOutput::text("BODY_IN_PROMPT"),
                        usage: None,
                        stop_reason: None,
                    });
                }
                Ok(ChatResponse {
                    output: AssistantOutput::from_tool_calls(vec![ToolCall {
                        call_id: "read-skill".into(),
                        tool_id: "read".into(),
                        arguments: serde_json::json!({"path": skill_path.unwrap()}),
                    }]),
                    usage: None,
                    stop_reason: None,
                })
            }
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
    // Causes: C1 a versioned Agent binding freezes one Skill into the Session;
    // C2 the model requests the
    // prompt path; C3 it reads the advertised `SKILL.md`; C4 the instructions return to
    // the same Run. Effects: E1 only `read` is invoked; E2 prompt contains
    // metadata/path but not body; E3 read returns the exact body; E4 the Run
    // commits `USED_SKILL`. Rule L1=C1+C2 -> E1+E2; L2=L1+C3+C4 -> E3+E4.
    let app =
        build_router_with_managed_skills(Arc::new(SkillUserModel), "scripted", vec![skill_spec()])
            .await;
    let id = create_session(&app).await;

    let list = send_message(&app, &id, "please deploy").await;
    let texts = message_texts(&list);

    assert!(!texts.contains(&"BODY_IN_PROMPT".to_string()));
    assert!(!texts.contains(&"NO_SKILL_ADVERTISED".to_string()));

    // Filesystem delivery invokes only the ordinary read tool.
    let tool_uses: Vec<String> = events_of(&list, "agent.tool_use")
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(tool_uses, ["read"]);
    assert!(!tool_uses.contains(&LIST_TOOL.to_string()));
    assert!(!tool_uses.contains(&SKILL_TOOL.to_string()));

    // The body arrives only through the on-demand file read.
    let results: Vec<String> = events_of(&list, "agent.tool_result")
        .iter()
        .map(|e| e["content"][0]["text"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        results.iter().any(|r| r.contains(SKILL_BODY)),
        "read returned the instructions body: {results:?}"
    );

    // used: the loop continued and the model replied; ended cleanly.
    assert!(texts.contains(&"USED_SKILL".to_string()));
    let idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|event| event["type"] == "session.status_idle")
        .expect("session.status_idle event");
    assert_eq!(idle["stop_reason"]["type"], "end_turn");
}

#[tokio::test]
async fn skill_tools_are_not_ambient_without_offering_a_skill() {
    // Causes: the canonical Resources component installs an empty SkillStore
    // (C1), while the Agent/Session offers no static, external, or pinned Skill
    // (C2=false). Effects: E1 no `list_skills`/`Skill` descriptor or invocation;
    // E2 a normal `NO_SKILL_ADVERTISED` terminal reply.
    // Constraints/invariants: store availability alone is never a capability
    // grant, and completion is observed only after the accepted receipt.
    // Decision rule: N1=C1+!C2 -> E1+E2.
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
