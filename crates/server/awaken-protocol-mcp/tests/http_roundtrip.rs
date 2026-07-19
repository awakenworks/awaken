//! End-to-end Streamable HTTP round trip: the in-repo MCP *client*
//! (`awaken-ext-mcp`) drives this crate's axum router over a real local port —
//! handshake + session, discovery, calls, SSE progress streaming, the bearer
//! challenge/refresh loop, and `tools/list_changed` over the standing GET
//! stream. Local sockets only, so it stays in the offline CI hot path.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::state::Store;
use awaken_ext_mcp::transport::{ListChangedKind, McpToolTransport};
use awaken_ext_mcp::{
    AuthChallenge, Credential, CredentialRefresher, HttpTransport, HttpTransportBuilder,
};
use awaken_mcp_wire::progress::McpProgressUpdate;
use awaken_protocol_mcp::export::{ProgressRawTool, ToolExportSource};
use awaken_protocol_mcp::{
    McpExportedTool, McpHttpConfig, McpToolService, SharedExports, StaticExports, router,
};
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use serde_json::json;
use tokio::sync::mpsc;

struct EchoTool;

#[async_trait]
impl RawTool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let message = call.arguments["message"].as_str().unwrap_or_default();
        Ok(ToolOutput::ok(call.call_id, format!("echo: {message}")))
    }
}

struct CountTool;

#[async_trait]
impl ProgressRawTool for CountTool {
    fn id(&self) -> &str {
        "count"
    }
    async fn invoke_with_progress(
        &self,
        call: ToolCall,
        progress: mpsc::Sender<McpProgressUpdate>,
    ) -> Result<ToolOutput, ToolError> {
        let steps = call.arguments["steps"].as_u64().unwrap_or(2);
        for step in 1..=steps {
            let _ = progress
                .send(McpProgressUpdate {
                    progress: step as f64,
                    total: Some(steps as f64),
                    message: None,
                })
                .await;
        }
        Ok(ToolOutput::ok(call.call_id, format!("counted {steps}")))
    }
}

fn descriptor(id: &str, description: &str) -> ToolDescriptor {
    ToolDescriptor::pinned("test", id, description, json!({ "type": "object" }))
}

fn exports() -> Vec<McpExportedTool> {
    vec![
        McpExportedTool::plain(descriptor("echo", "echoes"), Arc::new(EchoTool)),
        McpExportedTool::with_progress(descriptor("count", "counts"), Arc::new(CountTool)),
    ]
}

/// Serve the router on an ephemeral local port; returns the endpoint URL.
async fn serve(source: Arc<dyn ToolExportSource>, bearer_token: Option<String>) -> String {
    serve_with(
        McpToolService::new("http-test", "0.0.0", source),
        bearer_token,
    )
    .await
}

/// Serve a pre-built (possibly gated) service on an ephemeral local port.
async fn serve_with(service: McpToolService, bearer_token: Option<String>) -> String {
    let service = Arc::new(service);
    let app = router(
        service,
        McpHttpConfig {
            path: "/mcp".to_string(),
            bearer_token,
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server runs");
    });
    url
}

#[tokio::test]
async fn discovery_and_call_round_trip() {
    let url = serve(Arc::new(StaticExports::new(exports())), None).await;
    let transport = HttpTransport::connect(url, Credential::None)
        .await
        .expect("handshake");

    let tools = transport.list_tools().await.expect("tools/list");
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"echo") && names.contains(&"count"),
        "{names:?}"
    );

    let result = transport
        .call_tool("echo", json!({ "message": "over http" }))
        .await
        .expect("tools/call");
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn progress_streams_through_the_sse_post_response() {
    let url = serve(Arc::new(StaticExports::new(exports())), None).await;
    let transport = HttpTransport::connect(url, Credential::None)
        .await
        .expect("handshake");

    let (tx, mut rx) = mpsc::channel(64);
    let result = transport
        .call_tool_with_progress("count", json!({ "steps": 3 }), tx)
        .await
        .expect("count runs");
    assert!(!result.is_error.unwrap_or(false));

    let mut updates = Vec::new();
    while let Some(update) = rx.recv().await {
        updates.push(update);
    }
    assert_eq!(updates.len(), 3, "{updates:?}");
    assert_eq!(updates[2].progress, 3.0);
    assert_eq!(updates[2].total, Some(3.0));
}

