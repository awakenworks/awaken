//! Production MCP server assembly owned by the data plane.
//!
//! Export is opt-in: without a dedicated bearer token no route is mounted. The
//! process startup supplies an explicit neutral descriptor/executable set, so
//! nothing from the runtime registry leaks implicitly.

use std::sync::Arc;

use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::RawTool;
use axum::Router;

/// Product adapter for Session-owned tools handed to an ACP CLI. The neutral
/// Host decides the export lifecycle; this data-plane adapter owns MCP wire
/// assembly and returns an opaque shutdown lease.
pub struct SessionToolExporter;

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
impl awaken_runtime_host::AcpToolExporter for SessionToolExporter {
    async fn export_set(
        &self,
        server_name: &str,
        descriptors: Vec<ToolDescriptor>,
        tools: Vec<Arc<dyn RawTool>>,
    ) -> Result<awaken_runtime_host::AcpToolExport, String> {
        let exports = awaken_protocol_mcp::McpExportedTool::try_plain_set(descriptors, tools)
            .map_err(|error| error.to_string())?;
        let source = Arc::new(awaken_protocol_mcp::StaticExports::new(exports));
        let service = Arc::new(awaken_protocol_mcp::McpToolService::new(
            server_name,
            env!("CARGO_PKG_VERSION"),
            source,
        ));
        let app =
            awaken_protocol_mcp::router(service, awaken_protocol_mcp::McpHttpConfig::default());
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
        Ok(awaken_runtime_host::AcpToolExport::new(
            awaken_run_executor_acp::McpServerConfig {
                name: server_name.into(),
                transport: awaken_run_executor_acp::McpTransport::Http {
                    url: format!("http://{address}/mcp"),
                },
            },
            SessionToolExportLease {
                shutdown: Some(shutdown),
            },
        ))
    }
}

/// Mount an explicit tool set at `/v1/mcp` when `bearer_token` is configured.
pub fn router(
    descriptors: Vec<ToolDescriptor>,
    executables: Vec<Arc<dyn RawTool>>,
    bearer_token: Option<String>,
) -> Result<Router, String> {
    let Some(bearer_token) = bearer_token.filter(|token| !token.is_empty()) else {
        return Ok(Router::new());
    };
    let exports = awaken_protocol_mcp::McpExportedTool::try_plain_set(descriptors, executables)
        .map_err(|error| error.to_string())?;
    let source = Arc::new(awaken_protocol_mcp::StaticExports::new(exports));
    let service = Arc::new(awaken_protocol_mcp::McpToolService::new(
        "awaken",
        env!("CARGO_PKG_VERSION"),
        source,
    ));
    Ok(awaken_protocol_mcp::router(
        service,
        awaken_protocol_mcp::McpHttpConfig {
            path: "/v1/mcp".to_string(),
            bearer_token: Some(bearer_token),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_ext_mcp::{Credential, HttpTransport, McpToolTransport};
    use awaken_runtime_contract::tool::{ToolCall, ToolOutput};
    use awaken_runtime_host::AcpToolExporter;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn initialize(authorized: bool) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if authorized {
            request = request.header("authorization", "Bearer test-token");
        }
        request
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn endpoint_is_absent_until_a_bearer_is_configured() {
        let response = router(Vec::new(), Vec::new(), None)
            .expect("empty disabled export is valid")
            .oneshot(initialize(false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn configured_endpoint_requires_its_bearer_and_initializes() {
        let app = router(Vec::new(), Vec::new(), Some("test-token".into()))
            .expect("empty enabled export is valid");
        let denied = app.clone().oneshot(initialize(false)).await.unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        let accepted = app.oneshot(initialize(true)).await.unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        assert!(accepted.headers().contains_key("mcp-session-id"));
    }

    struct EchoTool(&'static str);

    #[async_trait::async_trait]
    impl RawTool for EchoTool {
        fn id(&self) -> &str {
            self.0
        }

        async fn invoke(
            &self,
            call: ToolCall,
        ) -> Result<ToolOutput, awaken_runtime_contract::tool::ToolError> {
            Ok(ToolOutput::ok(
                call.call_id,
                format!("same-tool:{}:{}", self.0, call.arguments),
            ))
        }
    }

    #[tokio::test]
    async fn session_export_discovers_calls_and_reclaims_exact_session_tool_set() {
        // Cause/effect decision table: A1 the exact Web + Skill + four-Memory
        // descriptor/executor set enters the neutral Host port -> one MCP endpoint
        // lists every id exactly once; A2 each listed family is called -> the
        // matching RawTool executes; A3 the sole export lease is dropped -> the
        // endpoint stops accepting connections. Constraint: transport may change
        // Native delivery into ACP MCP, but cannot split or rewrite the set.
        let ids = [
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
        let awaken_run_executor_acp::McpTransport::Http { url } = &export.server.transport else {
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
