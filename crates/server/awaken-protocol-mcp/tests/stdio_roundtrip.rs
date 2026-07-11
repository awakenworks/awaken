//! End-to-end stdio round trip: the in-repo MCP *client* (`awaken-ext-mcp`)
//! drives this crate's demo server binary as a real subprocess — handshake,
//! discovery, plain call, error result, and progress streaming all cross a
//! real process boundary. No network, so it runs in the offline CI hot path
//! (unlike the client's `--ignored` external-server test).

use std::sync::Arc;

use awaken_ext_mcp::transport::McpToolTransport;
use awaken_ext_mcp::{DEFAULT_TIMEOUT, StdioTransport, connect_tools};
use awaken_runtime_contract::tool::ToolCall;

async fn demo_server() -> StdioTransport {
    StdioTransport::connect(
        env!("CARGO_BIN_EXE_awaken-mcp-stdio-demo"),
        &[],
        DEFAULT_TIMEOUT,
    )
    .await
    .expect("spawn + handshake with the demo server")
}

#[tokio::test]
async fn discovery_and_echo_round_trip_through_the_raw_tool_port() {
    let transport: Arc<dyn McpToolTransport> = Arc::new(demo_server().await);
    let conn = connect_tools("demo", Arc::clone(&transport))
        .await
        .expect("discover tools");

    // The server's ToolDescriptor ids surface under the client's namespace.
    let echo = conn
        .tools
        .iter()
        .find(|t| t.id() == "mcp__demo__echo")
        .expect("echo is advertised");
    let out = echo
        .invoke(ToolCall {
            call_id: "c1".to_string(),
            tool_id: "mcp__demo__echo".to_string(),
            arguments: serde_json::json!({ "message": "hello server" }),
        })
        .await
        .expect("echo invokes");
    assert!(!out.is_error);
    assert_eq!(out.content, "echo: hello server");
}

#[tokio::test]
async fn progress_streams_from_server_tool_to_client_channel() {
    let transport = demo_server().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let result = transport
        .call_tool_with_progress("count", serde_json::json!({ "steps": 4 }), tx)
        .await
        .expect("count runs");
    assert!(!result.is_error.unwrap_or(false));

    let mut updates = Vec::new();
    while let Some(update) = rx.recv().await {
        updates.push(update);
    }
    assert_eq!(updates.len(), 4, "one update per step: {updates:?}");
    assert_eq!(updates[0].progress, 1.0);
    assert_eq!(updates[3].progress, 4.0);
    assert_eq!(updates[3].total, Some(4.0));
    assert_eq!(updates[0].message.as_deref(), Some("step 1 of 4"));
}

#[tokio::test]
async fn unknown_tool_surfaces_as_a_transport_error() {
    let transport = demo_server().await;
    let err = transport
        .call_tool("no-such-tool", serde_json::json!({}))
        .await
        .expect_err("unknown tool is a protocol-level failure");
    assert!(err.to_string().contains("no-such-tool"), "{err}");
}
