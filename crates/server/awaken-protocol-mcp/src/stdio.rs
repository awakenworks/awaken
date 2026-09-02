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

use std::sync::Arc;

use async_trait::async_trait;
use awaken_mcp_server_core::NotifySink;
use awaken_mcp_wire::jsonrpc::{
    JsonRpcNotifier, JsonRpcPeer, ServerRequestError, ServerRequestHandler,
};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::service::McpToolService;

/// Delivers notifications through the peer's write queue. The peer is built
/// around the request handler, so the notifier lands here one step later —
/// callers arriving before that publication wait at this one startup handshake
/// instead of silently losing their notification.
struct PeerSink {
    notifier: watch::Receiver<Option<JsonRpcNotifier>>,
}

impl PeerSink {
    async fn published_notifier(&self) -> JsonRpcNotifier {
        let mut notifier = self.notifier.clone();
        loop {
            if let Some(notifier) = notifier.borrow().clone() {
                return notifier;
            }
            notifier
                .changed()
                .await
                .expect("stdio peer publishes its notifier during construction");
        }
    }
}

#[async_trait]
impl NotifySink for PeerSink {
    async fn notify(&self, method: &str, params: Value) {
        let notifier = self.published_notifier().await;
        let _ = notifier.notify(method, params).await;
    }
}

/// Adapts the service to the peer's request seam.
struct ServiceHandler {
    service: Arc<McpToolService>,
    sink: Arc<PeerSink>,
}

#[async_trait]
impl ServerRequestHandler for ServiceHandler {
    async fn handle(
        &self,
        id: &Value,
        method: &str,
        params: Value,
    ) -> Result<Value, ServerRequestError> {
        self.service
            .handle_request(id, method, params, self.sink.as_ref())
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
        let (notifier_publisher, notifier) = watch::channel(None);
        let sink = Arc::new(PeerSink { notifier });
        let handler = Arc::new(ServiceHandler {
            service: Arc::clone(&service),
            sink: Arc::clone(&sink),
        });
        let (peer, mut notifications) = JsonRpcPeer::new(reader, writer, Some(handler));
        notifier_publisher.send_replace(Some(peer.notifier()));
        let peer = Arc::new(peer);

        // Client notifications (initialized, cancelled) forward to the service.
        let notified = Arc::clone(&service);
        tokio::spawn(async move {
            while let Some(notification) = notifications.recv().await {
                notified
                    .handle_notification(&notification.method, &notification.params)
                    .await;
            }
        });

        // Export-set changes owe the client a tools/list_changed.
        if let Some(mut changes) = service.source().changes() {
            tokio::spawn(async move {
                while changes.changed().await.is_ok() {
                    awaken_mcp_server_core::notify_tools_list_changed(sink.as_ref()).await;
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
    use awaken_runtime_contract::permission::ToolCall;
    use awaken_runtime_contract::resolved::ToolDescriptor;
    use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
    use std::future::Future;
    use std::task::Poll;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};

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
    async fn peer_sink_waits_for_startup_publication_and_preserves_exact_notifications() {
        // Causes: C1 a notification starts before the peer notifier is
        // published; C2 the unique peer notifier is then published; C3 a
        // later notification starts after publication. Effects: E1 C1 remains
        // pending instead of returning and dropping the message; E2 C2 resumes
        // C1 and writes its exact method/params once; E3 C3 writes its exact
        // method/params through the same notifier without another handshake.
        // The peer/write channel remains open in every rule.
        //
        // | Rule | published at notify | publication occurs | Effect |
        // |---|---|---|---|
        // | N1 | no | no | E1 |
        // | N2 | no | yes | E2 |
        // | N3 | yes | already | E3 |
        let (notifier_publisher, notifier) = watch::channel(None);
        let sink = PeerSink { notifier };
        let (client_side, server_side) = tokio::io::duplex(8192);
        let (server_reader, server_writer) = tokio::io::split(server_side);
        let (peer, _notifications) = JsonRpcPeer::new(server_reader, server_writer, None);
        let (client_reader, _client_writer) = tokio::io::split(client_side);
        let mut lines = BufReader::new(client_reader).lines();

        let mut startup_notification = Box::pin(sink.notify(
            "notifications/progress",
            json!({ "progressToken": "startup", "progress": 1 }),
        ));
        let pending_before_publication = std::future::poll_fn(|context| {
            Poll::Ready(matches!(
                startup_notification.as_mut().poll(context),
                Poll::Pending
            ))
        })
        .await;
        assert!(
            pending_before_publication,
            "N1/E1 an unpublished notifier must wait, not silently return"
        );

        notifier_publisher.send_replace(Some(peer.notifier()));
        startup_notification.await;
        let startup_wire = lines
            .next_line()
            .await
            .expect("N2 reads the wire")
            .expect("N2 peer remains open");
        assert_eq!(
            serde_json::from_str::<Value>(&startup_wire).expect("N2 valid JSON"),
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": { "progressToken": "startup", "progress": 1 },
            }),
            "N2/E2 the startup notification is preserved exactly"
        );

        sink.notify("notifications/tools/list_changed", json!({ "revision": 2 }))
            .await;
        let published_wire = lines
            .next_line()
            .await
            .expect("N3 reads the wire")
            .expect("N3 peer remains open");
        assert_eq!(
            serde_json::from_str::<Value>(&published_wire).expect("N3 valid JSON"),
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed",
                "params": { "revision": 2 },
            }),
            "N3/E3 a published notifier preserves the exact notification"
        );
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
        use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};

        struct SuspendGate;
        #[async_trait]
        impl ToolGateHook for SuspendGate {
            async fn gate(&self, _ctx: &ToolCall, _state: &Store) -> GateOutcome {
                GateOutcome::RequireConfirmation {
                    correlation_id: "t".into(),
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
                .contains("no authenticated client-request channel")
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
