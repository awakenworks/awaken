use std::sync::Arc;

use awaken_mcp_server_core::NullSink;
use awaken_protocol_mcp::{McpExportedTool, McpToolService, StaticExports};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tool_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use serde_json::json;

struct ObservedTool(tokio::sync::mpsc::UnboundedSender<ToolCall>);

#[async_trait::async_trait]
impl RawTool for ObservedTool {
    fn id(&self) -> &str {
        "observed"
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.0.send(call.clone()).unwrap();
        if call.arguments["pending"] == true {
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::ok(call.call_id, "completed"))
    }
}

#[tokio::test]
async fn rebuilt_exports_cannot_reuse_an_in_flight_or_completed_call_identity() {
    // Cause/effect graph: one executor survives export reconstruction; each
    // independent MCP invocation needs a fresh fallback identity, otherwise the
    // Hand can mistake new work for its unresolved old operation. Decision rows:
    // I1 same export + identical arguments; I2 rebuilt export + identical args;
    // I3 cancellation after dispatch + rebuilt export. Every row preserves the
    // tool/arguments and has distinct ids. These are invocation ids, not durable
    // retry tokens: the existing relay tests keep the old operation indeterminate.
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let tool = Arc::new(ObservedTool(sent));
    let mut calls = Vec::new();
    for _ in 0..2 {
        let service = McpToolService::new(
            "awaken_session",
            "test",
            Arc::new(StaticExports::new(vec![McpExportedTool::plain(
                ToolDescriptor::pinned("test", "observed", "observed", json!({"type":"object"})),
                tool.clone(),
            )])),
        );
        for pending in [false, false, true] {
            let arguments = json!({"pending":pending});
            let invocation = service.handle(
                "tools/call",
                json!({"name":"observed", "arguments":arguments}),
                Arc::new(NullSink),
            );
            tokio::pin!(invocation);
            if pending {
                tokio::select! {
                    result = &mut invocation => panic!("pending call completed: {result:?}"),
                    call = received.recv() => calls.push(call.unwrap()),
                }
                // Dropping this future cancels after the RawTool observed dispatch.
            } else {
                assert_eq!(invocation.await.unwrap()["isError"], false);
                calls.push(received.recv().await.unwrap());
            }
            let call = calls.last().unwrap();
            assert_eq!(call.tool_id, "observed");
            assert_eq!(call.arguments, arguments);
        }
    }
    let unique = calls
        .iter()
        .map(|call| &call.call_id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        unique.len(),
        calls.len(),
        "I1-I3 independent invocations must not alias"
    );
}

// Drive the existing wire adapters with one cancellation oracle. Every client
// starts its own request counter; no production request IDs are rewritten.
enum Client {
    Stdio(Arc<awaken_mcp_wire::jsonrpc::JsonRpcPeer>),
    Http(axum::Router, String, bool),
}

impl Client {
    async fn http_post(
        app: axum::Router,
        session: &str,
        message: serde_json::Value,
    ) -> axum::response::Response {
        use tower::ServiceExt as _;
        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if !session.is_empty() {
            request = request.header("Mcp-Session-Id", session);
        }
        app.oneshot(
            request
                .body(axum::body::Body::from(message.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    }
    async fn call(&self, pending: bool) -> String {
        let mut params = json!({"name":"observed","arguments":{"pending":pending}});
        match self {
            Self::Stdio(peer) => format!(
                "{:?}",
                peer.request("tools/call", params, std::time::Duration::from_secs(2))
                    .await
            ),
            Self::Http(app, session, progress) => {
                if *progress {
                    params["_meta"] = json!({"progressToken":42});
                }
                let response = Self::http_post(
                    app.clone(),
                    session,
                    json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":params}),
                )
                .await;
                assert_eq!(response.status(), axum::http::StatusCode::OK);
                String::from_utf8(
                    axum::body::to_bytes(response.into_body(), 8192)
                        .await
                        .unwrap()
                        .to_vec(),
                )
                .unwrap()
            }
        }
    }
    async fn cancel(&self) {
        let params = json!({"requestId":1});
        match self {
            Self::Stdio(peer) => peer
                .notify("notifications/cancelled", params)
                .await
                .unwrap(),
            Self::Http(app, session, _) => {
                let response = Self::http_post(
                    app.clone(),
                    session,
                    json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":params}),
                )
                .await;
                assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
            }
        }
    }
}

#[tokio::test]
async fn same_request_ids_cancel_only_their_own_stdio_or_http_client() {
    // Causes: C1 two clients share a tool service/executor; C2 both request id=1
    // are in flight; C3 cancel from the first; C4 cancel from the second;
    // C5 another normal invocation; T=stdio / HTTP JSON / HTTP SSE.
    // Rules I4 each T, C1+C2+C3 -> first cancelled, second still pending;
    // I5 C4 -> second cancelled through its own registry; I6 C5 -> tool still
    // executes with a fresh application call id. Cancellation and progress IDs
    // are connection-local, while the exported tool and gate retain one owner.
    for transport in ["stdio", "http", "sse"] {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
            let service = Arc::new(McpToolService::new("test", "1", Arc::new(StaticExports::new(vec![McpExportedTool::plain(
                ToolDescriptor::pinned("test", "observed", "observed", json!({"type":"object"})), Arc::new(ObservedTool(sent)),
            )]))));
            let app = awaken_protocol_mcp::router(service.clone(), awaken_protocol_mcp::McpHttpConfig::default());
            let mut servers = Vec::new();
            let mut clients = Vec::new();
            for _ in 0..2 {
                clients.push(Arc::new(if transport == "stdio" {
                    let (client, server) = tokio::io::duplex(8192);
                    let (reader, writer) = tokio::io::split(server);
                    servers.push(awaken_protocol_mcp::McpStdioServer::serve(service.clone(), reader, writer));
                    let (reader, writer) = tokio::io::split(client);
                    Client::Stdio(Arc::new(awaken_mcp_wire::jsonrpc::JsonRpcPeer::new(reader, writer, None).0))
                } else {
                    let response = Client::http_post(app.clone(), "", json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await;
                    Client::Http(app.clone(), response.headers()["Mcp-Session-Id"].to_str().unwrap().into(), transport == "sse")
                }));
            }
            let mut tasks = Vec::new();
            let mut ids = Vec::new();
            for client in &clients {
                let client = client.clone();
                tasks.push(tokio::spawn(async move { client.call(true).await }));
                ids.push(received.recv().await.unwrap().call_id);
            }
            clients[0].cancel().await;
            let first = tasks.remove(0).await.unwrap();
            assert!(first.contains("cancelled"), "I4 {transport}: {first}");
            assert!(!tasks[0].is_finished(), "I4 {transport}: other client was cancelled");
            clients[1].cancel().await;
            assert!(tasks.remove(0).await.unwrap().contains("cancelled"), "I5 {transport}");
            assert!(clients[1].call(false).await.contains("completed"), "I6 {transport}");
            ids.push(received.recv().await.unwrap().call_id);
            assert_eq!(ids.iter().collect::<std::collections::HashSet<_>>().len(), 3);
            for server in servers { server.close(); }
        }).await.unwrap_or_else(|_| panic!("connection-scoped cancellation timed out: {transport}"));
    }
}
