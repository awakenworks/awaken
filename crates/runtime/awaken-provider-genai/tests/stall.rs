// @formal evidence llm.inference_budget_requires_positive_response_before_actual_watchdog
// @formal evidence model_operation.reentry_requires_positive_execution_evidence
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
mod budget;
mod support;
use awaken_runtime_contract::llm::{Error, ModelInferenceBudget};
use awaken_runtime_contract::resilience::{RetryPolicy, RetryStop, RetryVerdict};
use budget::{budget, with_budget};

/// The retry loop's final word on `error` as the failed first attempt of a
/// default-budget request (jitter at its lower bound).
fn verdict(error: &Error) -> RetryVerdict {
    RetryPolicy::default().decide(
        error.class(),
        error.execution_evidence(),
        0,
        ModelInferenceBudget::DEFAULT.total(),
        0,
    )
}

#[tokio::test]
async fn every_native_dialect_obeys_the_inherited_deadline_before_http_dispatch() {
    use awaken_provider_genai::OpenAiResponsesExecutor;
    use awaken_runtime_contract::llm::ModelAttemptTiming;
    use awaken_runtime_contract::model_operation::with_model_invocation_context;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    // Adapters own no timeout, so an incoherent provider/runtime pair is no
    // longer representable; the adapter only reads the inherited attempt
    // timing. B1 the request's logical deadline is already spent => Timeout,
    // positive NotDispatched, zero requests, and the retry loop stops on the
    // deadline. B2 time remains => the original complete HTTP error, terminal
    // receipt, exactly one request, and a permanent 400 is not retried. Cross
    // both with Anthropic/OpenAI streaming/nonstreaming and Responses.
    for (logical_ms, dispatched) in [(0, false), (500, true)] {
        for (kind, streaming) in [(0, false), (0, true), (1, false), (1, true), (2, false)] {
            let requests = Arc::new(AtomicUsize::new(0));
            let base = spawn_sse_server(StreamFixture::HttpFailure {
                status: 400,
                body: "{}",
                complete_body: true,
                requests: requests.clone(),
            })
            .await;
            let executor: Box<dyn LlmExecutor> = if kind == 2 {
                Box::new(
                    OpenAiResponsesExecutor::from_materialized_endpoint("openai", &base, "test")
                        .unwrap(),
                )
            } else {
                Box::new(
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
                )
            };
            let started = awaken_time::mono::now();
            let timing = ModelAttemptTiming::start_at(
                started,
                budget(Duration::from_millis(300), Duration::from_millis(300)),
                awaken_time::mono::MonoDeadline::at(started + Duration::from_millis(logical_ms)),
            )
            .unwrap();
            let result = with_model_invocation_context(None, timing, async {
                if streaming {
                    executor.infer_streaming(request(), &NullSink).await
                } else {
                    executor.infer(request()).await
                }
            })
            .await;
            let error =
                result.expect_err("fixture returns a complete error or pre-dispatch refusal");
            let at = format!("{logical_ms}/{kind}/{streaming}");
            assert_eq!(
                requests.load(Ordering::SeqCst),
                usize::from(dispatched),
                "B {at}"
            );
            if dispatched {
                assert_eq!(
                    error.execution_evidence(),
                    ModelExecutionEvidence::Terminated(TerminalReceipt::from_response_envelope(
                        ResponseEnvelope::CompleteErrorBody
                    )),
                    "B2 {at}"
                );
                assert_eq!(
                    verdict(&error),
                    RetryVerdict::Stop(RetryStop::NotRetryable),
                    "B2 {at}"
                );
            } else {
                assert_eq!(error.code(), "timeout", "B1 {at}");
                assert!(
                    matches!(
                        error.execution_evidence(),
                        ModelExecutionEvidence::NotDispatched(_)
                    ),
                    "B1 {at}"
                );
                assert_eq!(
                    RetryPolicy::default().decide(
                        error.class(),
                        error.execution_evidence(),
                        0,
                        Duration::ZERO,
                        0
                    ),
                    RetryVerdict::Stop(RetryStop::DeadlineExhausted),
                    "B1 {at}"
                );
            }
        }
    }
}

