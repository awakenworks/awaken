//! An incomplete provider stream must fail retryably, never become a partial
//! success or hang the run. Protocol End owns completion; the per-event idle
//! bound owns silent stalls; the fixed total deadline bounds active streams.

use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_provider_genai::{AdapterKind, GenaiExecutor};
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, DeltaSink, LlmExecutor, StopReason};
use awaken_runtime_contract::model_operation::{
    ModelExecutionEvidence, ResponseEnvelope, TerminalReceipt,
};
use awaken_runtime_contract::resolved::ModelBinding;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone)]
enum StreamFixture {
    HttpFailure {
        status: u16,
        requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    },
    Boundary {
        anthropic: bool,
        tool: bool,
        terminal: bool,
        broken_body: bool,
    },
    AnthropicStall,
    AnthropicHeartbeat,
    OpenAiEmptyChoiceStall,
    OpenAiEmptyTerminal,
    OpenAiCompleteAfterEmptyChoices,
}

/// The one local SSE fixture owner. Each behavior changes only the provider
/// events after the shared HTTP boundary, so adjacent stream rules cannot drift
/// across copied socket servers.
async fn spawn_sse_server(fixture: StreamFixture) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let addr = listener.local_addr().expect("addr");
    let serving = fixture.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let fixture = serving.clone();
            tokio::spawn(async move {
                // Drain the request head; the response does not depend on it.
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                if let StreamFixture::HttpFailure { status, requests } = &fixture {
                    requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let response = format!(
                        "HTTP/1.1 {status} Failure\r\nlocation: http://{addr}/repeated\r\nretry-after: 0\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("failure response");
                    return;
                }
                if let StreamFixture::Boundary {
                    anthropic,
                    tool,
                    terminal,
                    broken_body,
                } = fixture
                {
                    let body = boundary_events(anthropic, tool, terminal);
                    let length = body.len() + if broken_body { 100 } else { 0 };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n"
                    );
                    socket
                        .write_all(head.as_bytes())
                        .await
                        .expect("boundary headers");
                    socket
                        .write_all(body.as_bytes())
                        .await
                        .expect("boundary body");
                    socket.shutdown().await.expect("boundary close");
                    return;
                }
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
                let _ = socket.write_all(head.as_bytes()).await;
                match fixture {
                    StreamFixture::HttpFailure { .. } => unreachable!("handled at HTTP boundary"),
                    StreamFixture::Boundary { .. } => unreachable!("handled at HTTP boundary"),
                    StreamFixture::AnthropicStall => {
                        let events = concat!(
                            "event: message_start\n",
                            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",",
                            "\"type\":\"message\",\"role\":\"assistant\",\"content\":[],",
                            "\"model\":\"m\",\"stop_reason\":null,\"stop_sequence\":null,",
                            "\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                            "event: content_block_start\n",
                            "data: {\"type\":\"content_block_start\",\"index\":0,",
                            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                            "event: content_block_delta\n",
                            "data: {\"type\":\"content_block_delta\",\"index\":0,",
                            "\"delta\":{\"type\":\"text_delta\",\"text\":\"hel\"}}\n\n",
                        );
                        let _ = socket.write_all(events.as_bytes()).await;
                        let _ = socket.flush().await;
                        std::future::pending::<()>().await;
                    }
                    StreamFixture::AnthropicHeartbeat => {
                        let start = concat!(
                            "event: message_start\n",
                            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",",
                            "\"type\":\"message\",\"role\":\"assistant\",\"content\":[],",
                            "\"model\":\"m\",\"stop_reason\":null,\"stop_sequence\":null,",
                            "\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                        );
                        let _ = socket.write_all(start.as_bytes()).await;
                        let _ = socket.flush().await;
                        loop {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            if socket
                                .write_all(b"event: ping\ndata: {\"type\":\"ping\"}\n\n")
                                .await
                                .is_err()
                            {
                                break;
                            }
                            let _ = socket.flush().await;
                        }
                    }
                    StreamFixture::OpenAiEmptyChoiceStall => loop {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        if socket.write_all(openai_empty_choice()).await.is_err() {
                            break;
                        }
                        let _ = socket.flush().await;
                    },
                    StreamFixture::OpenAiEmptyTerminal => {
                        let _ = socket.write_all(openai_empty_choice()).await;
                        let _ = socket.write_all(openai_finish()).await;
                        let _ = socket.write_all(b"data: [DONE]\n\n").await;
                        let _ = socket.flush().await;
                    }
                    StreamFixture::OpenAiCompleteAfterEmptyChoices => {
                        let _ = socket.write_all(openai_empty_choice()).await;
                        let _ = socket.write_all(openai_empty_choice()).await;
                        let _ = socket
                            .write_all(
                                concat!(
                                    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",",
                                    "\"created\":1,\"model\":\"model-test\",\"choices\":[{\"index\":0,",
                                    "\"delta\":{\"content\":\"complete\"},\"finish_reason\":null}]}\n\n",
                                )
                                .as_bytes(),
                            )
                            .await;
                        let _ = socket.write_all(openai_finish()).await;
                        let _ = socket.write_all(b"data: [DONE]\n\n").await;
                        let _ = socket.flush().await;
                        std::future::pending::<()>().await;
                    }
                }
            });
        }
    });
    let suffix = match fixture {
        StreamFixture::HttpFailure { .. } => "/v1",
        StreamFixture::Boundary {
            anthropic: true, ..
        } => "/",
        StreamFixture::Boundary {
            anthropic: false, ..
        } => "/v1",
        StreamFixture::AnthropicStall | StreamFixture::AnthropicHeartbeat => "/",
        StreamFixture::OpenAiEmptyChoiceStall
        | StreamFixture::OpenAiEmptyTerminal
        | StreamFixture::OpenAiCompleteAfterEmptyChoices => "/v1",
    };
    format!("http://{addr}{suffix}")
}

