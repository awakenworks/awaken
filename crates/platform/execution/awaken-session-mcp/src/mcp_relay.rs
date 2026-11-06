// @formal entrypoint credential.mcp_adopted_revocation_removes_accepting_generation_before_new_calls
//! Host-side MCP relay — the α-reference resolver.
//!
//! A sandboxed ACP agent's MCP client must authenticate to a real HTTP MCP server (e.g. the
//! GitHub MCP) without the raw token ever entering the sandbox. Instead of handing the CLI
//! the real URL + the secret,
//! [`project_mcp_session_transport`](crate::mcp::project_mcp_session_transport) points a
//! sandboxed server at an opaque exact-generation capability URL on this relay. The relay
//! holds the host-side bearer from private exact-generation transport material,
//! looks it up by `(session, attachment, generation)`, strips any workload-supplied `Authorization`,
//! injects the real bearer, and forwards to the real server — so the credential is resolved
//! **out of the sandbox's address space**. Local sandboxes use Worker loopback;
//! an isolated container may use an operator-configured relay origin reachable
//! only through its separately attested network boundary.
//!
//! Admission requires provider-enforced substitution plus a no-bypass network boundary;
//! loopback reachability alone is never custody evidence. The request body is buffered;
//! the response is streamed straight through, so an MCP Streamable-HTTP
//! `text/event-stream` reply is forwarded live rather than stalled.

use std::collections::HashMap;
#[cfg(test)]
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use awaken_sync::InvariantLock;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;

use crate::mcp_material::McpTransportMaterial;

mod server;

#[cfg(any(test, feature = "test-support"))]
mod test_support;
use awaken_lease_contract::fence::FenceSuccession;

/// Where one relay route forwards to, with the host-held credential to inject.
#[derive(Clone)]
struct Route {
    url: String,
    bearer: Option<awaken_agent_contract::RedactedString>,
    refresh: Option<Box<crate::mcp_material::McpRefreshMaterial>>,
    /// Opaque sandbox-held capability. It is scoped by the surrounding route
    /// key and never persisted as Session desired state.
    capability: String,
    generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    /// The existing MCP activity primitive, retained by this unique route
    /// owner so already-cloned Axum handlers cannot outlive generation drain.
    call_fence: awaken_ext_mcp::transport::McpCallFence,
}

type RouteKey = (String, String, u64);
type Routes = Arc<InvariantLock<HashMap<RouteKey, Route>>>;
/// An operation-local selection of one existing physical route insertion.
/// This carries no root authority and never discovers a later replacement.
pub(crate) struct CapturedRelayRoute {
    routes: Routes,
    key: RouteKey,
    generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    capability: String,
    call_fence: awaken_ext_mcp::transport::McpCallFence,
}

impl CapturedRelayRoute {
    pub(crate) fn call_fence(&self) -> awaken_ext_mcp::transport::McpCallFence {
        self.call_fence.clone()
    }

    pub(crate) fn remove_if_current(&self) {
        // A poisoned route map serves no request (`forward` refuses it), so
        // the removal is skipped.
        let Ok(mut routes) = self.routes.lock() else {
            return;
        };
        if routes.get(&self.key).is_some_and(|route| {
            route.generation.tenure() == self.generation.tenure()
                && route.capability == self.capability
        }) {
            routes.remove(&self.key);
        }
    }
}

type GenerationAuthority = Arc<
    dyn Fn(&awaken_session_configuration_contract::mcp::McpGenerationRef, u64) -> bool
        + Send
        + Sync,
>;

/// Deployment-owned issuer for the existing authenticated HTTPS CONNECT path.
/// It never chooses an MCP target or handles the upstream bearer; the relay
/// supplies the exact staged host for each call. Generation authority remains
/// solely in the Worker relay's capability, lease and call fence.
pub trait McpRelayOutboundProxy: Send + Sync {
    fn issue_for(&self, upstream_host: &str, now_unix_ms: u64) -> Result<String, String>;
}

/// A relayed MCP call may stream a long SSE response, so the bound is on
/// connecting and on upstream silence, never on total elapsed time.
const RELAY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const RELAY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Clone)]
struct RelayState {
    routes: Routes,
    stopping: awaken_runtime_contract::CancellationToken,
    authority: GenerationAuthority,
    /// The clock route admission reads: the owning Host's process clock.
    clock: Arc<dyn awaken_time::WallClock>,
    /// Local self-hosted traffic reuses one direct client. External container
    /// traffic obtains a fresh host-scoped CONNECT capability for every call.
    client: reqwest::Client,
    outbound_proxy: Option<Arc<dyn McpRelayOutboundProxy>>,
}

/// The relay's answer to one root-authority read. The sandbox-facing relay
/// never reveals why a route is refused (every refusal is the same 404), so
/// an unreadable authority is refused exactly like an unauthorized one; the
/// fault is reported here instead of to the sandbox.
///
/// | Rule | root authority read   | admitted | reported |
/// |---|---|---|---|
/// | A1 | `Ok(Some(true))`      | yes      | no       |
/// | A2 | `Ok(Some(false))`     | no       | no       |
/// | A3 | `Ok(None)` (absent)   | no       | no       |
/// | A4 | `Err(_)` (unreadable) | no       | yes      |
fn relay_admits_root(root: Result<Option<bool>, awaken_runtime_contract::runs::RunError>) -> bool {
    match root {
        Ok(root) => root == Some(true),
        Err(error) => {
            tracing::error!(
                code = %error.code,
                error = %error.message,
                "MCP relay refused a route: the Session root authority is unreadable"
            );
            false
        }
    }
}

impl RelayState {
    fn admits(&self, route: &Route, capability: &str) -> bool {
        !self.stopping.is_cancelled()
            && route.capability == capability
            && (self.authority)(
                &route.generation,
                awaken_time::WallClockExt::now_ms(self.clock.as_ref()),
            )
    }

