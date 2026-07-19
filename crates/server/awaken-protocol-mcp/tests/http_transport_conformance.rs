//! Shared Streamable HTTP conformance driven through the real axum router.
//!
//! This complements method-level conformance: status codes, content negotiation,
//! JSON-RPC envelope validation, negotiated-version headers, notification 202s,
//! standing GET SSE, and ordered progress SSE all cross the concrete transport.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_mcp_server_core::{McpHttpBody, McpHttpMethod, McpHttpReply};
use awaken_mcp_server_testkit::{McpHttpConformanceDriver, assert_mcp_http_transport_conformance};
use awaken_mcp_wire::progress::McpProgressUpdate;
use awaken_protocol_mcp::export::ProgressRawTool;
use awaken_protocol_mcp::{McpExportedTool, McpHttpConfig, McpToolService, StaticExports, router};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{ToolCall, ToolError, ToolOutput};
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tower::ServiceExt;

struct ProgressTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ProgressRawTool for ProgressTool {
    fn id(&self) -> &str {
        "progress"
    }

    async fn invoke_with_progress(
        &self,
        call: ToolCall,
        progress: mpsc::Sender<McpProgressUpdate>,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let steps = call.arguments["steps"].as_u64().unwrap_or(1);
        for step in 1..=steps {
            progress
                .send(McpProgressUpdate {
                    progress: step as f64,
                    total: Some(steps as f64),
                    message: None,
                })
                .await
                .expect("HTTP progress receiver");
        }
        Ok(ToolOutput::ok(call.call_id, format!("counted {steps}")))
    }
}

struct RouterDriver {
    app: axum::Router,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl McpHttpConformanceDriver for RouterDriver {
    async fn exchange(
        &self,
        method: McpHttpMethod,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> McpHttpReply {
        let method = match method {
            McpHttpMethod::Get => Method::GET,
            McpHttpMethod::Post => Method::POST,
            McpHttpMethod::Delete => Method::DELETE,
            McpHttpMethod::Other => Method::PUT,
        };
        let is_get = method == Method::GET;
        let mut request = Request::builder().method(method).uri("/mcp");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body.to_vec())).unwrap())
            .await
            .expect("MCP router response");
        let status = response.status();
        let response_headers = response.headers().clone();

        // A standing GET body is intentionally not collected: doing so would
        // wait forever. Its concrete SSE content type proves the router realized
        // the kernel's EventStream decision instead of returning an empty 200.
        let response_body = if response_headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"))
            && status.is_success()
            && body.is_empty()
            && is_get
        {
            McpHttpBody::EventStream
        } else {
            let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
                .await
                .expect("bounded MCP response body");
            decode_body(&response_headers, &bytes)
        };
        McpHttpReply {
            status,
            headers: response_headers,
            body: response_body,
        }
    }

    fn host_call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn decode_body(headers: &axum::http::HeaderMap, bytes: &[u8]) -> McpHttpBody {
    if bytes.is_empty() {
        return McpHttpBody::Empty;
    }
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        return McpHttpBody::Json(serde_json::from_slice(bytes).expect("JSON response"));
    }
    if content_type.starts_with("text/event-stream") {
        let text = std::str::from_utf8(bytes).expect("UTF-8 SSE response");
        let events = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str::<Value>(data).expect("JSON SSE data"))
            .collect();
        return McpHttpBody::Sse(events);
    }
    McpHttpBody::Text(String::from_utf8_lossy(bytes).into_owned())
}

#[tokio::test]
async fn awaken_router_passes_shared_http_transport_conformance() {
    let calls = Arc::new(AtomicUsize::new(0));
    let descriptor = ToolDescriptor::pinned(
        "http-conformance",
        "progress",
        "reports progress",
        json!({ "type": "object" }),
    );
    let source = StaticExports::new(vec![McpExportedTool::with_progress(
        descriptor,
        Arc::new(ProgressTool {
            calls: calls.clone(),
        }),
    )]);
    let app = router(
        Arc::new(McpToolService::new(
            "http-conformance",
            "1",
            Arc::new(source),
        )),
        McpHttpConfig::default(),
    );
    let driver = RouterDriver { app, calls };
    assert_mcp_http_transport_conformance(&driver).await;
}
