//! Real-server stdio integration test.
//!
//! Ignored by default: it spawns an external MCP server over stdio, so it needs
//! a network-fetched server binary and is not part of the offline CI hot path
//! (same policy as the live store tests). Run it manually with:
//!
//! ```sh
//! cargo test -p awaken-ext-mcp --test stdio_real_server -- --ignored --nocapture
//! ```
//!
//! It connects to the reference "everything" server, discovers its tools, and
//! calls `echo`, asserting the round trip through the neutral `RawTool` port.

use std::sync::Arc;

use awaken_ext_mcp::transport::McpToolTransport;
use awaken_ext_mcp::{DEFAULT_TIMEOUT, StdioTransport, connect_tools};
use awaken_runtime_contract::tool::ToolCall;

#[tokio::test]
#[ignore = "spawns an external MCP server; run with --ignored"]
async fn everything_server_echo_round_trips() {
    let transport = StdioTransport::connect(
        "npx",
        &[
            "-y".to_string(),
            "@modelcontextprotocol/server-everything".to_string(),
        ],
        DEFAULT_TIMEOUT,
    )
    .await
    .expect("spawn + handshake with the everything server");

    let transport: Arc<dyn McpToolTransport> = Arc::new(transport);
    let conn_transport = Arc::clone(&transport);
    let conn = connect_tools("everything", transport)
        .await
        .expect("discover tools");

    // The reference server advertises an `echo` tool.
    let echo = conn
        .tools
        .iter()
        .find(|t| t.id() == "mcp__everything__echo")
        .expect("echo tool is advertised");

    let out = echo
        .invoke(ToolCall {
            call_id: "c1".to_string(),
            tool_id: "mcp__everything__echo".to_string(),
            arguments: serde_json::json!({ "message": "hello mcp" }),
        })
        .await
        .expect("echo invokes");
    assert!(!out.is_error);
    assert!(out.content.contains("hello mcp"));

    // The everything server also exposes prompts and resources.
    let prompts = conn_transport.list_prompts().await.expect("list prompts");
    assert!(!prompts.is_empty(), "server advertises prompts");

    let resources = conn_transport
        .list_resources()
        .await
        .expect("list resources");
    assert!(!resources.is_empty(), "server advertises resources");
    let first = &resources[0];
    let read = conn_transport
        .read_resource(&first.uri)
        .await
        .expect("read resource");
    assert!(read.is_object() || read.is_array() || read.is_string());

    // The long-running-operation tool emits progress notifications; assert at
    // least one reaches the per-call channel. The call drops its progress sender
    // when it returns, so this loop drains the buffer and then ends. The wire
    // name (not the sanitized id) must be used for the call.
    let tools = conn_transport.list_tools().await.expect("list tools");
    let lro = tools
        .iter()
        .find(|t| {
            awaken_ext_mcp::to_tool_id("everything", &t.name)
                .ok()
                .as_deref()
                == Some("mcp__everything__trigger_long_running_operation")
        })
        .expect("long-running-operation tool present");
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let result = conn_transport
        .call_tool_with_progress(
            &lro.name,
            serde_json::json!({ "duration": 1, "steps": 3 }),
            tx,
        )
        .await
        .expect("long op runs");
    assert!(!result.is_error.unwrap_or(false), "{:?}", result.content);
    let mut progress_count = 0usize;
    while let Some(update) = rx.recv().await {
        assert!(update.progress >= 0.0);
        progress_count += 1;
    }
    assert!(progress_count >= 1, "received at least one progress update");
}
