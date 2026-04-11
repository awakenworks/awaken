//! MCP protocol end-to-end tests.
//!
//! Verifies the full flow: MCP client → JSON-RPC → McpServer → AgentMcpTool →
//! AgentRuntime → event collection → tool result response.

use async_trait::async_trait;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::registry_spec::AgentSpec;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelBinding;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::routes::build_router;
use awaken_stores::memory::InMemoryStore;
use axum::body::to_bytes;
use axum::http::{Request, Response, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

// ── Mock executor that returns a fixed text response ──

struct EchoExecutor;

#[async_trait]
impl awaken_contract::contract::executor::LlmExecutor for EchoExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        // Echo back the last user message as assistant response.
        let user_text = request
            .messages
            .iter()
            .rev()
            .find_map(|m| {
                if m.role == awaken_contract::contract::message::Role::User {
                    Some(m.text())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        Ok(StreamResult {
            content: vec![ContentBlock::text(format!("echo: {user_text}"))],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "echo"
    }
}

// ── App setup ──

fn make_mcp_app() -> axum::Router {
    let runtime = {
        let builder = AgentRuntimeBuilder::new()
            .with_model_binding(
                "test-model",
                ModelBinding {
                    provider_id: "mock".into(),
                    upstream_model: "mock-model".into(),
                },
            )
            .with_provider("mock", Arc::new(EchoExecutor))
            .with_agent_spec(AgentSpec {
                id: "echo".into(),
                model_id: "test-model".into(),
                system_prompt: "You are an echo bot".into(),
                max_rounds: 2,
                ..Default::default()
            });
        Arc::new(builder.build().expect("build runtime"))
    };
    let store = Arc::new(InMemoryStore::new());
    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        "test".to_string(),
        awaken_server::mailbox::MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime.clone(),
        mailbox,
        store,
        runtime.resolver_arc(),
        ServerConfig::default(),
    );
    build_router().with_state(state)
}

async fn mcp_post(
    app: &axum::Router,
    payload: Value,
    session_id: Option<&str>,
) -> Response<axum::body::Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/mcp")
        .header("content-type", "application/json");
    if let Some(session_id) = session_id {
        builder = builder
            .header("MCP-Session-Id", session_id)
            .header("MCP-Protocol-Version", mcp::MCP_PROTOCOL_VERSION);
    }

    app.clone()
        .oneshot(
            builder
                .body(axum::body::Body::from(
                    serde_json::to_vec(&payload).unwrap(),
                ))
                .expect("request build"),
        )
        .await
        .expect("app should handle request")
}

async fn response_json(resp: Response<axum::body::Body>) -> (StatusCode, Value) {
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let body = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body readable");
    let json = if content_type.starts_with("text/event-stream") {
        let text = String::from_utf8(body.to_vec()).expect("valid utf-8 sse body");
        text.split("\n\n")
            .filter_map(|event| {
                let payload = event
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("data: ")
                            .or_else(|| line.strip_prefix("data:"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if payload.trim().is_empty() {
                    None
                } else {
                    serde_json::from_str::<Value>(&payload).ok()
                }
            })
            .find(|value| value.get("id").is_some())
            .unwrap_or(json!(null))
    } else {
        serde_json::from_slice(&body).unwrap_or(json!(null))
    };
    (status, json)
}

async fn initialize_session(app: axum::Router) -> (axum::Router, String) {
    let init_response = mcp_post(
        &app,
        json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "params": {
                "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "1.0.0"}
            },
            "id": 1
        }),
        None,
    )
    .await;
    let session_id = init_response
        .headers()
        .get("MCP-Session-Id")
        .and_then(|value| value.to_str().ok())
        .expect("session id header")
        .to_string();
    let (_, init_json) = response_json(init_response).await;
    assert_eq!(
        init_json["result"]["protocolVersion"],
        mcp::MCP_PROTOCOL_VERSION
    );

    let initialized_response = mcp_post(
        &app,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
        Some(&session_id),
    )
    .await;
    assert_eq!(initialized_response.status(), StatusCode::ACCEPTED);

    (app, session_id)
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn mcp_initialize_returns_server_info() {
    let app = make_mcp_app();
    let (status, json) = response_json(
        mcp_post(
            &app,
            json!({
                "jsonrpc": "2.0",
                "method": "initialize",
                "params": {
                    "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "test-client", "version": "1.0.0"}
                },
                "id": 1
            }),
            None,
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(json["result"]["protocolVersion"].is_string());
    assert_eq!(json["result"]["serverInfo"]["name"], "awaken-mcp");
}

#[tokio::test]
async fn mcp_tools_list_discovers_agents() {
    let (app, session_id) = initialize_session(make_mcp_app()).await;
    let (status, json) = response_json(
        mcp_post(
            &app,
            json!({
                "jsonrpc": "2.0",
                "method": "tools/list",
                "id": 2
            }),
            Some(&session_id),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let tools = json["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "echo");
    assert!(
        tools[0]["description"]
            .as_str()
            .unwrap()
            .contains("echo bot")
    );

    // Verify schema
    let schema = &tools[0]["inputSchema"];
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["message"].is_object());
    let required = schema["required"].as_array().unwrap();
    assert!(required.contains(&json!("message")));
}

#[tokio::test]
async fn mcp_tools_call_runs_agent_and_returns_text() {
    let (app, session_id) = initialize_session(make_mcp_app()).await;
    let (status, json) = response_json(
        mcp_post(
            &app,
            json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {
                    "name": "echo",
                    "arguments": {
                        "message": "hello world"
                    }
                },
                "id": 3
            }),
            Some(&session_id),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);

    // The response should contain the echoed text.
    let content = &json["result"]["content"];
    assert!(content.is_array(), "expected content array, got: {json}");
    let content_arr = content.as_array().unwrap();
    assert!(!content_arr.is_empty(), "content should not be empty");

    let text = content_arr[0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("echo: hello world"),
        "expected echo response, got: {text}"
    );

    // isError should be false.
    assert_eq!(json["result"]["isError"], false);
}

#[tokio::test]
async fn mcp_tools_call_unknown_tool_returns_error() {
    let (app, session_id) = initialize_session(make_mcp_app()).await;
    let (status, json) = response_json(
        mcp_post(
            &app,
            json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {
                    "name": "nonexistent",
                    "arguments": {
                        "message": "hello"
                    }
                },
                "id": 4
            }),
            Some(&session_id),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // MCP returns isError=true for tool errors, not HTTP errors.
    assert_eq!(json["result"]["isError"], true);
}

#[tokio::test]
async fn mcp_tools_call_missing_message_returns_tool_error() {
    let (app, session_id) = initialize_session(make_mcp_app()).await;
    let (status, json) = response_json(
        mcp_post(
            &app,
            json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {
                    "name": "echo",
                    "arguments": {}
                },
                "id": 5
            }),
            Some(&session_id),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["result"]["isError"], true);
    let text = json["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("message"),
        "error should mention 'message' param"
    );
}

#[tokio::test]
async fn mcp_ping_responds() {
    let (app, session_id) = initialize_session(make_mcp_app()).await;
    let (status, json) = response_json(
        mcp_post(
            &app,
            json!({
                "jsonrpc": "2.0",
                "method": "ping",
                "id": 6
            }),
            Some(&session_id),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(json["result"].is_object());
}

#[tokio::test]
async fn mcp_unknown_method_returns_error() {
    let (app, session_id) = initialize_session(make_mcp_app()).await;
    let (status, json) = response_json(
        mcp_post(
            &app,
            json!({
                "jsonrpc": "2.0",
                "method": "unknown/method",
                "id": 7
            }),
            Some(&session_id),
        )
        .await,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(json["error"].is_object());
    assert_eq!(json["error"]["code"], -32601);
}

// ── Stdio E2E ──

#[tokio::test]
async fn stdio_e2e_full_flow() {
    let runtime = {
        let builder = AgentRuntimeBuilder::new()
            .with_model_binding(
                "test-model",
                ModelBinding {
                    provider_id: "mock".into(),
                    upstream_model: "mock-model".into(),
                },
            )
            .with_provider("mock", Arc::new(EchoExecutor))
            .with_agent_spec(AgentSpec {
                id: "echo".into(),
                model_id: "test-model".into(),
                system_prompt: "You are an echo bot".into(),
                max_rounds: 2,
                ..Default::default()
            });
        Arc::new(builder.build().expect("build runtime"))
    };

    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"id\":1}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"tools/list\",\"id\":2}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"echo\",\"arguments\":{\"message\":\"hi\"}},\"id\":3}\n",
    );

    let mut output = Vec::new();
    awaken_server::protocols::mcp::stdio::serve_stdio_io(runtime, input.as_bytes(), &mut output)
        .await;

    let output_str = String::from_utf8(output).unwrap();
    let lines: Vec<Value> = output_str
        .trim()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // Should have responses for initialize(1), tools/list(2), tools/call(3).
    // Plus possibly logging notifications.
    let responses: Vec<&Value> = lines.iter().filter(|v| v.get("id").is_some()).collect();
    assert!(
        responses.len() >= 3,
        "expected at least 3 responses, got {}: {lines:?}",
        responses.len()
    );

    // Verify initialize response.
    let init = responses.iter().find(|v| v["id"] == 1).unwrap();
    assert!(init["result"]["protocolVersion"].is_string());

    // Verify tools/list response.
    let list = responses.iter().find(|v| v["id"] == 2).unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "echo");

    // Verify tools/call response — should contain echo text.
    let call = responses.iter().find(|v| v["id"] == 3).unwrap();
    let content = call["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("echo: hi"),
        "expected echo response, got: {text}"
    );

    // Check for logging notifications.
    let notifications: Vec<&Value> = lines
        .iter()
        .filter(|v| v.get("method").is_some() && v.get("id").is_none())
        .collect();
    // Should have at least one log notification from the tool call.
    let has_logging = notifications
        .iter()
        .any(|n| n["method"] == "notifications/message");
    assert!(
        has_logging,
        "expected logging notifications, got: {notifications:?}"
    );
}
