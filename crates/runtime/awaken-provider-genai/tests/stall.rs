//! An incomplete provider stream must surface as a retryable `Timeout`, not hang
//! the run. The per-event idle bound owns silent stalls; the fixed total call
//! deadline owns streams that stay active forever without completing a response.

use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_provider_genai::{AdapterKind, GenaiExecutor};
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, DeltaSink, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy)]
enum StreamFixture {
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
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let fixture = fixture;
            tokio::spawn(async move {
                // Drain the request head; the response does not depend on it.
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
                let _ = socket.write_all(head.as_bytes()).await;
                match fixture {
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
        StreamFixture::AnthropicStall | StreamFixture::AnthropicHeartbeat => "/",
        StreamFixture::OpenAiEmptyChoiceStall
        | StreamFixture::OpenAiEmptyTerminal
        | StreamFixture::OpenAiCompleteAfterEmptyChoices => "/v1",
    };
    format!("http://{addr}{suffix}")
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
        model_binding: ModelBinding {
            provider_identity_ref: "p".to_string(),
            model_ref: "claude-test".to_string(),
            backend_ref: "b".to_string(),
        },
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
 * S2 C1+C2+C4+C5=>E2+E3.
 */
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_stream_times_out_as_a_retryable_timeout() {
    // Test design — Causes/effects/constraints and decision rule S1 are the
    // adjacent stream-deadline table: open+unfinished+idle=>retryable timeout,
    // with the outer guard proving the future cannot remain pending.
    let base_url = spawn_sse_server(StreamFixture::AnthropicStall).await;
    let executor =
        GenaiExecutor::from_materialized_endpoint(AdapterKind::Anthropic, base_url, "test-key")
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
    assert!(err.is_retryable(), "a stall is worth retrying");
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
    assert!(
        err.is_retryable(),
        "a total stream timeout is worth retrying"
    );
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
            .with_idle_timeout(Duration::from_secs(2));

    let error = executor
        .infer_streaming(request(), &NullSink)
        .await
        .expect_err("O3/E1 a terminal empty turn fails closed");
    assert_eq!(error.code(), "provider_error", "O3/E2");
    assert!(error.is_retryable(), "O3/E2");
}
