#![allow(missing_docs)]

use async_trait::async_trait;
use futures::stream;
use futures::StreamExt;
use genai::chat::{
    ChatOptions, ChatRequest, ChatResponse, ChatStreamEvent, MessageContent, StreamChunk,
    StreamEnd, ToolChunk,
};
use mcp::transport::McpServerConnectionConfig;
use serde_json::{json, Value};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tirea_agentos::contracts::thread::{Message, Thread};
use tirea_agentos::contracts::{AgentEvent, RunContext, RunPolicy};
use tirea_agentos::runtime::loop_runner::{run_loop_stream, Agent, BaseAgent, LlmExecutor};
use tirea_contract::runtime::tool_call::TOOL_CALL_PROGRESS_ACTIVITY_TYPE;
use tirea_contract::Transcoder;
use tirea_extension_mcp::McpToolRegistryManager;
use tirea_protocol_ai_sdk_v6::{AiSdkEncoder, UIStreamEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone)]
struct MockResponse {
    text: String,
    tool_calls: Vec<genai::chat::ToolCall>,
}

impl MockResponse {
    fn text(text: &str) -> Self {
        Self {
            text: text.to_string(),
            tool_calls: Vec::new(),
        }
    }

    fn tool_call(call_id: &str, name: &str, args: Value) -> Self {
        Self {
            text: String::new(),
            tool_calls: vec![genai::chat::ToolCall {
                call_id: call_id.to_string(),
                fn_name: name.to_string(),
                fn_arguments: Value::String(args.to_string()),
                thought_signatures: None,
            }],
        }
    }
}

struct MockStreamProvider {
    responses: Mutex<Vec<MockResponse>>,
}

impl MockStreamProvider {
    fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmExecutor for MockStreamProvider {
    async fn exec_chat_response(
        &self,
        _model: &str,
        _chat_req: ChatRequest,
        _options: Option<&ChatOptions>,
    ) -> genai::Result<ChatResponse> {
        unimplemented!("stream-only provider")
    }

    async fn exec_chat_stream_events(
        &self,
        _model: &str,
        _chat_req: ChatRequest,
        _options: Option<&ChatOptions>,
    ) -> genai::Result<tirea_agentos::runtime::loop_runner::LlmEventStream> {
        let response = {
            let mut guard = self.responses.lock().expect("responses lock");
            if guard.is_empty() {
                MockResponse::text("done")
            } else {
                guard.remove(0)
            }
        };

        let mut events: Vec<genai::Result<ChatStreamEvent>> = Vec::new();
        events.push(Ok(ChatStreamEvent::Start));

        if !response.text.is_empty() {
            events.push(Ok(ChatStreamEvent::Chunk(StreamChunk {
                content: response.text.clone(),
            })));
        }

        for call in &response.tool_calls {
            events.push(Ok(ChatStreamEvent::ToolCallChunk(ToolChunk {
                tool_call: call.clone(),
            })));
        }

        let end = StreamEnd {
            captured_content: if response.tool_calls.is_empty() {
                None
            } else {
                Some(MessageContent::from_tool_calls(response.tool_calls))
            },
            ..Default::default()
        };
        events.push(Ok(ChatStreamEvent::End(end)));

        Ok(Box::pin(stream::iter(events)))
    }

    fn name(&self) -> &'static str {
        "mock_stream_provider"
    }
}

async fn collect_agent_events(
    stream: Pin<Box<dyn futures::Stream<Item = AgentEvent> + Send>>,
) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let mut stream = stream;
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

#[derive(Clone)]
struct HttpResponseSpec {
    status: u16,
    content_type: &'static str,
    body: String,
}

impl HttpResponseSpec {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.to_string(),
        }
    }
}

fn http_status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

fn http_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn http_content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

async fn read_http_json_body(stream: &mut TcpStream) -> Option<Value> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 1024];
    let (header_end, body_len) = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let Some(end) = http_header_end(&buf) else {
            continue;
        };
        let headers = std::str::from_utf8(&buf[..end]).ok()?;
        let len = http_content_length(headers);
        break (end, len);
    };

    while buf.len() < header_end + body_len {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    serde_json::from_slice(&buf[header_end..header_end + body_len]).ok()
}

