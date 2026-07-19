use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_mcp_server_core::{
    AllowAllOrigins, CallToolResult, McpCall, McpHostError, McpHttpBody, McpHttpMethod, McpServer,
    McpToolDefinition, McpToolHost, NotifySink, NullSink, ToolContent, handle_streamable_http,
    jsonrpc_reply,
};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use proptest::prelude::*;
use serde_json::{Value, json};

struct DenyOrigin;

impl awaken_mcp_server_core::OriginPolicy for DenyOrigin {
    fn allows(&self, origin: Option<&str>) -> bool {
        origin != Some("https://denied")
    }
}

#[derive(Default)]
struct Host {
    lists: AtomicUsize,
    calls: AtomicUsize,
    notifications: AtomicUsize,
    block: bool,
}

#[async_trait]
impl McpToolHost<()> for Host {
    async fn list_tools(&self, _context: &()) -> Result<Vec<McpToolDefinition>, McpHostError> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        Ok(vec![McpToolDefinition::new("echo")])
    }

    async fn call_tool(
        &self,
        _context: &(),
        call: McpCall,
        notifications: &dyn NotifySink,
    ) -> Result<CallToolResult, McpHostError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.block {
            std::future::pending::<()>().await;
        }
        if let Some(token) = call
            .meta
            .as_ref()
            .and_then(|meta| meta.get("progressToken"))
        {
            notifications
                .notify(
                    "notifications/progress",
                    json!({ "progressToken": token, "progress": 1 }),
                )
                .await;
        }
        Ok(CallToolResult {
            content: vec![ToolContent::Text {
                text: "ok".into(),
                annotations: None,
                meta: None,
            }],
            structured_content: None,
            is_error: Some(false),
        })
    }

    async fn notification(&self, _context: &(), _method: &str, _params: &Value) {
        self.notifications.fetch_add(1, Ordering::SeqCst);
    }
}

fn post_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers
}

