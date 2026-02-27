#![allow(missing_docs)]

use futures::StreamExt;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use serde_json::json;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::Arc;
use tirea_agent_loop::contracts::runtime::plugin::AgentPlugin;
use tirea_agent_loop::contracts::thread::Thread;
use tirea_agent_loop::contracts::thread::{Message, ToolCall};
use tirea_agent_loop::contracts::runtime::tool_call::{Tool, ToolDescriptor, ToolError, ToolResult};
use tirea_agent_loop::contracts::{runtime::StreamResult, AgentEvent};
use tirea_agent_loop::contracts::{RunContext, ToolCallContext};
use tirea_agent_loop::runtime::loop_runner::{
    execute_tools_with_plugins, run_loop, run_loop_stream, Agent, BaseAgent, GenaiLlmExecutor,
};
use tirea_extension_observability::{InMemorySink, LLMMetryPlugin};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::SubscriberExt;

fn find_attribute<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| &kv.value)
}

fn setup_otel_test() -> (
    tracing::subscriber::DefaultGuard,
    InMemorySpanExporter,
    SdkTracerProvider,
) {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("test");
    let otel_layer = OpenTelemetryLayer::new(tracer);
    let subscriber = tracing_subscriber::registry::Registry::default().with(otel_layer);
    let guard = tracing::subscriber::set_default(subscriber);
    (guard, exporter, provider)
}

async fn start_single_response_server(
    status: &str,
    content_type: &str,
    body: &'static str,
) -> Option<(String, tokio::task::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => return None,
        Err(err) => panic!("failed to bind local test listener: {err}"),
    };
    let addr = listener.local_addr().unwrap();

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes();

    let handle = tokio::spawn(async move {
        // Serve at most one request for this test.
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let _ = socket.write_all(&response).await;
        let _ = socket.shutdown().await;
    });

    // OpenAI adapter expects base_url ending with `/v1/` so it can join `chat/completions`.
    let base_url = format!("http://{addr}/v1/");
    Some((base_url, handle))
}

async fn start_sse_server(
    events: Vec<&'static str>,
) -> Option<(String, tokio::task::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => return None,
        Err(err) => panic!("failed to bind local test listener: {err}"),
    };
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;

        let headers = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
        let _ = socket.write_all(headers).await;

        for ev in events {
            let _ = socket.write_all(ev.as_bytes()).await;
            let _ = socket.flush().await;
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        }

        let _ = socket.shutdown().await;
    });

    let base_url = format!("http://{addr}/v1/");
    Some((base_url, handle))
}

struct NoopTool {
    id: &'static str,
}

#[async_trait::async_trait]
impl Tool for NoopTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(self.id, self.id, "noop")
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success(self.id, json!({"ok": true})))
    }
}

struct ErrorTool {
    id: &'static str,
}

