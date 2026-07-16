//! Host-side loopback MCP relay — the α-reference resolver.
//!
//! A sandboxed ACP agent's MCP client must authenticate to a real HTTP MCP server (e.g. the
//! GitHub MCP) without the raw token ever entering the sandbox. Instead of handing the CLI
//! the real URL + the secret, [`project_staged_mcp`](crate::mcp::project_staged_mcp) points a
//! sandboxed server at `http://127.0.0.1:<port>/<thread>/<name>` on this relay. The relay
//! holds the host-side bearer (from [`PreparedMcpServer`](crate::host::PreparedMcpServer)),
//! looks it up by `(thread, name)`, strips the placeholder `Authorization` the sandbox sent,
//! injects the real bearer, and forwards to the real server — so the credential is resolved
//! **out of the sandbox's address space** (the sandbox only reaches loopback over shared-net).
//!
//! A deny-egress (`--unshare-net`) thread cannot reach loopback — but such a thread cannot
//! reach the real MCP server either, so MCP is moot there. The request body is buffered; the
//! response is streamed straight through, so an MCP Streamable-HTTP `text/event-stream` reply
//! is forwarded live rather than stalled.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::host::PreparedMcpServer;

/// Where one relay route forwards to, with the host-held credential to inject.
#[derive(Clone)]
struct Route {
    url: String,
    bearer: Option<awaken_agent_contract::RedactedString>,
}

type Routes = Arc<Mutex<HashMap<(String, String), Route>>>;

/// A running loopback MCP relay: its routing table plus the address it serves on. Cloneable
/// (shares the table); the served task lives for the process.
#[derive(Clone)]
pub(crate) struct McpRelay {
    routes: Routes,
    addr: SocketAddr,
}

impl McpRelay {
    /// Bind an ephemeral loopback port and serve the forwarding router in the background.
    pub(crate) async fn start() -> std::io::Result<Self> {
        let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = axum::Router::new()
            .route("/{thread}/{name}", axum::routing::any(forward))
            .with_state(routes.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { routes, addr })
    }

    /// Register (replace) the routes for a thread's staged MCP servers — each `(thread, name)`
    /// maps to that server's real url + host-held bearer. Called when the thread's MCP set is
    /// registered or rotated.
    pub(crate) fn set_routes(&self, thread: &str, servers: &[PreparedMcpServer]) {
        let mut routes = self.routes.lock().unwrap();
        for s in servers {
            routes.insert(
                (thread.to_string(), s.name.clone()),
                Route {
                    url: s.url.clone(),
                    bearer: s.bearer.clone(),
                },
            );
        }
    }

    /// The loopback URL a sandboxed CLI dials for `(thread, name)` — the value written into
    /// the sandboxed MCP config in place of the real url.
    pub(crate) fn route_url(&self, thread: &str, name: &str) -> String {
        format!("http://{}/{thread}/{name}", self.addr)
    }
}

/// Forward one MCP request to its real server with the host-held bearer injected. The
/// sandbox's placeholder `Authorization` is dropped; all other headers + the body pass
/// through. An unknown route or an upstream error fails closed (never opens unauthenticated).
async fn forward(
    State(routes): State<Routes>,
    Path((thread, name)): Path<(String, String)>,
    req: Request,
) -> Response {
    let Some(route) = routes.lock().unwrap().get(&(thread, name)).cloned() else {
        return (StatusCode::NOT_FOUND, "unknown relay route").into_response();
    };
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("relay body: {e}")).into_response(),
    };
    // Bridge axum (http 1.0) → reqwest (http 0.2) by string/bytes: the two crates pull
    // different `http` versions, so headers/method/status don't share types.
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::POST);
    let client = reqwest::Client::new();
    let mut rb = client.request(method, &route.url).body(bytes.to_vec());
    // Pass every header EXCEPT the sandbox's placeholder Authorization and the loopback Host.
    for (k, v) in parts.headers.iter() {
        if k == header::AUTHORIZATION || k == header::HOST {
            continue;
        }
        if let Ok(vs) = v.to_str() {
            rb = rb.header(k.as_str(), vs);
        }
    }
    // Inject the real credential host-side — it never entered the sandbox.
    if let Some(bearer) = &route.bearer {
        rb = rb.bearer_auth(bearer.expose_secret());
    }
    match rb.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            // Stream the response body straight through — MCP Streamable HTTP replies as an
            // SSE stream (`text/event-stream`), so buffering would stall long-poll notifications.
            let mut out = Response::builder().status(status);
            if let Some(ct) = content_type {
                out = out.header(header::CONTENT_TYPE, ct);
            }
            out.body(Body::from_stream(resp.bytes_stream()))
                .unwrap_or_else(|_| {
                    (StatusCode::BAD_GATEWAY, "relay response build").into_response()
                })
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("relay upstream: {e}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake upstream MCP server that echoes back the `Authorization` header it received, so
    /// the test can prove the relay injected the real bearer (and dropped the placeholder).
    async fn fake_upstream() -> SocketAddr {
        async fn echo_auth(req: Request) -> Response {
            let auth = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string();
            (StatusCode::OK, auth).into_response()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/", axum::routing::any(echo_auth));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    #[tokio::test]
    async fn relay_injects_the_real_bearer_and_drops_the_sandbox_placeholder() {
        let upstream = fake_upstream().await;
        let relay = McpRelay::start().await.unwrap();
        relay.set_routes(
            "t1",
            &[PreparedMcpServer {
                name: "github:repo".into(),
                url: format!("http://{upstream}/"),
                bearer: Some(awaken_agent_contract::RedactedString::from(
                    "ghp_real_secret".to_string(),
                )),
                refresh: None,
            }],
        );

        // Dial the relay as the sandboxed CLI would: the placeholder α reference as the bearer.
        let url = relay.route_url("t1", "github:repo");
        let body = reqwest::Client::new()
            .post(&url)
            .bearer_auth("session-mcp:github:repo")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        // The upstream saw the REAL bearer, never the sandbox's placeholder reference.
        assert_eq!(body, "Bearer ghp_real_secret");
        assert!(
            !body.contains("session-mcp"),
            "placeholder must not reach upstream: {body}"
        );
    }

    #[tokio::test]
    async fn relay_streams_an_sse_response_through_with_its_content_type() {
        // Upstream replies as `text/event-stream` — the MCP Streamable HTTP shape. The relay
        // must pass the stream (and content-type) through, not buffer/rewrite it.
        async fn sse(_req: Request) -> Response {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from(
                    "event: message\ndata: {\"jsonrpc\":\"2.0\"}\n\n",
                ))
                .unwrap()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/", axum::routing::any(sse));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let relay = McpRelay::start().await.unwrap();
        relay.set_routes(
            "t1",
            &[PreparedMcpServer {
                name: "gh".into(),
                url: format!("http://{addr}/"),
                bearer: Some(awaken_agent_contract::RedactedString::from(
                    "tok".to_string(),
                )),
                refresh: None,
            }],
        );
        let resp = reqwest::Client::new()
            .post(relay.route_url("t1", "gh"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream"),
            "the SSE content-type is preserved through the relay"
        );
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("data: {\"jsonrpc\""),
            "SSE body streamed through: {body}"
        );
    }

    #[tokio::test]
    async fn relay_fails_closed_on_an_unknown_route() {
        let relay = McpRelay::start().await.unwrap();
        let status = reqwest::Client::new()
            .post(relay.route_url("t-unknown", "nope"))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    }
}
