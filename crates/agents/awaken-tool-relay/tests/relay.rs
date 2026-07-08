//! ADR-0044 first vertical slice: prove the brain–hand split end to end over an
//! in-process duplex, plus the Indeterminate and idempotent-re-drive guarantees.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolExecutor, ToolOutput};
use awaken_tool_relay::wire::{HandErrorKind, HandRequest, HandResult};
use awaken_tool_relay::{HandSession, RemoteToolExecutor, serve_hand};

/// A tool that echoes its `text` argument and counts how many times it ran, so a
/// test can prove an effect ran exactly once.
struct CountingEcho {
    id: String,
    runs: Arc<AtomicU32>,
}

#[async_trait]
impl RawTool for CountingEcho {
    fn id(&self) -> &str {
        &self.id
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let text = call
            .arguments
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(ToolOutput::ok(&call.call_id, text))
    }
}

fn call(call_id: &str, tool_id: &str, text: &str) -> ToolCall {
    ToolCall {
        call_id: call_id.into(),
        tool_id: tool_id.into(),
        arguments: serde_json::json!({ "text": text }),
    }
}

#[tokio::test]
async fn remote_tool_runs_out_of_process_and_returns_output() {
    let runs = Arc::new(AtomicU32::new(0));
    let tool = Arc::new(CountingEcho {
        id: "echo".into(),
        runs: runs.clone(),
    });
    let session = HandSession::new([tool as Arc<dyn RawTool>]);

    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    let executor = RemoteToolExecutor::new(brain_end);
    let output = executor
        .invoke(&call("c1", "echo", "hello from the hand"))
        .await
        .expect("remote invoke");

    assert_eq!(output.content, "hello from the hand");
    assert!(!output.is_error);
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the effect ran exactly once");

    drop(executor);
    let _ = hand.await;
}

#[tokio::test]
async fn unknown_tool_reads_identically_to_the_local_path() {
    let session = HandSession::new(std::iter::empty::<Arc<dyn RawTool>>());
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    let executor = RemoteToolExecutor::new(brain_end);
    let err = executor
        .invoke(&call("c1", "nope", "x"))
        .await
        .expect_err("unknown tool is an error");

    // Same display as the in-process `ToolError::Unknown`.
    assert_eq!(err.to_string(), "unknown tool: nope");
    drop(executor);
    let _ = hand.await;
}

#[tokio::test]
async fn dropped_channel_after_dispatch_is_indeterminate() {
    // A hand that reads the request then vanishes without replying: the brain
    // cannot know whether the effect ran, so the outcome is Indeterminate.
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        use futures_util::StreamExt;
        use tokio_util::codec::{Framed, LengthDelimitedCodec};
        let mut framed = Framed::new(hand_end, LengthDelimitedCodec::new());
        let _ = framed.next().await; // consume the request, then drop the channel
    });

    let executor = RemoteToolExecutor::new(brain_end);
    let result = executor.call_hand(&call("c1", "echo", "x")).await;
    assert_eq!(result, HandResult::Indeterminate);
}

#[tokio::test]
async fn re_drive_with_same_correlation_id_runs_the_effect_at_most_once() {
    // Idempotency ledger (ADR-0044 D4): the second identical request returns the
    // recorded result without re-running the tool.
    let runs = Arc::new(AtomicU32::new(0));
    let tool = Arc::new(CountingEcho {
        id: "echo".into(),
        runs: runs.clone(),
    });
    let mut session = HandSession::new([tool as Arc<dyn RawTool>]);

    let request = HandRequest::new(42, call("c1", "echo", "once"));
    let first = session.handle(request.clone()).await;
    let second = session.handle(request).await;

    assert_eq!(first, second, "a re-drive returns the recorded reply");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the effect ran at most once");
    assert!(matches!(first.result, HandResult::Ok { .. }));
}

#[tokio::test]
async fn catalog_fingerprint_mismatch_fails_closed() {
    let tool = Arc::new(CountingEcho {
        id: "echo".into(),
        runs: Arc::new(AtomicU32::new(0)),
    });
    let mut session =
        HandSession::new([tool as Arc<dyn RawTool>]).with_catalog_fingerprint("hand-v1");

    let mut request = HandRequest::new(1, call("c1", "echo", "x"));
    request.catalog_fingerprint = Some("run-v2".into());

    let reply = session.handle(request).await;
    match reply.result {
        HandResult::Err { error } => {
            assert_eq!(error.kind, HandErrorKind::FingerprintMismatch)
        }
        other => panic!("expected fingerprint mismatch, got {other:?}"),
    }
}
