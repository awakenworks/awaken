//! HTTP run execution with SSE relay.

use bytes::Bytes;
use futures::StreamExt;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::transport::Transcoder;

use crate::event_relay::relay_events_stream;
use crate::http_sse::format_sse_data;
use crate::transport::replay_buffer::EventReplayBuffer;

/// RAII guard that decrements the SSE connections gauge on drop.
struct SseConnectionGuard;

impl Drop for SseConnectionGuard {
    fn drop(&mut self) {
        crate::metrics::dec_sse_connections();
    }
}

/// Spawn a background task that consumes agent events from a bounded receiver,
/// transcodes them via the protocol encoder, and sends SSE frames to the response.
///
/// Uses the shared [`relay_events_stream`] pipeline for the prologue->transcode->epilogue
/// logic and wraps each serialized item as an SSE `data:` frame.
///
/// When `replay_buffer` is provided, each SSE frame is assigned a sequential ID
/// and stored for client reconnection.
///
/// Returns the SSE byte receiver to feed into an HTTP response body.
#[tracing::instrument(skip_all)]
pub fn wire_sse_relay<E>(
    event_rx: mpsc::Receiver<AgentEvent>,
    encoder: E,
    buffer_size: usize,
    replay_buffer: Option<std::sync::Arc<EventReplayBuffer>>,
) -> mpsc::Receiver<Bytes>
where
    E: Transcoder<Input = AgentEvent> + 'static,
    E::Output: Serialize + Send + 'static,
{
    let (sse_tx, sse_rx) = mpsc::channel::<Bytes>(buffer_size);

    tokio::spawn(async move {
        crate::metrics::inc_sse_connections();
        let _sse_guard = SseConnectionGuard;

        let event_stream = ReceiverStream::new(event_rx);
        let mut stream = std::pin::pin!(relay_events_stream(encoder, event_stream));
        while let Some(json_bytes) = stream.next().await {
            let json = match String::from_utf8(json_bytes) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to decode relay output as UTF-8");
                    continue;
                }
            };
            let frame = if let Some(ref buf) = replay_buffer {
                let (_seq, frame) = buf.push_json(&json);
                frame
            } else {
                format_sse_data(&json)
            };
            if sse_tx.send(frame).await.is_err() {
                return;
            }
        }
    });

    sse_rx
}

