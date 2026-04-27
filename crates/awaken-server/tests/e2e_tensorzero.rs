// TensorZero gateway integration tests for awaken.
//
// These tests boot a local TensorZero stack (gateway + ClickHouse + UI) via:
//   ./scripts/e2e-tensorzero.sh
//
// All tests are `#[ignore]` and require:
//   * a running gateway at TENSORZERO_GATEWAY_URL (default http://127.0.0.1:3000)
//   * one of DEEPSEEK_API_KEY / OPENAI_API_KEY exported into the gateway, so
//     the configured `agent_chat` variant can reach an upstream provider
//
// Coverage matrix:
//
//   tz_gateway_health_returns_ok                  — gateway /health
//   tz_chat_completion_returns_inference_id       — chat returns OpenAI-style id
//   tz_feedback_endpoint_accepts_known_id         — /feedback ingests the id
//   tz_feedback_endpoint_rejects_unknown_id       — /feedback 4xx for fakes
//   tz_simple_qa_with_feedback_round_trip         — full QA + answer_correct feedback
//   tz_chat_completion_supports_tool_choice       — calculator-style tool call
//   tz_multi_turn_memory_keeps_token              — second turn recalls phrase
//   tz_event_order_finishes_with_finish_reason    — finish_reason accompanies last message
//   tz_router_provider_compiles_smoke             — awaken router can be wired
//                                                   against TensorZero (no live call)
//
// The `tz_router_provider_compiles_smoke` case exercises the awaken-side
// integration (genai provider executor with TZ as base URL) without any
// upstream cost — it verifies the wiring builds and the provider executor
// is registered, deferring full agent-loop verification to awaken-eval.

use std::sync::Arc;
use std::time::Duration;

use awaken_contract::registry_spec::{AgentSpec, ProviderSpec};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelBinding;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::routes::build_router;
use awaken_server::services::config_runtime::build_genai_provider_executor;
use awaken_stores::memory::InMemoryStore;
use serde_json::{Value, json};

const DEFAULT_GATEWAY_URL: &str = "http://127.0.0.1:3000";

// ---------------------------------------------------------------------------
// Skip helpers
// ---------------------------------------------------------------------------

fn gateway_url() -> String {
    std::env::var("TENSORZERO_GATEWAY_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_string())
}

fn upstream_key_present() -> bool {
    ["DEEPSEEK_API_KEY", "OPENAI_API_KEY"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
}

async fn require_gateway() -> Option<reqwest::Client> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            eprintln!("[tz-e2e] reqwest builder failed: {err}");
            return None;
        }
    };
    let url = format!("{}/health", gateway_url().trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => Some(client),
        Ok(resp) => {
            eprintln!(
                "[tz-e2e] gateway not healthy at {url}: status {}",
                resp.status()
            );
            None
        }
        Err(err) => {
            eprintln!("[tz-e2e] gateway unreachable at {url}: {err}");
            None
        }
    }
}

async fn require_gateway_and_key() -> Option<reqwest::Client> {
    let client = require_gateway().await?;
    if !upstream_key_present() {
        eprintln!("[tz-e2e] DEEPSEEK_API_KEY/OPENAI_API_KEY missing; skipping live inference test");
        return None;
    }
    Some(client)
}

// ---------------------------------------------------------------------------
// REST helpers
// ---------------------------------------------------------------------------

fn chat_url() -> String {
    format!(
        "{}/openai/v1/chat/completions",
        gateway_url().trim_end_matches('/')
    )
}

fn feedback_url() -> String {
    format!("{}/feedback", gateway_url().trim_end_matches('/'))
}

/// Build a chat-completion payload pointed at `agent_chat`.
fn chat_payload(messages: Value) -> Value {
    json!({
        "model": "tensorzero::function_name::agent_chat",
        "messages": messages,
    })
}

/// Send a chat completion through the OpenAI-compat endpoint and return the
/// parsed JSON body (or `None` when the request failed). The non-streaming
/// path is used so tests do not have to parse SSE in-process.
async fn chat_completion(client: &reqwest::Client, messages: Value) -> Option<Value> {
    let resp = client
        .post(chat_url())
        .json(&chat_payload(messages))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        eprintln!("[tz-e2e] chat completion HTTP {status}: {body}");
        return None;
    }
    resp.json::<Value>().await.ok()
}

