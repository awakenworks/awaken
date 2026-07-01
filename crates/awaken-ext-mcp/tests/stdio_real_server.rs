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
}
