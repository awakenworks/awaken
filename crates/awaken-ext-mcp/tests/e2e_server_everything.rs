//! End-to-end MCP tests against the official reference server
//! [`@modelcontextprotocol/server-everything`].
//!
//! These tests are `#[ignore]`'d so the default `cargo test` run does
//! not require Node.js or a network download. Opt in with:
//!
//! ```text
//! cargo test --test e2e_server_everything -- --ignored
//! ```
//!
//! ## Server resolution
//!
//! Tests resolve the server binary in this order:
//!   1. `MCP_E2E_SERVER_BIN` env var (path to a pre-installed
//!      `mcp-server-everything` executable). Preferred in CI so each
//!      test doesn't pay the `npx` startup cost.
//!   2. Fallback to the pinned `npx -y
//!      @modelcontextprotocol/server-everything@2026.1.26 stdio`.
//!      First run downloads (~5s); subsequent runs hit the npm cache.
//!
//! ## Scope
//!
//! Happy-path validation against a spec-compliant server. The wire
//! shape (protocol negotiation, tool dispatch, resource reads,
//! completion, list_changed) is exercised end-to-end. Fault paths
//! (404 session expired, mid-stream drops, malformed events) stay in
//! the synthetic mock tests in `mcp_tests.rs` — the reference server
//! doesn't fault on demand.

use awaken_ext_mcp::{McpServerConnectionConfig, McpToolRegistryManager};
use awaken_runtime_contract::contract::tool::ToolCallContext;
use serde_json::{Value, json};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

static E2E_SERVER_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
const SERVER_EVERYTHING_PACKAGE: &str = "@modelcontextprotocol/server-everything@2026.1.26";

async fn e2e_server_lock() -> tokio::sync::MutexGuard<'static, ()> {
    E2E_SERVER_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Resolve the command + args to spawn the reference server. See
/// module docs for the resolution order.
fn server_command() -> (String, Vec<String>) {
    if let Ok(path) = std::env::var("MCP_E2E_SERVER_BIN") {
        return (path, vec!["stdio".to_string()]);
    }
    (
        "npx".to_string(),
        vec![
            "-y".to_string(),
            SERVER_EVERYTHING_PACKAGE.to_string(),
            "stdio".to_string(),
        ],
    )
}

fn streamable_http_server_command() -> (String, Vec<String>) {
    if let Ok(path) = std::env::var("MCP_E2E_SERVER_BIN") {
        return (path, vec!["streamableHttp".to_string()]);
    }
    (
        "npx".to_string(),
        vec![
            "-y".to_string(),
            SERVER_EVERYTHING_PACKAGE.to_string(),
            "streamableHttp".to_string(),
        ],
    )
}

