// @formal surface requirement=protocol.adapters.adapter_projection
// @formal reason: Registers the read-only history projection under the Awaken namespace.
//! Read-only message history beside a Managed Session.

use axum::{Router, handler::Handler, routing::get};

#[derive(serde::Deserialize)]
pub struct ThreadMessagePageQuery {
    pub page: Option<String>,
    pub limit: Option<usize>,
}

#[derive(serde::Serialize)]
pub struct ThreadHistoryMessage {
    pub id: String,
    pub role: &'static str,
    pub text: String,
}

/// Mount the Session-scoped, read-only message history projection.
pub fn session_thread_history_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route(
        "/v1/awaken/sessions/{id}/threads/{tid}/messages",
        get(handler),
    )
}
