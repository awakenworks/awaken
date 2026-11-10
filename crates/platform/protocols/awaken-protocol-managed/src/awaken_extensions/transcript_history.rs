// @formal surface requirement=protocol.adapters.adapter_projection
// @formal reason: Registers the read-only history projection under the Awaken namespace.
//! Read-only message history beside a Managed Session.

use axum::{Router, handler::Handler, routing::get};

#[derive(serde::Deserialize)]
pub struct ThreadMessagePageQuery {
    pub page: Option<String>,
    pub limit: Option<usize>,
    pub direction: Option<HistoryDirection>,
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryDirection {
    Backward,
}

#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadHistoryAttachment {
    Image { file_id: String },
    Document { file_id: String },
}

#[derive(serde::Serialize)]
pub struct ThreadHistoryMessage {
    pub id: String,
    pub role: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ThreadHistoryAttachment>,
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
