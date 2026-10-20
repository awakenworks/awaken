//! Provider-wire contract tests for schemas generated from typed Awaken tools.
//! These run without credentials: a local HTTP peer captures the exact JSON body
//! emitted by each SDK adapter. Schema cases return empty JSON; round trips
//! return protocol fixtures through the same capture boundary.

use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_ext_builtin_tools::builtin_tools;
use awaken_provider_genai::{AdapterKind, GenaiExecutor};
use awaken_runtime_contract::UnspecifiedReasoning;
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, LlmExecutor};
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
mod budget;
use budget::{budget, with_budget};

fn generated_read_descriptor() -> ToolDescriptor {
    builtin_tools()
        .into_iter()
        .find(|tool| tool.descriptor().id == "read")
        .expect("the typed ReadTool is registered")
        .into_descriptor()
}

fn request(model: &str, tool: ToolDescriptor) -> ChatRequest {
    ChatRequest {
        model_binding: ModelBinding::new("compat", model, "genai"),
        inference: Default::default(),
        messages: vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::text("Read the requested fixture")],
        }],
        tools: vec![tool],
    }
}

async fn capture_one_request(
    body: Vec<u8>,
    content_type: &'static str,
) -> (String, tokio::sync::oneshot::Receiver<Value>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local provider fixture");
    let address = listener.local_addr().expect("fixture address");
    let (send, receive) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("provider request");
        let mut request = Vec::new();
        let mut expected_len = None;
        loop {
            let mut chunk = [0_u8; 4096];
            let read = socket
                .read(&mut chunk)
                .await
                .expect("read provider request");
            assert_ne!(read, 0, "provider request ended before its JSON body");
            request.extend_from_slice(&chunk[..read]);

            if expected_len.is_none()
                && let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
            {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_len = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(|| {
                            value
                                .trim()
                                .parse::<usize>()
                                .expect("numeric content-length")
                        })
                    })
                    .expect("JSON request carries content-length");
                expected_len = Some((header_end + 4, content_len));
            }
            if let Some((body_start, content_len)) = expected_len
                && request.len() >= body_start + content_len
            {
                let body = serde_json::from_slice(&request[body_start..body_start + content_len])
                    .expect("provider request body is JSON");
                send.send(body).ok();
                break;
            }
        }

        // Schema-only callers supply the original empty JSON. Round-trip cases
        // supply a protocol response through this same HTTP/capture boundary.
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("response head");
        socket.write_all(&body).await.expect("response body");
    });
    (format!("http://{address}/v1"), receive)
}

async fn provider_payload(adapter: AdapterKind) -> Value {
    let (base_url, payload) = capture_one_request(b"{}".to_vec(), "application/json").await;
    let executor = GenaiExecutor::from_materialized_endpoint(adapter, base_url, "fixture-secret")
        .expect("explicit client construction");
    let model = if adapter == AdapterKind::Vertex {
        "gemini-2.5-flash"
    } else {
        "deepseek-v4-pro"
    };
    let result = with_budget(
        budget(Duration::from_secs(5), Duration::from_secs(5)),
        executor.infer(request(model, generated_read_descriptor())),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), payload)
        .await
        .unwrap_or_else(|_| panic!("provider request was sent; adapter returned {result:?}"))
        .expect("capture task returned a payload")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frozen_deepseek_chat_reasoning_policy_reaches_the_streaming_wire() {
    // Publication-to-provider decision table. C1 an exact DeepSeek Chat
    // candidate freezes Disabled; C2 it freezes ProviderDefault instead; C3
    // streaming and tool schemas remain the same independent of that policy.
    // C4 the caller uses buffered or streaming inference. E1 C1 sends
    // `thinking.type=disabled` in the actual HTTP JSON; E2 C2 omits the
    // override; E3 both retain the same typed Read schema. Rules
    // P1=C1+C3+C4=>E1+E3; P2=C2+C3+C4=>E2+E3. The local peer captures SDK bytes,
    // not only ChatOptions, and cannot claim a live provider quality result.
    for streaming in [false, true] {
        for (policy, expected) in [
            (UnspecifiedReasoning::Disabled, Some("disabled")),
            (UnspecifiedReasoning::ProviderDefault, None),
        ] {
            let (base_url, payload) = capture_one_request(b"{}".to_vec(), "application/json").await;
            let executor = GenaiExecutor::from_materialized_provider_endpoint(
                "deepseek",
                AdapterKind::OpenAI,
                base_url,
                "fixture-secret",
                policy,
            )
            .expect("explicit client construction");
            let request = request("deepseek-v4-pro", generated_read_descriptor());
            if streaming {
                let _ = with_budget(
                    budget(Duration::from_secs(5), Duration::from_secs(5)),
                    executor.infer_streaming(request, &NoopSink),
                )
                .await;
            } else {
                let _ = with_budget(
                    budget(Duration::from_secs(5), Duration::from_secs(5)),
                    executor.infer(request),
                )
                .await;
            }
            let body = tokio::time::timeout(Duration::from_secs(5), payload)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                body.get("thinking")
                    .and_then(|thinking| thinking.get("type"))
                    .and_then(Value::as_str),
                expected,
                "P1/P2 {policy:?} streaming={streaming}"
            );
            assert_generated_read_schema(&body["tools"][0]["function"]["parameters"], "P1/P2/E3");
        }
    }
}

