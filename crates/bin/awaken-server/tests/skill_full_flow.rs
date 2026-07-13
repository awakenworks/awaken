//! End-to-end skill lifecycle over the Managed Agents wire, in one session:
//! **discover → author → fork → /name** (ADR-0036).
//!
//!   discover — the model calls `list_skills`; the delivered catalog comes back.
//!   author   — the model authors a new skill via `bash` (parks on the gate,
//!              the client confirms); a live re-scan surfaces it (AgentCreated).
//!   fork     — activating a `context: fork` skill runs a sub-agent whose reply
//!              is returned as the tool result.
//!   /name    — a user `/greet` invocation is expanded into the skill body.

use std::sync::Arc;

use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
};
use awaken_scenario_host::build_router_with_skills;
use awaken_server::{SkillContext, SkillSpec};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
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
        serde_json::json!({ "agent": "assistant" }),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string()
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

async fn confirm(app: &Router, session: &str, tool_use_id: &str) -> serde_json::Value {
    json_call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({ "events": [{ "type": "user.tool_confirmation", "tool_use_id": tool_use_id, "result": "allow" }] }),
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
    list["data"].as_array().unwrap().last().unwrap().clone()
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
        let output = match last.role {
            ChatRole::User => {
                if last_text.contains("FORK-REVIEW-BODY") {
                    // This is the forked sub-agent's turn (its input is the body).
                    AssistantOutput::text("FORK-DONE")
                } else if last_text.contains("GREETING for") {
                    // The /name expansion replaced the user's text with the body.
                    AssistantOutput::text(format!("NAMED:{last_text}"))
                } else if last_text.contains("discover") {
                    tool("l1", "list_skills", serde_json::json!({}))
                } else if last_text.contains("author") {
                    tool(
                        "w",
                        "bash",
                        serde_json::json!({ "command": "mkdir -p skills/notes && echo NOTE-BODY > skills/notes/SKILL.md" }),
                    )
                } else if last_text.contains("fork") {
                    tool("s1", "Skill", serde_json::json!({ "skill": "review" }))
                } else {
                    AssistantOutput::text("hmm")
                }
            }
            ChatRole::Tool => {
                if last_text == "FORK-DONE" {
                    AssistantOutput::text("FORKED")
                } else if last_text.contains("\"skills\"") {
                    // a list_skills catalog: notes present only after authoring.
                    if last_text.contains("notes") {
                        AssistantOutput::text("AUTHORED")
                    } else {
                        AssistantOutput::text("DISCOVERED")
                    }
                } else {
                    // the bash result: now list to observe the authored skill.
                    tool("l2", "list_skills", serde_json::json!({}))
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
async fn discover_author_fork_and_slash_name_end_to_end() {
    let greet = SkillSpec::new("greet", "Greet", "say hello", "GREETING for $ARGUMENTS");
    let review = SkillSpec::new("review", "Review", "review code", "FORK-REVIEW-BODY")
        .with_context(SkillContext::Fork);
    let app = build_router_with_skills(Arc::new(FullFlowModel), "scripted", vec![greet, review]);
    let id = create_session(&app).await;

    // 1) discover — the delivered catalog comes back (no `notes` yet).
    let list = send_message(&app, &id, "please discover").await;
    assert!(tool_uses(&list).contains(&"list_skills".to_string()));
    assert!(messages(&list).contains(&"DISCOVERED".to_string()));
    let catalog = tool_results(&list).join("");
    assert!(catalog.contains("greet") && catalog.contains("review"));
    assert!(!catalog.contains("notes"), "notes not authored yet");

    // 2) author — the model writes a skill via bash; it parks on the gate.
    let parked = send_message(&app, &id, "please author").await;
    let idle = last_idle(&parked);
    assert_eq!(idle["stop_reason"]["type"], "requires_action");
    assert_eq!(idle["stop_reason"]["event_ids"][0], "w");
    // Confirm → bash runs (rooted) → a re-scan surfaces the authored skill.
    let done = confirm(&app, &id, "w").await;
    assert!(messages(&done).contains(&"AUTHORED".to_string()));
    assert!(
        tool_results(&done)
            .iter()
            .any(|r| r.contains("notes") && r.contains("agent_created")),
        "authored skill surfaces as agent_created: {:?}",
        tool_results(&done)
    );

    // 3) fork — activating the fork skill runs a sub-agent; its reply comes back.
    let forked = send_message(&app, &id, "please fork").await;
    assert!(tool_uses(&forked).contains(&"Skill".to_string()));
    assert!(
        tool_results(&forked)
            .iter()
            .any(|r| r.contains("FORK-DONE"))
    );
    assert!(messages(&forked).contains(&"FORKED".to_string()));

    // 4) /name — the user invocation is expanded into the skill body.
    let named = send_message(&app, &id, "/greet World").await;
    assert!(
        messages(&named)
            .iter()
            .any(|m| m.contains("NAMED:") && m.contains("GREETING for World")),
        "slash-name expanded to the skill body: {:?}",
        messages(&named)
    );
}

/// Lists, reads a matching `.rs` file, lists again: the `paths`-conditional skill
/// is hidden until the read touches a matching path, then surfaces (ADR-0036 ③).
struct PathProbeModel;

#[async_trait::async_trait]
impl LlmExecutor for PathProbeModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last = request.messages.last().expect("a message");
        let last_text = text_of(last);
        let output = match last.role {
            ChatRole::User => tool("l1", "list_skills", serde_json::json!({})),
            ChatRole::Tool => {
                if last_text.contains("\"skills\"") {
                    // A catalog: `rusty` appears only after the read touched a match.
                    if last_text.contains("rusty") {
                        AssistantOutput::text("DONE")
                    } else {
                        // `read` is allowed (no park); the gate records its path.
                        tool(
                            "rd",
                            "read",
                            serde_json::json!({ "path": "src/app/main.rs" }),
                        )
                    }
                } else {
                    // the read result — list again to observe the surfaced skill.
                    tool("l2", "list_skills", serde_json::json!({}))
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

#[tokio::test]
async fn conditional_paths_skill_surfaces_after_touching_a_matching_file() {
    let greet = SkillSpec::new("greet", "Greet", "say hello", "hi");
    let rusty = SkillSpec::new("rusty", "Rusty", "rust review", "RUST-BODY")
        .with_paths(vec!["src/**/*.rs".into()]);
    let app = build_router_with_skills(Arc::new(PathProbeModel), "scripted", vec![greet, rusty]);
    let id = create_session(&app).await;

    let list = send_message(&app, &id, "go").await;
    let results = tool_results(&list);

    // Before the read: a catalog with `greet` but not the conditional `rusty`.
    assert!(
        results
            .iter()
            .any(|r| r.contains("greet") && !r.contains("rusty")),
        "conditional skill hidden before a matching file is touched: {results:?}"
    );
    // After reading src/app/main.rs (path recorded at the gate): `rusty` surfaces.
    assert!(
        results.iter().any(|r| r.contains("rusty")),
        "conditional skill surfaces after a matching path is touched: {results:?}"
    );
    assert!(messages(&list).contains(&"DONE".to_string()));
}