#[tokio::test]
async fn notification_is_202_without_a_jsonrpc_response() {
    let server = McpServer::new("test", "1", Host::default());
    let reply = handle_streamable_http(
        &server,
        &(),
        McpHttpMethod::Post,
        &post_headers(),
        br#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
        &AllowAllOrigins,
        &NullSink,
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.body, McpHttpBody::Empty);
    assert_eq!(server.host().notifications.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalid_params_and_unsupported_header_never_call_the_host() {
    let server = McpServer::new("test", "1", Host::default());
    let invalid = handle_streamable_http(
        &server,
        &(),
        McpHttpMethod::Post,
        &post_headers(),
        br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#,
        &AllowAllOrigins,
        &NullSink,
    )
    .await;
    let McpHttpBody::Json(invalid) = invalid.body else {
        panic!("JSON response")
    };
    assert_eq!(invalid["error"]["code"], -32602);

    let mut headers = post_headers();
    headers.insert(
        "mcp-protocol-version",
        HeaderValue::from_static("1900-01-01"),
    );
    let unsupported = handle_streamable_http(
        &server,
        &(),
        McpHttpMethod::Post,
        &headers,
        br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo"}}"#,
        &AllowAllOrigins,
        &NullSink,
    )
    .await;
    assert_eq!(unsupported.status, StatusCode::BAD_REQUEST);
    assert!(matches!(unsupported.body, McpHttpBody::Text(_)));
    assert_eq!(server.host().calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn progress_sse_is_ordered_and_ends_with_one_final_response() {
    let server = McpServer::new("test", "1", Host::default());
    let reply = handle_streamable_http(
        &server,
        &(),
        McpHttpMethod::Post,
        &post_headers(),
        br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"echo","_meta":{"progressToken":"p"}}}"#,
        &AllowAllOrigins,
        &NullSink,
    )
    .await;
    let McpHttpBody::Sse(events) = reply.body else {
        panic!("SSE response")
    };
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["method"], "notifications/progress");
    assert_eq!(events[1]["id"], 7);
    assert!(events[1].get("result").is_some());
    assert!(events[1].get("method").is_none());
}

#[tokio::test]
async fn accept_origin_get_and_method_rules_are_explicit() {
    let server = McpServer::new("test", "1", Host::default());
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static("image/png"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let unacceptable = handle_streamable_http(
        &server,
        &(),
        McpHttpMethod::Post,
        &headers,
        b"{}",
        &AllowAllOrigins,
        &NullSink,
    )
    .await;
    assert_eq!(unacceptable.status, StatusCode::NOT_ACCEPTABLE);

    headers.insert(header::ORIGIN, HeaderValue::from_static("https://denied"));
    let denied = handle_streamable_http(
        &server,
        &(),
        McpHttpMethod::Post,
        &headers,
        b"{}",
        &DenyOrigin,
        &NullSink,
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);

    let get_without_sse = handle_streamable_http(
        &server,
        &(),
        McpHttpMethod::Get,
        &HeaderMap::new(),
        b"",
        &AllowAllOrigins,
        &NullSink,
    )
    .await;
    assert_eq!(get_without_sse.status, StatusCode::NOT_ACCEPTABLE);
}

#[tokio::test]
async fn get_with_sse_accept_declares_a_standing_event_stream() {
    let server = McpServer::new("test", "1", Host::default());
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    let reply = handle_streamable_http(
        &server,
        &(),
        McpHttpMethod::Get,
        &headers,
        b"",
        &AllowAllOrigins,
        &NullSink,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, McpHttpBody::EventStream);
    assert_eq!(
        reply.headers.get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
}

#[tokio::test]
async fn post_requires_json_content_type_and_both_response_media_types() {
    let server = McpServer::new("test", "1", Host::default());
    let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;

    let mut missing_type = HeaderMap::new();
    missing_type.insert(
        header::ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    let reply = handle_streamable_http(
        &server,
        &(),
        McpHttpMethod::Post,
        &missing_type,
        body,
        &AllowAllOrigins,
        &NullSink,
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    for accept in [
        "application/json",
        "text/event-stream",
        "*/*",
        "application/json, text/event-stream;q=0",
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_str(accept).unwrap());
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        let reply = handle_streamable_http(
            &server,
            &(),
            McpHttpMethod::Post,
            &headers,
            body,
            &AllowAllOrigins,
            &NullSink,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_ACCEPTABLE, "Accept={accept}");
    }
}

#[tokio::test]
async fn malformed_jsonrpc_and_initialize_versions_are_http_bad_requests() {
    let server = McpServer::new("test", "1", Host::default());
    for body in [
        br#"{"id":1,"method":"ping"}"#.as_slice(),
        br#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#.as_slice(),
        br#"{"jsonrpc":"2.0","id":{},"method":"ping"}"#.as_slice(),
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.as_slice(),
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1900-01-01"}}"#.as_slice(),
    ] {
        let reply = handle_streamable_http(
            &server,
            &(),
            McpHttpMethod::Post,
            &post_headers(),
            body,
            &AllowAllOrigins,
            &NullSink,
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "body={:?}", body);
    }
    assert_eq!(server.host().calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_drops_the_host_future_and_produces_no_late_result() {
    let server = Arc::new(McpServer::new(
        "test",
        "1",
        Host {
            block: true,
            ..Host::default()
        },
    ));
    let running = server.clone();
    let task = tokio::spawn(async move {
        let id = json!(42);
        running
            .handle_request(
                &(),
                Some(&id),
                "tools/call",
                json!({ "name": "echo" }),
                &NullSink,
            )
            .await
    });
    while server.host().calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    server
        .handle_notification(&(), "notifications/cancelled", &json!({ "requestId": 42 }))
        .await;
    let result = task.await.expect("request task").expect_err("cancelled");
    assert!(result.message.contains("cancelled"));
}

proptest! {
    #[test]
    fn one_request_has_exactly_one_final_envelope(id in any::<i64>(), succeeds in any::<bool>()) {
        let outcome = if succeeds {
            Ok(json!({ "ok": true }))
        } else {
            Err(awaken_mcp_wire::jsonrpc::ServerRequestError::internal("failed"))
        };
        let reply = jsonrpc_reply(json!(id), outcome);
        prop_assert_eq!(reply["jsonrpc"].as_str(), Some("2.0"));
        prop_assert_eq!(reply["id"].as_i64(), Some(id));
        prop_assert_ne!(reply.get("result").is_some(), reply.get("error").is_some());
        prop_assert!(reply.get("method").is_none());
    }
}