#[tokio::test]
async fn credential_materialization_cannot_restart_the_inherited_response_budget() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    // B5 asynchronous pre-send materialization outlives response but not the
    // watchdog => its exact invocation budget is spent, no HTTP request and
    // positive NotDispatched. Do not reset the clock when the adapter is built.
    let requests = Arc::new(AtomicUsize::new(0));
    let base = spawn_sse_server(StreamFixture::HttpFailure {
        status: 400,
        body: "{}",
        complete_body: true,
        requests: requests.clone(),
    })
    .await;
    let error = with_budget(
        budget(Duration::from_millis(10), Duration::from_millis(10)),
        async {
            // awaken-wait: pre-dispatch materialization intentionally consumes the
            // entire configured 10ms response budget; this deadline is the cause.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let executor =
                GenaiExecutor::from_materialized_endpoint(AdapterKind::OpenAI, base, "test")
                    .unwrap();
            executor
                .infer_streaming(request(), &NullSink)
                .await
                .unwrap_err()
        },
    )
    .await;
    assert_eq!(error.code(), "timeout", "B5 exact spent-budget error");
    assert!(
        matches!(
            error.execution_evidence(),
            ModelExecutionEvidence::NotDispatched(_)
        ),
        "B5"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "B5 no initiating request"
    );
}

