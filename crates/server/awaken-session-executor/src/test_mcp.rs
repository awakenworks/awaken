use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

pub(crate) type SeenMcpRequests = Arc<Mutex<Vec<(String, String)>>>;

/// One shared Streamable-HTTP MCP fixture for Host and relay tests. Keeping the
/// protocol emulator here prevents each security test from growing a subtly
/// different initialize/list/call implementation.
pub(crate) async fn start(required_bearer: Option<&str>) -> (String, SeenMcpRequests) {
    async fn mcp(
        State((seen, required_bearer)): State<(SeenMcpRequests, Option<String>)>,
        req: Request,
    ) -> Response {
        let bearer = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("<none>")
            .to_string();
        if required_bearer
            .as_deref()
            .is_some_and(|required| required != bearer)
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let body = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
            Ok(body) => body,
            Err(error) => {
                return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
            }
        };
        let value: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(error) => {
                return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
            }
        };
        let method = value
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<missing>")
            .to_string();
        seen.lock().unwrap().push((method.clone(), bearer));
        let Some(id) = value.get("id").cloned() else {
            return StatusCode::ACCEPTED.into_response();
        };
        let result = match method.as_str() {
            "initialize" => serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "host-test", "version": "1" }
            }),
            "tools/list" => serde_json::json!({
                "tools": [{
                    "name": "echo",
                    "description": "echo one value",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "value": { "type": "string" } },
                        "required": ["value"]
                    }
                }]
            }),
            "tools/call" => serde_json::json!({
                "content": [{ "type": "text", "text": value["params"]["arguments"]["value"] }],
                "isError": false
            }),
            _ => {
                return axum::Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "method not found" }
                }))
                .into_response();
            }
        };
        axum::Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }))
        .into_response()
    }

    let seen = SeenMcpRequests::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(mcp))
        .with_state((seen.clone(), required_bearer.map(str::to_string)));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/mcp"), seen)
}
