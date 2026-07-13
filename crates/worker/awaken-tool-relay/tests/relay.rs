//! ADR-0044 first vertical slice: prove the brain–hand split end to end over an
//! in-process duplex, plus the Indeterminate and idempotent-re-drive guarantees.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolExecutor, ToolOutput};
use awaken_tool_relay::wire::{HandErrorKind, HandReply, HandRequest, HandResult};
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
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the effect ran exactly once"
    );

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
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the effect ran at most once"
    );
    assert!(matches!(first.result, HandResult::Ok { .. }));
}

/// A tool whose own `invoke` returns an error — the hand reports it as an
/// execution `HandError`, and the brain maps it back to a `ToolError`.
struct FailingTool;

#[async_trait]
impl RawTool for FailingTool {
    fn id(&self) -> &str {
        "fail"
    }
    async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Execution("boom".to_string()))
    }
}

#[tokio::test]
async fn a_tool_execution_error_round_trips_as_a_tool_error() {
    let session = HandSession::new([Arc::new(FailingTool) as Arc<dyn RawTool>]);
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    let executor = RemoteToolExecutor::new(brain_end);
    let err = executor
        .invoke(&call("c1", "fail", "x"))
        .await
        .expect_err("execution error surfaces");
    assert!(err.to_string().contains("boom"), "got: {err}");
    drop(executor);
    let _ = hand.await;
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

#[tokio::test]
async fn a_write_that_fails_before_dispatch_is_a_definite_error_not_indeterminate() {
    // The hand's read half is already gone, so the very first send fails: the call
    // never left, so it is a definite Execution error, never Indeterminate.
    let (brain_end, hand_end) = tokio::io::duplex(64);
    drop(hand_end);
    let executor = RemoteToolExecutor::new(brain_end);
    match executor.call_hand(&call("c1", "echo", "x")).await {
        HandResult::Err { error } => {
            assert_eq!(error.kind, HandErrorKind::Execution);
            assert!(
                error.message.contains("closed before dispatch"),
                "got: {}",
                error.message
            );
        }
        other => panic!("expected a definite pre-dispatch error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_reply_with_a_mismatched_correlation_id_is_indeterminate() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut framed = Framed::new(hand_end, LengthDelimitedCodec::new());
        if let Some(Ok(frame)) = framed.next().await {
            let req: HandRequest = serde_json::from_slice(&frame).unwrap();
            // A well-formed reply, but for the wrong correlation id → the brain
            // cannot match it and must treat the call as indeterminate.
            let reply = HandReply {
                correlation_id: req.correlation_id.wrapping_add(999),
                result: HandResult::ok(ToolOutput::ok("c1", "stale")),
            };
            let _ = framed.send(serde_json::to_vec(&reply).unwrap().into()).await;
        }
    });
    let executor = RemoteToolExecutor::new(brain_end);
    assert_eq!(
        executor.call_hand(&call("c1", "echo", "x")).await,
        HandResult::Indeterminate
    );
}

#[tokio::test]
async fn a_reply_frame_that_is_not_a_hand_reply_is_indeterminate() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut framed = Framed::new(hand_end, LengthDelimitedCodec::new());
        if framed.next().await.is_some() {
            // Garbage that does not decode to a HandReply → indeterminate.
            let _ = framed.send(b"not-a-hand-reply".to_vec().into()).await;
        }
    });
    let executor = RemoteToolExecutor::new(brain_end);
    assert_eq!(
        executor.call_hand(&call("c1", "echo", "x")).await,
        HandResult::Indeterminate
    );
}

#[tokio::test]
async fn the_brain_stamps_its_catalog_fingerprint_and_a_matching_hand_accepts() {
    let session = HandSession::new([Arc::new(CountingEcho {
        id: "echo".into(),
        runs: Arc::new(AtomicU32::new(0)),
    }) as Arc<dyn RawTool>])
    .with_catalog_fingerprint("v1");
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    let executor = RemoteToolExecutor::new(brain_end).with_catalog_fingerprint("v1");
    let out = executor
        .invoke(&call("c1", "echo", "ok"))
        .await
        .expect("a matching fingerprint is accepted");
    assert_eq!(out.content, "ok");
    drop(executor);
    let _ = hand.await;
}

#[tokio::test]
async fn a_brain_fingerprint_drift_is_rejected_end_to_end() {
    let session = HandSession::new([Arc::new(CountingEcho {
        id: "echo".into(),
        runs: Arc::new(AtomicU32::new(0)),
    }) as Arc<dyn RawTool>])
    .with_catalog_fingerprint("hand-v1");
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    let executor = RemoteToolExecutor::new(brain_end).with_catalog_fingerprint("run-v2");
    let err = executor
        .invoke(&call("c1", "echo", "x"))
        .await
        .expect_err("a drifted fingerprint is rejected");
    assert!(
        err.to_string().contains("fingerprint mismatch"),
        "got: {err}"
    );
    drop(executor);
    let _ = hand.await;
}

#[tokio::test]
async fn serve_hand_fails_closed_on_a_frame_that_is_not_a_request() {
    use futures_util::SinkExt;
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    let session = HandSession::new(std::iter::empty::<Arc<dyn RawTool>>());
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    // A well-framed byte blob that does not decode to a HandRequest.
    let mut framed = Framed::new(brain_end, LengthDelimitedCodec::new());
    framed.send(b"garbage-frame".to_vec().into()).await.unwrap();

    match hand.await.unwrap() {
        Err(e) => assert!(
            e.to_string().contains("not a HandRequest"),
            "got: {e}"
        ),
        Ok(()) => panic!("expected the hand to fail closed on a non-request frame"),
    }
}
