//! Awaken-only retained Memory proposals; never an Anthropic Memory route.
use axum::Router;
use axum::handler::Handler;
use axum::routing::get;

/// Reuse the Resource adapter's scope, visibility and page projection, just as
/// the Resource Manifest extension injects its method adapter. This crate alone
/// owns the extension path; there is no second Memory application or wire parser.
pub fn memory_copy_conflicts_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/v1/awaken/memory-stores/{id}/copy-conflicts", get(handler))
}
