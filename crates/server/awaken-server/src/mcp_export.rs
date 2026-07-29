//! Production MCP server assembly owned by the data plane.
//!
//! Export is opt-in: without a dedicated bearer token no route is mounted. The
//! composition root supplies an explicit neutral descriptor/executable set, so
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
    async fn export(
        &self,
        server_name: &str,
        descriptor: ToolDescriptor,
        tool: Arc<dyn RawTool>,
    ) -> Result<awaken_runtime_host::AcpToolExport, String> {
        let source = Arc::new(awaken_protocol_mcp::StaticExports::new(vec![
            awaken_protocol_mcp::McpExportedTool::plain(descriptor, tool),
        ]));
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
) -> Router {
    let Some(bearer_token) = bearer_token.filter(|token| !token.is_empty()) else {
        return Router::new();
    };
    let exports = descriptors
        .into_iter()
        .zip(executables)
        .map(|(descriptor, executable)| {
            awaken_protocol_mcp::McpExportedTool::plain(descriptor, executable)
        })
        .collect();
    let source = Arc::new(awaken_protocol_mcp::StaticExports::new(exports));
    let service = Arc::new(awaken_protocol_mcp::McpToolService::new(
        "awaken",
        env!("CARGO_PKG_VERSION"),
        source,
    ));
    awaken_protocol_mcp::router(
        service,
        awaken_protocol_mcp::McpHttpConfig {
            path: "/v1/mcp".to_string(),
            bearer_token: Some(bearer_token),
        },
    )
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
            .oneshot(initialize(false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn configured_endpoint_requires_its_bearer_and_initializes() {
        let app = router(Vec::new(), Vec::new(), Some("test-token".into()));
        let denied = app.clone().oneshot(initialize(false)).await.unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        let accepted = app.oneshot(initialize(true)).await.unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        assert!(accepted.headers().contains_key("mcp-session-id"));
    }

    struct EchoSearch;

    #[async_trait::async_trait]
    impl RawTool for EchoSearch {
        fn id(&self) -> &str {
            "web_search"
        }

        async fn invoke(
            &self,
            call: ToolCall,
        ) -> Result<ToolOutput, awaken_runtime_contract::tool::ToolError> {
            Ok(ToolOutput::ok(
                call.call_id,
                format!("same-tool:{}", call.arguments["query"]),
            ))
        }
    }

    #[tokio::test]
    async fn session_export_discovers_calls_and_reclaims_one_raw_tool() {
        // Cause/effect: one descriptor/executable enters the neutral Host port;
        // MCP discovery sees exactly it, tools/call reaches the same RawTool, and
        // dropping the returned export owns terminal server shutdown.
        let export = SessionToolExporter
            .export(
                "awaken_web_search",
                ToolDescriptor::pinned(
                    "test",
                    "web_search",
                    "search",
                    serde_json::json!({ "type": "object" }),
                ),
                Arc::new(EchoSearch),
            )
            .await
            .unwrap();
        let awaken_run_executor_acp::McpTransport::Http { url } = &export.server.transport else {
            panic!("session export must use HTTP MCP");
        };
        let transport = HttpTransport::connect(url, Credential::None).await.unwrap();
        let tools = transport.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "web_search");
        let result = transport
            .call_tool("web_search", serde_json::json!({ "query": "ddd" }))
            .await
            .unwrap();
        assert!(
            serde_json::to_string(&result)
                .unwrap()
                .contains("same-tool")
        );
        drop(export);
    }
}
