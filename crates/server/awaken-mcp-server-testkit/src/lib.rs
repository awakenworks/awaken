//! Shared black-box conformance assertions for MCP server adapters.
//!
//! A second host (for example a flow/resource runtime) implements the tiny
//! [`McpConformanceDriver`] over its facade and runs the same suite; no awaken
//! runtime vocabulary enters these assertions.

use std::sync::Mutex;

use async_trait::async_trait;
use awaken_mcp_server_core::{McpHttpBody, McpHttpMethod, McpHttpReply, NotifySink};
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

    /// Capability expected from initialize. Fixed tool sets keep the safe
    /// default `false`; dynamic adapters override only when every transport is
    /// wired to a real change source.
    fn tools_list_changed_capability(&self) -> bool {
        false
    }
}

/// Black-box Streamable HTTP driver. Both the axum-free decision kernel and a
/// concrete HTTP router can map their response into [`McpHttpReply`], allowing
/// one suite to pin status, headers, JSON envelopes, and SSE ordering without
/// importing a framework into this neutral testkit.
#[async_trait]
pub trait McpHttpConformanceDriver: Send + Sync {
    async fn exchange(
        &self,
        method: McpHttpMethod,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> McpHttpReply;

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
    assert_eq!(
        initialized["capabilities"]["tools"]["listChanged"],
        driver.tools_list_changed_capability()
    );

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

/// Run the complete Streamable HTTP conformance matrix.
///
/// The suite deliberately distinguishes transport errors (4xx, no JSON-RPC
/// success envelope) from valid JSON-RPC method errors (HTTP 200). It also pins
/// exact content negotiation and the standing GET stream contract.
pub async fn assert_mcp_http_transport_conformance(driver: &dyn McpHttpConformanceDriver) {
    const POST_HEADERS: &[(&str, &str)] = &[
        ("content-type", "application/json; charset=utf-8"),
        ("accept", "application/json, text/event-stream"),
    ];
    const PING: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;

    let get = driver
        .exchange(McpHttpMethod::Get, &[("accept", "text/event-stream")], b"")
        .await;
    assert_eq!(get.status.as_u16(), 200);
    assert_eq!(get.body, McpHttpBody::EventStream);
    assert_eq!(
        get.headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );

    for headers in [
        Vec::new(),
        vec![("accept", "application/json")],
        vec![("accept", "text/event-stream;q=0")],
    ] {
        let reply = driver.exchange(McpHttpMethod::Get, &headers, b"").await;
        assert_eq!(reply.status.as_u16(), 406, "GET headers={headers:?}");
    }

    let missing_content_type = driver
        .exchange(
            McpHttpMethod::Post,
            &[("accept", "application/json, text/event-stream")],
            PING,
        )
        .await;
    assert_eq!(missing_content_type.status.as_u16(), 415);

    for accept in [
        "application/json",
        "text/event-stream",
        "*/*",
        "application/json, text/event-stream;q=0",
    ] {
        let reply = driver
            .exchange(
                McpHttpMethod::Post,
                &[("content-type", "application/json"), ("accept", accept)],
                PING,
            )
            .await;
        assert_eq!(reply.status.as_u16(), 406, "Accept={accept}");
    }

    let before_invalid = driver.host_call_count();
    for body in [
        b"not-json".as_slice(),
        br#"{"id":1,"method":"ping"}"#.as_slice(),
        br#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#.as_slice(),
        br#"{"jsonrpc":"2.0","id":{},"method":"ping"}"#.as_slice(),
        br#"{"jsonrpc":"2.0","id":1,"result":{}}"#.as_slice(),
    ] {
        let reply = driver
            .exchange(McpHttpMethod::Post, POST_HEADERS, body)
            .await;
        assert_eq!(reply.status.as_u16(), 400, "body={body:?}");
    }
    assert_eq!(driver.host_call_count(), before_invalid);

    let invalid_progress = driver
        .exchange(
            McpHttpMethod::Post,
            POST_HEADERS,
            br#"{"jsonrpc":"1.0","id":9,"method":"tools/call","params":{"name":"progress","arguments":{"steps":1},"_meta":{"progressToken":"bad"}}}"#,
        )
        .await;
    assert_eq!(invalid_progress.status.as_u16(), 400);
    assert_eq!(driver.host_call_count(), before_invalid);

    for method in [McpHttpMethod::Get, McpHttpMethod::Delete] {
        let headers = if method == McpHttpMethod::Get {
            vec![
                ("accept", "text/event-stream"),
                ("mcp-protocol-version", "1900-01-01"),
            ]
        } else {
            vec![("mcp-protocol-version", "1900-01-01")]
        };
        let reply = driver.exchange(method, &headers, b"").await;
        assert_eq!(reply.status.as_u16(), 400, "method={method:?}");
    }

    for body in [
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.as_slice(),
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":7}}"#.as_slice(),
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1900-01-01"}}"#.as_slice(),
    ] {
        let reply = driver
            .exchange(McpHttpMethod::Post, POST_HEADERS, body)
            .await;
        assert_eq!(reply.status.as_u16(), 400, "initialize={body:?}");
    }

    let unsupported_header = driver
        .exchange(
            McpHttpMethod::Post,
            &[
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
                ("mcp-protocol-version", "1900-01-01"),
            ],
            PING,
        )
        .await;
    assert_eq!(unsupported_header.status.as_u16(), 400);

    let initialized = driver
        .exchange(
            McpHttpMethod::Post,
            POST_HEADERS,
            br#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        )
        .await;
    assert_eq!(initialized.status.as_u16(), 200);
    assert_eq!(
        initialized
            .headers
            .get("mcp-protocol-version")
            .and_then(|value| value.to_str().ok()),
        Some("2025-06-18")
    );
    let McpHttpBody::Json(initialized) = initialized.body else {
        panic!("initialize must return JSON")
    };
    assert_eq!(initialized["jsonrpc"], "2.0");
    assert_eq!(initialized["id"], 2);
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");

    let notification = driver
        .exchange(
            McpHttpMethod::Post,
            POST_HEADERS,
            br#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
        )
        .await;
    assert_eq!(notification.status.as_u16(), 202);
    assert_eq!(notification.body, McpHttpBody::Empty);

    let progress = driver
        .exchange(
            McpHttpMethod::Post,
            POST_HEADERS,
            br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"progress","arguments":{"steps":3},"_meta":{"progressToken":"p-http"}}}"#,
        )
        .await;
    assert_eq!(progress.status.as_u16(), 200);
    let McpHttpBody::Sse(events) = progress.body else {
        panic!("progress call must return SSE")
    };
    assert_eq!(events.len(), 4);
    for (index, event) in events[..3].iter().enumerate() {
        assert_eq!(event["jsonrpc"], "2.0");
        assert_eq!(event["method"], "notifications/progress");
        assert_eq!(
            event["params"]["progress"].as_f64(),
            Some((index + 1) as f64)
        );
    }
    assert_eq!(events[3]["id"], 3);
    assert!(events[3].get("result").is_some());

    let deleted = driver.exchange(McpHttpMethod::Delete, &[], b"").await;
    assert_eq!(deleted.status.as_u16(), 204);
    assert_eq!(deleted.body, McpHttpBody::Empty);

    let other = driver.exchange(McpHttpMethod::Other, &[], b"").await;
    assert_eq!(other.status.as_u16(), 405);
}
