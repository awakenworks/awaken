//! A provider stream that goes silent mid-turn must surface as a retryable
//! `Timeout`, not hang the run: the overall call timeout only guards opening
//! the stream, so a per-event idle timeout has to cover the consumption loop.

use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::{ChatMessage, ChatRequest, ChatRole, DeltaSink, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// An Anthropic-shaped SSE endpoint that streams the start of a turn, then
/// stalls forever with the connection open.
async fn spawn_stalling_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                // Drain the request head; the response does not depend on it.
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
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
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(events.as_bytes()).await;
                let _ = socket.flush().await;
                // Stall: no further events, no close.
                std::future::pending::<()>().await;
            });
        }
    });
    format!("http://{addr}/")
}

struct NullSink;

#[async_trait::async_trait]
impl DeltaSink for NullSink {
    async fn on_text(&self, _chunk: &str) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_stream_times_out_as_a_retryable_timeout() {
    let base_url = spawn_stalling_server().await;
    let executor = GenaiExecutor::anthropic_compatible(base_url, "test-key")
        .with_idle_timeout(Duration::from_millis(300));

    let request = ChatRequest {
        model_binding: ModelBinding {
            provider_instance_ref: "p".to_string(),
            model_ref: "claude-test".to_string(),
            backend_ref: "b".to_string(),
        },
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ContentBlock::text("hi")],
        }],
        tools: Vec::new(),
    };

    // Without the idle timeout the consumption loop would hang forever; the
    // 5s guard turns that hang into a test failure.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        executor.infer_streaming(request, &NullSink),
    )
    .await
    .expect("the idle timeout fires instead of hanging");
    let err = result.expect_err("a stalled stream is an error, not a turn");
    assert_eq!(err.code(), "timeout");
    assert!(err.is_retryable(), "a stall is worth retrying");
}
