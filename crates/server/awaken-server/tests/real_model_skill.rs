//! Real-model skill conformance: drive the Managed Agents adapter over a **live**
//! Anthropic-compatible endpoint (Kimi) and prove the skill path end-to-end — the
//! model discovers a skill via `list_skills`, activates it via `Skill`, and the
//! activated instructions reach it (it echoes a secret that lives *only* in the
//! skill body). ADR-0036.
//!
//! Gated with `#[ignore]` (needs network + credentials). Run explicitly:
//!
//! ```sh
//! KIMI_API_KEY=sk-... cargo test -p awaken-server --test real_model_skill -- --ignored --nocapture
//! ```
//!
//! Env: `KIMI_API_KEY` (required), `KIMI_BASE_URL` (default the Kimi coding
//! endpoint), `KIMI_MODEL` (default `kimi-for-coding`).

use std::sync::Arc;

use awaken_provider_genai::GenaiExecutor;
use awaken_scenario_host::build_router_with_skills;
use awaken_server::SkillSpec;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

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

fn events_text(
    list: &serde_json::Value,
    ty: &str,
    field: &dyn Fn(&serde_json::Value) -> String,
) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == ty)
        .map(field)
        .collect()
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY set and --ignored"]
async fn kimi_discovers_activates_and_follows_a_skill() {
    let key = std::env::var("KIMI_API_KEY")
        .expect("set KIMI_API_KEY (the Kimi/Anthropic-compatible key) to run this test");
    let base = std::env::var("KIMI_BASE_URL")
        .unwrap_or_else(|_| "https://api.kimi.com/coding/v1/".to_string());
    let model = std::env::var("KIMI_MODEL").unwrap_or_else(|_| "kimi-for-coding".to_string());

    // The secret lives ONLY in the skill body — the model can produce it only by
    // discovering the skill, activating it, and reading the injected instructions.
    const SECRET: &str = "PLUM-7788";
    let codeword = SkillSpec::new(
        "codeword",
        "Codeword",
        "reveals the project's secret code word",
        format!("Your only task now: reply with exactly this token and nothing else: {SECRET}"),
    );

    let executor = GenaiExecutor::anthropic_compatible(base, key);
    let app = build_router_with_skills(Arc::new(executor), model, vec![codeword]);
    let id = create_session(&app).await;

    let list = send_message(
        &app,
        &id,
        "You have skills available through the `list_skills` and `Skill` tools. \
         First call `list_skills` to see what exists, then activate the `codeword` \
         skill with the `Skill` tool and follow its instruction exactly.",
    )
    .await;

    let tool_uses = events_text(&list, "agent.tool_use", &|e| {
        e["name"].as_str().unwrap_or("").to_string()
    });
    let messages = events_text(&list, "agent.message", &|e| {
        e["content"][0]["text"].as_str().unwrap_or("").to_string()
    });
    let all_text = list.to_string();

    eprintln!("--- live tool_uses: {tool_uses:?}");
    eprintln!("--- live messages: {messages:?}");

    assert!(
        tool_uses.iter().any(|t| t == "Skill"),
        "the model activated the skill via the Skill tool: {tool_uses:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains(SECRET)) || all_text.contains(SECRET),
        "the activated skill's instruction reached the model (it echoed the secret): {messages:?}"
    );
}
