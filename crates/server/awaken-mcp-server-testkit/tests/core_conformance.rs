use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_mcp_server_core::{
    AllowAllOrigins, CallToolResult, McpCall, McpHostError, McpHttpMethod, McpHttpReply, McpServer,
    McpToolDefinition, McpToolHost, NotifySink, NullSink, ToolContent, handle_streamable_http,
};
use awaken_mcp_server_testkit::{
    McpConformanceDriver, McpHttpConformanceDriver, assert_mcp_http_transport_conformance,
    assert_mcp_server_conformance,
};
use awaken_mcp_wire::jsonrpc::ServerRequestError;
use serde_json::{Value, json};

struct Host {
    calls: AtomicUsize,
}

#[async_trait]
impl McpToolHost<()> for Host {
    async fn list_tools(&self, _context: &()) -> Result<Vec<McpToolDefinition>, McpHostError> {
        Ok(vec![
            McpToolDefinition::new("echo").with_schema(json!({ "type": "object" })),
            McpToolDefinition::new("progress").with_schema(json!({ "type": "object" })),
        ])
    }

    async fn call_tool(
        &self,
        _context: &(),
        call: McpCall,
        notifications: &dyn NotifySink,
    ) -> Result<CallToolResult, McpHostError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = match call.name.as_str() {
            "echo" => format!(
                "echo: {}",
                call.arguments["message"].as_str().unwrap_or_default()
            ),
            "progress" => {
                let steps = call.arguments["steps"].as_u64().unwrap_or(1);
                let token = call
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("progressToken"))
                    .cloned()
                    .unwrap_or(Value::Null);
                for progress in 1..=steps {
                    notifications
                        .notify(
                            "notifications/progress",
                            json!({ "progressToken": token, "progress": progress }),
                        )
                        .await;
                }
                format!("counted {steps}")
            }
            name => return Err(McpHostError::NotFound(name.into())),
        };
        Ok(CallToolResult {
            content: vec![ToolContent::Text {
                text,
                annotations: None,
                meta: None,
            }],
            structured_content: None,
            is_error: Some(false),
        })
    }
}

struct Driver {
    server: McpServer<Host, ()>,
}

#[async_trait]
impl McpConformanceDriver for Driver {
    async fn request(
        &self,
        method: &str,
        params: Value,
        sink: &dyn NotifySink,
    ) -> Result<Value, ServerRequestError> {
        self.server.handle(&(), method, params, sink).await
    }

    fn host_call_count(&self) -> usize {
        self.server.host().calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl McpHttpConformanceDriver for Driver {
    async fn exchange(
        &self,
        method: McpHttpMethod,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> McpHttpReply {
        let mut map = http::HeaderMap::new();
        for (name, value) in headers {
            map.append(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        handle_streamable_http(
            &self.server,
            &(),
            method,
            &map,
            body,
            &AllowAllOrigins,
            &NullSink,
        )
        .await
    }

    fn host_call_count(&self) -> usize {
        self.server.host().calls.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn neutral_core_passes_the_shared_conformance_suite() {
    let driver = Driver {
        server: McpServer::new(
            "conformance",
            "1",
            Host {
                calls: AtomicUsize::new(0),
            },
        ),
    };
    assert_mcp_server_conformance(&driver).await;
}

#[tokio::test]
async fn neutral_core_passes_the_shared_http_transport_suite() {
    let driver = Driver {
        server: McpServer::new(
            "http-conformance",
            "1",
            Host {
                calls: AtomicUsize::new(0),
            },
        ),
    };
    assert_mcp_http_transport_conformance(&driver).await;
}