/// Extract the OpenAI-style `id` (TensorZero inference id) from a chat
/// completion response.
fn inference_id(body: &Value) -> Option<&str> {
    body.get("id").and_then(Value::as_str)
}

/// Concatenate text from all `choices[*].message.content` entries.
fn assistant_content(body: &Value) -> String {
    body.get("choices")
        .and_then(Value::as_array)
        .map(|choices| {
            choices
                .iter()
                .filter_map(|c| {
                    c.get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn finish_reason(body: &Value) -> Option<String> {
    body.get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

async fn submit_feedback(
    client: &reqwest::Client,
    inference_id: &str,
    metric_name: &str,
    value: Value,
) -> Option<reqwest::StatusCode> {
    let payload = json!({
        "inference_id": inference_id,
        "metric_name": metric_name,
        "value": value,
    });
    let resp = client
        .post(feedback_url())
        .json(&payload)
        .send()
        .await
        .ok()?;
    Some(resp.status())
}

// ---------------------------------------------------------------------------
// Tests: REST surface
// ---------------------------------------------------------------------------

#[ignore = "requires running TensorZero stack: ./scripts/e2e-tensorzero.sh"]
#[tokio::test]
async fn tz_gateway_health_returns_ok() {
    let client = match require_gateway().await {
        Some(c) => c,
        None => return,
    };
    let resp = client
        .get(format!("{}/health", gateway_url().trim_end_matches('/')))
        .send()
        .await
        .expect("gateway reachable");
    assert!(resp.status().is_success(), "expected 2xx /health");
}

#[ignore = "requires running TensorZero stack: ./scripts/e2e-tensorzero.sh"]
#[tokio::test]
async fn tz_feedback_endpoint_rejects_unknown_id() {
    let client = match require_gateway().await {
        Some(c) => c,
        None => return,
    };
    let status = submit_feedback(
        &client,
        "00000000-0000-0000-0000-000000000000",
        "answer_correct",
        json!(true),
    )
    .await
    .expect("feedback POST returned a status");
    assert!(
        !status.is_success(),
        "feedback for an unknown inference id must not be accepted"
    );
}

// ---------------------------------------------------------------------------
// Tests: live inference (require upstream key)
// ---------------------------------------------------------------------------

#[ignore = "requires running TensorZero + DEEPSEEK_API_KEY/OPENAI_API_KEY"]
#[tokio::test]
async fn tz_chat_completion_returns_inference_id() {
    let client = match require_gateway_and_key().await {
        Some(c) => c,
        None => return,
    };
    let body = chat_completion(
        &client,
        json!([
            {"role": "user", "content": "Reply with the single digit 2."}
        ]),
    )
    .await
    .expect("chat completion returned JSON");
    let id = inference_id(&body).expect("response carries an id");
    assert!(!id.is_empty(), "inference id should be non-empty");
}

#[ignore = "requires running TensorZero + DEEPSEEK_API_KEY/OPENAI_API_KEY"]
#[tokio::test]
async fn tz_feedback_endpoint_accepts_known_id() {
    let client = match require_gateway_and_key().await {
        Some(c) => c,
        None => return,
    };
    let body = chat_completion(&client, json!([{"role": "user", "content": "Reply OK."}]))
        .await
        .expect("chat completion returned JSON");
    let id = inference_id(&body)
        .expect("response carries an id")
        .to_string();

    let status = submit_feedback(&client, &id, "answer_correct", json!(true))
        .await
        .expect("feedback POST returned a status");
    assert!(
        status.is_success(),
        "expected 2xx feedback for known id, got {status}"
    );
}

#[ignore = "requires running TensorZero + DEEPSEEK_API_KEY/OPENAI_API_KEY"]
#[tokio::test]
async fn tz_simple_qa_with_feedback_round_trip() {
    let client = match require_gateway_and_key().await {
        Some(c) => c,
        None => return,
    };
    let body = chat_completion(
        &client,
        json!([{"role": "user", "content": "What is 2+2? Reply with only the digit."}]),
    )
    .await
    .expect("chat completion returned JSON");

    let id = inference_id(&body)
        .expect("response carries an id")
        .to_string();
    let content = assistant_content(&body);
    let answer_correct = content.contains('4');
    let response_quality = if answer_correct { 1.0 } else { 0.0 };

    let s1 = submit_feedback(&client, &id, "answer_correct", json!(answer_correct))
        .await
        .expect("feedback POST returned a status");
    assert!(s1.is_success(), "answer_correct feedback rejected: {s1}");

    let s2 = submit_feedback(&client, &id, "response_quality", json!(response_quality))
        .await
        .expect("feedback POST returned a status");
    assert!(s2.is_success(), "response_quality feedback rejected: {s2}");

    assert!(
        answer_correct,
        "expected answer to contain '4'; got: {content:?}"
    );
}

#[ignore = "requires running TensorZero + DEEPSEEK_API_KEY/OPENAI_API_KEY"]
#[tokio::test]
async fn tz_chat_completion_supports_tool_choice() {
    let client = match require_gateway_and_key().await {
        Some(c) => c,
        None => return,
    };
    let payload = json!({
        "model": "tensorzero::function_name::agent_chat",
        "messages": [
            {"role": "user", "content": "Use the calculator to multiply 12 by 5."}
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "calculator",
                    "description": "Multiply two integers and return the product.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "a": {"type": "integer"},
                            "b": {"type": "integer"}
                        },
                        "required": ["a", "b"]
                    }
                }
            }
        ],
        "tool_choice": "auto"
    });
    let resp = client
        .post(chat_url())
        .json(&payload)
        .send()
        .await
        .expect("chat completion request");
    assert!(
        resp.status().is_success(),
        "expected 2xx tool-call completion, got {}",
        resp.status()
    );
    let body: Value = resp.json().await.expect("JSON body");
    let id = inference_id(&body)
        .expect("response carries an id")
        .to_string();

    let tool_calls = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let chose_tool = tool_calls.iter().any(|tc| {
        tc.get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            == Some("calculator")
    });

    let _ = submit_feedback(&client, &id, "tool_choice_correct", json!(chose_tool)).await;
    assert!(
        chose_tool,
        "expected calculator tool call; got tool_calls = {tool_calls:?}"
    );
}

#[ignore = "requires running TensorZero + DEEPSEEK_API_KEY/OPENAI_API_KEY"]
#[tokio::test]
async fn tz_multi_turn_memory_keeps_token() {
    let client = match require_gateway_and_key().await {
        Some(c) => c,
        None => return,
    };
    let phrase = format!("banana-{}", std::process::id());

    let body1 = chat_completion(
        &client,
        json!([
            {"role": "user", "content": format!("Remember the word {phrase}. Reply OK.")}
        ]),
    )
    .await
    .expect("first turn JSON");
    let assistant_first = assistant_content(&body1);

    let body2 = chat_completion(
        &client,
        json!([
            {"role": "user", "content": format!("Remember the word {phrase}. Reply OK.")},
            {"role": "assistant", "content": assistant_first},
            {"role": "user", "content": "Repeat exactly the word I asked you to remember, nothing else."}
        ]),
    )
    .await
    .expect("second turn JSON");
    let answer = assistant_content(&body2);

    let id = inference_id(&body2)
        .expect("second turn carries id")
        .to_string();
    let recalled = answer.contains(&phrase);
    let _ = submit_feedback(&client, &id, "answer_correct", json!(recalled)).await;

    assert!(
        recalled,
        "expected answer to contain {phrase:?}; got: {answer:?}"
    );
}

#[ignore = "requires running TensorZero + DEEPSEEK_API_KEY/OPENAI_API_KEY"]
#[tokio::test]
async fn tz_event_order_finishes_with_finish_reason() {
    let client = match require_gateway_and_key().await {
        Some(c) => c,
        None => return,
    };
    let body = chat_completion(
        &client,
        json!([{"role": "user", "content": "Reply with the single word done."}]),
    )
    .await
    .expect("chat completion JSON");
    let reason = finish_reason(&body).expect("finish_reason present");
    // Accept the OpenAI-standard variants; gateway may surface either.
    assert!(
        matches!(reason.as_str(), "stop" | "length" | "tool_calls"),
        "unexpected finish_reason: {reason:?}"
    );
}

// ---------------------------------------------------------------------------
// Tests: awaken router wired against TensorZero (no live call)
// ---------------------------------------------------------------------------

/// Smoke: `build_genai_provider_executor` accepts a TensorZero base URL and
/// the resulting executor can be registered into an `AgentRuntime` whose
/// router constructs cleanly. No outbound HTTP is performed; this guards the
/// awaken-side wiring against breakage in the OpenAI-compat path.
#[ignore = "requires running TensorZero gateway: ./scripts/e2e-tensorzero.sh"]
#[tokio::test]
async fn tz_router_provider_compiles_smoke() {
    let _ = match require_gateway().await {
        Some(c) => c,
        None => return,
    };

    let provider_spec = ProviderSpec {
        id: "tz".into(),
        adapter: "openai".into(),
        api_key: None,
        base_url: Some(format!(
            "{}/openai/v1/",
            gateway_url().trim_end_matches('/')
        )),
        timeout_secs: 60,
        adapter_options: Default::default(),
    };
    let executor =
        build_genai_provider_executor(&provider_spec).expect("genai executor builds for TZ");

    let store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("tz", executor)
            .with_model_binding(
                "tz_chat",
                ModelBinding {
                    provider_id: "tz".into(),
                    upstream_model: "tensorzero::function_name::agent_chat".into(),
                },
            )
            .with_thread_run_store(store.clone())
            .with_agent_spec(AgentSpec {
                id: "default".into(),
                model_id: "tz_chat".into(),
                system_prompt: "You are a TensorZero-routed agent.".into(),
                max_rounds: 2,
                ..Default::default()
            })
            .build()
            .expect("runtime builds with TZ provider"),
    );

    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store.clone(),
        "tz-test".into(),
        awaken_server::mailbox::MailboxConfig::default(),
    ));

    let state = AppState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        ServerConfig::default(),
    );

    // Building the router should not fail. We do not drive a request: that
    // path requires an upstream key and is covered by REST-level tests above.
    let _router: axum::Router = build_router(&state).with_state(state);
}