struct NoopSink;

#[async_trait::async_trait]
impl awaken_runtime_contract::llm::DeltaSink for NoopSink {
    async fn on_text(&self, _: &str) {}
}

fn assert_generated_read_schema(schema: &Value, rule: &str) {
    assert_eq!(schema["type"], "object", "{rule}: object root");
    assert_eq!(
        schema["required"],
        serde_json::json!(["file_path"]),
        "{rule}: serde rename and requiredness"
    );
    assert_eq!(
        schema["properties"]["view_range"]["minItems"], 2,
        "{rule}: fixed tuple lower bound"
    );
    assert_eq!(
        schema["properties"]["view_range"]["maxItems"], 2,
        "{rule}: fixed tuple upper bound"
    );
    assert_eq!(
        schema["additionalProperties"], false,
        "{rule}: unknown arguments fail closed"
    );
    assert!(schema.get("$schema").is_none(), "{rule}: no meta-schema");
    assert!(schema.get("$defs").is_none(), "{rule}: no external defs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deepseek_tool_history_serializes_empty_reasoning_on_the_actual_wire() {
    // W1 missing Thinking + exact DeepSeek/OpenAI tool request -> the HTTP
    // assistant row includes reasoning_content="", not null or an omitted key.
    // W2 present empty/nonempty Thinking -> exact bytes, once. Text-only and
    // tool-call history both require this field when tools are advertised.
    // Reuse the existing capture peer; this proves SDK serialization, not a
    // model-quality verdict. No real transcript or credential enters the test.
    for reasoning in [None, Some(""), Some("synthetic retained context")] {
        for with_call in [false, true] {
            let (base_url, payload) = capture_one_request(b"{}".to_vec(), "application/json").await;
            let executor = GenaiExecutor::from_materialized_provider_endpoint(
                "deepseek",
                AdapterKind::OpenAI,
                base_url,
                "fixture-secret",
                UnspecifiedReasoning::ProviderDefault,
            )
            .unwrap();
            let mut request = request("deepseek-v4-pro", generated_read_descriptor());
            let mut content = reasoning
                .map(ContentBlock::thinking)
                .into_iter()
                .collect::<Vec<_>>();
            content.push(if with_call {
                ContentBlock::tool_use(
                    "call-1",
                    "read",
                    serde_json::json!({"file_path":"fixture.txt"}),
                )
            } else {
                ContentBlock::text("synthetic answer")
            });
            request.messages.push(ChatMessage {
                role: Role::Assistant,
                content,
            });
            if with_call {
                request.messages.push(ChatMessage {
                    role: Role::Tool,
                    content: vec![ContentBlock::tool_result(
                        "call-1",
                        vec![ContentBlock::text("fixture result")],
                    )],
                });
            }
            let _ = with_budget(
                budget(Duration::from_secs(5), Duration::from_secs(5)),
                executor.infer(request),
            )
            .await;
            let body = tokio::time::timeout(Duration::from_secs(5), payload)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                body["messages"][1].get("reasoning_content"),
                Some(&Value::String(reasoning.unwrap_or_default().into()))
            );
            assert_eq!(body["model"], "deepseek-v4-pro");
            assert!(body["messages"][0].get("reasoning_content").is_none());
            if with_call {
                assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "call-1");
                assert_eq!(body["messages"][2]["tool_call_id"], "call-1");
                assert!(body["messages"][2].get("reasoning_content").is_none());
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_tool_schema_reaches_every_managed_compatibility_wire() {
    // Cause/effect matrix: C1 ReadArgs derives JsonSchema; C2 the descriptor is
    // generated through ToolDescriptor::for_tool; C3 one selected provider wire
    // serializes the request. E1 the exact strong contract reaches that wire;
    // E2 provider-specific field names are correct. This test captures actual
    // HTTP JSON, so a future SDK change cannot silently invalidate DeepSeek or
    // Google Cloud while leaving Awaken's neutral request tests green.
    //
    // | Rule | Provider surface             | Schema location                                      |
    // | R1   | DeepSeek OpenAI compatible   | tools[].function.parameters                         |
    // | R2   | DeepSeek Anthropic compatible| tools[].input_schema                                |
    // | R3   | Google Cloud Gemini/Vertex   | tools[].functionDeclarations[].parametersJsonSchema |
    let openai = provider_payload(AdapterKind::OpenAI).await;
    assert_generated_read_schema(&openai["tools"][0]["function"]["parameters"], "R1/E1");
    assert_eq!(openai["tools"][0]["function"]["name"], "read", "R1/E2");

    let anthropic = provider_payload(AdapterKind::Anthropic).await;
    assert_generated_read_schema(&anthropic["tools"][0]["input_schema"], "R2/E1");
    assert_eq!(anthropic["tools"][0]["name"], "read", "R2/E2");

    let vertex = provider_payload(AdapterKind::Vertex).await;
    let declaration = &vertex["tools"][0]["functionDeclarations"][0];
    assert_generated_read_schema(&declaration["parametersJsonSchema"], "R3/E1");
    assert_eq!(declaration["name"], "read", "R3/E2");
    assert!(
        declaration.get("parameters").is_none(),
        "R3/E2 uses Gemini's JSON-Schema-native field"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovered_tool_reference_survives_nested_tool_result_on_provider_wire() {
    // Causes: a committed search result contains one ToolReference, optionally
    // alongside text; OpenAI needs text while Anthropic accepts typed references.
    // Effects: the returned discovery is nonempty, ordered and correlated to its
    // call. No neutral transcript mutation.
    // R1 reference-only -> discovery text/typed ref; R2 mixed -> preserve order;
    // Captures actual HTTP, not just neutral values.
    for adapter in [AdapterKind::OpenAI, AdapterKind::Anthropic] {
        for mixed in [false, true] {
            let (base_url, payload) = capture_one_request(b"{}".to_vec(), "application/json").await;
            let executor =
                GenaiExecutor::from_materialized_endpoint(adapter, base_url, "fixture-secret")
                    .unwrap();
            let mut req = request("deepseek-v4-pro", generated_read_descriptor());
            req.messages.push(ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "search-1",
                    "tool_search",
                    serde_json::json!({"query":"read"}),
                )],
            });
            let mut content = Vec::new();
            if mixed {
                content.push(ContentBlock::text("Found a match:"));
            }
            content.push(ContentBlock::tool_reference("read"));
            req.messages.push(ChatMessage {
                role: Role::Tool,
                content: vec![ContentBlock::tool_result("search-1", content)],
            });
            let _ = with_budget(
                budget(Duration::from_secs(5), Duration::from_secs(5)),
                executor.infer(req),
            )
            .await;
            let body = tokio::time::timeout(Duration::from_secs(5), payload)
                .await
                .unwrap()
                .unwrap();
            if adapter == AdapterKind::OpenAI {
                let result = &body["messages"][2];
                assert_eq!(result["tool_call_id"], "search-1");
                assert!(
                    result["content"]
                        .as_str()
                        .unwrap()
                        .contains("Deferred tool `read` is now available."),
                    "R1/R2 discovery vanished: {result}"
                );
                if mixed {
                    assert!(
                        result["content"]
                            .as_str()
                            .unwrap()
                            .starts_with("Found a match:")
                    );
                }
            } else {
                let result = &body["messages"][2]["content"][0];
                assert_eq!(result["tool_use_id"], "search-1");
                assert_eq!(
                    result["content"][usize::from(mixed)]["type"],
                    "tool_reference"
                );
                assert_eq!(result["content"][usize::from(mixed)]["tool_name"], "read");
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openrouter_reply_keeps_the_existing_neutral_reasoning_boundary() {
    // Cause/effect table OR1–OR8: C1 reasoning_details absent/present, C2 scalar
    // reasoning absent/present, C3 text-only/text+typed-call. E1 public bytes,
    // tool identity/arguments and the optional scalar survive exactly; E2 the
    // next request replays only that existing neutral grammar. Cross C1×C2×C3.
    // Constraint: beta21 already ignored reasoning_details. rc8's new Custom
    // parts do not acquire a new SDK content variant, opaque store or replay
    // capability. This proves the old boundary, not new signed OpenRouter support.
    for details in [false, true] {
        for scalar in [false, true] {
            for tool in [false, true] {
                let mut message = serde_json::json!({"role":"assistant","content":"public 世界"});
                if details {
                    message["reasoning_details"] = serde_json::json!([
                        {"type":"reasoning.text","text":"private","signature":"opaque-proof","format":"anthropic-claude-v1","index":0},
                        {"type":"reasoning.encrypted","data":"opaque-encrypted","format":"openai-responses-v1","index":1}
                    ]);
                }
                if scalar {
                    message["reasoning"] = Value::String("private scalar".into());
                }
                if tool {
                    message["tool_calls"] = serde_json::json!([{
                        "id":"call/original","type":"function",
                        "function":{"name":"read","arguments":"{\"file_path\":\"fixture.txt\"}"}
                    }]);
                }
                let reply = serde_json::json!({
                    "model":"claude-sonnet-4-6",
                    "choices":[{"message":message,"finish_reason":if tool {"tool_calls"} else {"stop"}}],
                    "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}
                });
                let (base, first_wire) =
                    capture_one_request(serde_json::to_vec(&reply).unwrap(), "application/json")
                        .await;
                let executor = GenaiExecutor::from_materialized_provider_endpoint(
                    "openrouter",
                    AdapterKind::OpenAI,
                    base,
                    "fixture-secret",
                    UnspecifiedReasoning::ProviderDefault,
                )
                .unwrap();
                let response = with_budget(
                    budget(Duration::from_secs(5), Duration::from_secs(5)),
                    executor.infer(request("claude-sonnet-4-6", generated_read_descriptor())),
                )
                .await
                .unwrap();
                assert_eq!(first_wire.await.unwrap()["model"], "claude-sonnet-4-6");
                let mut expected = Vec::new();
                if scalar {
                    expected.push(ContentBlock::thinking("private scalar"));
                }
                expected.push(ContentBlock::text("public 世界"));
                if tool {
                    expected.push(ContentBlock::tool_use(
                        "call/original",
                        "read",
                        serde_json::json!({"file_path":"fixture.txt"}),
                    ));
                }
                assert_eq!(
                    response.output.blocks, expected,
                    "OR {details}/{scalar}/{tool}: E1"
                );
                let (base, replay_wire) =
                    capture_one_request(b"{}".to_vec(), "application/json").await;
                let replay = GenaiExecutor::from_materialized_provider_endpoint(
                    "openrouter",
                    AdapterKind::OpenAI,
                    base,
                    "fixture-secret",
                    UnspecifiedReasoning::ProviderDefault,
                )
                .unwrap();
                let mut next = request("claude-sonnet-4-6", generated_read_descriptor());
                next.messages.push(ChatMessage {
                    role: Role::Assistant,
                    content: response.output.blocks,
                });
                if tool {
                    next.messages.push(ChatMessage {
                        role: Role::Tool,
                        content: vec![ContentBlock::tool_result(
                            "call/original",
                            vec![ContentBlock::text("result")],
                        )],
                    });
                }
                let _ = with_budget(
                    budget(Duration::from_secs(5), Duration::from_secs(5)),
                    replay.infer(next),
                )
                .await;
                let wire = tokio::time::timeout(Duration::from_secs(5), replay_wire)
                    .await
                    .expect("the replay reaches the existing HTTP boundary")
                    .expect("the capture peer returns its payload");
                let assistant = &wire["messages"][1];
                assert_eq!(assistant["content"], "public 世界", "OR E2");
                assert_eq!(
                    assistant.get("reasoning_content"),
                    scalar
                        .then(|| Value::String("private scalar".into()))
                        .as_ref(),
                    "OR E2"
                );
                assert!(
                    assistant.get("reasoning_details").is_none(),
                    "OR E2 no new capability"
                );
                if tool {
                    assert_eq!(assistant["tool_calls"][0]["id"], "call/original", "OR E2");
                    assert_eq!(
                        wire["messages"][2]["tool_call_id"], "call/original",
                        "OR E2"
                    );
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_round_trip_keeps_content_order_and_tool_correlation_on_the_wire() {
    // Cause/effect table GG1–GG16: cross C1 Gemini/Vertex, C2 buffered/streamed,
    // C3 synthetic/provider call ids, C4 absent/present call signatures. E1 two
    // same-name calls keep distinct ids and exact args; E2 buffered content keeps
    // wire order, while streaming keeps the existing captured-text aggregation;
    // E3 the next HTTP request correlates each result by the original name and
    // only real ids. All 16 combinations are exercised at the real SDK boundary.
    // Constraint: the existing neutral grammar has never retained signature-only
    // Gemini call proofs. Do not promote rc8's per-call mirror to a new store or
    // silently claim new signed-Gemini support. The original schema and live
    // round-trip cases remain separate witnesses.
    for adapter in [AdapterKind::Gemini, AdapterKind::Vertex] {
        for streaming in [false, true] {
            for provider_ids in [false, true] {
                for signed in [false, true] {
                    let mut parts = vec![serde_json::json!({"text":"before "})];
                    let ids = if provider_ids {
                        ["opaque/first".to_owned(), "opaque/second".to_owned()]
                    } else {
                        ["call#read#0".to_owned(), "call#read#1".to_owned()]
                    };
                    for (index, id) in ids.iter().enumerate() {
                        let mut call = serde_json::json!({"functionCall":{"name":"read","args":{"file_path":format!("{index}.txt")}}});
                        if provider_ids {
                            call["functionCall"]["id"] = Value::String(id.clone());
                        }
                        if signed {
                            call["thoughtSignature"] = Value::String(format!("proof-{index}"));
                        }
                        parts.push(call);
                        parts.push(
                            serde_json::json!({"text":if index == 0 {"between "} else {"after"}}),
                        );
                    }
                    let reply = serde_json::json!({
                        "candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}],
                        "usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1,"totalTokenCount":3}
                    });
                    let (body, content_type) = if streaming {
                        (
                            format!("data: {reply}\n\n").into_bytes(),
                            "text/event-stream",
                        )
                    } else {
                        (serde_json::to_vec(&reply).unwrap(), "application/json")
                    };
                    let (base, first_wire) = capture_one_request(body, content_type).await;
                    let executor =
                        GenaiExecutor::from_materialized_endpoint(adapter, base, "fixture-secret")
                            .unwrap();
                    let req = request("gemini-2.5-flash", generated_read_descriptor());
                    let response = with_budget(
                        budget(Duration::from_secs(5), Duration::from_secs(5)),
                        async {
                            if streaming {
                                executor.infer_streaming(req, &NoopSink).await
                            } else {
                                executor.infer(req).await
                            }
                        },
                    )
                    .await
                    .unwrap();
                    assert!(
                        first_wire.await.unwrap().get("contents").is_some(),
                        "GG exact dialect"
                    );
                    let calls = response.output.tool_calls();
                    assert_eq!(calls.len(), 2, "GG E1");
                    for (index, call) in calls.iter().enumerate() {
                        assert_eq!(call.call_id, ids[index], "GG E1");
                        assert_eq!(call.tool_id, "read", "GG E1");
                        assert_eq!(
                            call.arguments,
                            serde_json::json!({"file_path":format!("{index}.txt")}),
                            "GG E1"
                        );
                    }
                    let call_block = |index: usize| {
                        ContentBlock::tool_use(
                            ids[index].clone(),
                            "read",
                            serde_json::json!({"file_path":format!("{index}.txt")}),
                        )
                    };
                    let expected = if streaming {
                        vec![
                            ContentBlock::text("before between after"),
                            call_block(0),
                            call_block(1),
                        ]
                    } else {
                        vec![
                            ContentBlock::text("before "),
                            call_block(0),
                            ContentBlock::text("between "),
                            call_block(1),
                            ContentBlock::text("after"),
                        ]
                    };
                    assert_eq!(
                        response.output.blocks, expected,
                        "GG E2 {adapter:?}/{streaming}/{provider_ids}/{signed}"
                    );
                    let (base, replay_wire) =
                        capture_one_request(b"{}".to_vec(), "application/json").await;
                    let replay =
                        GenaiExecutor::from_materialized_endpoint(adapter, base, "fixture-secret")
                            .unwrap();
                    let mut next = request("gemini-2.5-flash", generated_read_descriptor());
                    next.messages.push(ChatMessage {
                        role: Role::Assistant,
                        content: response.output.blocks,
                    });
                    next.messages.push(ChatMessage {
                        role: Role::Tool,
                        content: ids
                            .iter()
                            .enumerate()
                            .map(|(index, id)| {
                                ContentBlock::tool_result(
                                    id.clone(),
                                    vec![ContentBlock::text(format!("result-{index}"))],
                                )
                            })
                            .collect(),
                    });
                    let _ = with_budget(
                        budget(Duration::from_secs(5), Duration::from_secs(5)),
                        replay.infer(next),
                    )
                    .await;
                    let wire = tokio::time::timeout(Duration::from_secs(5), replay_wire)
                        .await
                        .expect("the replay reaches the existing HTTP boundary")
                        .expect("the capture peer returns its payload");
                    let contents = wire["contents"].as_array().unwrap();
                    let model = contents.iter().find(|row| row["role"] == "model").unwrap();
                    let sent_calls: Vec<_> = model["parts"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|part| part.get("functionCall"))
                        .collect();
                    let sent_results: Vec<_> = contents
                        .iter()
                        .flat_map(|row| row["parts"].as_array().unwrap())
                        .filter_map(|part| part.get("functionResponse"))
                        .collect();
                    assert_eq!(sent_calls.len(), 2, "GG E3");
                    assert_eq!(sent_results.len(), 2, "GG E3");
                    for index in 0..2 {
                        assert_eq!(sent_calls[index]["name"], "read", "GG E3");
                        assert_eq!(sent_results[index]["name"], "read", "GG E3");
                        assert_eq!(
                            sent_calls[index].get("id").and_then(Value::as_str),
                            provider_ids.then_some(ids[index].as_str()),
                            "GG E3"
                        );
                        assert_eq!(
                            sent_results[index].get("id").and_then(Value::as_str),
                            provider_ids.then_some(ids[index].as_str()),
                            "GG E3"
                        );
                        assert_eq!(
                            sent_results[index]["response"]["content"],
                            format!("result-{index}"),
                            "GG E3"
                        );
                    }
                }
            }
        }
    }
}