    fn client_for(&self, route: &Route) -> Result<reqwest::Client, ()> {
        let Some(issuer) = &self.outbound_proxy else {
            return Ok(self.client.clone());
        };
        let target = reqwest::Url::parse(&route.url).map_err(|_| ())?;
        if target.scheme() != "https"
            || !target.username().is_empty()
            || target.password().is_some()
        {
            return Err(());
        }
        let host = target
            .host_str()
            .and_then(awaken_container_contract::normalize_allowlist_host)
            .ok_or(())?;
        let proxy_url = issuer
            .issue_for(
                &host,
                awaken_time::WallClockExt::now_ms(self.clock.as_ref()),
            )
            .map_err(|_| ())?;
        let proxy = reqwest::Proxy::https(proxy_url).map_err(|_| ())?;
        awaken_outbound_http::rustls_client_builder(
            awaken_outbound_http::TransportDeadline::Streaming {
                connect: RELAY_CONNECT_TIMEOUT,
                read: RELAY_READ_TIMEOUT,
            },
        )
        .proxy(proxy)
        .build()
        .map_err(|_| ())
    }
}

/// One running loopback MCP relay. Explicit clones share the same physical
/// server and route table. Last-owner Drop requests closure; close_and_wait
/// acknowledges joined local completion, separately from remote tool effects.
#[derive(Clone)]
pub struct McpRelay {
    routes: Routes,
    server: Arc<server::RelayServer>,
    advertised_origin: String,
    alive: Arc<AtomicBool>,
    #[cfg(test)]
    state: RelayState,
    #[cfg(any(test, feature = "test-support"))]
    server_abort: tokio::task::AbortHandle,
}

impl McpRelay {
    /// Bind an ephemeral loopback port (or the deployment's container
    /// exposure) and serve the forwarding router in the background. Routes are
    /// admitted only while `authority` holds the current Session root.
    pub async fn for_authority(
        authority: std::sync::Weak<dyn crate::mcp::McpGenerationAuthority>,
        exposure: Option<&awaken_session_environment_contract::ContainerMcpRelayExposure>,
        clock: Arc<dyn awaken_time::WallClock>,
        outbound_proxy: Option<Arc<dyn McpRelayOutboundProxy>>,
    ) -> std::io::Result<Self> {
        if authority.upgrade().is_none() {
            return Err(std::io::Error::other(
                "MCP relay Host authority is unavailable",
            ));
        }
        if exposure.is_some() && outbound_proxy.is_none() {
            return Err(std::io::Error::other(
                "external MCP relay requires an installed authenticated outbound proxy",
            ));
        }
        Self::start_with_authority_and_exposure(
            Arc::new(move |generation, _now| {
                authority.upgrade().is_some_and(|authority| {
                    relay_admits_root(authority.mcp_generation_root_authority(generation))
                })
            }),
            exposure,
            clock,
            outbound_proxy,
        )
        .await
    }

    pub fn matches_exposure(
        &self,
        exposure: &awaken_session_environment_contract::ContainerMcpRelayExposure,
    ) -> bool {
        self.advertised_origin == exposure.advertised_origin().as_str().trim_end_matches('/')
    }

