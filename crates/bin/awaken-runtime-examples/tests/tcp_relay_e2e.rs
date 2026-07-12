//! Brain–hand tool relay over a REAL TCP socket (ADR-0044/0045): the middle layer
//! between `awaken-tool-relay`'s in-process `duplex` unit tests and the full k3d
//! `topology_e2e.sh`. It proves the executor-channel guarantees hold over an actual
//! loopback network socket (not just an in-memory pipe), and — the security half —
//! that the hand fails CLOSED on an unknown tool, on a catalog-fingerprint mismatch,
//! and reports Indeterminate when the connection drops mid-dispatch, over that same
//! real transport. No effect ever runs on a rejected path.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_connection_plan::{ChannelFactory, ConnectionPlan, TokioChannelFactory, bind_tcp};
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolExecutor, ToolOutput};
use awaken_tool_relay::wire::HandResult;
use awaken_tool_relay::{HandSession, RemoteToolExecutor, serve_hand};

/// Echoes its `text` and counts runs, so a test can prove an effect ran (or did not).
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
            .unwrap_or("");
        Ok(ToolOutput::ok(&call.call_id, text.to_string()))
    }
}

fn call(call_id: &str, tool_id: &str, text: &str) -> ToolCall {
    ToolCall {
        call_id: call_id.into(),
        tool_id: tool_id.into(),
        arguments: serde_json::json!({ "text": text }),
    }
}

/// Bind a loopback TCP hand on an ephemeral port; return its dial address and the
/// bound listener so the caller can accept + serve on a task.
async fn bind_loopback_hand() -> (String, awaken_connection_plan::TcpHandListener) {
    let listener = bind_tcp(&ConnectionPlan::tcp_listen("127.0.0.1:0"))
        .await
        .expect("bind tcp hand");
    let addr = listener.local_addr().expect("local addr").to_string();
    (addr, listener)
}

async fn dial_brain(addr: &str) -> Box<dyn AgentChannel> {
    TokioChannelFactory
        .connect(&ConnectionPlan::tcp_dial(addr))
        .await
        .expect("brain dials hand over tcp")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_tool_runs_over_real_tcp() {
    let runs = Arc::new(AtomicU32::new(0));
    let tool = Arc::new(CountingEcho {
        id: "echo".into(),
        runs: runs.clone(),
    });
    let (addr, listener) = bind_loopback_hand().await;

    let hand = tokio::spawn(async move {
        let channel = listener.accept().await.expect("accept");
        let session = HandSession::new([tool as Arc<dyn RawTool>]);
        let _ = serve_hand(channel, session).await;
    });

    let executor = RemoteToolExecutor::new(dial_brain(&addr).await);
    let output = executor
        .invoke(&call("c1", "echo", "over the wire"))
        .await
        .expect("remote invoke over tcp");

    assert_eq!(output.content, "over the wire");
    assert!(!output.is_error);
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the effect ran exactly once, on the hand"
    );
    hand.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_tool_over_tcp_fails_closed() {
    // A hand serving an empty registry: any tool is unknown → error, nothing runs.
    let (addr, listener) = bind_loopback_hand().await;
    let hand = tokio::spawn(async move {
        let channel = listener.accept().await.expect("accept");
        let session = HandSession::new(std::iter::empty::<Arc<dyn RawTool>>());
        let _ = serve_hand(channel, session).await;
    });

    let executor = RemoteToolExecutor::new(dial_brain(&addr).await);
    let err = executor
        .invoke(&call("c1", "nope", "x"))
        .await
        .expect_err("unknown tool must be a closed error over tcp");
    assert_eq!(err.to_string(), "unknown tool: nope");
    hand.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_fingerprint_mismatch_over_tcp_fails_closed() {
    // The hand pins catalog "hand-v1"; the brain stamps a run whose catalog is
    // "run-v2". The hand must reject BEFORE executing — no effect on a stale catalog.
    let runs = Arc::new(AtomicU32::new(0));
    let tool = Arc::new(CountingEcho {
        id: "echo".into(),
        runs: runs.clone(),
    });
    let (addr, listener) = bind_loopback_hand().await;
    let hand = tokio::spawn(async move {
        let channel = listener.accept().await.expect("accept");
        let session =
            HandSession::new([tool as Arc<dyn RawTool>]).with_catalog_fingerprint("hand-v1");
        let _ = serve_hand(channel, session).await;
    });

    let executor =
        RemoteToolExecutor::new(dial_brain(&addr).await).with_catalog_fingerprint("run-v2");
    let err = executor
        .invoke(&call("c1", "echo", "x"))
        .await
        .expect_err("a catalog fingerprint mismatch must fail closed");
    assert!(
        err.to_string().to_lowercase().contains("fingerprint"),
        "got: {err}"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "the effect must NOT run on a mismatch"
    );
    hand.abort();
}

/// A tool call whose hand accepts the connection then vanishes without replying: the
/// brain cannot know whether the effect ran, so the outcome is Indeterminate — over a
/// real socket, not just an in-memory pipe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_hand_over_tcp_is_indeterminate() {
    let (addr, listener) = bind_loopback_hand().await;
    tokio::spawn(async move {
        let channel = listener.accept().await.expect("accept");
        drop(channel); // accept, then vanish before replying
    });

    let executor = RemoteToolExecutor::new(dial_brain(&addr).await);
    let result = executor.call_hand(&call("c1", "echo", "x")).await;
    assert_eq!(result, HandResult::Indeterminate);
}
