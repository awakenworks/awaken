//! Stdio MCP server.
//!
//! Serves a [`McpToolService`] over a byte-stream pair through the shared
//! [`JsonRpcPeer`] — the same symmetric demux the client transport uses, run
//! from the other end: incoming client requests dispatch (each on its own
//! task) into the service, and `notifications/progress` /
//! `notifications/tools/list_changed` ride back on the peer's write queue.
//!
//! [`McpStdioServer::serve`] is generic over the streams so tests drive it
//! with an in-memory duplex; [`McpStdioServer::serve_stdio`] binds the real
//! process stdin/stdout for a subprocess-launched server.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use awaken_mcp_wire::jsonrpc::{
    JsonRpcNotifier, JsonRpcPeer, ServerRequestError, ServerRequestHandler,
};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::service::{McpToolService, NotifySink};

/// Delivers notifications through the peer's write queue. The peer is built
/// around the request handler, so the notifier lands here one step later —
/// before it does, there is no connection to notify.
struct PeerSink {
    notifier: Arc<OnceLock<JsonRpcNotifier>>,
}

#[async_trait]
impl NotifySink for PeerSink {
    async fn notify(&self, method: &str, params: Value) {
        if let Some(notifier) = self.notifier.get() {
            let _ = notifier.notify(method, params).await;
        }
    }
}

/// Adapts the service to the peer's request seam.
struct ServiceHandler {
    service: Arc<McpToolService>,
    sink: Arc<PeerSink>,
}

#[async_trait]
impl ServerRequestHandler for ServiceHandler {
    async fn handle(&self, method: &str, params: Value) -> Result<Value, ServerRequestError> {
        self.service
            .handle(
                method,
                params,
                Arc::clone(&self.sink) as Arc<dyn NotifySink>,
            )
            .await
    }
}

/// One served stdio connection. Dropping it stops nothing by itself — the
/// peer's tasks run until the streams close; await [`closed`](Self::closed)
/// to exit when the client disconnects.
pub struct McpStdioServer {
    peer: Arc<JsonRpcPeer>,
}

impl McpStdioServer {
    /// Serve `service` over an arbitrary byte-stream pair (tests use an
    /// in-memory duplex).
    pub fn serve<R, W>(service: Arc<McpToolService>, reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let notifier_cell = Arc::new(OnceLock::new());
        let handler = Arc::new(ServiceHandler {
            service: Arc::clone(&service),
            sink: Arc::new(PeerSink {
                notifier: Arc::clone(&notifier_cell),
            }),
        });
        let (peer, mut notifications) = JsonRpcPeer::new(reader, writer, Some(handler));
        notifier_cell
            .set(peer.notifier())
            .unwrap_or_else(|_| unreachable!("notifier cell is set exactly once"));
        let peer = Arc::new(peer);

        // Client notifications (initialized, cancelled) forward to the service.
        let notified = Arc::clone(&service);
        tokio::spawn(async move {
            while let Some(notification) = notifications.recv().await {
                notified.handle_notification(&notification.method, &notification.params);
            }
        });

        // Export-set changes owe the client a tools/list_changed.
        if let Some(mut changes) = service.source().changes() {
            let notifier = peer.notifier();
            tokio::spawn(async move {
                while changes.changed().await.is_ok() {
                    let _ = notifier
                        .notify("notifications/tools/list_changed", json!({}))
                        .await;
                }
            });
        }

        Self { peer }
    }

    /// Serve over the process's own stdin/stdout — the entry point for a
    /// subprocess-launched MCP server binary.
    pub fn serve_stdio(service: Arc<McpToolService>) -> Self {
        Self::serve(service, tokio::io::stdin(), tokio::io::stdout())
    }

    /// Whether the connection is still open.
    pub fn is_alive(&self) -> bool {
        self.peer.is_alive()
    }

