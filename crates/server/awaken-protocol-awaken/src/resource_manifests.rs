//! Explicit route ownership for Awaken's complete Session Resource Manifest.

use axum::Router;
use axum::handler::Handler;
use axum::routing::put;

/// Mount a caller-supplied Manifest method adapter under the Awaken extension
/// namespace. The injected method owns DTO lowering; this crate exclusively owns
/// the non-Anthropic path so the compatibility router cannot acquire a second
/// protocol surface.
pub fn session_resource_manifest_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/v1/awaken/sessions/{id}/resources", put(handler))
}
