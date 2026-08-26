//! End-to-end filesystem Skill lifecycle over the Managed Agents wire:
//! **discover/read → author → read authored Skill** (ADR-0036).
//!
//!   discover — prompt metadata advertises an exact `SKILL.md`, then `read` loads it.
//!   author   — the model authors a new skill via `bash` (awaits on the gate,
//!              the client confirms) at the canonical repository path.
//!   use      — the confirmed Run reads the authored repository Skill.

mod support;

use std::sync::Arc;

use awaken_agent_contract::agent::message::Role;
use awaken_coordinator::SkillSpec;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_scenario_host::build_router_with_skills;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use support::wait_for_session_events;
use tower::ServiceExt;

// ── managed-protocol harness ─────────────────────────────────────────────────

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
    json_call(
        app,
        "POST",
        "/v1/sessions",
        serde_json::json!({
            "agent": "assistant",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        }),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string()
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
        "the skill command's terminal Session event",
        |events| {
            events
                .iter()
                .any(|event| event["type"] == "session.status_idle")
        },
    )
    .await
}

async fn confirm(app: &Router, session: &str, tool_use_id: &str) -> serde_json::Value {
    let receipt = json_call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({ "events": [{ "type": "user.tool_confirmation", "tool_use_id": tool_use_id, "result": "allow" }] }),
    )
    .await;
    wait_for_session_events(
        app,
        session,
        Some(&receipt),
        "the confirmed skill command's terminal Session event",
        |events| {
            events.iter().any(|event| {
                event["type"] == "session.status_idle"
                    && event["stop_reason"]["type"] != "requires_action"
            })
        },
    )
    .await
}

fn messages(list: &serde_json::Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .map(|e| e["content"][0]["text"].as_str().unwrap_or("").to_string())
        .collect()
}

fn tool_uses(list: &serde_json::Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.tool_use")
        .map(|e| e["name"].as_str().unwrap_or("").to_string())
        .collect()
}

fn tool_results(list: &serde_json::Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.tool_result")
        .map(|e| e["content"][0]["text"].as_str().unwrap_or("").to_string())
        .collect()
}

fn last_idle(list: &serde_json::Value) -> serde_json::Value {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|event| event["type"] == "session.status_idle")
        .expect("session.status_idle event")
        .clone()
}

// ── the model under test ─────────────────────────────────────────────────────

struct FullFlowModel;

fn text_of(m: &awaken_runtime_contract::llm::ChatMessage) -> String {
    use awaken_agent_contract::agent::content::ContentBlock;
    let mut out = String::new();
    for block in &m.content {
        match block {
            ContentBlock::Text { text } => out.push_str(text),
            // A tool result carries its text in nested blocks.
            ContentBlock::ToolResult { content, .. } => {
                for inner in content {
                    if let ContentBlock::Text { text } = inner {
                        out.push_str(text);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[async_trait::async_trait]
impl LlmExecutor for FullFlowModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last = request.messages.last().expect("a message");
        let last_text = text_of(last);
        let system = request
            .messages
            .iter()
            .filter(|message| message.role == Role::System)
            .map(text_of)
            .collect::<Vec<_>>()
            .join("\n");
        let advertised = |suffix: &str| {
            system
                .split('`')
                .find(|part| part.ends_with(suffix))
                .map(str::to_string)
        };
        let output = match last.role {
            Role::User => {
                if last_text.contains("discover") {
                    match advertised("greet/SKILL.md") {
                        Some(path) => tool("read-greet", "read", serde_json::json!({"path": path})),
                        None => AssistantOutput::text("MISSING-GREET-METADATA"),
                    }
                } else if last_text.contains("author") {
                    tool(
                        "w",
                        "bash",
                        serde_json::json!({ "command": "mkdir -p .claude/skills/notes && printf '%s' '---\nname: notes\ndescription: authored notes\n---\nNOTE-BODY' > .claude/skills/notes/SKILL.md" }),
                    )
                } else {
                    AssistantOutput::text("hmm")
                }
            }
            Role::Tool => {
                if last_text.contains("GREETING") {
                    AssistantOutput::text("DISCOVERED")
                } else if last_text.contains("NOTE-BODY") {
                    AssistantOutput::text("AUTHORED-SKILL-USED")
                } else {
                    tool(
                        "read-authored",
                        "read",
                        serde_json::json!({"path": ".claude/skills/notes/SKILL.md"}),
                    )
                }
            }
            _ => AssistantOutput::text("hmm"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

fn tool(call_id: &str, tool_id: &str, arguments: serde_json::Value) -> AssistantOutput {
    AssistantOutput::from_tool_calls(vec![ToolCall {
        call_id: call_id.into(),
        tool_id: tool_id.into(),
        arguments,
    }])
}

// ── the test ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn discover_read_author_and_use_authored_skill_end_to_end() {
    // Causes: C1 a frozen Skill is offered; C2 the model follows its prompt path;
    // C3 authoring invokes approval-gated bash and the client confirms it; C4 a
    // confirmed Run continues to the authored Skill. Effects: E1 `read` returns the
    // frozen body with no semantic tools; E2 approval names the exact public
    // bash Event; E3 `.claude/skills/notes/SKILL.md` is readable through the same
    // sandbox boundary; E4 its body reaches the final reply.
    // Constraints/invariants: every observation is anchored to its accepted
    // receipt and committed terminal Event; Session/Run remains the sole driver,
    // and one canonical read-only wait helper performs no execution or retry.
    // Decision rules: F1=C1+C2 -> E1; F2=F1+C3 -> E2;
    // F3=F2+C4 -> E3+E4.
    let greet = SkillSpec::new("greet", "Greet", "say hello", "GREETING for $ARGUMENTS");
    let app = build_router_with_skills(Arc::new(FullFlowModel), "scripted", vec![greet]);
    let id = create_session(&app).await;

    // 1) discover and load the frozen Skill through its advertised path.
    let list = send_message(&app, &id, "please discover").await;
    assert_eq!(tool_uses(&list), ["read"]);
    assert!(messages(&list).contains(&"DISCOVERED".to_string()));
    assert!(tool_results(&list).join("").contains("GREETING"));

    // 2) author — the model writes a skill via bash; it awaits on the gate.
    let awaiting = send_message(&app, &id, "please author").await;
    let idle = last_idle(&awaiting);
    assert_eq!(idle["stop_reason"]["type"], "requires_action");
    let public_tool_id = idle["stop_reason"]["event_ids"][0]
        .as_str()
        .expect("requires_action carries the answerable public Event id");
    let bash = awaiting["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "agent.tool_use" && event["name"] == "bash")
        .expect("the approval-gated bash Event is projected");
    assert_eq!(bash["id"], public_tool_id, "F2 public identity");
    // Confirm → bash writes the canonical repository Skill path → read loads it.
    let done = confirm(&app, &id, public_tool_id).await;
    assert!(
        messages(&done).contains(&"AUTHORED-SKILL-USED".to_string()),
        "authored Skill body reached the model: {:?}",
        messages(&done)
    );
    assert_eq!(tool_uses(&done), ["read", "bash", "read"]);
}