    /// Resolve when the client disconnects (either stream half closes).
    pub async fn closed(&self) {
        self.peer.closed().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{McpExportedTool, SharedExports, StaticExports};
    use awaken_runtime_contract::resolved::ToolDescriptor;
    use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
    use std::time::Duration;

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

    fn exported_echo() -> McpExportedTool {
        McpExportedTool::plain(
            ToolDescriptor::pinned("test", "echo", "echoes", json!({ "type": "object" })),
            Arc::new(EchoTool),
        )
    }

    /// Serve over a duplex and return the *client* peer driving it.
    fn served(
        service: Arc<McpToolService>,
    ) -> (
        McpStdioServer,
        JsonRpcPeer,
        tokio::sync::mpsc::Receiver<awaken_mcp_wire::jsonrpc::ServerNotification>,
    ) {
        let (client_side, server_side) = tokio::io::duplex(8192);
        let (server_r, server_w) = tokio::io::split(server_side);
        let server = McpStdioServer::serve(service, server_r, server_w);
        let (client_r, client_w) = tokio::io::split(client_side);
        let (client, notifications) = JsonRpcPeer::new(client_r, client_w, None);
        (server, client, notifications)
    }

    #[tokio::test]
    async fn initialize_list_call_round_trip() {
        let source = StaticExports::new(vec![exported_echo()]);
        let service = Arc::new(McpToolService::new("stdio-test", "0.0.0", Arc::new(source)));
        let (_server, client, _notifications) = served(service);
        let timeout = Duration::from_secs(5);

        let init = client
            .request(
                "initialize",
                json!({ "protocolVersion": "2025-06-18" }),
                timeout,
            )
            .await
            .expect("initialize");
        assert_eq!(init["serverInfo"]["name"], "stdio-test");

        let tools = client
            .request("tools/list", json!({}), timeout)
            .await
            .expect("tools/list");
        assert_eq!(tools["tools"][0]["name"], "echo");

        let result = client
            .request(
                "tools/call",
                json!({ "name": "echo", "arguments": { "message": "hi" } }),
                timeout,
            )
            .await
            .expect("tools/call");
        assert_eq!(result["content"][0]["text"], "echo: hi");
    }

    #[tokio::test]
    async fn a_suspend_gate_fails_closed_over_stdio() {
        use awaken_agent_contract::agent::state::Store;
        use awaken_runtime_contract::permission::{GateOutcome, PermissionContext, ToolGateHook};

        struct SuspendGate;
        #[async_trait]
        impl ToolGateHook for SuspendGate {
            async fn gate(&self, _ctx: &PermissionContext, _state: &Store) -> GateOutcome {
                GateOutcome::Suspend {
                    ticket_id: "t".into(),
                }
            }
        }

        let source = StaticExports::new(vec![exported_echo()]);
        let service = Arc::new(
            McpToolService::new("stdio-test", "0.0.0", Arc::new(source))
                .with_gate(Arc::new(SuspendGate)),
        );
        let (_server, client, _notifications) = served(service);
        let timeout = Duration::from_secs(5);
        client
            .request(
                "initialize",
                json!({ "protocolVersion": "2025-06-18" }),
                timeout,
            )
            .await
            .expect("initialize");
        let result = client
            .request("tools/call", json!({ "name": "echo" }), timeout)
            .await
            .expect("tools/call returns a result, not a transport error");
        assert_eq!(result["isError"], true);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("out-of-band approval")
        );
    }

    #[tokio::test]
    async fn export_set_change_emits_tools_list_changed() {
        let source = Arc::new(SharedExports::new(vec![exported_echo()]));
        let service = Arc::new(McpToolService::new(
            "stdio-test",
            "0.0.0",
            Arc::clone(&source) as Arc<dyn crate::export::ToolExportSource>,
        ));
        let (_server, _client, mut notifications) = served(service);

        source.replace(vec![]);

        let seen = tokio::time::timeout(Duration::from_secs(5), notifications.recv())
            .await
            .expect("a notification arrives")
            .expect("channel open");
        assert_eq!(seen.method, "notifications/tools/list_changed");
    }

    #[tokio::test]
    async fn closed_resolves_when_the_client_disconnects() {
        let source = StaticExports::new(vec![exported_echo()]);
        let service = Arc::new(McpToolService::new("stdio-test", "0.0.0", Arc::new(source)));
        // Hold the client end raw (no peer): dropping it closes the stream.
        let (client_side, server_side) = tokio::io::duplex(8192);
        let (server_r, server_w) = tokio::io::split(server_side);
        let server = McpStdioServer::serve(service, server_r, server_w);
        assert!(server.is_alive());
        drop(client_side);
        tokio::time::timeout(Duration::from_secs(5), server.closed())
            .await
            .expect("server observes the disconnect");
        assert!(!server.is_alive());
    }
}