async fn spawn_http_server(
    handler: Arc<dyn Fn(Value) -> HttpResponseSpec + Send + Sync>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind http listener");
    let addr = listener.local_addr().expect("listener addr");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let Some(request_body) = read_http_json_body(&mut stream).await else {
                    return;
                };
                let response = handler(request_body);
                let payload = response.body;
                let head = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.status,
                    http_status_text(response.status),
                    response.content_type,
                    payload.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(payload.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("http://{}", addr), handle)
}

fn assert_progress_chain(agent_events: &[AgentEvent], ui_events: &[UIStreamEvent], call_id: &str) {
    let stream_id = format!("tool_call:{call_id}");
    assert!(agent_events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::ActivitySnapshot {
                message_id,
                activity_type,
                content,
                ..
            } if message_id == &stream_id
                && activity_type == TOOL_CALL_PROGRESS_ACTIVITY_TYPE
                && content["type"] == json!("tool-call-progress")
                && content["schema"] == json!("tool-call-progress.v1")
                && content["node_id"] == json!(stream_id.as_str())
                && content["progress"] == json!(0.25)
        )
    }));
    assert!(agent_events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::ActivitySnapshot {
                message_id,
                activity_type,
                content,
                ..
            } if message_id == &stream_id
                && activity_type == TOOL_CALL_PROGRESS_ACTIVITY_TYPE
                && content["type"] == json!("tool-call-progress")
                && content["schema"] == json!("tool-call-progress.v1")
                && content["progress"] == json!(1.0)
        )
    }));

    assert!(ui_events.iter().any(|event| {
        matches!(
            event,
            UIStreamEvent::Data { data_type, data, .. }
                if data_type == "data-activity-snapshot"
                    && data["messageId"] == json!(stream_id.as_str())
                    && data["activityType"] == json!(TOOL_CALL_PROGRESS_ACTIVITY_TYPE)
                    && data["content"]["schema"] == json!("tool-call-progress.v1")
                    && data["content"]["progress"] == json!(0.25)
        )
    }));
    assert!(ui_events.iter().any(|event| {
        matches!(
            event,
            UIStreamEvent::Data { data_type, data, .. }
                if data_type == "data-activity-snapshot"
                    && data["messageId"] == json!(stream_id.as_str())
                    && data["activityType"] == json!(TOOL_CALL_PROGRESS_ACTIVITY_TYPE)
                    && data["content"]["type"] == json!("tool-call-progress")
                    && data["content"]["progress"] == json!(1.0)
        )
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn real_mcp_progress_notifications_flow_to_ui_data_events() {
    let server_script = r#"
import json
import sys
import time

def send(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    message = json.loads(raw)
    method = message.get("method")
    msg_id = message.get("id")

    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "protocolVersion": "2025-03-26",
                "serverInfo": {"name": "progress-server", "version": "0.1.0"},
                "capabilities": {}
            }
        })
    elif method == "tools/list":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "tools": [{
                    "name": "echo_progress",
                    "title": "Echo Progress",
                    "description": "Echo text with progress notifications",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "message": {"type": "string"}
                        },
                        "required": ["message"]
                    }
                }]
            }
        })
    elif method == "tools/call":
        params = message.get("params") or {}
        meta = params.get("_meta") or {}
        token = meta.get("progressToken")
        if token is not None:
            send({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {
                    "progressToken": token,
                    "progress": 1.0,
                    "total": 4.0
                }
            })
            time.sleep(0.01)
            send({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {
                    "progressToken": token,
                    "progress": 4.0,
                    "total": 4.0
                }
            })

        arguments = params.get("arguments") or {}
        text = arguments.get("message") or "ok"
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "content": [{"type": "text", "text": text}]
            }
        })
    else:
        if msg_id is not None:
            send({"jsonrpc": "2.0", "id": msg_id, "result": {}})
