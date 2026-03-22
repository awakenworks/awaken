//! Transport layer tests — migrated from tirea-agentos-server/tests/transport.rs.
//!
//! Validates SSE relay, transcoder integration, channel sink behavior,
//! and protocol-specific SSE encoding.

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::transport::{Identity, Transcoder};
use awaken_server::http_run::{format_relay_error, wire_sse_relay};
use awaken_server::http_sse::{format_sse_data, sse_response};
use awaken_server::protocols::acp::encoder::AcpEncoder;
use awaken_server::protocols::ag_ui::encoder::AgUiEncoder;
use awaken_server::protocols::ai_sdk_v6::encoder::AiSdkEncoder;
use awaken_server::transport::transcoder::{
    encode_epilogue_to_sse, encode_event_to_sse, encode_prologue_to_sse,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::json;
use std::convert::Infallible;
use tokio::sync::mpsc;

// ============================================================================
// SSE data formatting
// ============================================================================

#[test]
fn format_sse_data_produces_correct_format() {
    let result = format_sse_data(r#"{"type":"test"}"#);
    assert_eq!(result, Bytes::from("data: {\"type\":\"test\"}\n\n"));
}

#[test]
fn format_sse_data_with_complex_json() {
    let data = json!({"events": [1, 2, 3], "nested": {"key": "val"}});
    let result = format_sse_data(&serde_json::to_string(&data).unwrap());
    let s = String::from_utf8(result.to_vec()).unwrap();
    assert!(s.starts_with("data: "));
    assert!(s.ends_with("\n\n"));
    assert!(s.contains("events"));
}

// ============================================================================
// SSE response headers
// ============================================================================

#[test]
fn sse_response_has_correct_headers() {
    let stream = futures::stream::empty::<Result<Bytes, Infallible>>();
    let response = sse_response(stream);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-cache");
    assert_eq!(response.headers().get("connection").unwrap(), "keep-alive");
}

// ============================================================================
// SSE body stream
// ============================================================================

#[tokio::test]
async fn sse_body_stream_yields_all_chunks() {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
    let stream = awaken_server::http_sse::sse_body_stream(rx);
    tokio::pin!(stream);

    tx.send(Bytes::from("a")).await.unwrap();
    tx.send(Bytes::from("b")).await.unwrap();
    drop(tx);

    let items: Vec<Bytes> = stream.map(|r| r.unwrap()).collect().await;
    assert_eq!(items, vec![Bytes::from("a"), Bytes::from("b")]);
}

#[tokio::test]
async fn sse_body_stream_empty_on_immediate_close() {
    let (_tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
    let stream = awaken_server::http_sse::sse_body_stream(rx);
    tokio::pin!(stream);
    drop(_tx);

    let items: Vec<Bytes> = stream.map(|r| r.unwrap()).collect().await;
    assert!(items.is_empty());
}

// ============================================================================
// Wire SSE relay with identity encoder
// ============================================================================

#[tokio::test]
async fn wire_sse_relay_transcodes_identity() {
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let encoder = Identity::<AgentEvent>::default();
    let mut sse_rx = wire_sse_relay(rx, encoder, 16);

    tx.send(AgentEvent::TextDelta {
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
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let encoder = Identity::<AgentEvent>::default();
    let mut sse_rx = wire_sse_relay(rx, encoder, 16);
    drop(tx);
    assert!(sse_rx.recv().await.is_none());
}

#[tokio::test]
async fn wire_sse_relay_multiple_events() {
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let encoder = Identity::<AgentEvent>::default();
    let mut sse_rx = wire_sse_relay(rx, encoder, 16);

    tx.send(AgentEvent::TextDelta { delta: "a".into() })
        .unwrap();
    tx.send(AgentEvent::TextDelta { delta: "b".into() })
        .unwrap();
    tx.send(AgentEvent::StepEnd).unwrap();
    drop(tx);

    let mut chunks = Vec::new();
    while let Some(chunk) = sse_rx.recv().await {
        chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
    }
    assert_eq!(chunks.len(), 3);
}

// ============================================================================
// Wire SSE relay with custom transcoder (prologue/epilogue)
// ============================================================================

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
        vec![json!({"type": "stream_start"})]
    }

    fn transcode(&mut self, item: &AgentEvent) -> Vec<serde_json::Value> {
        self.seq += 1;
        vec![json!({
            "seq": self.seq,
            "event": serde_json::to_value(item).unwrap_or_default(),
        })]
    }

    fn epilogue(&mut self) -> Vec<serde_json::Value> {
        vec![json!({"type": "stream_end"})]
    }
}

#[tokio::test]
async fn wire_sse_relay_with_custom_transcoder() {
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let encoder = EnvelopeTranscoder::new();
    let mut sse_rx = wire_sse_relay(rx, encoder, 16);

    tx.send(AgentEvent::TextDelta {
        delta: "test".into(),
    })
    .unwrap();
    drop(tx);

    let mut chunks = Vec::new();
    while let Some(chunk) = sse_rx.recv().await {
        chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
    }

    assert_eq!(chunks.len(), 3);
    assert!(chunks[0].contains("stream_start"));
    assert!(chunks[1].contains("\"seq\":1"));
    assert!(chunks[2].contains("stream_end"));
}

#[tokio::test]
async fn wire_sse_relay_empty_stream_emits_prologue_and_epilogue() {
    let (_tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let encoder = EnvelopeTranscoder::new();
    let mut sse_rx = wire_sse_relay(rx, encoder, 16);
    drop(_tx);

    let mut chunks = Vec::new();
    while let Some(chunk) = sse_rx.recv().await {
        chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
    }

    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].contains("stream_start"));
    assert!(chunks[1].contains("stream_end"));
}

// ============================================================================
// Minimal transcoder (no prologue/epilogue)
// ============================================================================

struct MinimalEncoder;

impl Transcoder for MinimalEncoder {
    type Input = AgentEvent;
    type Output = String;

    fn transcode(&mut self, _item: &AgentEvent) -> Vec<String> {
        vec!["evt".to_string()]
    }
}

#[tokio::test]
async fn wire_sse_relay_minimal_encoder() {
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let encoder = MinimalEncoder;
    let mut sse_rx = wire_sse_relay(rx, encoder, 16);

    tx.send(AgentEvent::TextDelta { delta: "a".into() })
        .unwrap();
    tx.send(AgentEvent::TextDelta { delta: "b".into() })
        .unwrap();
    drop(tx);

    let mut chunks = Vec::new();
    while let Some(chunk) = sse_rx.recv().await {
        chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
    }
    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].contains("evt"));
}

