//! Shared black-box conformance assertions for MCP server adapters.
//!
//! A second host (for example a flow/resource runtime) implements the tiny
//! [`McpConformanceDriver`] over its facade and runs the same suite; no awaken
//! runtime vocabulary enters these assertions.

use std::sync::Mutex;

use async_trait::async_trait;
use awaken_mcp_server_core::NotifySink;
use awaken_mcp_wire::jsonrpc::ServerRequestError;
use serde_json::{Value, json};

/// Black-box request driver supplied by each adapter. The fixture must expose
/// `echo` and `progress` tools with the behavior described by
/// [`assert_mcp_server_conformance`].
#[async_trait]
pub trait McpConformanceDriver: Send + Sync {
    async fn request(
        &self,
        method: &str,
        params: Value,
        sink: &dyn NotifySink,
    ) -> Result<Value, ServerRequestError>;

    /// Number of calls that crossed into the business host. Used to prove invalid
    /// params are rejected before dispatch.
    fn host_call_count(&self) -> usize;
}

/// Thread-safe notification recorder used by the shared suite and downstream
/// adapter-specific tests.
#[derive(Default)]
pub struct RecordingSink {
    seen: Mutex<Vec<(String, Value)>>,
}

impl RecordingSink {
    #[must_use]
    pub fn snapshot(&self) -> Vec<(String, Value)> {
        self.seen.lock().expect("notification recorder").clone()
    }
}

#[async_trait]
impl NotifySink for RecordingSink {
    async fn notify(&self, method: &str, params: Value) {
        self.seen
            .lock()
            .expect("notification recorder")
            .push((method.to_string(), params));
    }
}

/// Run the shared lifecycle/list/call/validation/progress/list-changed
/// conformance matrix.
///
/// Fixture contract:
/// - `echo({message})` returns one text block `echo: <message>`;
/// - `progress({steps}, _meta.progressToken)` emits ordered progress 1..steps and
///   returns one final result only after those notifications.
pub async fn assert_mcp_server_conformance(driver: &dyn McpConformanceDriver) {
    let null = awaken_mcp_server_core::NullSink;

    let initialized = driver
        .request(
            "initialize",
            json!({ "protocolVersion": "2025-11-25" }),
            &null,
        )
        .await
        .expect("supported initialize");
    assert_eq!(initialized["protocolVersion"], "2025-11-25");
    assert_eq!(initialized["capabilities"]["tools"]["listChanged"], true);

    let unsupported = driver
        .request(
            "initialize",
            json!({ "protocolVersion": "1900-01-01" }),
            &null,
        )
        .await
        .expect_err("unsupported version fails before host dispatch");
    assert_eq!(unsupported.code, -32602);

    let listed = driver
        .request("tools/list", json!({}), &null)
        .await
        .expect("tools/list");
    let tools = listed["tools"].as_array().expect("tools array");
    assert!(tools.iter().any(|tool| tool["name"] == "echo"));
    assert!(tools.iter().any(|tool| tool["name"] == "progress"));

    let before_invalid = driver.host_call_count();
    let invalid = driver
        .request("tools/call", json!({ "arguments": {} }), &null)
        .await
        .expect_err("missing name is invalid params");
    assert_eq!(invalid.code, -32602);
    assert_eq!(
        driver.host_call_count(),
        before_invalid,
        "invalid params must not invoke the host"
    );

    let echoed = driver
        .request(
            "tools/call",
            json!({ "name": "echo", "arguments": { "message": "hello" } }),
            &null,
        )
        .await
        .expect("echo call");
    assert_eq!(echoed["content"][0]["text"], "echo: hello");
    assert_eq!(echoed["isError"], false);

    let progress = RecordingSink::default();
    let completed = driver
        .request(
            "tools/call",
            json!({
                "name": "progress",
                "arguments": { "steps": 3 },
                "_meta": { "progressToken": "p-1" }
            }),
            &progress,
        )
        .await
        .expect("progress call");
    assert_eq!(completed["content"][0]["text"], "counted 3");
    let seen = progress.snapshot();
    assert_eq!(seen.len(), 3);
    for (index, (method, params)) in seen.iter().enumerate() {
        assert_eq!(method, "notifications/progress");
        assert_eq!(params["progressToken"], "p-1");
        assert_eq!(params["progress"], (index + 1) as f64);
    }

    let list_changed = RecordingSink::default();
    awaken_mcp_server_core::notify_tools_list_changed(&list_changed).await;
    assert_eq!(
        list_changed.snapshot(),
        vec![("notifications/tools/list_changed".to_string(), json!({}))]
    );

    let unknown = driver
        .request("unknown/method", json!({}), &null)
        .await
        .expect_err("unknown method");
    assert_eq!(unknown.code, -32601);
}
