//! Production MCP server assembly owned by the data plane.
//!
//! Export is opt-in: without a dedicated bearer token no route is mounted. The
//! composition root supplies an explicit neutral descriptor/executable set, so
//! nothing from the runtime registry leaks implicitly.

use std::sync::Arc;

use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::RawTool;
use axum::Router;

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
}