    pub fn is_live(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Whether this original server still accepts new routes and calls.
    pub fn is_accepting(&self) -> bool {
        self.is_live() && !self.server.is_closing()
    }

    /// Close admission, drain original local route activity, and join the
    /// original server. A cancelled waiter cannot lose that task or its result.
    /// Completion proves local relay shutdown, not remote tool termination.
    pub async fn close_and_wait(&self) -> std::io::Result<()> {
        self.server.close_and_wait().await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn abort_for_test(&self) {
        self.server_abort.abort();
    }

    async fn start_with_authority_and_exposure(
        authority: GenerationAuthority,
        exposure: Option<&awaken_session_environment_contract::ContainerMcpRelayExposure>,
        clock: Arc<dyn awaken_time::WallClock>,
        outbound_proxy: Option<Arc<dyn McpRelayOutboundProxy>>,
    ) -> std::io::Result<Self> {
        let routes: Routes = Arc::new(InvariantLock::new(HashMap::new()));
        let bind = exposure.map_or_else(
            || "127.0.0.1:0".parse().expect("literal MCP relay address"),
            awaken_session_environment_contract::ContainerMcpRelayExposure::bind,
        );
        let listener = tokio::net::TcpListener::bind(bind).await?;
        let addr = listener.local_addr()?;
        let advertised_origin = exposure.map_or_else(
            || format!("http://{addr}"),
            |exposure| {
                exposure
                    .advertised_origin()
                    .as_str()
                    .trim_end_matches('/')
                    .to_owned()
            },
        );
        let stopping = awaken_runtime_contract::CancellationToken::new();
        let state = RelayState {
            routes: routes.clone(),
            stopping: stopping.clone(),
            authority,
            clock,
            client: awaken_outbound_http::rustls_client_builder(
                awaken_outbound_http::TransportDeadline::Streaming {
                    connect: RELAY_CONNECT_TIMEOUT,
                    read: RELAY_READ_TIMEOUT,
                },
            )
            .no_proxy()
            .build()
            .map_err(std::io::Error::other)?,
            outbound_proxy,
        };
        let app = axum::Router::new()
            .route(
                "/{session}/{attachment}/{generation}/{capability}",
                axum::routing::any(forward),
            )
            .with_state(state.clone());
        let alive = Arc::new(AtomicBool::new(true));
        let server =
            server::RelayServer::start(listener, app, routes.clone(), alive.clone(), stopping);
        #[cfg(any(test, feature = "test-support"))]
        let server_abort = server.abort_handle();
        Ok(Self {
            routes,
            server,
            advertised_origin,
            alive,
            #[cfg(test)]
            state,
            #[cfg(any(test, feature = "test-support"))]
            server_abort,
        })
    }

    /// Install one exact-generation route. Replaying the same generation
    /// replaces only that route; it never changes another generation by name.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_route(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
        server: &McpTransportMaterial,
    ) {
        if !self.stage_route(generation, server) {
            assert!(self.update_staged_route(generation, server));
        }
    }

    /// Claim a previously absent exact-generation route during realization
    /// staging. A concurrent or leaked route is never overwritten with another
    /// request's material.
    pub fn stage_route(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
        server: &McpTransportMaterial,
    ) -> bool {
        self.stage_route_captured(generation, server).is_some()
    }

    pub(crate) fn stage_route_captured(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
        server: &McpTransportMaterial,
    ) -> Option<CapturedRelayRoute> {
        let url = server.http_url()?;
        let Ok(mut routes) = self.routes.lock() else {
            return None;
        };
        if self.server.is_closing() {
            return None;
        }
        let key = route_key(generation);
        if routes.contains_key(&key) {
            return None;
        }
        let route = Route {
            url: url.to_string(),
            bearer: server.bearer().cloned(),
            refresh: server.refresh(),
            capability: uuid::Uuid::new_v4().simple().to_string(),
            generation: generation.clone(),
            call_fence: awaken_ext_mcp::transport::McpCallFence::open(),
        };
        let captured = CapturedRelayRoute {
            routes: self.routes.clone(),
            key: key.clone(),
            generation: route.generation.clone(),
            capability: route.capability.clone(),
            call_fence: route.call_fence.clone(),
        };
        routes.insert(key, route);
        Some(captured)
    }

    pub(crate) fn capture_route(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Option<CapturedRelayRoute> {
        let Ok(routes) = self.routes.lock() else {
            return None;
        };
        let key = route_key(generation);
        let route = routes
            .get(&key)
            .filter(|route| route.generation.tenure() == generation.tenure())?;
        Some(self.capture(&key, route))
    }

    pub(crate) fn capture_routes(&self, thread: &str) -> Vec<CapturedRelayRoute> {
        let Ok(routes) = self.routes.lock() else {
            return Vec::new();
        };
        routes
            .iter()
            .filter(|(key, _)| key.0 == thread)
            .map(|(key, route)| self.capture(key, route))
            .collect()
    }

    fn capture(&self, key: &RouteKey, route: &Route) -> CapturedRelayRoute {
        CapturedRelayRoute {
            routes: self.routes.clone(),
            key: key.clone(),
            generation: route.generation.clone(),
            capability: route.capability.clone(),
            call_fence: route.call_fence.clone(),
        }
    }

    /// Advance facts for an already-staged exact route without manufacturing a
    /// missing effect. Publication and renewal must fail closed if staging did
    /// not create the route through the canonical realizer path.
    pub fn update_staged_route(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
        server: &McpTransportMaterial,
    ) -> bool {
        let Ok(mut routes) = self.routes.lock() else {
            return false;
        };
        if self.server.is_closing() {
            return false;
        }
        // Only a renewal of the staging tenure may advance the route: a
        // predecessor or successor tenure of the same key, or a shortened
        // expiry, fails closed.
        let Some(route) = routes.get_mut(&route_key(generation)).filter(|route| {
            route.generation.succession(generation) == Some(FenceSuccession::Renewal)
        }) else {
            return false;
        };
        let Some(url) = server.http_url() else {
            return false;
        };
        route.url = url.to_string();
        route.bearer = server.bearer().cloned();
        route.refresh = server.refresh();
        route.generation = generation.clone();
        true
    }

    /// Remove an exact route only when it was never published or after its
    /// shared call fence has proved quiescent. Callers own that ordering; raw
    /// map removal is deliberately not a cancellation or drain mechanism.
    ///
    /// The route is removed only for its own tenure: a late drain asserted
    /// under a predecessor (or any other) tenure of the same route key leaves
    /// the current owner's route in place.
    pub fn remove_route(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) {
        let Ok(mut routes) = self.routes.lock() else {
            return;
        };
        let key = route_key(generation);
        if routes
            .get(&key)
            .is_some_and(|route| route.generation.tenure() == generation.tenure())
        {
            routes.remove(&key);
        }
    }

    /// Clone the exact route's process-local activity handle. The route stays
    /// the sole owner; canonical MCP drain closes this shared handle before it
    /// removes the route and acknowledges `Removed`. Another tenure of the
    /// same route key gets nothing, so it can never close the owner's fence.
    pub fn route_call_fence(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Option<awaken_ext_mcp::transport::McpCallFence> {
        self.routes
            .lock()
            .ok()?
            .get(&route_key(generation))
            .filter(|route| route.generation.tenure() == generation.tenure())
            .map(|route| route.call_fence.clone())
    }

    /// Remove residual bearer-bearing routes only after canonical per-
    /// generation drain has succeeded for the whole Session.
    pub fn remove_routes(&self, thread: &str) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.retain(|(route_session, _, _), _| route_session != thread);
        }
    }

    /// The loopback URL a sandboxed CLI dials for `(thread, name)` — the value written into
    /// the sandboxed MCP config in place of the real url. Only the route's own
    /// tenure may read its capability.
    pub fn route_url(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Option<String> {
        let capability = self
            .routes
            .lock()
            .ok()?
            .get(&route_key(generation))
            .filter(|route| route.generation.tenure() == generation.tenure())?
            .capability
            .clone();
        Some(format!(
            "{}/{}/{}/{}/{}",
            self.advertised_origin,
            generation.session_id(),
            generation.attachment_id().0,
            generation.generation().version(),
            capability
        ))
    }
}

fn route_key(
    generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
) -> RouteKey {
    (
        generation.session_id().to_string(),
        generation.attachment_id().0.clone(),
        generation.generation().version(),
    )
}

/// Forward one MCP request to its real server with the host-held bearer injected. The
/// workload-supplied `Authorization` is dropped; all other headers + the body pass
/// through. An unknown route or an upstream error fails closed (never opens unauthenticated).
async fn forward(
    State(state): State<RelayState>,
    Path((session, attachment, generation, capability)): Path<(String, String, u64, String)>,
    req: Request,
) -> Response {
    let route_key = (session, attachment, generation);
    let routes = &state.routes;
    let Some(route) = routes
        .lock()
        .ok()
        .and_then(|routes| routes.get(&route_key).cloned())
    else {
        return (StatusCode::NOT_FOUND, "unknown relay route").into_response();
    };
    if !state.admits(&route, &capability) {
        return (StatusCode::NOT_FOUND, "unknown relay route").into_response();
    }
    let Some(call) = route.call_fence.try_enter() else {
        return (StatusCode::NOT_FOUND, "draining relay route").into_response();
    };
    let (parts, body) = req.into_parts();
    let body = tokio::select! {
        biased;
        _ = call.cancelled() => {
            return (StatusCode::NOT_FOUND, "draining relay route").into_response();
        }
        result = axum::body::to_bytes(body, usize::MAX) => result,
    };
    let bytes = match body {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("relay body: {e}")).into_response(),
    };
    let auth_retry_safety = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map(|body| awaken_ext_mcp::auth_retry_safety(&body))
        .unwrap_or(awaken_ext_mcp::AuthRetrySafety::Never);
    let client = match state.client_for(&route) {
        Ok(client) => client,
        Err(()) => return (StatusCode::BAD_GATEWAY, "relay egress unavailable").into_response(),
    };
    // Bridge axum (http 1.0) → reqwest (http 0.2) by string/bytes: the two crates pull
    // different `http` versions, so headers/method/status don't share types.
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::POST);
    let send = |route: &Route| {
        let mut request = client
            .request(method.clone(), &route.url)
            .body(bytes.to_vec());
        // Pass every header EXCEPT workload-supplied Authorization and the loopback Host.
        for (name, value) in &parts.headers {
            if name == header::AUTHORIZATION || name == header::HOST {
                continue;
            }
            if let Ok(value) = value.to_str() {
                request = request.header(name.as_str(), value);
            }
        }
        // Inject the real credential host-side — it never entered the sandbox.
        if let Some(bearer) = &route.bearer {
            request = request.bearer_auth(bearer.expose_secret());
        }
        request
    };
    // Body buffering may outlive ownership. Recheck the same root decision at
    // the outbound boundary; the existing call fence still owns cancellation.
    if !state.admits(&route, &capability) {
        return (StatusCode::NOT_FOUND, "unknown relay route").into_response();
    }
    let first_send = tokio::select! {
        biased;
        _ = call.cancelled() => {
            return (StatusCode::NOT_FOUND, "draining relay route").into_response();
        }
        result = send(&route).send() => result,
    };
    let mut upstream = match first_send {
        Ok(response) => response,
        Err(error) => {
            return (StatusCode::BAD_GATEWAY, format!("relay upstream: {error}")).into_response();
        }
    };
    if matches!(upstream.status().as_u16(), 401 | 403)
        && let Some(refresh) = route.refresh.as_ref()
    {
        let challenge = awaken_ext_mcp::AuthChallenge {
            status: upstream.status().as_u16(),
            www_authenticate: upstream
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        };
        let refreshed_credential = tokio::select! {
            biased;
            _ = call.cancelled() => {
                return (StatusCode::NOT_FOUND, "draining relay route").into_response();
            }
            result = refresh.0.refresh(&challenge) => result,
        };
        if let Some(awaken_ext_mcp::Credential::Bearer(bearer)) = refreshed_credential {
            // The Session slot owns authority. Never enter that owner while
            // holding the route map mutex; refresh must not introduce a lock
            // order dependency between the two projections.
            if !state.admits(&route, &capability) {
                return (StatusCode::NOT_FOUND, "unknown relay route").into_response();
            }
            let refreshed = {
                let Ok(mut routes) = routes.lock() else {
                    return (StatusCode::NOT_FOUND, "unknown relay route").into_response();
                };
                let Some(current) = routes.get_mut(&route_key) else {
                    return (StatusCode::NOT_FOUND, "unknown relay route").into_response();
                };
                if current.capability != capability || current.generation != route.generation {
                    return (StatusCode::NOT_FOUND, "unknown relay route").into_response();
                }
                current.bearer = Some(awaken_agent_contract::RedactedString::from(bearer));
                current.clone()
            };
            if auth_retry_safety == awaken_ext_mcp::AuthRetrySafety::Never {
                // Credential rotation is useful for the next request, but the
                // challenged effectful/unknown request is not evidence-safe to
                // replay. Return its original upstream challenge unchanged.
                let _ = refreshed;
            } else {
                if !state.admits(&refreshed, &capability) {
                    return (StatusCode::NOT_FOUND, "unknown relay route").into_response();
                }
                let retry_send = tokio::select! {
                    biased;
                    _ = call.cancelled() => {
                        return (StatusCode::NOT_FOUND, "draining relay route").into_response();
                    }
                    result = send(&refreshed).send() => result,
                };
                upstream = match retry_send {
                    Ok(response) => response,
                    Err(error) => {
                        return (StatusCode::BAD_GATEWAY, format!("relay upstream: {error}"))
                            .into_response();
                    }
                };
            }
        }
    }
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let response_headers = awaken_mcp_wire::http::relay_response_headers(
        upstream
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_bytes())),
    );
    if call.is_cancelled() {
        return (StatusCode::NOT_FOUND, "draining relay route").into_response();
    }
    // Stream the response body straight through — MCP Streamable HTTP replies as an
    // SSE stream (`text/event-stream`), so buffering would stall long-poll notifications.
    // The opaque permit moves into the stream state: a cloned Route can no
    // longer escape drain merely because Axum has already returned headers.
    let upstream = Box::pin(upstream.bytes_stream());
    let stream =
        futures_util::stream::unfold((upstream, call), |(mut upstream, call)| async move {
            let next = tokio::select! {
                biased;
                _ = call.cancelled() => return None,
                next = upstream.next() => next,
            };
            next.map(|item| (item, (upstream, call)))
        });
    let mut response = Response::builder().status(status);
    for (name, value) in response_headers {
        response = response.header(name.as_str(), value.as_slice());
    }
    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| (StatusCode::BAD_GATEWAY, "relay response build").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_ext_mcp::{Credential, HttpTransportBuilder, McpToolTransport};
    use std::sync::Mutex;

    #[test]
    fn relay_admits_only_a_readable_live_root() {
        // Rows A1-A4 of `relay_admits_root`: the sandbox-facing answer is the
        // same refusal for refused, absent and unreadable roots.
        assert!(relay_admits_root(Ok(Some(true))), "A1");
        assert!(!relay_admits_root(Ok(Some(false))), "A2");
        assert!(!relay_admits_root(Ok(None)), "A3");
        assert!(
            !relay_admits_root(Err(
                awaken_runtime_contract::runs::RunError::unavailable_classified(
                    "session_realization_lock_poisoned",
                    "installed Sessions lock poisoned",
                )
            )),
            "A4"
        );
    }

    fn generation(
        session: &str,
        attachment: &str,
        generation: u64,
    ) -> awaken_session_configuration_contract::mcp::McpGenerationRef {
        crate::test_mcp::mcp_fence(session, attachment, generation, "runtime-1", 4, u64::MAX)
    }

    fn http_material(
        name: impl Into<String>,
        url: impl Into<String>,
        bearer: Option<String>,
    ) -> McpTransportMaterial {
        McpTransportMaterial {
            name: name.into(),
            prompts_as_skills: false,
            transport: crate::mcp_material::McpTransportMaterialKind::Http {
                url: url.into(),
                bearer: bearer.map(awaken_agent_contract::RedactedString::from),
                refresh: None,
            },
        }
    }

    struct FixedProxyIssuer {
        hosts: Mutex<Vec<String>>,
        fails: bool,
    }

    impl McpRelayOutboundProxy for FixedProxyIssuer {
        fn issue_for(&self, upstream_host: &str, _now_unix_ms: u64) -> Result<String, String> {
            self.hosts.lock().unwrap().push(upstream_host.to_owned());
            if self.fails {
                Err("issuer unavailable".into())
            } else {
                Ok("http://capability:x@127.0.0.1:8091".into())
            }
        }
    }

    #[test]
    fn exposed_relay_uses_exact_host_issuer_and_never_falls_back_to_direct_egress() {
        // Causes: C1 HTTPS target with canonical host, C2 issuer available,
        // C3 HTTP/invalid target or issuer failure. Effects: E1 one host-scoped
        // proxy client; E2 closed refusal, never the reusable direct client.
        // Decision rows: R1 C1+C2+!C3 -> E1; R2 C3 -> E2.
        let issuer = Arc::new(FixedProxyIssuer {
            hosts: Mutex::new(Vec::new()),
            fails: false,
        });
        let state = RelayState {
            routes: Arc::new(InvariantLock::new(HashMap::new())),
            stopping: awaken_runtime_contract::CancellationToken::new(),
            authority: Arc::new(|_, _| true),
            clock: awaken_time::system_clock(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            outbound_proxy: Some(issuer.clone()),
        };
        let route = |url: &str| Route {
            url: url.into(),
            bearer: None,
            refresh: None,
            capability: "cap".into(),
            generation: generation("session", "attachment", 1),
            call_fence: awaken_ext_mcp::transport::McpCallFence::open(),
        };
        assert!(
            state
                .client_for(&route("https://Mcp.Example.test/rpc"))
                .is_ok()
        );
        assert_eq!(
            issuer.hosts.lock().unwrap().clone(),
            vec!["mcp.example.test".to_owned()],
            "R1/E1"
        );
        assert!(
            state
                .client_for(&route("http://mcp.example.test/rpc"))
                .is_err()
        );
        assert_eq!(issuer.hosts.lock().unwrap().len(), 1, "R2/E2 no issuer");
        let failed = RelayState {
            outbound_proxy: Some(Arc::new(FixedProxyIssuer {
                hosts: Mutex::new(Vec::new()),
                fails: true,
            })),
            ..state
        };
        assert!(
            failed
                .client_for(&route("https://mcp.example.test/rpc"))
                .is_err()
        );
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
    async fn external_exposure_binds_exactly_and_preserves_default_loopback() {
        // Cause/effect table: R1 no external exposure + route => the sole relay
        // keeps its default loopback origin. R2 validated external bind/origin +
        // route => the listener owns that Worker port and the Sandbox sees only
        // the advertised origin plus its opaque capability. Neither rule
        // projects the bind coordinate or credential material.
        let default = McpRelay::start().await.unwrap();
        let default_generation = generation("session-default", "docs", 2);
        default.set_route(
            &default_generation,
            &http_material(
                "docs",
                "http://upstream.example.test/mcp",
                Some("secret".into()),
            ),
        );
        let default_route = default.route_url(&default_generation).unwrap();
        assert!(
            default_route.starts_with("http://127.0.0.1:"),
            "R1 default remains loopback: {default_route}"
        );

        let reservation = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let exposure = awaken_session_environment_contract::ContainerMcpRelayExposure::new(
            format!("0.0.0.0:{port}").parse().unwrap(),
            "http://awaken-worker.awaken-cloud.svc:8081",
        )
        .unwrap();
        let relay = McpRelay::start_with_authority_and_exposure(
            Arc::new(|_, _| true),
            Some(&exposure),
            awaken_time::system_clock(),
            None,
        )
        .await
        .unwrap();
        let generation = generation("session-external", "docs", 3);
        relay.set_route(
            &generation,
            &http_material(
                "docs",
                "http://upstream.example.test/mcp",
                Some("secret".into()),
            ),
        );
        let route = relay.route_url(&generation).unwrap();
        assert!(
            route.starts_with("http://awaken-worker.awaken-cloud.svc:8081/"),
            "R2 {route}"
        );
        assert!(!route.contains("0.0.0.0"), "R2 bind is private");
        assert!(!route.contains("secret"), "R2 material is private");
        assert!(
            std::net::TcpListener::bind(("0.0.0.0", port)).is_err(),
            "R2 exact port is owned by the running relay"
        );
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

        let routes = relay
            .routes
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"));
        assert!(!routes.contains_key(&route_key(&old)));
        assert!(routes.contains_key(&route_key(&new)));
        assert!(routes.contains_key(&route_key(&other)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accepted_route_request_is_counted_until_cancellation_drop_settles() {
        use std::sync::Condvar;
        use std::task::Poll;

        struct DropGate(Arc<(Mutex<bool>, Condvar)>);

        impl Drop for DropGate {
            fn drop(&mut self) {
                let (released, signal) = self.0.as_ref();
                let mut released = released.lock().expect("relay drop gate mutex");
                while !*released {
                    released = signal.wait(released).expect("relay drop gate wait");
                }
            }
        }

        // Cause/effect graph: C1 an exact live Route accepts a request before
        // drain; C2 close cancels while request-body cleanup is still active;
        // C3 a later request reaches the same route; C4 cleanup settles.
        // Effects: E1 the accepted handler remains counted; E2 close cannot
        // return a false quiescence proof during C2; E3 C3 is rejected before
        // forwarding/credential use; E4 only C4 releases the final permit.
        // Decision rules: A1=C1+!C2=>E1; A2=C1+C2+!C4=>E2;
        // A3=C2+C3=>E3; A4=C1+C2+C4=>E4. The Route remains the unique exact-
        // generation effect owner; the test does not create another registry.
        let relay = McpRelay::start().await.unwrap();
        let generation = generation("relay-drain", "mcp-docs", 1);
        relay.set_route(
            &generation,
            &http_material("docs", "https://example.invalid/mcp", Some("secret".into())),
        );
        let key = route_key(&generation);
        let capability = relay
            .routes
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .get(&key)
            .unwrap()
            .capability
            .clone();
        let started = Arc::new(tokio::sync::Notify::new());
        let drop_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let body = {
            let started = started.clone();
            let drop_gate = DropGate(drop_gate.clone());
            let stream = futures_util::stream::poll_fn(move |_| {
                let _keep_drop_gate = &drop_gate;
                started.notify_one();
                Poll::<Option<Result<axum::body::Bytes, std::io::Error>>>::Pending
            });
            Body::from_stream(stream)
        };
        let accepted = tokio::spawn(forward(
            State(relay.state.clone()),
            Path((
                generation.session_id().to_string(),
                generation.attachment_id().0.clone(),
                generation.generation().version(),
                capability.clone(),
            )),
            Request::builder().method("POST").body(body).unwrap(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(30), started.notified())
            .await
            .expect("timed out waiting for started.notified()");

        let fence = relay.route_call_fence(&generation).unwrap();
        let close = tokio::spawn(async move { fence.close_and_wait().await });
        tokio::task::yield_now().await;
        assert!(!close.is_finished(), "A2/E2");
        let rejected = forward(
            State(relay.state.clone()),
            Path((
                generation.session_id().to_string(),
                generation.attachment_id().0.clone(),
                generation.generation().version(),
                capability,
            )),
            Request::builder()
                .method("POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(rejected.status(), StatusCode::NOT_FOUND, "A3/E3");
        assert!(
            relay
                .routes
                .lock()
                .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                .contains_key(&key),
            "A2 exact Route remains installed while its activity settles"
        );

        let (released, signal) = drop_gate.as_ref();
        *released.lock().expect("relay release mutex") = true;
        signal.notify_all();
        assert_eq!(
            accepted.await.unwrap().status(),
            StatusCode::NOT_FOUND,
            "A4"
        );
        close.await.unwrap();
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
        live = live.expiring_at(u64::MAX - 1);
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
        renewed = renewed.expiring_at(u64::MAX);
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
                .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                .get(&route_key(&renewed))
                .unwrap()
                .generation
                .lease_expires_at_unix_ms(),
            u64::MAX,
            "V4 extended expiry"
        );

        let mut expired = generation("session-expired", "mcp-github", 1);
        expired = expired.expiring_at(0);
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

    // Causes: C1 exact route and bearer; C2 stateful upstream returns a session
    // id; C3 client uses it on following POST/GET/DELETE; C4 upstream Cookie.
    // R1 C1+C2+C3 -> full authenticated flow and SSE; R2 !C3 -> refusal;
    // R3 DELETE -> following call refused; R4 C4 -> Cookie stripped. Session
    // continuity belongs to the upstream/client, never a second relay store.
    #[tokio::test]
    async fn injected_credential_preserves_the_complete_mcp_tool_flow() {
        let (upstream, seen) =
            crate::test_mcp::start_stateful(Some("Bearer relay-only-secret")).await;

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

        let client = reqwest::Client::new();
        let route = relay.route_url(&generation).unwrap();
        let response = client
            .post(&route)
            .json(&serde_json::json!({
                "jsonrpc":"2.0", "id":91, "method":"initialize", "params":{}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK, "R1");
        assert!(response.headers().get("set-cookie").is_none(), "R4");
        assert_eq!(
            response.headers().get("mcp-protocol-version").unwrap(),
            "2025-06-18",
            "R1"
        );
        let session = response.headers().get("mcp-session-id").unwrap().clone();
        let missing = client
            .post(&route)
            .json(&serde_json::json!({
                "jsonrpc":"2.0", "id":92, "method":"tools/list", "params":{}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::BAD_REQUEST, "R2");
        let events = client
            .get(&route)
            .header("mcp-session-id", session.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(
            events.headers().get("content-type").unwrap(),
            "text/event-stream",
            "R1"
        );
        assert!(
            events
                .text()
                .await
                .unwrap()
                .contains("notifications/tools/list_changed"),
            "R1"
        );
        let deleted = client
            .delete(&route)
            .header("mcp-session-id", session.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT, "R3");
        let later = client
            .post(&route)
            .header("mcp-session-id", session)
            .json(&serde_json::json!({"jsonrpc":"2.0", "id":93, "method":"tools/list"}))
            .send()
            .await
            .unwrap();
        assert_eq!(later.status(), reqwest::StatusCode::BAD_REQUEST, "R3");

        let seen = seen.lock();
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
    async fn relay_reloads_bearer_once_after_auth_challenge() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ReloadBearer {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl awaken_ext_mcp::CredentialRefresher for ReloadBearer {
            async fn refresh(
                &self,
                _challenge: &awaken_ext_mcp::AuthChallenge,
            ) -> Option<awaken_ext_mcp::Credential> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Some(awaken_ext_mcp::Credential::Bearer("current-token".into()))
            }
        }

        let (upstream, _) = crate::test_mcp::start(Some("Bearer current-token")).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let material = McpTransportMaterial {
            name: "rotating".into(),
            prompts_as_skills: false,
            transport: crate::mcp_material::McpTransportMaterialKind::Http {
                url: upstream,
                bearer: Some(awaken_agent_contract::RedactedString::from(
                    "expired-token".to_string(),
                )),
                refresh: Some(Box::new(crate::mcp_material::McpRefreshMaterial(Arc::new(
                    ReloadBearer {
                        calls: calls.clone(),
                    },
                )))),
            },
        };
        let relay = McpRelay::start().await.unwrap();
        let generation = generation("session-refresh", "mcp-rotating", 1);
        relay.set_route(&generation, &material);

        let transport = HttpTransportBuilder::new(relay.route_url(&generation).unwrap())
            .credential(Credential::None)
            .build();
        transport
            .list_tools()
            .await
            .expect("the refreshed route remains usable");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            relay
                .routes
                .lock()
                .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                .get(&route_key(&generation))
                .and_then(|route| route.bearer.as_ref())
                .map(awaken_agent_contract::RedactedString::expose_secret),
            Some("current-token")
        );
    }

    // Boundary decision table: C1 admission initially has authority; C2 body
    // buffering revokes it; C3 credential refresh revokes it after a 401.
    // B1=C1+C2 => 404, zero upstream calls; B2=C1+C3 => 404, one challenged
    // call and no retry or credential write. C2/C3 are exclusive injection
    // points; ordinary refresh and effectful non-replay have sibling tests.
    // Every authority callback also asserts the route mutex is unlocked, so
    // Session-authority reads cannot create a route -> Session lock order.
    #[tokio::test]
    async fn relay_rechecks_authority_after_body_and_credential_io() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct RevokeOnRefresh(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl awaken_ext_mcp::CredentialRefresher for RevokeOnRefresh {
            async fn refresh(&self, _: &awaken_ext_mcp::AuthChallenge) -> Option<Credential> {
                self.0.store(false, Ordering::SeqCst);
                Some(Credential::Bearer("new-token".into()))
            }
        }

        for during_body in [true, false] {
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = axum::Router::new().route(
                "/",
                axum::routing::post(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        StatusCode::UNAUTHORIZED
                    }
                }),
            );
            let upstream = tokio::spawn(async move { axum::serve(listener, app).await });
            let live = Arc::new(AtomicBool::new(true));
            let relay = McpRelay::start().await.unwrap();
            let generation = generation("boundary", "browser", 1);
            let mut material = http_material(
                "browser",
                format!("http://{addr}/"),
                Some("old-token".into()),
            );
            let crate::mcp_material::McpTransportMaterialKind::Http { refresh, .. } =
                &mut material.transport
            else {
                unreachable!()
            };
            *refresh = Some(Box::new(crate::mcp_material::McpRefreshMaterial(Arc::new(
                RevokeOnRefresh(live.clone()),
            ))));
            relay.set_route(&generation, &material);
            let key = route_key(&generation);
            let capability = relay
                .routes
                .lock()
                .unwrap_or_else(|poisoned| panic!("{poisoned}"))[&key]
                .capability
                .clone();
            let mut state = relay.state.clone();
            let routes = relay.routes.clone();
            let authority = live.clone();
            state.authority = Arc::new(move |_, _| {
                assert!(
                    routes.try_lock().is_ok(),
                    "authority is read outside route mutex"
                );
                authority.load(Ordering::SeqCst)
            });
            let body = Body::from_stream(futures_util::stream::once(async move {
                if during_body {
                    live.store(false, Ordering::SeqCst);
                }
                Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                    br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                ))
            }));
            let response = forward(
                State(state),
                Path((key.0.clone(), key.1.clone(), key.2, capability)),
                Request::builder().method("POST").body(body).unwrap(),
            )
            .await;
            upstream.abort();
            let _ = upstream.await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(!during_body));
            assert_eq!(
                relay
                    .routes
                    .lock()
                    .unwrap_or_else(|poisoned| panic!("{poisoned}"))[&key]
                    .bearer
                    .as_ref()
                    .unwrap()
                    .expose_secret(),
                "old-token"
            );
        }
    }

    #[tokio::test]
    async fn relay_rotates_but_never_replays_an_auth_challenged_tool_call() {
        /*
         * Auth-recovery cause/effect table (ADR-0067): C1 the current bearer is
         * expired; C2 refresh returns a valid bearer; C3 method is tools/call
         * (effectful). E1 refresh exactly once; E2 original 401 is returned;
         * E3 upstream invocation count stays one; E4 the rotated bearer is
         * retained for a later request. Rule A1=C1+C2+C3=>E1+E2+E3+E4. The
         * sibling read-only rule is covered by relay_reloads_bearer_once... .
         * FMECA control: an auth challenge may mean the server executed before
         * rejecting/losing its response, so automatic tools/call replay has
         * severity-5 duplicate-effect risk and is forbidden.
         */
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone)]
        struct UpstreamState {
            calls: Arc<AtomicUsize>,
        }

        async fn auth_upstream(State(state): State<UpstreamState>, req: Request) -> Response {
            state.calls.fetch_add(1, Ordering::SeqCst);
            let current = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                == Some("Bearer current-token");
            if current {
                StatusCode::OK.into_response()
            } else {
                StatusCode::UNAUTHORIZED.into_response()
            }
        }

        struct ReloadBearer(Arc<AtomicUsize>);

        #[async_trait::async_trait]
        impl awaken_ext_mcp::CredentialRefresher for ReloadBearer {
            async fn refresh(
                &self,
                _challenge: &awaken_ext_mcp::AuthChallenge,
            ) -> Option<awaken_ext_mcp::Credential> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Some(awaken_ext_mcp::Credential::Bearer("current-token".into()))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/", axum::routing::any(auth_upstream))
            .with_state(UpstreamState {
                calls: calls.clone(),
            });
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let refreshes = Arc::new(AtomicUsize::new(0));
        let material = McpTransportMaterial {
            name: "effectful-rotation".into(),
            prompts_as_skills: false,
            transport: crate::mcp_material::McpTransportMaterialKind::Http {
                url: format!("http://{upstream}/"),
                bearer: Some(awaken_agent_contract::RedactedString::from(
                    "expired-token".to_string(),
                )),
                refresh: Some(Box::new(crate::mcp_material::McpRefreshMaterial(Arc::new(
                    ReloadBearer(refreshes.clone()),
                )))),
            },
        };
        let relay = McpRelay::start().await.unwrap();
        let generation = generation("session-effect", "mcp-effect", 1);
        relay.set_route(&generation, &material);
        let route = relay.route_url(&generation).unwrap();
        let client = reqwest::Client::new();

        let challenged = client
            .post(&route)
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "charge", "arguments": {} }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            challenged.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "A1/E2"
        );
        assert_eq!(refreshes.load(Ordering::SeqCst), 1, "A1/E1");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "A1/E3");

        let later = client
            .post(route)
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(later.status(), reqwest::StatusCode::OK, "A1/E4");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one later request only");
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

    /// Live end-to-end against the REAL GitHub MCP (`api.githubcopilot.com/mcp/`).
    /// Explicit selection requires `AWAKEN_GITHUB_MCP_TOKEN` (a fine-grained PAT or
    /// installation token). Proves the Managed path: the workload sends no credential and the relay
    /// injects the real token, so GitHub MCP authenticates the call.
    #[tokio::test]
    #[ignore = "requires an explicitly supplied live GitHub MCP credential"]
    async fn relay_reaches_the_real_github_mcp_when_a_token_is_present() {
        // C1 explicitly selected live qualification, C2 credential available.
        // R1 C1+C2 -> real upstream acceptance; R2 C1+!C2 -> failure, never a
        // successful skipped test. Ordinary runs report this case as ignored.
        let token = std::env::var("AWAKEN_GITHUB_MCP_TOKEN")
            .expect("live GitHub MCP qualification requires AWAKEN_GITHUB_MCP_TOKEN");
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
            "{}/t-unknown/mcp-nope/1/not-a-route-capability",
            relay.advertised_origin
        );
        let status = reqwest::Client::new()
            .post(unknown)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    }
    #[tokio::test]
    async fn captured_route_cleanup_preserves_same_tenure_reinsertion_and_is_idempotent() {
        let upstream = fake_upstream().await;
        let relay = McpRelay::start().await.unwrap();
        let generation = generation("same-tenure-reinsertion", "docs", 1);
        let selected = relay
            .stage_route_captured(
                &generation,
                &http_material(
                    "docs",
                    format!("http://{upstream}/"),
                    Some("old-secret".into()),
                ),
            )
            .unwrap();
        let old_url = relay.route_url(&generation).unwrap();
        selected.call_fence().close_and_wait().await;
        selected.remove_if_current();
        assert!(relay.route_url(&generation).is_none());
        assert!(relay.stage_route(
            &generation,
            &http_material(
                "docs",
                format!("http://{upstream}/"),
                Some("new-secret".into())
            )
        ));
        let current_url = relay.route_url(&generation).unwrap();
        assert_ne!(
            old_url, current_url,
            "same tenure is a distinct physical insertion capability"
        );
        selected.remove_if_current();
        selected.remove_if_current();
        assert_eq!(
            relay.route_url(&generation).as_deref(),
            Some(current_url.as_str())
        );
        let response = reqwest::Client::new()
            .post(&current_url)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "Bearer new-secret");
        assert_eq!(
            reqwest::Client::new()
                .post(&old_url)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn captured_orphan_routes_do_not_rescan_a_later_session_insertion() {
        let upstream = fake_upstream().await;
        let relay = McpRelay::start().await.unwrap();
        let old = generation("captured-session", "docs", 1);
        let successor = generation("captured-session", "docs", 2);
        let unrelated = generation("other-session", "docs", 1);
        let server = http_material("docs", format!("http://{upstream}/"), Some("token".into()));
        assert!(relay.stage_route(&old, &server));
        assert!(relay.stage_route(&unrelated, &server));
        // There is deliberately no projection: capture selects the existing
        // physical map record, not merely published generation projections.
        let selected = relay.capture_routes("captured-session");
        assert_eq!(selected.len(), 1);
        let old_url = relay.route_url(&old).unwrap();
        assert!(relay.stage_route(&successor, &server));
        let successor_url = relay.route_url(&successor).unwrap();
        for route in &selected {
            route.call_fence().close_and_wait().await;
            route.remove_if_current();
            route.remove_if_current();
        }
        assert_eq!(
            reqwest::Client::new()
                .post(&old_url)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );
        assert_eq!(
            reqwest::Client::new()
                .post(&successor_url)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        assert_eq!(
            reqwest::Client::new()
                .post(relay.route_url(&unrelated).unwrap())
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
    }
}

#[cfg(test)]
mod relay_lifetime_tests;
