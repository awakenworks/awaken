//! Host-side loopback MCP relay — the α-reference resolver.
//!
//! A sandboxed ACP agent's MCP client must authenticate to a real HTTP MCP server (e.g. the
//! GitHub MCP) without the raw token ever entering the sandbox. Instead of handing the CLI
//! the real URL + the secret, [`project_mcp_transport`](crate::mcp::project_mcp_transport) points a
//! sandboxed server at an opaque exact-generation capability URL on this relay. The relay
//! holds the host-side bearer from private exact-generation transport material,
//! looks it up by `(session, attachment, generation)`, strips any workload-supplied `Authorization`,
//! injects the real bearer, and forwards to the real server — so the credential is resolved
//! **out of the sandbox's address space** (the sandbox only reaches loopback over shared-net).
//!
//! Admission requires provider-enforced substitution plus a no-bypass network boundary;
//! loopback reachability alone is never custody evidence. The request body is buffered;
//! the response is streamed straight through, so an MCP Streamable-HTTP
//! `text/event-stream` reply is forwarded live rather than stalled.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::mcp::McpTransportMaterial;

/// Where one relay route forwards to, with the host-held credential to inject.
#[derive(Clone)]
struct Route {
    url: String,
    bearer: Option<awaken_agent_contract::RedactedString>,
    /// Opaque sandbox-held capability. It is scoped by the surrounding route
    /// key and never persisted as Session desired state.
    capability: String,
    lease_expires_at_unix_ms: u64,
}

type RouteKey = (String, String, u64);
type Routes = Arc<Mutex<HashMap<RouteKey, Route>>>;

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
            .route(
                "/{session}/{attachment}/{generation}/{capability}",
                axum::routing::any(forward),
            )
            .with_state(routes.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { routes, addr })
    }

    /// Install one exact-generation route. Replaying the same generation
    /// replaces only that route; it never changes another generation by name.
    #[cfg(test)]
    pub(crate) fn set_route(
        &self,
        generation: &awaken_session_contract::McpGenerationRef,
        server: &McpTransportMaterial,
    ) {
        if !self.stage_route(generation, server) {
            assert!(self.update_staged_route(generation, server));
        }
    }

    /// Claim a previously absent exact-generation route during realization
    /// staging. A concurrent or leaked route is never overwritten with another
    /// request's material.
    pub(crate) fn stage_route(
        &self,
        generation: &awaken_session_contract::McpGenerationRef,
        server: &McpTransportMaterial,
    ) -> bool {
        let Some(url) = server.http_url() else {
            return false;
        };
        let mut routes = self.routes.lock().unwrap();
        let key = route_key(generation);
        if routes.contains_key(&key) {
            return false;
        }
        routes.insert(
            key,
            Route {
                url: url.to_string(),
                bearer: server.bearer().cloned(),
                capability: uuid::Uuid::new_v4().simple().to_string(),
                lease_expires_at_unix_ms: generation.lease_expires_at_unix_ms,
            },
        );
        true
    }

    /// Advance facts for an already-staged exact route without manufacturing a
    /// missing effect. Publication and renewal must fail closed if staging did
    /// not create the route through the canonical realizer path.
    pub(crate) fn update_staged_route(
        &self,
        generation: &awaken_session_contract::McpGenerationRef,
        server: &McpTransportMaterial,
    ) -> bool {
        let mut routes = self.routes.lock().unwrap();
        let Some(route) = routes.get_mut(&route_key(generation)) else {
            return false;
        };
        let Some(url) = server.http_url() else {
            return false;
        };
        route.url = url.to_string();
        route.bearer = server.bearer().cloned();
        route.lease_expires_at_unix_ms = generation.lease_expires_at_unix_ms;
        true
    }

    pub(crate) fn remove_route(&self, generation: &awaken_session_contract::McpGenerationRef) {
        self.routes.lock().unwrap().remove(&route_key(generation));
    }

    /// Remove every bearer-bearing route for a terminal Session.
    pub(crate) fn remove_routes(&self, thread: &str) {
        self.routes
            .lock()
            .expect("MCP relay routes mutex poisoned")
            .retain(|(route_session, _, _), _| route_session != thread);
    }

    /// The loopback URL a sandboxed CLI dials for `(thread, name)` — the value written into
    /// the sandboxed MCP config in place of the real url.
    pub(crate) fn route_url(
        &self,
        generation: &awaken_session_contract::McpGenerationRef,
    ) -> Option<String> {
        let capability = self
            .routes
            .lock()
            .expect("MCP relay routes mutex poisoned")
            .get(&route_key(generation))?
            .capability
            .clone();
        Some(format!(
            "http://{}/{}/{}/{}/{}",
            self.addr,
            generation.session_id,
            generation.attachment_id.0,
            generation.generation.0,
            capability
        ))
    }
}

