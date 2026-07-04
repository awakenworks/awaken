//! Real-server bearer verification for the HTTP transport.
//!
//! Ignored by default: it talks to a live MCP endpoint over the network, so it
//! is not part of the offline CI hot path. Point it at any bearer-protected
//! Streamable HTTP server, e.g. GitHub's hosted MCP with a PAT:
//!
//! ```sh
//! MCP_TEST_URL=https://api.githubcopilot.com/mcp/ \
//! MCP_TEST_BEARER=$(gh auth token) \
//! cargo test -p awaken-ext-mcp --test http_real_server -- --ignored --nocapture
//! ```
//!
//! It verifies the positive path (handshake + tools/list under a valid
//! bearer) and the negative path (a bogus bearer surfaces as an
//! `auth challenge:` transport error, not a hang or a panic).

use awaken_ext_mcp::{Credential, HttpTransportBuilder, McpToolTransport};

fn test_target() -> Option<(String, String)> {
    let url = std::env::var("MCP_TEST_URL").ok()?;
    let bearer = std::env::var("MCP_TEST_BEARER").ok()?;
    Some((url, bearer))
}

#[tokio::test]
#[ignore = "talks to a live MCP server; set MCP_TEST_URL/MCP_TEST_BEARER and run with --ignored"]
async fn bearer_handshake_and_tool_discovery_round_trip() {
    let Some((url, bearer)) = test_target() else {
        eprintln!("skipped: MCP_TEST_URL / MCP_TEST_BEARER not set");
        return;
    };
    let transport = HttpTransportBuilder::new(url)
        .credential(Credential::Bearer(bearer))
        .connect()
        .await
        .expect("initialize handshake succeeds under a valid bearer");
    let tools = transport.list_tools().await.expect("tools/list succeeds");
    assert!(!tools.is_empty(), "a real server advertises tools");
    eprintln!(
        "discovered {} tools, e.g. {:?}",
        tools.len(),
        tools.iter().take(3).map(|t| &t.name).collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "talks to a live MCP server; set MCP_TEST_URL and run with --ignored"]
async fn bogus_bearer_surfaces_an_auth_challenge() {
    let Some(url) = std::env::var("MCP_TEST_URL").ok() else {
        eprintln!("skipped: MCP_TEST_URL not set");
        return;
    };
    // Well-formed but invalid, so it reaches the auth layer instead of being
    // rejected as malformed (GitHub answers 400 to a garbage-shaped token).
    // Override with MCP_TEST_BOGUS_BEARER for servers with other token shapes.
    let bogus = std::env::var("MCP_TEST_BOGUS_BEARER")
        .unwrap_or_else(|_| format!("ghp_{}", "0".repeat(36)));
    let result = HttpTransportBuilder::new(url)
        .credential(Credential::Bearer(bogus))
        .connect()
        .await;
    let Err(err) = result else {
        panic!("a bogus bearer must be rejected");
    };
    let message = err.to_string();
    eprintln!("rejected with: {message}");
    assert!(message.contains("auth challenge: HTTP 401"), "{message}");
}