#[tokio::test]
async fn production_model_clients_do_not_redirect_or_retry_ambiguous_http_responses() {
    use awaken_provider_genai::OpenAiResponsesExecutor;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    // H1 redirect 307/308; H2 retryable status 429/503; H3 Anthropic/OpenAI
    // streaming and nonstreaming + Responses. All H1/H2 * H3 must make exactly
    // one HTTP request and retain Unknown, even with Location and Retry-After.
    // This exercises production construction, not a custom test HTTP client.
    for status in [307, 308, 429, 503] {
        for (kind, streaming) in [(0, false), (0, true), (1, false), (1, true), (2, false)] {
            let requests = Arc::new(AtomicUsize::new(0));
            let base = spawn_sse_server(StreamFixture::HttpFailure {
                status,
                requests: requests.clone(),
            })
            .await;
            let executor: Box<dyn LlmExecutor> = match kind {
                2 => Box::new(
                    OpenAiResponsesExecutor::from_materialized_endpoint("openai", &base, "test")
                        .unwrap(),
                ),
                _ => Box::new(
                    GenaiExecutor::from_materialized_endpoint(
                        if kind == 0 {
                            AdapterKind::Anthropic
                        } else {
                            AdapterKind::OpenAI
                        },
                        base,
                        "test",
                    )
                    .unwrap(),
                ),
            };
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                if streaming {
                    executor.infer_streaming(request(), &NullSink).await
                } else {
                    executor.infer(request()).await
                }
            })
            .await
            .expect("no hidden retry delay");
            let error = result.expect_err("HTTP failure cannot become model completion");
            assert_eq!(
                requests.load(Ordering::SeqCst),
                1,
                "{status}/{kind}/{streaming}"
            );
            assert_eq!(error.execution_evidence(), ModelExecutionEvidence::Unknown);
        }
    }
}