// ============================================================================
// Fanout transcoder (multiple events per input)
// ============================================================================

struct FanoutEncoder;

impl Transcoder for FanoutEncoder {
    type Input = AgentEvent;
    type Output = String;

    fn prologue(&mut self) -> Vec<String> {
        vec!["start".to_string()]
    }

    fn transcode(&mut self, item: &AgentEvent) -> Vec<String> {
        match item {
            AgentEvent::TextDelta { delta } => {
                vec![format!("text:{delta}"), format!("echo:{delta}")]
            }
            _ => vec!["other".to_string()],
        }
    }

    fn epilogue(&mut self) -> Vec<String> {
        vec!["end".to_string()]
    }
}

#[tokio::test]
async fn wire_sse_relay_fanout_encoder() {
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let encoder = FanoutEncoder;
    let mut sse_rx = wire_sse_relay(rx, encoder, 16);

    tx.send(AgentEvent::TextDelta { delta: "hi".into() })
        .unwrap();
    tx.send(AgentEvent::StepEnd).unwrap();
    drop(tx);

    let mut chunks = Vec::new();
    while let Some(chunk) = sse_rx.recv().await {
        chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
    }

    // prologue(1) + text(2) + step(1) + epilogue(1) = 5
    assert_eq!(chunks.len(), 5);
    assert!(chunks[0].contains("start"));
    assert!(chunks[1].contains("text:hi"));
    assert!(chunks[2].contains("echo:hi"));
    assert!(chunks[3].contains("other"));
    assert!(chunks[4].contains("end"));
}

// ============================================================================
// Format relay error
// ============================================================================

#[test]
fn format_relay_error_is_valid_sse() {
    let err = format_relay_error("test error");
    let s = String::from_utf8(err.to_vec()).unwrap();
    assert!(s.starts_with("data: "));
    assert!(s.contains("RELAY_ERROR"));
    assert!(s.contains("test error"));
    assert!(s.ends_with("\n\n"));
}

// ============================================================================
// Protocol-specific SSE encoding
// ============================================================================

#[test]
fn encode_identity_event_to_sse() {
    let mut encoder = Identity::<AgentEvent>::default();
    let event = AgentEvent::TextDelta { delta: "hi".into() };
    let frames = encode_event_to_sse(&mut encoder, &event);
    assert_eq!(frames.len(), 1);
    let frame = String::from_utf8(frames[0].to_vec()).unwrap();
    assert!(frame.starts_with("data: "));
    assert!(frame.contains("text_delta"));
    assert!(frame.ends_with("\n\n"));
}

#[test]
fn encode_prologue_empty_for_identity() {
    let mut encoder = Identity::<AgentEvent>::default();
    assert!(encode_prologue_to_sse(&mut encoder).is_empty());
}

#[test]
fn encode_epilogue_empty_for_identity() {
    let mut encoder = Identity::<AgentEvent>::default();
    assert!(encode_epilogue_to_sse(&mut encoder).is_empty());
}

#[test]
fn encode_ai_sdk_event_to_sse() {
    let mut encoder = AiSdkEncoder::new();
    let event = AgentEvent::TextDelta {
        delta: "hello".into(),
    };
    let frames = encode_event_to_sse(&mut encoder, &event);
    assert!(!frames.is_empty());
    let frame = String::from_utf8(frames[0].to_vec()).unwrap();
    assert!(frame.starts_with("data: "));
}