fn make_config(name: &str) -> McpServerConnectionConfig {
    let (cmd, args) = server_command();
    let mut cfg = McpServerConnectionConfig::stdio(name, cmd, args);
    // `npx` first-run + npm install can take a while; give the
    // initialize handshake enough headroom.
    cfg.timeout_secs = 60;
    cfg
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    async fn stop(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn reserve_local_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port for HTTP e2e");
    listener.local_addr().expect("read ephemeral port").port()
}

async fn spawn_streamable_http_server() -> (ChildGuard, String) {
    let port = reserve_local_port();
    let (cmd, args) = streamable_http_server_command();
    let child = Command::new(cmd)
        .args(args)
        .env("PORT", port.to_string())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn server-everything streamableHttp");

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "server-everything streamableHttp did not open port {port}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    (ChildGuard { child }, format!("http://127.0.0.1:{port}/mcp"))
}

/// Find a tool by substring (the server-everything tool names use
/// `-` separators, which awaken's `to_tool_id` rewrites to `_`; tests
/// search rather than equality-match so a future server rename
/// doesn't break this layer).
fn find_tool_id(ids: &[String], needle: &str) -> Option<String> {
    ids.iter().find(|id| id.contains(needle)).cloned()
}

/// Smoke: connect → tools/list → tools/call(echo). Validates the
/// full happy-path surface awaken claims to support: initialize
/// handshake (protocol version negotiation against the server's
/// `2025-11-25`), capability advertisement, tools/list discovery,
/// tool id mapping, and a tools/call round trip.
#[tokio::test]
#[ignore]
async fn e2e_initialize_and_echo_tool() {
    let _guard = e2e_server_lock().await;
    let cfg = make_config("e2e-echo");
    let manager = McpToolRegistryManager::connect([cfg])
        .await
        .expect("connect to server-everything");
    let registry = manager.registry();
    let ids: Vec<String> = registry.ids().into_iter().collect();

    let echo_id = find_tool_id(&ids, "echo")
        .unwrap_or_else(|| panic!("server-everything should expose an echo tool; got {ids:?}"));
    let tool = registry
        .get(&echo_id)
        .expect("registry returns tool for known id");

    let ctx = ToolCallContext::test_default();
    let result = tool
        .execute(json!({"message": "hello-e2e"}), &ctx)
        .await
        .expect("echo tool call succeeds");

    let text = result.result.data.to_string();
    assert!(
        text.contains("hello-e2e"),
        "echo result should round-trip our input; got: {text}"
    );
}

/// Real Streamable HTTP e2e against the official reference server.
/// server-everything returns initialize as `text/event-stream` and
/// assigns `MCP-Session-Id`, so this covers the stateful HTTP path that
/// the synthetic integration tests exercise under failure conditions.
#[tokio::test]
#[ignore]
async fn e2e_streamable_http_initialize_and_echo_tool() {
    let _guard = e2e_server_lock().await;
    let (server, endpoint) = spawn_streamable_http_server().await;
    let mut cfg = McpServerConnectionConfig::http("e2e-http", endpoint);
    cfg.timeout_secs = 60;

    let manager = McpToolRegistryManager::connect([cfg])
        .await
        .expect("connect to server-everything streamableHttp");
    let registry = manager.registry();
    let ids: Vec<String> = registry.ids().into_iter().collect();

    let echo_id = find_tool_id(&ids, "echo").unwrap_or_else(|| {
        panic!("server-everything streamableHttp should expose an echo tool; got {ids:?}")
    });
    let tool = registry
        .get(&echo_id)
        .expect("registry returns tool for known HTTP id");

    let result = tool
        .execute(
            json!({"message": "hello-streamable-http"}),
            &ToolCallContext::test_default(),
        )
        .await
        .expect("HTTP echo tool call succeeds");

    let text = result.result.data.to_string();
    assert!(
        text.contains("hello-streamable-http"),
        "HTTP echo result should round-trip our input; got: {text}"
    );

    manager.close_all().await.expect("close HTTP MCP manager");
    server.stop().await;
}

/// Structured arguments + numeric result. server-everything's `get-sum`
/// tool takes two numbers and returns their sum; exercises numeric
/// argument serialization and result deserialization.
#[tokio::test]
#[ignore]
async fn e2e_get_sum_structured_args() {
    let _guard = e2e_server_lock().await;
    let cfg = make_config("e2e-sum");
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let registry = manager.registry();
    let ids: Vec<String> = registry.ids().into_iter().collect();

    let sum_id = find_tool_id(&ids, "get_sum")
        .unwrap_or_else(|| panic!("server-everything should expose get-sum; got {ids:?}"));
    let tool = registry.get(&sum_id).unwrap();
    let ctx = ToolCallContext::test_default();

    let result = tool
        .execute(json!({"a": 7, "b": 35}), &ctx)
        .await
        .expect("get-sum call succeeds");

    // The server formats the sum into the tool result text.
    let text = result.result.data.to_string();
    assert!(
        text.contains("42"),
        "sum of 7 + 35 should appear in result; got: {text}"
    );
}

/// resources/list + read. server-everything exposes a fixed set of
/// numbered text resources (`test://static/resource/<n>`); we don't
/// pin a specific URI shape, just that:
///   - list_resources returns at least one entry,
///   - read_resource on that entry returns content.
#[tokio::test]
#[ignore]
async fn e2e_resources_list_and_read() {
    let _guard = e2e_server_lock().await;
    let cfg = make_config("e2e-resources");
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();

    let resources = manager.list_resources().await.expect("list_resources");
    assert!(
        !resources.is_empty(),
        "server-everything exposes resources; list must be non-empty"
    );

    // Read the first resource. The wire shape is a `Value` containing
    // the server's `ReadResourceResult` (`{ contents: [...] }`).
    let first = &resources[0];
    let body = manager
        .read_resource(&first.server_name, &first.resource.uri)
        .await
        .expect("read_resource on the first listed entry");

    // Don't pin the exact content shape — server-everything's text
    // resources have prose bodies that may evolve. Just verify the
    // server returned a `contents` array with at least one entry.
    let contents = body
        .get("contents")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("read_resource result missing `contents` array: {body}"));
    assert!(
        !contents.is_empty(),
        "contents array should be non-empty for {}",
        first.resource.uri
    );
}

/// prompts/list. server-everything ships several reference prompts.
/// Just verify discovery works against a real server (the local mock
/// tests already cover the wire shape).
#[tokio::test]
#[ignore]
async fn e2e_prompts_list() {
    let _guard = e2e_server_lock().await;
    let cfg = make_config("e2e-prompts");
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();
    let prompts = manager.list_prompts().await.expect("list_prompts");
    assert!(
        !prompts.is_empty(),
        "server-everything ships reference prompts; list must be non-empty"
    );
}

/// completion/complete against a known prompt ref. server-everything
/// advertises `completions: {}` and supports argument autocomplete
/// for its `simple_prompt` / `complex_prompt` prompts. We probe
/// `complex_prompt`'s `temperature` argument — the exact completion
/// set isn't pinned (server-side data may evolve); we just verify
/// the call succeeds and returns a `CompleteResult` shape.
#[tokio::test]
#[ignore]
async fn e2e_completion_complete() {
    let _guard = e2e_server_lock().await;
    use mcp::{CompleteArgument, CompleteParams};

    let cfg = make_config("e2e-complete");
    let manager = McpToolRegistryManager::connect([cfg]).await.unwrap();

    // First discover the prompt name so the test isn't brittle to
    // server-side renames.
    let prompts = manager.list_prompts().await.expect("list_prompts");
    let target = prompts
        .iter()
        .find(|p| p.prompt.name.contains("prompt"))
        .unwrap_or_else(|| panic!("expected at least one prompt; got {prompts:?}"))
        .clone();

    let params = CompleteParams {
        r#ref: json!({
            "type": "ref/prompt",
            "name": target.prompt.name,
        }),
        argument: CompleteArgument {
            name: "temperature".to_string(),
            value: "".to_string(),
        },
        context: None,
    };

    // The call must succeed against a server that advertises
    // completions. The result's `values` array MAY be empty (the
    // prompt may not have suggestions for this argument); we only
    // assert the wire round-trip didn't error.
    let result = manager
        .complete(&target.server_name, params)
        .await
        .expect("completion/complete round trip");
    // The completion result struct is well-formed if `values` is a
    // (possibly empty) Vec. The deserializer would've already
    // rejected an unexpected shape.
    let _ = result.completion.values;
}