fn boundary_events(anthropic: bool, tool: bool, terminal: bool) -> String {
    use serde_json::json;
    if anthropic {
        let mut events = vec![
            json!({"type":"message_start","message":{"id":"msg_boundary","type":"message","role":"assistant","model":"m","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block": if tool {
                json!({"type":"tool_use","id":"call_boundary","name":"measure","input":{}})
            } else { json!({"type":"text","text":""}) }}),
            json!({"type":"content_block_delta","index":0,"delta": if tool {
                json!({"type":"input_json_delta","partial_json":"{\"x\":1}"})
            } else { json!({"type":"text_delta","text":"partial answer"}) }}),
        ];
        if terminal {
            events.extend([
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":if tool {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":1}}),
                json!({"type":"message_stop"}),
            ]);
        }
        events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().expect("event type")
                )
            })
            .collect()
    } else {
        let delta = if tool {
            json!({"tool_calls":[{"index":0,"id":"call_boundary","type":"function","function":{"name":"measure","arguments":"{\"x\":1}"}}]})
        } else {
            json!({"content":"partial answer"})
        };
        let chunk = json!({"id":"chatcmpl-boundary","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
        let mut body = format!("data: {chunk}\n\n");
        if terminal {
            let end = json!({"id":"chatcmpl-boundary","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":if tool {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
            body.push_str(&format!("data: {end}\n\ndata: [DONE]\n\n"));
        }
        body
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_protocol_end_not_http_eof_owns_response_success() {
    // Cause/effect table: C1 Anthropic/OpenAI wire; C2 text/typed tool progress;
    // C3 protocol End present/absent; C4 HTTP body complete/truncated. Rules:
    // E1 !C3 => retryable error, never a committed partial response/tool;
    // E2 C3 => exact captured response without waiting for EOF, even if the
    // server closes a longer declared body after that terminal. Every C1/C2/C4
    // combination is exercised for both C3 values through the real pinned SDK.
    // This proves a provider boundary, not the cause of a live network failure.
    for anthropic in [true, false] {
        for tool in [false, true] {
            for terminal in [false, true] {
                for broken_body in [false, true] {
                    let base_url = spawn_sse_server(StreamFixture::Boundary {
                        anthropic,
                        tool,
                        terminal,
                        broken_body,
                    })
                    .await;
                    let adapter = if anthropic {
                        AdapterKind::Anthropic
                    } else {
                        AdapterKind::OpenAI
                    };
                    let executor =
                        GenaiExecutor::from_materialized_endpoint(adapter, base_url, "test-key")
                            .expect("boundary client")
                            .with_idle_timeout(Duration::from_secs(1));
                    let result = tokio::time::timeout(
                        Duration::from_secs(3),
                        executor.infer_streaming(request(), &NullSink),
                    )
                    .await
                    .expect("boundary settles without a stalled read");
                    if terminal {
                        let response = result.expect("protocol End permits success");
                        assert_eq!(
                            response.stop_reason,
                            Some(if tool {
                                StopReason::ToolUse
                            } else {
                                StopReason::NaturalEnd
                            })
                        );
                        assert!(response.usage.is_some(), "terminal usage is retained");
                        if tool {
                            let calls = response.output.tool_calls();
                            assert_eq!(calls.len(), 1);
                            assert_eq!(calls[0].call_id, "call_boundary");
                            assert_eq!(calls[0].tool_id, "measure");
                            assert_eq!(calls[0].arguments, serde_json::json!({"x":1}));
                        } else {
                            assert_eq!(response.output.text_content(), "partial answer");
                        }
                    } else {
                        let error =
                            result.expect_err("HTTP EOF cannot authorize a partial answer or tool");
                        assert_eq!(error.code(), "provider_error");
                        assert_eq!(error.execution_evidence(), ModelExecutionEvidence::Unknown);
                        if !broken_body {
                            assert!(error.to_string().contains("before protocol completion"));
                        }
                        assert!(
                            error.is_retryable(),
                            "{adapter:?}, tool={tool}, broken={broken_body}: {error}"
                        );
                    }
                }
            }
        }
    }
}

fn openai_empty_choice() -> &'static [u8] {
    concat!(
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",",
        "\"created\":1,\"model\":\"model-test\",\"choices\":[{\"index\":0,",
        "\"delta\":{\"role\":\"assistant\",\"content\":null},\"finish_reason\":null}]}\n\n",
    )
    .as_bytes()
}

fn openai_finish() -> &'static [u8] {
    concat!(
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",",
        "\"created\":1,\"model\":\"model-test\",\"choices\":[{\"index\":0,",
        "\"delta\":{},\"finish_reason\":\"stop\"}],",
        "\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
    )
    .as_bytes()
}

struct NullSink;

#[async_trait::async_trait]
impl DeltaSink for NullSink {
    async fn on_text(&self, _chunk: &str) {}
}

fn request() -> ChatRequest {
    ChatRequest {
        model_binding: ModelBinding::new(
            "p".to_string(),
            "claude-test".to_string(),
            "b".to_string(),
        ),
        inference: Default::default(),
        messages: vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::text("hi")],
        }],
        tools: Vec::new(),
    }
}

/*
 * Stream deadline cause/effect decision table. Causes: C1 the response stream
 * opens; C2 no terminal event arrives; C3 no event arrives within the idle
 * window; C4 no-op events do arrive below the idle window; C5 the fixed total
 * deadline expires. Effects: E1 return a retryable timeout naming an idle
 * stall; E2 return a retryable timeout naming the total deadline; E3 never
 * leave the inference future pending. Constraints: C3 and C4 are exclusive;
 * C2 is required for either timeout. Rules: S1 C1+C2+C3=>E1+E3;
 * S2 C1+C2+C4+C5=>E2+E3. In both rules physical evidence stays Unknown:
 * a retryable error class describes health, not permission to submit again.
 */
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_stream_times_out_as_a_retryable_timeout() {
    // Test design — Causes/effects/constraints and decision rule S1 are the
    // adjacent stream-deadline table: open+unfinished+idle=>retryable timeout,
    // with the outer guard proving the future cannot remain pending.
    let base_url = spawn_sse_server(StreamFixture::AnthropicStall).await;
    let executor =
        GenaiExecutor::from_materialized_endpoint(AdapterKind::Anthropic, base_url, "test-key")
            .expect("explicit client construction")
            .with_idle_timeout(Duration::from_millis(300));

    // Without the idle timeout the consumption loop would hang forever; the
    // 5s guard turns that hang into a test failure.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        executor.infer_streaming(request(), &NullSink),
    )
    .await
    .expect("the idle timeout fires instead of hanging");
    let err = result.expect_err("a stalled stream is an error, not a response");
    assert_eq!(err.code(), "timeout");
    assert!(err.is_retryable(), "health classification is retained");
    assert_eq!(err.execution_evidence(), ModelExecutionEvidence::Unknown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeating_unfinished_stream_obeys_the_total_call_deadline() {
    // Test design — Causes: an open unfinished stream emits no-op events inside
    // the idle window until the fixed deadline. Effects: it returns a retryable
    // total-timeout error. Constraints/invariants: heartbeats reset neither the
    // total call budget nor terminal requirement. Decision rule S2 from the
    // adjacent table: C1+C2+C4+C5=>E2+E3.
    let base_url = spawn_sse_server(StreamFixture::AnthropicHeartbeat).await;
    let executor =
        GenaiExecutor::from_materialized_endpoint(AdapterKind::Anthropic, base_url, "test-key")
            .expect("explicit client construction")
            .with_timeout(Duration::from_millis(300))
            .with_idle_timeout(Duration::from_secs(5));

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        executor.infer_streaming(request(), &NullSink),
    )
    .await
    .expect("the fixed call deadline fires instead of hanging");
    let err = result.expect_err("an unfinished stream is not a completed response");
    assert_eq!(err.code(), "timeout");
    assert!(err.to_string().contains("total timeout"), "got: {err}");
    assert_eq!(err.execution_evidence(), ModelExecutionEvidence::Unknown);
    assert!(
        err.is_retryable(),
        "a total stream timeout is worth retrying"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_heartbeats_do_not_count_as_model_progress() {
    // Test design — Causes: an open unfinished stream emits provider ping/no-op
    // events inside the transport interval but no text, reasoning, tool call,
    // or terminal event. Effects: the useful-progress idle deadline wins before
    // the larger total response budget and remains retryable. Constraint: raw
    // SSE activity cannot become a second definition of assistant progress.
    // Decision rule S1 variant: C1+C2+C3(raw activity only)=>E1+E3.
    let base_url = spawn_sse_server(StreamFixture::AnthropicHeartbeat).await;
    let executor =
        GenaiExecutor::from_materialized_endpoint(AdapterKind::Anthropic, base_url, "test-key")
            .expect("explicit client construction")
            .with_timeout(Duration::from_secs(5))
            .with_idle_timeout(Duration::from_millis(300));

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        executor.infer_streaming(request(), &NullSink),
    )
    .await
    .expect("raw heartbeats cannot keep inference pending");
    let error = result.expect_err("an unfinished heartbeat-only response fails");
    assert_eq!(error.code(), "timeout");
    assert!(error.is_retryable());
    assert!(error.to_string().contains("no useful event"), "{error}");
    assert_eq!(error.execution_evidence(), ModelExecutionEvidence::Unknown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_end_completes_without_waiting_for_transport_eof() {
    // OpenAI empty-choice cause/effect table. Causes: C1 one or more role/no-op
    // choice chunks precede output; C2 useful content later arrives; C3 a
    // protocol terminal arrives; C4 the TCP connection remains open. Effects:
    // E1 ignore no-op chunks without declaring failure; E2 commit exact useful
    // content promptly; E3 never wait for transport EOF. Rule O1=C1+C2+C3+C4
    // =>E1+E2+E3. Adjacent O2/O3 are covered below. Invariant: an empty chunk is
    // neither a completed empty response nor useful progress of its own.
    let base_url = spawn_sse_server(StreamFixture::OpenAiCompleteAfterEmptyChoices).await;
    let executor =
        GenaiExecutor::from_materialized_endpoint(AdapterKind::OpenAI, base_url, "test-key")
            .expect("explicit client construction")
            .with_idle_timeout(Duration::from_secs(5));

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        executor.infer_streaming(request(), &NullSink),
    )
    .await
    .expect("the protocol terminal event completes the turn")
    .expect("a complete stream is successful");
    assert_eq!(result.output.text_content(), "complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_empty_choice_heartbeats_do_not_defeat_the_idle_timeout() {
    // Rule O2: C1 repeated no-op choice chunks + no useful content + no terminal
    // => retryable idle timeout. Effects: E1 the inference future terminates;
    // E2 no empty success is returned. Constraint: lower-level socket activity
    // cannot reset the adapter's useful-event deadline.
    let base_url = spawn_sse_server(StreamFixture::OpenAiEmptyChoiceStall).await;
    let executor =
        GenaiExecutor::from_materialized_endpoint(AdapterKind::OpenAI, base_url, "test-key")
            .expect("explicit client construction")
            .with_timeout(Duration::from_secs(5))
            .with_idle_timeout(Duration::from_millis(300));

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        executor.infer_streaming(request(), &NullSink),
    )
    .await
    .expect("O2/E1 empty-choice traffic cannot keep inference pending");
    let error = result.expect_err("O2/E2 no-op choices are never a response");
    assert_eq!(error.code(), "timeout", "O2/E1");
    assert!(error.is_retryable(), "O2/E1");
    assert_eq!(error.execution_evidence(), ModelExecutionEvidence::Unknown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_empty_terminal_response_fails_closed() {
    // Rule O3: C1 empty choice + C3 terminal + no C2 useful content => retryable
    // provider error. Effects: E1 reject the protocol-complete empty turn; E2
    // preserve the canonical provider retry classification. Constraint: `[DONE]`
    // ends transport consumption but never manufactures assistant content.
    let base_url = spawn_sse_server(StreamFixture::OpenAiEmptyTerminal).await;
    let executor =
        GenaiExecutor::from_materialized_endpoint(AdapterKind::OpenAI, base_url, "test-key")
            .expect("explicit client construction")
            .with_idle_timeout(Duration::from_secs(2));

    let error = executor
        .infer_streaming(request(), &NullSink)
        .await
        .expect_err("O3/E1 a terminal empty turn fails closed");
    assert_eq!(error.code(), "provider_error", "O3/E2");
    assert!(error.is_retryable(), "O3/E2");
    // O3 also proves physical completion independently of response quality,
    // and the recorded wire fact names WHY: the streaming protocol's own end
    // frame arrived. The stall/timeout rows above stay Unknown precisely
    // because no such frame ever reached this adapter.
    assert_eq!(
        error.execution_evidence(),
        ModelExecutionEvidence::Terminated(TerminalReceipt::from_response_envelope(
            ResponseEnvelope::StreamEndFrame
        ))
    );
}