"#;

    let cfg = McpServerConnectionConfig::stdio(
        "progress_server",
        "python3",
        vec![
            "-u".to_string(),
            "-c".to_string(),
            server_script.to_string(),
        ],
    );

    let manager = McpToolRegistryManager::connect([cfg])
        .await
        .expect("connect MCP server");
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__echo_progress"))
        .expect("discover echo_progress tool");

    let llm = MockStreamProvider::new(vec![
        MockResponse::tool_call("call_progress", &tool_id, json!({ "message": "hello" })),
        MockResponse::text("done"),
    ]);
    let config = BaseAgent::new("mock").with_llm_executor(Arc::new(llm) as Arc<dyn LlmExecutor>);

    let thread = Thread::new("thread-mcp-progress").with_message(Message::user("run"));
    let run_ctx = RunContext::from_thread(&thread, RunPolicy::default()).expect("run context");
    let agent_events = collect_agent_events(run_loop_stream(
        Arc::new(config) as Arc<dyn Agent>,
        registry.snapshot(),
        run_ctx,
        None,
        None,
        None,
    ))
    .await;

    let mut encoder = AiSdkEncoder::new();
    let mut ui_events = Vec::new();
    for event in &agent_events {
        ui_events.extend(encoder.transcode(event));
    }

    assert_progress_chain(&agent_events, &ui_events, "call_progress");
}

#[tokio::test(flavor = "multi_thread")]
async fn real_http_mcp_progress_notifications_flow_to_ui_data_events() {
    let (endpoint, server) = spawn_http_server(Arc::new(|request| {
        let method = request["method"].as_str().unwrap_or_default();
        match method {
            "tools/list" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "echo_http_progress",
                        "title": "Echo HTTP Progress",
                        "description": "Echo text with HTTP progress notifications",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "message": {"type": "string"}
                            },
                            "required": ["message"]
                        }
                    }]
                }
            })),
            "tools/call" => {
                let token = request["params"]["_meta"]["progressToken"].clone();
                let text = request["params"]["arguments"]["message"]
                    .as_str()
                    .unwrap_or("ok");
                HttpResponseSpec::json(json!([
                    {
                        "jsonrpc": "2.0",
                        "method": "notifications/progress",
                        "params": {
                            "progressToken": token,
                            "progress": 1.0,
                            "total": 4.0
                        }
                    },
                    {
                        "jsonrpc": "2.0",
                        "method": "notifications/progress",
                        "params": {
                            "progressToken": request["params"]["_meta"]["progressToken"].clone(),
                            "progress": 4.0,
                            "total": 4.0
                        }
                    },
                    {
                        "jsonrpc": "2.0",
                        "id": request["id"].clone(),
                        "result": {
                            "content": [{"type":"text","text": text}]
                        }
                    }
                ]))
            }
            "initialize" => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": {
                    "protocolVersion": "2025-03-26",
                    "serverInfo": {"name": "progress-server", "version": "0.1.0"},
                    "capabilities": {}
                }
            })),
            _ => HttpResponseSpec::json(json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": {}
            })),
        }
    }))
    .await;

    let cfg = McpServerConnectionConfig::http("http_progress_server", endpoint);
    let manager = McpToolRegistryManager::connect([cfg])
        .await
        .expect("connect HTTP MCP server");
    let registry = manager.registry();
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.ends_with("__echo_http_progress"))
        .expect("discover echo_http_progress tool");

    let llm = MockStreamProvider::new(vec![
        MockResponse::tool_call(
            "call_http_progress",
            &tool_id,
            json!({ "message": "hello-http" }),
        ),
        MockResponse::text("done"),
    ]);
    let config = BaseAgent::new("mock").with_llm_executor(Arc::new(llm) as Arc<dyn LlmExecutor>);

    let thread = Thread::new("thread-mcp-http-progress").with_message(Message::user("run"));
    let run_ctx = RunContext::from_thread(&thread, RunPolicy::default()).expect("run context");
    let agent_events = collect_agent_events(run_loop_stream(
        Arc::new(config) as Arc<dyn Agent>,
        registry.snapshot(),
        run_ctx,
        None,
        None,
        None,
    ))
    .await;

    let mut encoder = AiSdkEncoder::new();
    let mut ui_events = Vec::new();
    for event in &agent_events {
        ui_events.extend(encoder.transcode(event));
    }

    server.abort();
    assert_progress_chain(&agent_events, &ui_events, "call_http_progress");
}
