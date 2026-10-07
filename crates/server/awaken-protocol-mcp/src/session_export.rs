// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: The MCP adapter assembles the Host-selected descriptor/tool set into the existing tool
// @formal reason: service. A scoped export lease revokes channel peers on shutdown; no permissions or tool
// @formal reason: state are reconstructed in the transport.
//! MCP assembly for the existing Session tool export port.

use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tool_contract::tool::RawTool;
use std::sync::Arc;

/// Product adapter for Session-owned tools handed to an ACP CLI. The neutral
/// Host decides the export lifecycle; this data-plane adapter owns MCP wire
/// assembly and returns an opaque shutdown lease.
pub struct SessionToolExporter;

fn tool_service(
    server_name: &str,
    descriptors: Vec<ToolDescriptor>,
    tools: Vec<Arc<dyn RawTool>>,
) -> Result<Arc<crate::McpToolService>, String> {
    let exports = crate::McpExportedTool::try_plain_set(descriptors, tools)
        .map_err(|error| error.to_string())?;
    Ok(Arc::new(crate::McpToolService::new(
        server_name,
        env!("CARGO_PKG_VERSION"),
        Arc::new(crate::StaticExports::new(exports)),
    )))
}

struct SessionToolExportLease {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for SessionToolExportLease {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_environment_contract::tool_export::AttachedChannelToolExporter
    for SessionToolExporter
{
    async fn export_channel_set(
        &self,
        server: awaken_runtime_contract::resolved::AcpMcpServer,
        descriptors: Vec<ToolDescriptor>,
        tools: Vec<Arc<dyn RawTool>>,
        reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    ) -> Result<awaken_session_environment_contract::tool_export::AcpToolExport, String> {
        if !matches!(
            server.transport,
            awaken_runtime_contract::resolved::AcpMcpTransport::Stdio { .. }
        ) {
            return Err("attached ACP tool export requires a stdio projection".into());
        }
        let service = tool_service(&server.name, descriptors, tools)?;
        let mut channel = awaken_connection_plan::MultiplexedChannel::server(
            awaken_agent_channel::SplitChannel::new(reader, writer),
        );
        let (shutdown, mut stopped) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut peers: Vec<crate::McpStdioServer> = Vec::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    incoming = channel.accept() => {
                        let Some(Ok(stream)) = incoming else { break };
                        peers.retain(crate::McpStdioServer::is_alive);
                        let (reader, writer) = tokio::io::split(stream);
                        peers.push(crate::McpStdioServer::serve(service.clone(), reader, writer));
                    }
                }
            }
            for peer in peers {
                peer.close();
            }
            // Dropping the one physical owner revokes its streams as well.
        });
        Ok(
            awaken_session_environment_contract::tool_export::AcpToolExport::new(
                server,
                SessionToolExportLease {
                    shutdown: Some(shutdown),
                },
            ),
        )
    }
}

#[async_trait::async_trait]
impl awaken_session_environment_contract::tool_export::AcpToolExporter for SessionToolExporter {
    fn attached_channels(
        &self,
    ) -> Option<&dyn awaken_session_environment_contract::tool_export::AttachedChannelToolExporter>
    {
        Some(self)
    }