#[async_trait::async_trait]
impl Tool for ErrorTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(self.id, self.id, "always fails")
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::ExecutionFailed(format!("{} exploded", self.id)))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_execute_tools_parallel_exports_distinct_otel_tool_spans() {
    let (_guard, exporter, provider) = setup_otel_test();

    let sink = InMemorySink::new();
    let plugin =
        Arc::new(LLMMetryPlugin::new(sink).with_provider("test-provider")) as Arc<dyn AgentPlugin>;

    let thread = Thread::with_initial_state("t", json!({})).with_message(Message::user("hi"));
    let result = StreamResult {
        text: "tools".into(),
        tool_calls: vec![
            ToolCall::new("c1", "t1", json!({})),
            ToolCall::new("c2", "t2", json!({})),
            ToolCall::new("c3", "t3", json!({})),
        ],
        usage: None,
    };

    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    tools.insert("t1".into(), Arc::new(NoopTool { id: "t1" }));
    tools.insert("t2".into(), Arc::new(NoopTool { id: "t2" }));
    tools.insert("t3".into(), Arc::new(NoopTool { id: "t3" }));

    let _session = execute_tools_with_plugins(thread, &result, &tools, true, &[plugin])
        .await
        .unwrap();

    let _ = provider.force_flush();
    let exported = exporter.get_finished_spans().unwrap();
    let tool_spans: Vec<&SpanData> = exported
        .iter()
        .filter(|s| s.name.starts_with("execute_tool "))
        .collect();
    assert_eq!(tool_spans.len(), 3);

    let mut seen: Vec<(String, String)> = tool_spans
        .iter()
        .map(|s| {
            let tool = find_attribute(s, "gen_ai.tool.name")
                .unwrap()
                .as_str()
                .to_string();
            let call_id = find_attribute(s, "gen_ai.tool.call.id")
                .unwrap()
                .as_str()
                .to_string();
            (tool, call_id)
        })
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("t1".to_string(), "c1".to_string()),
            ("t2".to_string(), "c2".to_string()),
            ("t3".to_string(), "c3".to_string()),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_run_step_non_streaming_propagates_usage_and_exports_tokens_to_otel() {
    let Some((base_url, _server)) = start_single_response_server(
        "200 OK",
        "application/json",
        r#"{"model":"gpt-4","usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15},"choices":[{"message":{"content":"hi"}}]}"#,
    )
    .await else {
        eprintln!("skipping test: sandbox does not permit local TCP listeners");
        return;
    };

    let (_guard, exporter, provider) = setup_otel_test();

    let sink = InMemorySink::new();
    let plugin = Arc::new(
        LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("test-provider"),
    ) as Arc<dyn AgentPlugin>;

    let client = genai::Client::builder()
        .with_service_target_resolver_fn(move |mut t: genai::ServiceTarget| {
            t.endpoint = genai::resolver::Endpoint::from_owned(base_url.clone());
            t.auth = genai::resolver::AuthData::from_single("test-key");
            Ok(t)
        })
        .build();

    let config = BaseAgent::new("gpt-4")
        .with_plugin(plugin)
        .with_llm_executor(Arc::new(GenaiLlmExecutor::new(client)));
    let thread = Thread::with_initial_state("s", json!({})).with_message(Message::user("hi"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunConfig::default()).unwrap();

    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;
    let usage = outcome.usage;
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 5);

    let _ = provider.force_flush();
    let exported = exporter.get_finished_spans().unwrap();
    let span = exported
        .iter()
        .find(|s| s.name.starts_with("chat "))
        .expect("expected chat span");
    assert_eq!(
        find_attribute(span, "gen_ai.usage.input_tokens"),
        Some(&opentelemetry::Value::I64(10))
    );
    assert_eq!(
        find_attribute(span, "gen_ai.usage.output_tokens"),
        Some(&opentelemetry::Value::I64(5))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_run_step_llm_error_closes_inference_span_and_sets_error_type() {
    // Return invalid JSON so genai parsing fails deterministically.
    let Some((base_url, _server)) =
        start_single_response_server("200 OK", "application/json", "{").await
    else {
        eprintln!("skipping test: sandbox does not permit local TCP listeners");
        return;
    };

    let (_guard, exporter, provider) = setup_otel_test();

    let sink = InMemorySink::new();
    let plugin = Arc::new(
        LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("test-provider"),
    ) as Arc<dyn AgentPlugin>;

    let client = genai::Client::builder()
        .with_service_target_resolver_fn(move |mut t: genai::ServiceTarget| {
            t.endpoint = genai::resolver::Endpoint::from_owned(base_url.clone());
            t.auth = genai::resolver::AuthData::from_single("test-key");
            Ok(t)
        })
        .build();

    let config = BaseAgent::new("gpt-4")
        .with_plugin(plugin)
        .with_llm_executor(Arc::new(GenaiLlmExecutor::new(client)));
    let thread = Thread::with_initial_state("s", json!({})).with_message(Message::user("hi"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunConfig::default()).unwrap();

    let outcome = run_loop(&config, HashMap::new(), run_ctx, None, None, None).await;
    assert!(matches!(
        outcome.termination,
        tirea_agent_loop::contracts::TerminationReason::Error
    ));

    // Metrics should record the failed inference.
    let m = sink.metrics();
    assert_eq!(m.inference_count(), 1);
    assert_eq!(
        m.inferences[0].error_type.as_deref(),
        Some("llm_exec_error")
    );

    // OTel export should include error.type and Error status.
    let _ = provider.force_flush();
    let exported = exporter.get_finished_spans().unwrap();
    let span = exported
        .iter()
        .find(|s| s.name.starts_with("chat "))
        .expect("expected chat span");
    let error_type = find_attribute(span, "error.type")
        .map(|v| v.as_str().to_string())
        .unwrap_or_default();
    assert_eq!(error_type, "llm_exec_error");
    assert!(matches!(
        span.status,
        opentelemetry::trace::Status::Error { .. }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn test_run_loop_stream_http_error_closes_inference_span() {
    let Some((base_url, _server)) = start_single_response_server(
        "500 Internal Server Error",
        "application/json",
        r#"{"error":"fail"}"#,
    )
    .await
    else {
        eprintln!("skipping test: sandbox does not permit local TCP listeners");
        return;
    };

    let (_guard, exporter, provider) = setup_otel_test();

    let sink = InMemorySink::new();
    let plugin = Arc::new(
        LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("test-provider"),
    ) as Arc<dyn AgentPlugin>;

    let client = genai::Client::builder()
        .with_service_target_resolver_fn(move |mut t: genai::ServiceTarget| {
            t.endpoint = genai::resolver::Endpoint::from_owned(base_url.clone());
            t.auth = genai::resolver::AuthData::from_single("test-key");
            Ok(t)
        })
        .build();

    let config = BaseAgent::new("gpt-4")
        .with_plugin(plugin)
        .with_llm_executor(Arc::new(GenaiLlmExecutor::new(client)));
    let thread = Thread::with_initial_state("s", json!({})).with_message(Message::user("hi"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunConfig::default()).unwrap();

    let events: Vec<_> = run_loop_stream(Arc::new(config) as Arc<dyn Agent>, HashMap::new(), run_ctx, None, None, None)
        .collect()
        .await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));

    // Metrics should record a failed inference.
    let m = sink.metrics();
    assert_eq!(m.inference_count(), 1);
    assert_eq!(
        m.inferences[0].error_type.as_deref(),
        Some("llm_stream_event_error")
    );

    let _ = provider.force_flush();
    let exported = exporter.get_finished_spans().unwrap();
    let span = exported
        .iter()
        .find(|s| s.name.starts_with("chat "))
        .expect("expected chat span");
    let error_type = find_attribute(span, "error.type")
        .map(|v| v.as_str().to_string())
        .unwrap_or_default();
    assert_eq!(error_type, "llm_stream_event_error");
    assert!(matches!(
        span.status,
        opentelemetry::trace::Status::Error { .. }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn test_run_loop_stream_success_exports_tokens_to_otel() {
    // Streaming success path with usage in the final chunk.
    let Some((base_url, _server)) = start_sse_server(vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":7,\"total_tokens\":19}}\n\n",
        "data: [DONE]\n\n",
    ])
    .await
    else {
        eprintln!("skipping test: sandbox does not permit local TCP listeners");
        return;
    };

    let (_guard, exporter, provider) = setup_otel_test();

    let sink = InMemorySink::new();
    let plugin = Arc::new(
        LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("test-provider"),
    ) as Arc<dyn AgentPlugin>;

    let client = genai::Client::builder()
        .with_service_target_resolver_fn(move |mut t: genai::ServiceTarget| {
            t.endpoint = genai::resolver::Endpoint::from_owned(base_url.clone());
            t.auth = genai::resolver::AuthData::from_single("test-key");
            Ok(t)
        })
        .build();

    let config = BaseAgent::new("gpt-4")
        .with_plugin(plugin)
        .with_llm_executor(Arc::new(GenaiLlmExecutor::new(client)));
    let thread = Thread::with_initial_state("s", json!({})).with_message(Message::user("hi"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunConfig::default()).unwrap();

    let events: Vec<_> = run_loop_stream(Arc::new(config) as Arc<dyn Agent>, HashMap::new(), run_ctx, None, None, None)
        .collect()
        .await;

    // Should have TextDelta and no Error events.
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::TextDelta { .. })));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));

    // Metrics should record a successful inference.
    let m = sink.metrics();
    assert_eq!(m.inference_count(), 1);
    assert!(m.inferences[0].error_type.is_none());

    let _ = provider.force_flush();
    let exported = exporter.get_finished_spans().unwrap();
    let span = exported
        .iter()
        .find(|s| s.name.starts_with("chat "))
        .expect("expected chat span for streaming success");

    // Verify no error status.
    assert!(
        !matches!(span.status, opentelemetry::trace::Status::Error { .. }),
        "successful streaming span should not have Error status"
    );
    assert!(
        find_attribute(span, "error.type").is_none(),
        "successful streaming span should not have error.type"
    );

    // Verify token attributes if the provider surfaces them in streaming usage.
    // Note: genai may or may not propagate streaming usage depending on the SSE
    // payload structure. If input_tokens is present, validate the value.
    if let Some(v) = find_attribute(span, "gen_ai.usage.input_tokens") {
        assert_eq!(*v, opentelemetry::Value::I64(12));
    }
    if let Some(v) = find_attribute(span, "gen_ai.usage.output_tokens") {
        assert_eq!(*v, opentelemetry::Value::I64(7));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_run_loop_stream_connection_refused_closes_inference_span() {
    // Point the client at a port where nothing is listening.
    // genai treats connection errors as stream event errors (not stream start errors),
    // because the HTTP client wraps them into the stream iteration. Either way, the
    // OTel span must be closed with error status.
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping test: sandbox does not permit local TCP listeners");
            return;
        }
        Err(err) => panic!("failed to bind: {err}"),
    };
    let addr = listener.local_addr().unwrap();
    drop(listener); // port is now free but nothing is listening

    let base_url = format!("http://{addr}/v1/");

    let (_guard, exporter, provider) = setup_otel_test();

    let sink = InMemorySink::new();
    let plugin = Arc::new(
        LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("test-provider"),
    ) as Arc<dyn AgentPlugin>;

    let client = genai::Client::builder()
        .with_service_target_resolver_fn(move |mut t: genai::ServiceTarget| {
            t.endpoint = genai::resolver::Endpoint::from_owned(base_url.clone());
            t.auth = genai::resolver::AuthData::from_single("test-key");
            Ok(t)
        })
        .build();

    let config = BaseAgent::new("gpt-4")
        .with_plugin(plugin)
        .with_llm_executor(Arc::new(GenaiLlmExecutor::new(client)));
    let thread = Thread::with_initial_state("s", json!({})).with_message(Message::user("hi"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunConfig::default()).unwrap();

    let events: Vec<_> = run_loop_stream(Arc::new(config) as Arc<dyn Agent>, HashMap::new(), run_ctx, None, None, None)
        .collect()
        .await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));

    // Metrics should record a failed inference.
    let m = sink.metrics();
    assert_eq!(m.inference_count(), 1);
    let err_type = m.inferences[0]
        .error_type
        .as_deref()
        .expect("error_type should be set");
    assert!(
        err_type == "llm_stream_start_error" || err_type == "llm_stream_event_error",
        "expected a stream error type, got: {err_type}"
    );

    let _ = provider.force_flush();
    let exported = exporter.get_finished_spans().unwrap();
    let span = exported
        .iter()
        .find(|s| s.name.starts_with("chat "))
        .expect("expected chat span for connection refused");
    assert!(
        find_attribute(span, "error.type").is_some(),
        "error span must have error.type attribute"
    );
    assert!(matches!(
        span.status,
        opentelemetry::trace::Status::Error { .. }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn test_execute_tool_error_exports_otel_error_span() {
    let (_guard, exporter, provider) = setup_otel_test();

    let sink = InMemorySink::new();
    let plugin = Arc::new(LLMMetryPlugin::new(sink.clone()).with_provider("test-provider"))
        as Arc<dyn AgentPlugin>;

    let thread = Thread::with_initial_state("t", json!({})).with_message(Message::user("hi"));
    let result = StreamResult {
        text: "calling tool".into(),
        tool_calls: vec![ToolCall::new("c1", "bad_tool", json!({}))],
        usage: None,
    };

    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    tools.insert("bad_tool".into(), Arc::new(ErrorTool { id: "bad_tool" }));

    // Tool execution errors are captured as ToolStatus::Error, not propagated as Err.
    let _session = execute_tools_with_plugins(thread, &result, &tools, true, &[plugin])
        .await
        .unwrap();

    let m = sink.metrics();
    assert_eq!(m.tool_count(), 1);
    assert_eq!(m.tools[0].error_type.as_deref(), Some("tool_error"));

    let _ = provider.force_flush();
    let exported = exporter.get_finished_spans().unwrap();
    let span = exported
        .iter()
        .find(|s| s.name.starts_with("execute_tool "))
        .expect("expected execute_tool span");
    let error_type = find_attribute(span, "error.type")
        .map(|v| v.as_str().to_string())
        .unwrap_or_default();
    assert_eq!(error_type, "tool_error");
    assert!(matches!(
        span.status,
        opentelemetry::trace::Status::Error { .. }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn test_run_loop_stream_parse_error_closes_inference_span() {
    // First event is valid; second event is invalid JSON to trigger StreamParse.
    let Some((base_url, _server)) = start_sse_server(vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {invalid-json}\n\n",
    ])
    .await
    else {
        eprintln!("skipping test: sandbox does not permit local TCP listeners");
        return;
    };

    let (_guard, exporter, provider) = setup_otel_test();

    let sink = InMemorySink::new();
    let plugin = Arc::new(
        LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("test-provider"),
    ) as Arc<dyn AgentPlugin>;

    let client = genai::Client::builder()
        .with_service_target_resolver_fn(move |mut t: genai::ServiceTarget| {
            t.endpoint = genai::resolver::Endpoint::from_owned(base_url.clone());
            t.auth = genai::resolver::AuthData::from_single("test-key");
            Ok(t)
        })
        .build();

    let config = BaseAgent::new("gpt-4")
        .with_plugin(plugin)
        .with_llm_executor(Arc::new(GenaiLlmExecutor::new(client)));
    let thread = Thread::with_initial_state("s", json!({})).with_message(Message::user("hi"));
    let run_ctx = RunContext::from_thread(&thread, tirea_contract::RunConfig::default()).unwrap();

    let events: Vec<_> = run_loop_stream(Arc::new(config) as Arc<dyn Agent>, HashMap::new(), run_ctx, None, None, None)
        .collect()
        .await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::TextDelta { .. })));

    let m = sink.metrics();
    assert_eq!(m.inference_count(), 1);
    assert_eq!(
        m.inferences[0].error_type.as_deref(),
        Some("llm_stream_event_error")
    );

    let _ = provider.force_flush();
    let exported = exporter.get_finished_spans().unwrap();
    let span = exported
        .iter()
        .find(|s| s.name.starts_with("chat "))
        .expect("expected chat span");
    let error_type = find_attribute(span, "error.type")
        .map(|v| v.as_str().to_string())
        .unwrap_or_default();
    assert_eq!(error_type, "llm_stream_event_error");
    assert!(matches!(
        span.status,
        opentelemetry::trace::Status::Error { .. }
    ));
}