#[tokio::test]
async fn healthy_text_reasoning_and_typed_tool_progress_finish_after_the_old_ten_minute_limit() {
    use awaken_runtime_contract::llm::ModelAttemptTiming;
    use awaken_runtime_contract::model_operation::with_model_invocation_context;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Progress(tokio::sync::mpsc::UnboundedSender<(&'static str, String)>);
    #[async_trait::async_trait]
    impl DeltaSink for Progress {
        async fn on_text(&self, text: &str) {
            let _ = self.0.send(("text", text.into()));
        }
        async fn on_reasoning(&self, text: &str) {
            let _ = self.0.send(("reasoning", text.into()));
        }
        async fn on_tool_call_delta(&self, _: &str, _: &str, args: &str) {
            let _ = self.0.send(("tool", args.into()));
        }
    }
    fn wire(delta: serde_json::Value, finish: serde_json::Value) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"id":"long-response","object":"chat.completion.chunk",
            "created":1,"model":"model-test","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
        )
    }
    // Causes: L1 real HTTP + the pinned SDK parser; L2 text, reasoning or typed
    // tool progress every 200s; L3 complete at 800s, beyond old 600s but before
    // response1200s/watchdog1230s. E1 every actual DeltaSink observation arrives;
    // E2 exact complete output/arguments and protocol-end receipt, one request.
    // Virtual time advances only between confirmed wire observations. Resume
    // while awaiting real socket I/O, so 2s failure bounds remain real and an
    // unrecognized event cannot strand a paused-clock test indefinitely.
    for kind in ["text", "reasoning", "tool"] {
        let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
        let requests = Arc::new(AtomicUsize::new(0));
        let base = spawn_sse_server(StreamFixture::Controlled {
            content_type: "text/event-stream",
            entered: None,
            commands: Arc::new(tokio::sync::Mutex::new(receiver)),
            requests: requests.clone(),
        })
        .await;
        let executor =
            GenaiExecutor::from_materialized_endpoint(AdapterKind::OpenAI, base, "test").unwrap();
        let (progress, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let mut request = request();
        if kind == "tool" {
            request.tools = vec![support::compatibility_tool()];
        }
        let timing =
            ModelAttemptTiming::standalone(awaken_time::mono::now(), ModelInferenceBudget::DEFAULT)
                .unwrap();
        let start = tokio::time::Instant::now();
        let mut inference = tokio::spawn(async move {
            let progress = Progress(progress);
            with_model_invocation_context(None, timing, async {
                tokio::time::timeout_at(
                    timing.watchdog_deadline().instant(),
                    executor.infer_streaming(request, &progress),
                )
                .await
            })
            .await
        });
        let mut expected = String::new();
        for index in 0..=4 {
            if index > 0 {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(200)).await;
                tokio::time::resume();
            }
            let chunk = "x".repeat(512);
            expected.push_str(&chunk);
            let delta = match kind {
                "text" => serde_json::json!({"content":chunk}),
                "reasoning" => serde_json::json!({"reasoning_content":chunk}),
                "tool" if index == 0 => {
                    serde_json::json!({"tool_calls":[{"index":0,"id":"long-tool","type":"function",
                    "function":{"name":"read_compatibility_fixture","arguments":format!("{{\"marker\":\"{chunk}")}}]})
                }
                "tool" => {
                    serde_json::json!({"tool_calls":[{"index":0,"function":{"arguments":chunk}}]})
                }
                _ => unreachable!(),
            };
            commands.send(wire(delta, serde_json::Value::Null)).unwrap();
            let (channel, _) = tokio::select! {
                frame = tokio::time::timeout(Duration::from_secs(2), observed.recv()) => frame.expect("L1 actual progress callback arrives").expect("L1 sink remains live"),
                result = &mut inference => panic!("L2 healthy {kind} ended at progress {index}: {result:?}"),
            };
            assert_eq!(channel, kind, "L1 exact useful-progress channel");
        }
        if kind == "tool" {
            commands
                .send(wire(
                    serde_json::json!({"tool_calls":[{"index":0,"function":{"arguments":"\"}"}}]}),
                    serde_json::Value::Null,
                ))
                .unwrap();
        } else if kind == "reasoning" {
            commands
                .send(wire(
                    serde_json::json!({"content":"done"}),
                    serde_json::Value::Null,
                ))
                .unwrap();
        }
        commands
            .send(
                wire(
                    serde_json::json!({}),
                    serde_json::json!(if kind == "tool" { "tool_calls" } else { "stop" }),
                ) + "data: [DONE]\n\n",
            )
            .unwrap();
        drop(commands);
        let response = tokio::time::timeout(Duration::from_secs(2), inference)
            .await
            .expect("L3 completes in its remaining response budget")
            .unwrap()
            .expect("L3 outer watchdog did not preempt")
            .expect("L3 exact protocol completion");
        assert_eq!(
            response.stop_reason,
            Some(if kind == "tool" {
                StopReason::ToolUse
            } else {
                StopReason::NaturalEnd
            }),
            "L3 exact protocol terminal, not an EOF assumption"
        );
        assert!(
            start.elapsed() >= Duration::from_secs(800)
                && start.elapsed() < Duration::from_secs(1_200),
            "L3 observed virtual duration"
        );
        match kind {
            "text" => assert_eq!(response.output.text_content(), expected),
            "reasoning" => {
                assert_eq!(response.output.text_content(), "done");
                assert!(
                    response.output.blocks.iter().any(
                        |b| matches!(b, ContentBlock::Thinking { text, .. } if text == &expected)
                    ),
                    "L3 exact captured reasoning"
                );
            }
            "tool" => {
                let calls = response.output.tool_calls();
                assert_eq!(calls.len(), 1);
                assert_eq!(
                    calls[0].arguments["marker"], expected,
                    "L3 exact typed JSON across the old budget"
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "L3 one initiating request"
        );
    }
}

#[derive(Clone)]
enum StreamFixture {
    Controlled {
        content_type: &'static str,
        entered: Option<std::sync::Arc<tokio::sync::Notify>>,
        commands: std::sync::Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<String>>>,
        requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    },
    HttpFailure {
        status: u16,
        body: &'static str,
        complete_body: bool,
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
    OpenAiTextBurst,
}

#[tokio::test]
async fn responses_complete_body_outlives_the_old_two_minute_cap_and_deadline_remains_unknown() {
    use awaken_provider_genai::OpenAiResponsesExecutor;
    use awaken_runtime_contract::llm::ModelAttemptTiming;
    use awaken_runtime_contract::model_operation::with_model_invocation_context;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    // R1 real JSON waits180s for its complete terminal body, inside the shared
    // 20min budget => exact success, one request. R2 the same dispatched body
    // never finishes under a custom100ms budget => Timeout/Unknown, one request.
    // HTTP headers and abandoning a future are never termination receipts.
    for complete in [true, false] {
        let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
        let requests = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let base = spawn_sse_server(StreamFixture::Controlled {
            content_type: "application/json",
            entered: Some(entered.clone()),
            commands: Arc::new(tokio::sync::Mutex::new(receiver)),
            requests: requests.clone(),
        })
        .await;
        let executor =
            OpenAiResponsesExecutor::from_materialized_endpoint("openai", &base, "test").unwrap();
        let timing = ModelAttemptTiming::standalone(
            awaken_time::mono::now(),
            if complete {
                ModelInferenceBudget::DEFAULT
            } else {
                budget(Duration::from_millis(100), Duration::from_millis(100))
            },
        )
        .unwrap();
        let mut inference = tokio::spawn(async move {
            with_model_invocation_context(None, timing, executor.infer(request())).await
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("exact native HTTP enrollment before moving time");
        if complete {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(180)).await;
            tokio::time::resume();
            assert!(
                !inference.is_finished(),
                "R1 old120s cap cannot preempt the response"
            );
            commands
                .send(
                    serde_json::json!({"status":"completed","output":[{"type":"message",
                "content":[{"type":"output_text","text":"complete after180s"}]}]})
                    .to_string(),
                )
                .unwrap();
            drop(commands);
            let response = tokio::time::timeout(Duration::from_secs(2), &mut inference)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(
                response.output.text_content(),
                "complete after180s",
                "R1 exact body"
            );
        } else {
            let error = tokio::time::timeout(Duration::from_secs(2), &mut inference)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(error.code(), "timeout", "R2");
            assert_eq!(
                error.execution_evidence(),
                ModelExecutionEvidence::Unknown,
                "R2"
            );
            drop(commands);
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "R1/R2 one initiating request"
        );
    }
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
                if let StreamFixture::Controlled {
                    commands,
                    requests,
                    content_type,
                    entered,
                } = &fixture
                {
                    requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    socket
                        .write_all(
                            format!("HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\n\r\n")
                                .as_bytes(),
                        )
                        .await
                        .unwrap();
                    if let Some(entered) = entered {
                        entered.notify_one();
                    }
                    let mut commands = commands.lock().await;
                    while let Some(chunk) = commands.recv().await {
                        if socket.write_all(chunk.as_bytes()).await.is_err() {
                            break;
                        }
                        socket.flush().await.unwrap();
                    }
                    return;
                }
                if let StreamFixture::HttpFailure {
                    status,
                    body,
                    complete_body,
                    requests,
                } = &fixture
                {
                    requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let declared_length = body.len() + if *complete_body { 0 } else { 100 };
                    let response = format!(
                        "HTTP/1.1 {status} Failure\r\nlocation: http://{addr}/repeated\r\nretry-after: 0\r\ncontent-length: {declared_length}\r\nconnection: close\r\n\r\n{body}"
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
                    StreamFixture::Controlled { .. } => unreachable!("handled at HTTP boundary"),
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
                    StreamFixture::OpenAiTextBurst => {
                        for _ in 0..40 {
                            let _ = socket.write_all(concat!(
                                "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",",
                                "\"created\":1,\"model\":\"model-test\",\"choices\":[{\"index\":0,",
                                "\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n",
                            ).as_bytes()).await;
                        }
                        let _ = socket.write_all(openai_finish()).await;
                        let _ = socket.write_all(b"data: [DONE]\n\n").await;
                        let _ = socket.flush().await;
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
        StreamFixture::Controlled { .. }
        | StreamFixture::OpenAiEmptyChoiceStall
        | StreamFixture::OpenAiEmptyTerminal
        | StreamFixture::OpenAiCompleteAfterEmptyChoices
        | StreamFixture::OpenAiTextBurst => "/v1",
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
    // streaming and nonstreaming + Responses. Every row makes exactly one HTTP
    // request. A fully consumed 4xx/5xx error body proves exact return; redirects
    // and the adjacent truncated-body matrix remain Unknown.
    for status in [307, 308, 429, 503] {
        for (kind, streaming) in [(0, false), (0, true), (1, false), (1, true), (2, false)] {
            let requests = Arc::new(AtomicUsize::new(0));
            let base = spawn_sse_server(StreamFixture::HttpFailure {
                status,
                body: "{}",
                complete_body: true,
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
            let expected = if status >= 400 {
                ModelExecutionEvidence::Terminated(TerminalReceipt::from_response_envelope(
                    ResponseEnvelope::CompleteErrorBody,
                ))
            } else {
                ModelExecutionEvidence::Unknown
            };
            assert_eq!(
                error.execution_evidence(),
                expected,
                "{status}/{kind}/{streaming}"
            );
        }
    }
}

#[tokio::test]
async fn truncated_http_error_bodies_never_mint_termination_evidence() {
    use awaken_provider_genai::OpenAiResponsesExecutor;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    // Causes: C1 final HTTP status is visible, C2 declared body is truncated,
    // C3 each production transport/dialect path. Effects: E1 exactly one wire
    // request, E2 failure remains Unknown. Rule R1=C1+C2+C3=>E1+E2.
    // Constraint: neither a status line nor socket close is a response-envelope
    // receipt; the complete-body cases are the adjacent positive matrix.
    for (kind, streaming) in [(0, false), (0, true), (1, false), (1, true), (2, false)] {
        let requests = Arc::new(AtomicUsize::new(0));
        let base = spawn_sse_server(StreamFixture::HttpFailure {
            status: 503,
            body: "{}",
            complete_body: false,
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
        let result = if streaming {
            executor.infer_streaming(request(), &NullSink).await
        } else {
            executor.infer(request()).await
        };
        let error = result.expect_err("truncated HTTP error cannot complete inference");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "R1/E1: {kind}/{streaming}"
        );
        assert_eq!(
            error.execution_evidence(),
            ModelExecutionEvidence::Unknown,
            "R1/E2: {kind}/{streaming}"
        );
    }
}

#[tokio::test]
async fn every_dialect_classifies_http_failures_by_status_before_body() {
    use awaken_provider_genai::OpenAiResponsesExecutor;
    use std::sync::{Arc, atomic::AtomicUsize};
    // Causes: C1 HTTP status, C2 provider-authored body (the fixture always
    // sends `retry-after: 0`), C3 each production dialect/transport path.
    // Effects: E1 the contract code, E2 the Retry-After hint, E3 retry
    // eligibility. R1 is the regression: a 500 whose body quotes 413 stays a
    // retryable provider failure on every path, never a context overflow.
    type Row = (
        &'static str,
        u16,
        &'static str,
        &'static str,
        Option<Duration>,
        bool,
    );
    let rows: &[Row] = &[
        (
            "R1",
            500,
            r#"{"error":{"message":"upstream replied 413 twice"}}"#,
            "provider_error",
            None,
            true,
        ),
        ("R2", 429, "{}", "rate_limited", Some(Duration::ZERO), true),
        ("R3", 503, "{}", "overloaded", Some(Duration::ZERO), true),
        ("R4", 413, "{}", "context_overflow", None, false),
    ];
    for (row, status, body, code, retry_after, retryable) in rows.iter().copied() {
        for (kind, streaming) in [(0, false), (0, true), (1, false), (1, true), (2, false)] {
            let base = spawn_sse_server(StreamFixture::HttpFailure {
                status,
                body,
                complete_body: true,
                requests: Arc::new(AtomicUsize::new(0)),
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
            let error = if streaming {
                executor.infer_streaming(request(), &NullSink).await
            } else {
                executor.infer(request()).await
            }
            .expect_err("HTTP failure cannot complete inference");
            let at = format!("{row}: {kind}/{streaming}");
            assert_eq!(error.code(), code, "{at} E1");
            assert_eq!(error.retry_after(), retry_after, "{at} E2");
            assert_eq!(
                matches!(verdict(&error), RetryVerdict::Retry { .. }),
                retryable,
                "{at} E3"
            );
        }
    }
}

#[tokio::test]
async fn a_refused_connection_proves_the_request_never_left_on_every_dialect() {
    use awaken_provider_genai::OpenAiResponsesExecutor;
    use awaken_runtime_contract::model_operation::{NoDispatchProof, PreDispatchStage};
    // Cause: nothing listens on the endpoint, so the connect phase fails.
    // Effects: E1 positive NotDispatched evidence naming the transport
    // connect stage (no request byte was written); E2 the retry loop may
    // therefore submit the request again. Every dialect and transport path.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    for (kind, streaming) in [(0, false), (0, true), (1, false), (1, true), (2, false)] {
        let executor: Box<dyn LlmExecutor> = match kind {
            2 => Box::new(
                OpenAiResponsesExecutor::from_materialized_endpoint(
                    "openai",
                    &format!("http://{address}/v1"),
                    "test",
                )
                .unwrap(),
            ),
            _ => Box::new(
                GenaiExecutor::from_materialized_endpoint(
                    if kind == 0 {
                        AdapterKind::Anthropic
                    } else {
                        AdapterKind::OpenAI
                    },
                    format!("http://{address}/v1"),
                    "test",
                )
                .unwrap(),
            ),
        };
        let error = with_budget(
            budget(Duration::from_secs(5), Duration::from_secs(5)),
            async {
                if streaming {
                    executor.infer_streaming(request(), &NullSink).await
                } else {
                    executor.infer(request()).await
                }
            },
        )
        .await
        .expect_err("nothing is listening");
        let at = format!("{kind}/{streaming}: {error}");
        assert_eq!(
            error.execution_evidence(),
            ModelExecutionEvidence::NotDispatched(NoDispatchProof::transport_connect()),
            "E1 {at}"
        );
        let ModelExecutionEvidence::NotDispatched(proof) = error.execution_evidence() else {
            unreachable!()
        };
        assert_eq!(proof.stage(), PreDispatchStage::TransportConnect, "E1 {at}");
        assert!(
            matches!(verdict(&error), RetryVerdict::Retry { .. }),
            "E2 {at}"
        );
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
                            .expect("boundary client");
                    let result = tokio::time::timeout(
                        Duration::from_secs(3),
                        with_budget(
                            budget(Duration::from_secs(3), Duration::from_secs(1)),
                            executor.infer_streaming(request(), &NullSink),
                        ),
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
                        // A truncated stream proves nothing about the remote
                        // stop, so the retry loop may not submit it again.
                        assert_eq!(
                            verdict(&error),
                            RetryVerdict::Stop(RetryStop::ExecutionUncertain),
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

#[derive(Default)]
struct ObservedSink {
    text: std::sync::Mutex<Vec<String>>,
    arrived: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl DeltaSink for ObservedSink {
    async fn on_text(&self, chunk: &str) {
        self.text.lock().unwrap().push(chunk.to_owned());
        self.arrived.notify_one();
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn burst_text_uses_fewer_live_calls_and_keeps_the_committed_answer() {
    // Causes: C1 forty adjacent provider text fragments arrive before the
    // terminal frame; C2 the SDK captures the complete response independently.
    // Effects: E1 far fewer live sink calls carry the same concatenated bytes;
    // E2 committed content and stop reason remain exact. Rule L1=C1+C2=>E1+E2.
    let base_url = spawn_sse_server(StreamFixture::OpenAiTextBurst).await;
    let executor =
        GenaiExecutor::from_materialized_endpoint(AdapterKind::OpenAI, base_url, "test-key")
            .unwrap();
    let sink = ObservedSink::default();
    let response = executor.infer_streaming(request(), &sink).await.unwrap();
    let chunks = sink.text.lock().unwrap().clone();
    assert_eq!(chunks.concat(), "x".repeat(40), "L1/E1 exact live bytes");
    assert!(chunks.len() < 10, "L1/E1 bounded remote round trips");
    assert_eq!(response.output.text_content(), "x".repeat(40), "L1/E2");
    assert_eq!(response.stop_reason, Some(StopReason::NaturalEnd), "L1/E2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_provider_flushes_live_text_before_the_idle_error() {
    // Causes: C1 one partial text delta arrives; C2 the provider then stops
    // sending events without a terminal frame. Effects: E1 a short timer emits
    // the visible prefix before the longer idle deadline; E2 the existing
    // retryable timeout still owns the terminal result. Rule L2=C1+C2=>E1+E2.
    let base_url = spawn_sse_server(StreamFixture::AnthropicStall).await;
    let executor =
        GenaiExecutor::from_materialized_endpoint(AdapterKind::Anthropic, base_url, "test-key")
            .unwrap();
    let sink = std::sync::Arc::new(ObservedSink::default());
    let observed = sink.clone();
    let run = tokio::spawn(async move {
        with_budget(
            budget(Duration::from_secs(10), Duration::from_secs(2)),
            executor.infer_streaming(request(), observed.as_ref()),
        )
        .await
    });
    tokio::time::timeout(Duration::from_millis(700), sink.arrived.notified())
        .await
        .expect("L2/E1 visible prefix arrives before the idle error");
    assert_eq!(sink.text.lock().unwrap().concat(), "hel", "L2/E1");
    let error = run.await.unwrap().unwrap_err();
    assert_eq!(error.code(), "timeout", "L2/E2");
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
            .expect("explicit client construction");

    // Without the idle timeout the consumption loop would hang forever; the
    // 5s guard turns that hang into a test failure.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        with_budget(
            budget(Duration::from_secs(5), Duration::from_millis(300)),
            executor.infer_streaming(request(), &NullSink),
        ),
    )
    .await
    .expect("the idle timeout fires instead of hanging");
    let err = result.expect_err("a stalled stream is an error, not a response");
    assert_eq!(err.code(), "timeout");
    assert_eq!(err.execution_evidence(), ModelExecutionEvidence::Unknown);
    assert_eq!(
        verdict(&err),
        RetryVerdict::Stop(RetryStop::ExecutionUncertain),
        "an unconfirmed stream is never submitted again"
    );
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
            .expect("explicit client construction");

    // The silence window may not exceed the response window, so both are
    // 300ms: the response deadline was fixed when the attempt started, before
    // the stream opened, and therefore fires first.
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        with_budget(
            budget(Duration::from_millis(300), Duration::from_millis(300)),
            executor.infer_streaming(request(), &NullSink),
        ),
    )
    .await
    .expect("the fixed call deadline fires instead of hanging");
    let err = result.expect_err("an unfinished stream is not a completed response");
    assert_eq!(err.code(), "timeout");
    assert!(err.to_string().contains("total timeout"), "got: {err}");
    assert_eq!(err.execution_evidence(), ModelExecutionEvidence::Unknown);
    assert_eq!(
        verdict(&err),
        RetryVerdict::Stop(RetryStop::ExecutionUncertain),
        "a dispatched stream that timed out is never submitted again"
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
            .expect("explicit client construction");

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        with_budget(
            budget(Duration::from_secs(5), Duration::from_millis(300)),
            executor.infer_streaming(request(), &NullSink),
        ),
    )
    .await
    .expect("raw heartbeats cannot keep inference pending");
    let error = result.expect_err("an unfinished heartbeat-only response fails");
    assert_eq!(error.code(), "timeout");
    assert!(error.to_string().contains("no useful event"), "{error}");
    assert_eq!(
        verdict(&error),
        RetryVerdict::Stop(RetryStop::ExecutionUncertain)
    );
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
            .expect("explicit client construction");

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        with_budget(
            budget(Duration::from_secs(5), Duration::from_secs(5)),
            executor.infer_streaming(request(), &NullSink),
        ),
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
            .expect("explicit client construction");

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        with_budget(
            budget(Duration::from_secs(5), Duration::from_millis(300)),
            executor.infer_streaming(request(), &NullSink),
        ),
    )
    .await
    .expect("O2/E1 empty-choice traffic cannot keep inference pending");
    let error = result.expect_err("O2/E2 no-op choices are never a response");
    assert_eq!(error.code(), "timeout", "O2/E1");
    assert_eq!(
        verdict(&error),
        RetryVerdict::Stop(RetryStop::ExecutionUncertain),
        "O2/E1"
    );
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
            .expect("explicit client construction");

    let error = with_budget(
        budget(Duration::from_secs(5), Duration::from_secs(2)),
        executor.infer_streaming(request(), &NullSink),
    )
    .await
    .expect_err("O3/E1 a terminal empty turn fails closed");
    assert_eq!(error.code(), "provider_error", "O3/E2");
    assert!(
        matches!(verdict(&error), RetryVerdict::Retry { .. }),
        "O3/E2 a terminated empty turn is retried"
    );
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