    async fn export_set(
        &self,
        server_name: &str,
        descriptors: Vec<ToolDescriptor>,
        tools: Vec<Arc<dyn RawTool>>,
    ) -> Result<awaken_session_environment_contract::tool_export::AcpToolExport, String> {
        let service = tool_service(server_name, descriptors, tools)?;
        let app = crate::router(service, crate::McpHttpConfig::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| format!("bind ACP tool export: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read ACP tool export address: {error}"))?;
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stopped.await;
                })
                .await;
        });
        Ok(
            awaken_session_environment_contract::tool_export::AcpToolExport::new(
                awaken_runtime_contract::resolved::AcpMcpServer {
                    name: server_name.into(),
                    transport: awaken_runtime_contract::resolved::AcpMcpTransport::Http {
                        url: format!("http://{address}/mcp"),
                    },
                },
                SessionToolExportLease {
                    shutdown: Some(shutdown),
                },
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_ext_mcp::{Credential, HttpTransport, McpToolTransport};
    use awaken_session_environment_contract::tool_export::{
        AcpToolExporter, AttachedChannelToolExporter,
    };
    use awaken_tool_contract::tool::{ToolCall, ToolOutput};

    struct EchoTool(&'static str);

    #[async_trait::async_trait]
    impl RawTool for EchoTool {
        fn id(&self) -> &str {
            self.0
        }

        async fn invoke(
            &self,
            call: ToolCall,
        ) -> Result<ToolOutput, awaken_tool_contract::tool::ToolError> {
            Ok(ToolOutput::ok(
                call.call_id,
                format!("same-tool:{}:{}", self.0, call.arguments),
            ))
        }
    }

    #[tokio::test]
    async fn attached_export_isolates_clients_and_revokes_every_peer_on_lease_drop() {
        // Causes: C1 valid descriptor/executor + stdio; C2 concurrent clients
        // with independent counters starting at id=1; C3 one client closes;
        // C4 later client; C5 export lease ends; C6 invalid transport/tool set.
        // Rules S1 C1+C2 -> same catalog, distinct arguments/results per peer;
        // S2 C3+C4 -> remaining and later peers work; S3 C5 -> all peers close,
        // no new stream admission; S4 C6 -> reject before serving. The shared
        // service owns tool identity; the transport never rewrites request IDs.
        use awaken_mcp_wire::jsonrpc::JsonRpcPeer;
        use awaken_runtime_contract::resolved::{AcpMcpServer, AcpMcpTransport};
        use std::time::Duration;
        let projection = AcpMcpServer {
            name: "awaken_session".into(),
            transport: AcpMcpTransport::Stdio {
                command: "/helper".into(),
                args: vec!["channel-connect".into(), "--unix".into(), "/socket".into()],
            },
        };
        let descriptor =
            ToolDescriptor::pinned("test", "read", "read", serde_json::json!({"type":"object"}));
        let (client, server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(server);
        let export = SessionToolExporter
            .export_channel_set(
                projection.clone(),
                vec![descriptor.clone()],
                vec![Arc::new(EchoTool("read"))],
                Box::new(reader),
                Box::new(writer),
            )
            .await
            .unwrap();
        assert_eq!(export.server, projection);
        let channel = awaken_connection_plan::MultiplexedChannel::client(client);
        let timeout = Duration::from_secs(2);
        let mut peers = Vec::new();
        for _ in 0..2 {
            let (reader, writer) = tokio::io::split(channel.open().await.unwrap());
            let (peer, _notifications) = JsonRpcPeer::new(reader, writer, None);
            peers.push(peer);
        }
        let exercise = async |peer: &JsonRpcPeer, marker: &'static str| {
            peer.request("initialize", serde_json::json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":marker,"version":"1"}}), timeout).await.unwrap();
            let listed = peer
                .request("tools/list", serde_json::json!({}), timeout)
                .await
                .unwrap();
            assert_eq!(listed["tools"].as_array().unwrap().len(), 1);
            assert_eq!(listed["tools"][0]["name"], "read");
            let called = peer
                .request(
                    "tools/call",
                    serde_json::json!({"name":"read","arguments":{"marker":marker}}),
                    timeout,
                )
                .await
                .unwrap();
            assert_eq!(
                called["content"][0]["text"],
                format!("same-tool:read:{{\"marker\":\"{marker}\"}}"),
                "S1 isolated result"
            );
        };
        tokio::join!(exercise(&peers[0], "first"), exercise(&peers[1], "second"));
        peers.remove(0).close();
        exercise(&peers[0], "survivor").await;
        let (reader, writer) = tokio::io::split(channel.open().await.unwrap());
        let (peer, _notifications) = JsonRpcPeer::new(reader, writer, None);
        exercise(&peer, "later").await;
        peers.push(peer);
        drop(export);
        for peer in &peers {
            tokio::time::timeout(timeout, peer.closed())
                .await
                .expect("S3 every live peer revoked");
            assert!(
                peer.request("tools/list", serde_json::json!({}), timeout)
                    .await
                    .is_err()
            );
        }
        tokio::time::timeout(timeout, async {
            loop {
                if channel.open().await.is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("S3 physical owner rejects new streams");
        for wrong_transport in [false, true] {
            let mut projection = projection.clone();
            if wrong_transport {
                projection.transport = AcpMcpTransport::Http {
                    url: "http://127.0.0.1:1/mcp".into(),
                };
            }
            let (channel, _client) = tokio::io::duplex(64);
            let (reader, writer) = tokio::io::split(channel);
            let error = SessionToolExporter
                .export_channel_set(
                    projection,
                    vec![descriptor.clone()],
                    vec![],
                    Box::new(reader),
                    Box::new(writer),
                )
                .await
                .err()
                .unwrap();
            assert!(!error.is_empty());
        }
    }

    #[tokio::test]
    async fn session_export_discovers_calls_and_reclaims_exact_session_tool_set() {
        // Cause/effect decision table: A1 the exact Hand + Web + Skill + four-Memory
        // descriptor/executor set enters the neutral Host port -> one MCP endpoint
        // lists every id exactly once; A2 each listed family is called -> the
        // matching RawTool executes; A3 the sole export lease is dropped -> the
        // endpoint stops accepting connections. Constraint: transport may change
        // Native delivery into ACP MCP, but cannot split or rewrite the set.
        let ids = [
            "read",
            "write",
            "edit",
            "bash",
            "web_search",
            "list_skills",
            "Skill",
            "list_memories",
            "read_memory",
            "write_memory",
            "delete_memory",
        ];
        let export = SessionToolExporter
            .export_set(
                "awaken_session",
                ids.iter()
                    .map(|id| {
                        ToolDescriptor::pinned(
                            "test",
                            *id,
                            *id,
                            serde_json::json!({ "type": "object" }),
                        )
                    })
                    .collect(),
                ids.iter()
                    .map(|id| Arc::new(EchoTool(id)) as Arc<dyn RawTool>)
                    .collect(),
            )
            .await
            .unwrap();
        let awaken_runtime_contract::resolved::AcpMcpTransport::Http { url } =
            &export.server.transport
        else {
            panic!("session export must use HTTP MCP");
        };
        let transport = HttpTransport::connect(url, Credential::None).await.unwrap();
        let tools = transport.list_tools().await.unwrap();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ids,
            "A1 exact ordered Session tool set"
        );
        for id in ids {
            let result = transport
                .call_tool(id, serde_json::json!({ "marker": id }))
                .await
                .unwrap();
            let serialized = serde_json::to_string(&result).unwrap();
            assert!(serialized.contains(&format!("same-tool:{id}")), "A2 {id}");
        }
        let authority = url
            .strip_prefix("http://")
            .and_then(|url| url.strip_suffix("/mcp"))
            .expect("test exporter returns an HTTP MCP URL")
            .to_string();
        drop(transport);
        drop(export);
        let mut stopped = false;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(&authority).await.is_err() {
                stopped = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(stopped, "A3 dropping the one lease stops the one endpoint");
    }
}
