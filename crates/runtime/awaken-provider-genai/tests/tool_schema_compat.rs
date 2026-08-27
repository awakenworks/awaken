//! Provider-wire contract tests for schemas generated from typed Awaken tools.
//! These run without credentials: a local HTTP peer captures the exact JSON body
//! emitted by each SDK adapter before returning an intentionally empty response.

use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_ext_builtin_tools::builtin_tools;
use awaken_provider_genai::{AdapterKind, GenaiExecutor};
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, LlmExecutor};
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

async fn capture_one_request() -> (String, tokio::sync::oneshot::Receiver<Value>) {
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

        // The payload is the assertion target. A minimal response is enough to
        // let the adapter return without coupling this test to response parsing.
        let body = b"{}";
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("response head");
        socket.write_all(body).await.expect("response body");
    });
    (format!("http://{address}/v1"), receive)
}

async fn provider_payload(adapter: AdapterKind) -> Value {
    let (base_url, payload) = capture_one_request().await;
    let executor = GenaiExecutor::from_materialized_endpoint(adapter, base_url, "fixture-secret")
        .with_timeout(Duration::from_secs(5));
    let model = if adapter == AdapterKind::Vertex {
        "gemini-2.5-flash"
    } else {
        "deepseek-v4-pro"
    };
    let result = executor
        .infer(request(model, generated_read_descriptor()))
        .await;
    tokio::time::timeout(Duration::from_secs(5), payload)
        .await
        .unwrap_or_else(|_| panic!("provider request was sent; adapter returned {result:?}"))
        .expect("capture task returned a payload")
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