/// Error-framed SSE data for relay errors.
pub fn format_relay_error(msg: &str) -> Bytes {
    let error = serde_json::json!({
        "type": "error",
        "message": msg,
        "code": "RELAY_ERROR",
    });
    let payload = serde_json::to_string(&error).unwrap_or_else(|_| {
        r#"{"type":"error","message":"relay error","code":"RELAY_ERROR"}"#.to_string()
    });
    format_sse_data(&payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::event::AgentEvent;
    use awaken_contract::contract::transport::Identity;

    #[tokio::test]
    async fn wire_sse_relay_transcodes_identity() {
        let (tx, rx) = mpsc::channel::<AgentEvent>(256);
        let encoder = Identity::<AgentEvent>::default();
        let mut sse_rx = wire_sse_relay(rx, encoder, 16, None);

        tx.try_send(AgentEvent::TextDelta {
            delta: "hello".into(),
        })
        .unwrap();
        drop(tx);

        let chunk = sse_rx.recv().await.unwrap();
        let chunk_str = String::from_utf8(chunk.to_vec()).unwrap();
        assert!(chunk_str.starts_with("data: "));
        assert!(chunk_str.contains("text_delta"));
        assert!(chunk_str.contains("hello"));
        assert!(chunk_str.ends_with("\n\n"));
    }

    #[tokio::test]
    async fn wire_sse_relay_completes_on_sender_drop() {
        let (tx, rx) = mpsc::channel::<AgentEvent>(256);
        let encoder = Identity::<AgentEvent>::default();
        let mut sse_rx = wire_sse_relay(rx, encoder, 16, None);

        drop(tx);

        // Should receive None when relay completes
        let result = sse_rx.recv().await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn wire_sse_relay_multiple_events() {
        let (tx, rx) = mpsc::channel::<AgentEvent>(256);
        let encoder = Identity::<AgentEvent>::default();
        let mut sse_rx = wire_sse_relay(rx, encoder, 16, None);

        tx.try_send(AgentEvent::TextDelta { delta: "a".into() })
            .unwrap();
        tx.try_send(AgentEvent::TextDelta { delta: "b".into() })
            .unwrap();
        tx.try_send(AgentEvent::StepEnd).unwrap();
        drop(tx);

        let mut chunks = Vec::new();
        while let Some(chunk) = sse_rx.recv().await {
            chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
        }
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn format_relay_error_is_valid_sse() {
        let err = format_relay_error("test error");
        let s = String::from_utf8(err.to_vec()).unwrap();
        assert!(s.starts_with("data: "));
        assert!(s.contains("RELAY_ERROR"));
        assert!(s.ends_with("\n\n"));
    }

    /// Custom transcoder that wraps events in a JSON envelope for testing.
    struct EnvelopeTranscoder {
        seq: u64,
    }

    impl EnvelopeTranscoder {
        fn new() -> Self {
            Self { seq: 0 }
        }
    }

    impl Transcoder for EnvelopeTranscoder {
        type Input = AgentEvent;
        type Output = serde_json::Value;

        fn prologue(&mut self) -> Vec<serde_json::Value> {
            vec![serde_json::json!({"type": "stream_start"})]
        }

        fn transcode(&mut self, item: &AgentEvent) -> Vec<serde_json::Value> {
            self.seq += 1;
            vec![serde_json::json!({
                "seq": self.seq,
                "event": serde_json::to_value(item).unwrap_or_default(),
            })]
        }

        fn epilogue(&mut self) -> Vec<serde_json::Value> {
            vec![serde_json::json!({"type": "stream_end"})]
        }
    }

    #[tokio::test]
    async fn wire_sse_relay_with_custom_transcoder() {
        let (tx, rx) = mpsc::channel::<AgentEvent>(256);
        let encoder = EnvelopeTranscoder::new();
        let mut sse_rx = wire_sse_relay(rx, encoder, 16, None);

        tx.try_send(AgentEvent::TextDelta {
            delta: "test".into(),
        })
        .unwrap();
        drop(tx);

        let mut chunks = Vec::new();
        while let Some(chunk) = sse_rx.recv().await {
            chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
        }

        // Should have: prologue + 1 event + epilogue = 3 chunks
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].contains("stream_start"));
        assert!(chunks[1].contains("\"seq\":1"));
        assert!(chunks[2].contains("stream_end"));
    }

    #[tokio::test]
    async fn resumable_relay_assigns_sequential_ids() {
        let (tx, rx) = mpsc::channel::<AgentEvent>(256);
        let encoder = Identity::<AgentEvent>::default();
        let replay_buffer = std::sync::Arc::new(EventReplayBuffer::new(64));
        let mut sse_rx = wire_sse_relay(rx, encoder, 16, Some(replay_buffer));

        tx.try_send(AgentEvent::TextDelta { delta: "a".into() })
            .unwrap();
        tx.try_send(AgentEvent::TextDelta { delta: "b".into() })
            .unwrap();
        tx.try_send(AgentEvent::StepEnd).unwrap();
        drop(tx);

        let mut chunks = Vec::new();
        while let Some(chunk) = sse_rx.recv().await {
            chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
        }
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].starts_with("id: 1\n"));
        assert!(chunks[1].starts_with("id: 2\n"));
        assert!(chunks[2].starts_with("id: 3\n"));
    }

    #[tokio::test]
    async fn resumable_relay_stores_in_buffer() {
        let (tx, rx) = mpsc::channel::<AgentEvent>(256);
        let encoder = Identity::<AgentEvent>::default();
        let replay_buffer = std::sync::Arc::new(EventReplayBuffer::new(64));
        let buf_ref = std::sync::Arc::clone(&replay_buffer);
        let mut sse_rx = wire_sse_relay(rx, encoder, 16, Some(replay_buffer));

        tx.try_send(AgentEvent::TextDelta { delta: "a".into() })
            .unwrap();
        tx.try_send(AgentEvent::TextDelta { delta: "b".into() })
            .unwrap();
        drop(tx);

        // Drain the receiver to let the relay task complete.
        while sse_rx.recv().await.is_some() {}

        assert_eq!(buf_ref.len(), 2);
        assert_eq!(buf_ref.current_seq(), 2);
    }

    #[tokio::test]
    async fn resumable_relay_completes_on_sender_drop() {
        let (tx, rx) = mpsc::channel::<AgentEvent>(256);
        let encoder = Identity::<AgentEvent>::default();
        let replay_buffer = std::sync::Arc::new(EventReplayBuffer::new(64));
        let mut sse_rx = wire_sse_relay(rx, encoder, 16, Some(replay_buffer));

        drop(tx);
        let result = sse_rx.recv().await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn wire_sse_relay_backpressure_with_small_buffer() {
        let (tx, rx) = mpsc::channel::<AgentEvent>(256);
        let encoder = Identity::<AgentEvent>::default();
        // buffer_size=1 means the SSE mpsc channel can hold at most 1 frame,
        // forcing the relay task to wait (backpressure) when the consumer is slow.
        let mut sse_rx = wire_sse_relay(rx, encoder, 1, None);

        let event_count = 20;
        for i in 0..event_count {
            tx.try_send(AgentEvent::TextDelta {
                delta: format!("msg-{i}"),
            })
            .unwrap();
        }
        drop(tx);

        let mut chunks = Vec::new();
        while let Some(chunk) = sse_rx.recv().await {
            chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
        }

        // All events must be delivered despite the tiny buffer.
        assert_eq!(chunks.len(), event_count);
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(
                chunk.contains(&format!("msg-{i}")),
                "chunk {i} missing expected payload"
            );
        }
    }

    #[tokio::test]
    async fn wire_sse_relay_without_replay_no_id_prefix() {
        let (tx, rx) = mpsc::channel::<AgentEvent>(256);
        let encoder = Identity::<AgentEvent>::default();
        // replay_buffer=None means frames should use format_sse_data (no id).
        let mut sse_rx = wire_sse_relay(rx, encoder, 16, None);

        tx.try_send(AgentEvent::TextDelta { delta: "x".into() })
            .unwrap();
        tx.try_send(AgentEvent::StepEnd).unwrap();
        drop(tx);

        let mut chunks = Vec::new();
        while let Some(chunk) = sse_rx.recv().await {
            chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
        }

        assert_eq!(chunks.len(), 2);
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(
                !chunk.contains("id:"),
                "chunk {i} should not contain an id: prefix without replay_buffer"
            );
            assert!(
                chunk.starts_with("data: "),
                "chunk {i} should start with data: prefix"
            );
            assert!(
                chunk.ends_with("\n\n"),
                "chunk {i} should end with double newline"
            );
        }
    }
}