#[test]
fn encode_ag_ui_event_to_sse() {
    let mut encoder = AgUiEncoder::new();
    encoder.on_agent_event(&AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    });
    let event = AgentEvent::TextDelta {
        delta: "hello".into(),
    };
    let frames = encode_event_to_sse(&mut encoder, &event);
    assert!(!frames.is_empty());
}

#[test]
fn encode_acp_event_to_sse() {
    let mut encoder = AcpEncoder::new();
    let event = AgentEvent::TextDelta {
        delta: "hello".into(),
    };
    let frames = encode_event_to_sse(&mut encoder, &event);
    assert_eq!(frames.len(), 1);
}

// ============================================================================
// Wire relay with AG-UI encoder
// ============================================================================

#[tokio::test]
async fn wire_sse_relay_with_ag_ui_encoder() {
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let encoder = AgUiEncoder::new();
    let mut sse_rx = wire_sse_relay(rx, encoder, 16);

    tx.send(AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    })
    .unwrap();
    tx.send(AgentEvent::TextDelta {
        delta: "hello".into(),
    })
    .unwrap();
    tx.send(AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    })
    .unwrap();
    drop(tx);

    let mut chunks = Vec::new();
    while let Some(chunk) = sse_rx.recv().await {
        chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
    }

    // RunStarted(1) + TextMessageStart(1) + TextMessageContent(1) + TextMessageEnd(1) + RunFinished(1)
    assert!(
        chunks.len() >= 4,
        "expected at least 4 chunks, got {}",
        chunks.len()
    );
    assert!(chunks[0].contains("RUN_STARTED"));
    assert!(chunks.last().unwrap().contains("RUN_FINISHED"));
}

// ============================================================================
// Wire relay with AI SDK encoder
// ============================================================================

#[tokio::test]
async fn wire_sse_relay_with_ai_sdk_encoder() {
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let encoder = AiSdkEncoder::new();
    let mut sse_rx = wire_sse_relay(rx, encoder, 16);

    tx.send(AgentEvent::RunStart {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        parent_run_id: None,
    })
    .unwrap();
    tx.send(AgentEvent::TextDelta {
        delta: "hello".into(),
    })
    .unwrap();
    tx.send(AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    })
    .unwrap();
    drop(tx);

    let mut chunks = Vec::new();
    while let Some(chunk) = sse_rx.recv().await {
        chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
    }

    // MessageStart + RunInfo + TextStart + TextDelta + TextEnd + Finish
    assert!(
        chunks.len() >= 5,
        "expected at least 5 chunks, got {}",
        chunks.len()
    );
    assert!(chunks[0].contains("start")); // MessageStart
}

// ============================================================================
// Wire relay with ACP encoder
// ============================================================================

#[tokio::test]
async fn wire_sse_relay_with_acp_encoder() {
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let encoder = AcpEncoder::new();
    let mut sse_rx = wire_sse_relay(rx, encoder, 16);

    tx.send(AgentEvent::TextDelta {
        delta: "hello".into(),
    })
    .unwrap();
    tx.send(AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        result: None,
        termination: TerminationReason::NaturalEnd,
    })
    .unwrap();
    drop(tx);

    let mut chunks = Vec::new();
    while let Some(chunk) = sse_rx.recv().await {
        chunks.push(String::from_utf8(chunk.to_vec()).unwrap());
    }

    // agent_message(1) + finished(1) = 2
    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].contains("session/update"));
    assert!(chunks[1].contains("session/update"));
}

// ============================================================================
// Channel sink behavior
// ============================================================================

#[tokio::test]
async fn channel_sink_forwards_events() {
    use awaken_contract::contract::event_sink::EventSink;
    use awaken_server::transport::channel_sink::ChannelEventSink;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = ChannelEventSink::new(tx);

    sink.emit(AgentEvent::TextDelta {
        delta: "hello".into(),
    })
    .await;
    sink.emit(AgentEvent::StepEnd).await;

    let e1 = rx.recv().await.unwrap();
    assert!(matches!(e1, AgentEvent::TextDelta { delta } if delta == "hello"));
    let e2 = rx.recv().await.unwrap();
    assert!(matches!(e2, AgentEvent::StepEnd));
}

#[tokio::test]
async fn channel_sink_drops_silently_on_closed_receiver() {
    use awaken_contract::contract::event_sink::EventSink;
    use awaken_server::transport::channel_sink::ChannelEventSink;

    let (tx, rx) = mpsc::unbounded_channel();
    let sink = ChannelEventSink::new(tx);
    drop(rx);

    // Should not panic
    sink.emit(AgentEvent::TextDelta {
        delta: "ignored".into(),
    })
    .await;
}

#[tokio::test]
async fn channel_sink_close_is_noop() {
    use awaken_contract::contract::event_sink::EventSink;
    use awaken_server::transport::channel_sink::ChannelEventSink;

    let (tx, _rx) = mpsc::unbounded_channel();
    let sink = ChannelEventSink::new(tx);
    sink.close().await;
}