fn route_key(generation: &awaken_session_contract::McpGenerationRef) -> RouteKey {
    (
        generation.session_id.clone(),
        generation.attachment_id.0.clone(),
        generation.generation.0,
    )
}

/// Forward one MCP request to its real server with the host-held bearer injected. The
/// workload-supplied `Authorization` is dropped; all other headers + the body pass
/// through. An unknown route or an upstream error fails closed (never opens unauthenticated).
async fn forward(
    State(routes): State<Routes>,
    Path((session, attachment, generation, capability)): Path<(String, String, u64, String)>,
    req: Request,
) -> Response {
    let Some(route) = routes
        .lock()
        .unwrap()
        .get(&(session, attachment, generation))
        .cloned()
    else {
        return (StatusCode::NOT_FOUND, "unknown relay route").into_response();
    };
    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default();
    if route.capability != capability
        || !awaken_session_contract::realization_lease_is_live_at(
            route.lease_expires_at_unix_ms,
            now_unix_ms,
        )
    {
        return (StatusCode::NOT_FOUND, "unknown relay route").into_response();
    }
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
    // Pass every header EXCEPT workload-supplied Authorization and the loopback Host.
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
    use awaken_ext_mcp::{Credential, HttpTransportBuilder, McpToolTransport};

    fn generation(
        session: &str,
        attachment: &str,
        generation: u64,
    ) -> awaken_session_contract::McpGenerationRef {
        awaken_session_contract::McpGenerationRef {
            session_id: session.into(),
            attachment_id: awaken_session_contract::McpAttachmentId(attachment.into()),
            generation: awaken_session_contract::McpGeneration(generation),
            runtime_incarnation: "runtime-1".into(),
            lease_epoch: 4,
            lease_expires_at_unix_ms: u64::MAX,
        }
    }

    fn http_material(
        name: impl Into<String>,
        url: impl Into<String>,
        bearer: Option<String>,
    ) -> McpTransportMaterial {
        McpTransportMaterial {
            name: name.into(),
            prompts_as_skills: false,
            transport: crate::mcp::McpTransportMaterialKind::Http {
                url: url.into(),
                bearer: bearer.map(awaken_agent_contract::RedactedString::from),
                refresh: None,
            },
        }
    }

    /// A fake upstream MCP server that echoes back the `Authorization` header it received, so
    /// the test can prove the relay injected the real bearer.
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
    async fn exact_generation_routes_coexist_and_drain_independently() {
        let relay = McpRelay::start().await.unwrap();
        let server = |name: &str| http_material(name, "https://example.invalid/mcp", None);
        let old = generation("thread", "mcp", 1);
        let new = generation("thread", "mcp", 2);
        let other = generation("other", "mcp", 1);
        relay.set_route(&old, &server("old"));
        relay.set_route(&new, &server("new"));
        relay.set_route(&other, &server("unrelated"));
        relay.remove_route(&old);

        let routes = relay.routes.lock().unwrap();
        assert!(!routes.contains_key(&route_key(&old)));
        assert!(routes.contains_key(&route_key(&new)));
        assert!(routes.contains_key(&route_key(&other)));
    }

    #[tokio::test]
    async fn relay_injects_the_real_bearer_and_drops_workload_authorization() {
        let upstream = fake_upstream().await;
        let relay = McpRelay::start().await.unwrap();
        let generation = generation("t1", "mcp-github", 1);
        relay.set_route(
            &generation,
            &http_material(
                "github:repo",
                format!("http://{upstream}/"),
                Some("ghp_real_secret".into()),
            ),
        );

        // A compromised client may try to override the route credential. The relay must
        // strip it and inject only the generation-scoped material it owns.
        let url = relay.route_url(&generation).unwrap();
        let body = reqwest::Client::new()
            .post(&url)
            .bearer_auth("session-mcp:github:repo")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        // The upstream saw the real bearer, never the workload-supplied value.
        assert_eq!(body, "Bearer ghp_real_secret");
        assert!(
            !body.contains("session-mcp"),
            "workload authorization must not reach upstream: {body}"
        );
    }

    // Relay routing cases are generated from this cause graph:
    // active route + exact current credential -> forward/inject; replacement ->
    // next request uses only the new credential; removal -> reject before upstream.
    //
    // | Rule | Route state | Generation action | Effect |
    // |------|-------------|-------------------|--------|
    // | R1   | active      | none              | inject current |
    // | R2   | active      | replace           | inject replacement only |
    // | R3   | removed     | remove            | 404, no forward |
    #[tokio::test]
    async fn rotating_and_removing_a_route_updates_the_next_mcp_request() {
        let upstream = fake_upstream().await;
        let relay = McpRelay::start().await.unwrap();
        let server = |secret: &str| {
            http_material(
                "github",
                format!("http://{upstream}/"),
                Some(secret.to_string()),
            )
        };
        let client = reqwest::Client::new();
        let old_generation = generation("session", "mcp-github", 1);
        let new_generation = generation("session", "mcp-github", 2);
        relay.set_route(&old_generation, &server("old-secret"));
        let old_route = relay.route_url(&old_generation).unwrap();
        let first = client
            .post(&old_route)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(first, "Bearer old-secret");

        relay.set_route(&new_generation, &server("new-secret"));
        let new_route = relay.route_url(&new_generation).unwrap();
        let second = client
            .post(&new_route)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(second, "Bearer new-secret");
        assert!(!second.contains("old-secret"));

        relay.remove_route(&old_generation);
        let removed = client.post(&old_route).send().await.unwrap();
        assert_eq!(removed.status(), reqwest::StatusCode::NOT_FOUND);
        let replacement_still_active = client
            .post(&new_route)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(replacement_still_active, "Bearer new-secret");
    }

    /// Virtual route-capability cause graph: an exact generation route is usable
    /// only when C1 the opaque capability matches and C2 its Session lease is
    /// live. Exact restaging preserves the capability for idempotent recovery;
    /// another generation receives a different capability. Drain is covered by
    /// R3 above and remains the explicit revocation edge.
    ///
    /// | Rule | capability | lease | action | Effect |
    /// |---|---|---|---|---|
    /// | V1 | exact | live | call | forward |
    /// | V2 | wrong | live | call | 404/no forward |
    /// | V3 | exact | expired | call | 404/no forward |
    /// | V4 | exact | live | restage same generation | same capability |
    /// | V5 | other generation | live | stage | distinct capability |
    #[tokio::test]
    async fn route_capability_is_generation_scoped_expiring_and_idempotent() {
        let upstream = fake_upstream().await;
        let relay = McpRelay::start().await.unwrap();
        let server = http_material(
            "github",
            format!("http://{upstream}/"),
            Some("route-secret".into()),
        );
        let mut live = generation("session-cap", "mcp-github", 1);
        live.lease_expires_at_unix_ms = u64::MAX - 1;
        relay.set_route(&live, &server);
        let live_url = relay.route_url(&live).unwrap();
        let client = reqwest::Client::new();
        let forwarded = client.post(&live_url).send().await.unwrap();
        assert_eq!(forwarded.status(), reqwest::StatusCode::OK, "V1");

        let mut wrong = live_url.rsplit_once('/').unwrap().0.to_string();
        wrong.push_str("/wrong-capability");
        let denied = client.post(wrong).send().await.unwrap();
        assert_eq!(denied.status(), reqwest::StatusCode::NOT_FOUND, "V2");

        let mut renewed = live.clone();
        renewed.lease_expires_at_unix_ms = u64::MAX;
        relay.set_route(&renewed, &server);
        assert_eq!(
            relay.route_url(&renewed).as_deref(),
            Some(live_url.as_str()),
            "V4"
        );
        assert_eq!(
            relay
                .routes
                .lock()
                .unwrap()
                .get(&route_key(&renewed))
                .unwrap()
                .lease_expires_at_unix_ms,
            u64::MAX,
            "V4 extended expiry"
        );

        let mut expired = generation("session-expired", "mcp-github", 1);
        expired.lease_expires_at_unix_ms = 0;
        relay.set_route(&expired, &server);
        let expired_call = client
            .post(relay.route_url(&expired).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(expired_call.status(), reqwest::StatusCode::NOT_FOUND, "V3");

        let next = generation("session-cap", "mcp-github", 2);
        relay.set_route(&next, &server);
        assert_ne!(relay.route_url(&next), Some(live_url), "V5");
    }

    // Protocol-flow decision row: when R1 holds, initialize, tools/list and
    // tools/call must all traverse the same authenticated route; any missing
    // injection yields upstream 401 and no successful tool result.
    #[tokio::test]
    async fn injected_credential_preserves_the_complete_mcp_tool_flow() {
        let (upstream, seen) = crate::test_mcp::start(Some("Bearer relay-only-secret")).await;

        let relay = McpRelay::start().await.unwrap();
        let generation = generation("session-1", "mcp-functional", 1);
        relay.set_route(
            &generation,
            &http_material("functional", upstream, Some("relay-only-secret".into())),
        );

        // This is the sandbox-side client: it receives the loopback route and no
        // credential at all. The relay alone owns and injects the real bearer.
        let transport = HttpTransportBuilder::new(relay.route_url(&generation).unwrap())
            .credential(Credential::None)
            .connect()
            .await
            .expect("initialize survives host-side injection");
        let tools = transport
            .list_tools()
            .await
            .expect("tools/list survives host-side injection");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let result = transport
            .call_tool("echo", serde_json::json!({ "value": "round-trip" }))
            .await
            .expect("tools/call survives host-side injection");
        let result_json = serde_json::to_value(&result).expect("tool result serializes");
        assert_eq!(result_json["content"][0]["type"], "text");
        assert_eq!(result_json["content"][0]["text"], "round-trip");
        assert_eq!(result.is_error, Some(false));

        let seen = seen.lock().unwrap();
        for expected in [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call",
        ] {
            assert!(
                seen.iter().any(|(method, _)| method == expected),
                "the upstream MCP server must receive {expected}: {seen:?}"
            );
        }
        assert!(
            seen.iter()
                .all(|(_, bearer)| bearer == "Bearer relay-only-secret"),
            "every MCP operation is authenticated by the relay: {seen:?}"
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
        let generation = generation("t1", "mcp-gh", 1);
        relay.set_route(
            &generation,
            &http_material("gh", format!("http://{addr}/"), Some("tok".into())),
        );
        let resp = reqwest::Client::new()
            .post(relay.route_url(&generation).unwrap())
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

    /// Live end-to-end against the REAL GitHub MCP (`api.githubcopilot.com/mcp/`), gated on
    /// `AWAKEN_GITHUB_MCP_TOKEN` (a fine-grained PAT / installation token) — self-skips
    /// otherwise. Proves the Managed path: the workload sends no credential and the relay
    /// injects the real token, so GitHub MCP authenticates the call.
    #[tokio::test]
    async fn relay_reaches_the_real_github_mcp_when_a_token_is_present() {
        let Ok(token) = std::env::var("AWAKEN_GITHUB_MCP_TOKEN") else {
            eprintln!(
                "skipping: set AWAKEN_GITHUB_MCP_TOKEN to run the live GitHub MCP relay test"
            );
            return;
        };
        let relay = McpRelay::start().await.unwrap();
        let generation = generation("t1", "mcp-github", 1);
        relay.set_route(
            &generation,
            &http_material("github", "https://api.githubcopilot.com/mcp/", Some(token)),
        );
        // An MCP `initialize` handshake through the relay without a workload credential.
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "awaken-relay-test", "version": "0" }
            }
        });
        let resp = reqwest::Client::new()
            .post(relay.route_url(&generation).unwrap())
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(&body)
            .send()
            .await
            .expect("relay forwards to the real GitHub MCP");
        // A bad/absent token 401s at GitHub — so not-401 proves the relay injected the real
        // credential outside the sandbox's address space and GitHub accepted it.
        assert_ne!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "the relay-injected token must authenticate to the real GitHub MCP (got {})",
            resp.status()
        );
        assert!(
            resp.status().is_success() || resp.status().is_redirection(),
            "GitHub MCP responded through the relay: {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn relay_fails_closed_on_an_unknown_route() {
        let relay = McpRelay::start().await.unwrap();
        let unknown = format!(
            "http://{}/t-unknown/mcp-nope/1/not-a-route-capability",
            relay.addr
        );
        let status = reqwest::Client::new()
            .post(unknown)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    }
}