struct StaticRefresher {
    fresh: Option<Credential>,
}

#[async_trait]
impl CredentialRefresher for StaticRefresher {
    async fn refresh(&self, challenge: &AuthChallenge) -> Option<Credential> {
        assert_eq!(challenge.status, 401);
        assert!(
            challenge
                .www_authenticate
                .as_deref()
                .is_some_and(|value| value.starts_with("Bearer")),
            "server sends a bearer challenge"
        );
        self.fresh.clone()
    }
}

#[tokio::test]
async fn wrong_bearer_surfaces_the_challenge() {
    let url = serve(
        Arc::new(StaticExports::new(exports())),
        Some("right-token".to_string()),
    )
    .await;
    let Err(err) = HttpTransport::connect(url, Credential::Bearer("wrong".to_string())).await
    else {
        panic!("connect must fail on a 401 without a refresher");
    };
    assert!(err.to_string().contains("auth challenge"), "{err}");
}

#[tokio::test]
async fn refresher_recovers_from_a_stale_bearer() {
    let url = serve(
        Arc::new(StaticExports::new(exports())),
        Some("right-token".to_string()),
    )
    .await;
    let transport = HttpTransportBuilder::new(url)
        .credential(Credential::Bearer("stale".to_string()))
        .refresher(Arc::new(StaticRefresher {
            fresh: Some(Credential::Bearer("right-token".to_string())),
        }))
        .connect()
        .await
        .expect("refresh + retry succeeds");
    let tools = transport
        .list_tools()
        .await
        .expect("authorized after refresh");
    assert!(!tools.is_empty());
}

#[tokio::test]
async fn export_change_reaches_the_client_as_tools_list_changed() {
    let source = Arc::new(SharedExports::new(exports()));
    let url = serve(Arc::clone(&source) as Arc<dyn ToolExportSource>, None).await;
    // Streaming mode: the client holds the standing GET SSE stream.
    let transport = HttpTransport::connect_streaming(url, Credential::None, None)
        .await
        .expect("handshake");
    let mut list_changed = transport.subscribe_list_changed();
    // Give the GET stream a beat to attach before changing the set.
    tokio::time::sleep(Duration::from_millis(200)).await;

    source.replace(vec![McpExportedTool::plain(
        descriptor("echo", "echoes"),
        Arc::new(EchoTool),
    )]);

    let kind = tokio::time::timeout(Duration::from_secs(5), list_changed.recv())
        .await
        .expect("notified before timeout")
        .expect("broadcast open");
    assert_eq!(kind, ListChangedKind::Tools);

    // The refreshed list reflects the new export set.
    let tools = transport.list_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
}

/// A gate that awaits every call for out-of-band approval — the `ask`/HITL outcome
/// a permission policy produces. An external MCP client has no run to await, so the
/// service must fail this closed rather than execute the tool.
struct SuspendGate;

#[async_trait]
impl ToolGateHook for SuspendGate {
    async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateOutcome {
        GateOutcome::RequireConfirmation {
            correlation_id: "ticket-1".to_string(),
        }
    }
}

#[tokio::test]
async fn an_approval_gated_tool_fails_closed_for_an_external_mcp_client() {
    // HITL over MCP: a tool the gate would await (suspend) cannot be honored — an
    // external client has no run to suspend — so `tools/call` returns a
    // model-visible error instead of running the effect (ADR-0035, fail closed).
    let service = McpToolService::new(
        "http-test",
        "0.0.0",
        Arc::new(StaticExports::new(exports())),
    )
    .with_gate(Arc::new(SuspendGate));
    let url = serve_with(service, None).await;
    let transport = HttpTransport::connect(url, Credential::None)
        .await
        .expect("handshake");

    let result = transport
        .call_tool("echo", json!({ "message": "should never run" }))
        .await
        .expect("tools/call returns a result (the transport succeeds; the call is refused)");

    assert!(
        result.is_error.unwrap_or(false),
        "a suspend/ask gate must fail closed over MCP, got {result:?}",
    );
    let body = format!("{:?}", result.content);
    assert!(
        body.contains("out-of-band approval"),
        "the refusal names the missing out-of-band approval, got {body}",
    );
}