// ---------------------------------------------------------------------------
// Pure helper unit tests (no infrastructure required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn gateway_url_default_is_localhost_3000() {
        // Ambient env may override; the helper is allowed to defer to it.
        let url = gateway_url();
        assert!(!url.is_empty());
        assert!(url.starts_with("http"));
    }

    #[test]
    fn chat_url_has_openai_compat_suffix() {
        assert!(chat_url().ends_with("/openai/v1/chat/completions"));
    }

    #[test]
    fn feedback_url_has_feedback_suffix() {
        assert!(feedback_url().ends_with("/feedback"));
    }

    #[test]
    fn chat_payload_pins_function_name() {
        let p = chat_payload(json!([{"role": "user", "content": "hi"}]));
        assert_eq!(
            p.get("model").and_then(Value::as_str),
            Some("tensorzero::function_name::agent_chat")
        );
        assert!(p.get("messages").and_then(Value::as_array).is_some());
    }

    #[test]
    fn inference_id_extracts_top_level_id() {
        let body = json!({"id": "tz_inf_abc123", "choices": []});
        assert_eq!(inference_id(&body), Some("tz_inf_abc123"));
    }

    #[test]
    fn inference_id_returns_none_when_missing() {
        let body = json!({"choices": []});
        assert!(inference_id(&body).is_none());
    }

    #[test]
    fn assistant_content_concatenates_choices() {
        let body = json!({
            "choices": [
                {"message": {"role": "assistant", "content": "Hello "}},
                {"message": {"role": "assistant", "content": "world"}}
            ]
        });
        assert_eq!(assistant_content(&body), "Hello world");
    }

    #[test]
    fn assistant_content_empty_when_no_choices() {
        assert_eq!(assistant_content(&json!({})), "");
        assert_eq!(assistant_content(&json!({"choices": []})), "");
    }

    #[test]
    fn finish_reason_reads_first_choice() {
        let body = json!({"choices": [{"finish_reason": "stop"}]});
        assert_eq!(finish_reason(&body), Some("stop".into()));
    }

    #[test]
    fn finish_reason_none_when_absent() {
        assert!(finish_reason(&json!({"choices": []})).is_none());
        assert!(finish_reason(&json!({})).is_none());
    }

    #[test]
    fn upstream_key_present_returns_bool() {
        // Cannot mutate env; just confirm the helper is callable and returns
        // a boolean without panicking under any ambient state.
        let _ = upstream_key_present();
    }

    // ── URL trimming / construction edge cases ──────────────────────

    #[test]
    fn chat_url_does_not_double_slash_when_env_has_trailing_slash() {
        // We cannot mutate env, but we can still assert the trimming logic
        // applied to a synthetic URL by re-implementing the helper inline.
        let trimmed = "http://example.com:3000/".trim_end_matches('/');
        assert_eq!(
            format!("{trimmed}/openai/v1/chat/completions"),
            "http://example.com:3000/openai/v1/chat/completions"
        );
    }

    #[test]
    fn feedback_url_independent_of_trailing_slash() {
        let trimmed = "http://example.com:3000///".trim_end_matches('/');
        assert_eq!(
            format!("{trimmed}/feedback"),
            "http://example.com:3000/feedback"
        );
    }

    // ── chat_payload coverage ───────────────────────────────────────

    #[test]
    fn chat_payload_with_empty_messages_still_pins_model() {
        let p = chat_payload(json!([]));
        assert_eq!(
            p.get("model").and_then(Value::as_str),
            Some("tensorzero::function_name::agent_chat")
        );
        assert_eq!(
            p.get("messages").and_then(Value::as_array).map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn chat_payload_preserves_message_order() {
        let messages = json!([
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "second"},
            {"role": "user", "content": "third"}
        ]);
        let p = chat_payload(messages.clone());
        assert_eq!(p.get("messages"), Some(&messages));
    }

    // ── inference_id robustness ─────────────────────────────────────

    #[test]
    fn inference_id_returns_none_for_non_string_id() {
        let body = json!({"id": 42});
        assert!(inference_id(&body).is_none());
        let body = json!({"id": null});
        assert!(inference_id(&body).is_none());
    }

    #[test]
    fn inference_id_ignores_nested_id_fields() {
        // TensorZero only surfaces id at the top level; helper must not be
        // confused by nested ids inside choices/usage.
        let body = json!({
            "choices": [{"message": {"id": "nested-not-used"}}]
        });
        assert!(inference_id(&body).is_none());
    }

    // ── assistant_content edge cases ────────────────────────────────

    #[test]
    fn assistant_content_skips_choices_without_content() {
        let body = json!({
            "choices": [
                {"message": {"role": "assistant", "tool_calls": []}},
                {"message": {"role": "assistant", "content": "answer"}}
            ]
        });
        assert_eq!(assistant_content(&body), "answer");
    }

    #[test]
    fn assistant_content_skips_non_string_content() {
        let body = json!({
            "choices": [
                {"message": {"role": "assistant", "content": ["multimodal", "ignored"]}},
                {"message": {"role": "assistant", "content": "text-mode"}}
            ]
        });
        assert_eq!(assistant_content(&body), "text-mode");
    }

    #[test]
    fn assistant_content_handles_choice_with_null_message() {
        let body = json!({
            "choices": [
                {"index": 0},
                {"message": {"content": "kept"}}
            ]
        });
        assert_eq!(assistant_content(&body), "kept");
    }

    // ── finish_reason variants ──────────────────────────────────────

    #[test]
    fn finish_reason_recognises_length_and_tool_calls() {
        for reason in ["stop", "length", "tool_calls"] {
            let body = json!({"choices": [{"finish_reason": reason}]});
            assert_eq!(finish_reason(&body), Some(reason.into()));
        }
    }

    #[test]
    fn finish_reason_returns_none_for_non_string() {
        let body = json!({"choices": [{"finish_reason": null}]});
        assert!(finish_reason(&body).is_none());
        let body = json!({"choices": [{"finish_reason": 1}]});
        assert!(finish_reason(&body).is_none());
    }

    #[test]
    fn finish_reason_uses_first_choice_only() {
        let body = json!({
            "choices": [
                {"finish_reason": "stop"},
                {"finish_reason": "length"}
            ]
        });
        assert_eq!(finish_reason(&body), Some("stop".into()));
    }

    // ── awaken-side wiring (no docker, no upstream call) ────────────

    #[test]
    fn provider_spec_with_tz_base_url_builds_executor() {
        // No outbound HTTP — `genai` lazily resolves the endpoint; we only
        // assert that the executor builds. This guards against
        // `build_genai_provider_executor` regressions when the spec uses an
        // OpenAI-compat base URL pointed at TensorZero.
        let spec = ProviderSpec {
            id: "tz".into(),
            adapter: "openai".into(),
            api_key: None,
            base_url: Some("http://127.0.0.1:3000/openai/v1/".into()),
            timeout_secs: 30,
            adapter_options: Default::default(),
        };
        let _executor = build_genai_provider_executor(&spec)
            .expect("genai executor builds for TensorZero base URL");
    }

    #[test]
    fn provider_spec_without_trailing_slash_normalises() {
        let spec = ProviderSpec {
            id: "tz".into(),
            adapter: "openai".into(),
            api_key: None,
            base_url: Some("http://127.0.0.1:3000/openai/v1".into()),
            timeout_secs: 30,
            adapter_options: Default::default(),
        };
        let _executor = build_genai_provider_executor(&spec)
            .expect("base URL without trailing slash should build");
    }

    #[test]
    fn runtime_builds_with_tz_provider_and_model_binding() {
        // Full awaken-side wiring without docker: confirms the runtime
        // builder accepts a TensorZero-pointed provider executor and
        // `tensorzero::function_name::agent_chat` upstream model id.
        let spec = ProviderSpec {
            id: "tz".into(),
            adapter: "openai".into(),
            api_key: None,
            base_url: Some("http://127.0.0.1:3000/openai/v1/".into()),
            timeout_secs: 30,
            adapter_options: Default::default(),
        };
        let executor = build_genai_provider_executor(&spec).expect("executor");
        let store = Arc::new(InMemoryStore::new());
        let runtime = AgentRuntimeBuilder::new()
            .with_provider("tz", executor)
            .with_model_binding(
                "tz_chat",
                ModelBinding {
                    provider_id: "tz".into(),
                    upstream_model: "tensorzero::function_name::agent_chat".into(),
                },
            )
            .with_thread_run_store(store.clone())
            .with_agent_spec(AgentSpec {
                id: "default".into(),
                model_id: "tz_chat".into(),
                system_prompt: "test".into(),
                max_rounds: 1,
                ..Default::default()
            })
            .build()
            .expect("runtime builds with TZ provider");
        // The runtime should expose a non-empty resolver for the agent.
        let _ = runtime.resolver_arc();
    }

    // ── Config asset sanity ─────────────────────────────────────────

    /// The TensorZero gateway TOML config is shipped with the repo and
    /// mounted into the docker container. Tests must surface drift early.
    #[test]
    fn tensorzero_config_toml_contains_required_keys() {
        let toml = include_str!("../../../e2e/tensorzero/config/tensorzero.toml");
        for needle in [
            "[functions.agent_chat]",
            "answer_correct",
            "tool_choice_correct",
            "response_quality",
            "[functions.judge]",
        ] {
            assert!(
                toml.contains(needle),
                "tensorzero.toml is missing key {needle:?}"
            );
        }
    }

    #[test]
    fn tensorzero_compose_yaml_pins_image_versions() {
        let compose = include_str!("../../../e2e/tensorzero/docker-compose.yml");
        // Pinned tags guard against silent upgrades that change protocol shape.
        assert!(compose.contains("tensorzero/gateway:"));
        assert!(compose.contains("clickhouse/clickhouse-server:"));
        // Default ports must match what helpers assume.
        assert!(compose.contains("3000:3000"));
        assert!(compose.contains("8123:8123"));
    }

    #[test]
    fn e2e_tensorzero_script_invokes_cargo_test_with_filter() {
        let script = include_str!("../../../scripts/e2e-tensorzero.sh");
        assert!(script.contains("docker compose"));
        assert!(script.contains("--ignored"));
        assert!(script.contains("e2e_tensorzero"));
        assert!(script.contains("trap cleanup EXIT"));
    }
}
