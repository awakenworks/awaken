//! The live-inbox protocol: a small edit surface layered *beside* the Managed
//! Agents session API, not part of it.
//!
//! Claude's Managed Agents wire has no live-inbox concept — queuing, reordering,
//! and withdrawing not-yet-consumed messages against a running attempt is an
//! Awaken extension. It rides the same `/v1/sessions/:id/...` host and the same
//! [`ManagedState`] port, but its routes, bodies, and error contract are its own,
//! so this module stays cleanly separable from the managed-compatible surface.
//!
//! The edit contract maps 1:1 onto HTTP: an id that no longer exists is 404; a
//! stale reorder is 409 (re-GET and retry); an inactive queue is 410 (the attempt
//! is gone — send a normal event instead).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};

use awaken_agent_contract::agent::content::ContentBlock;

use crate::dto::ErrorResponse;
use crate::router::{error_response, ManagedJson};
use crate::state::{LiveInboxSnapshot, ManagedState};

/// Build the live-inbox router. Merged into the managed router at the same host,
/// but a distinct protocol: its own routes, bodies, and error mapping.
pub fn live_inbox_router(state: Arc<ManagedState>) -> Router {
    Router::new()
        .route(
            "/v1/sessions/:id/live-inbox",
            get(live_inbox_snapshot).post(live_inbox_queue),
        )
        .route("/v1/sessions/:id/live-inbox/order", put(live_inbox_reorder))
        .route(
            "/v1/sessions/:id/live-inbox/:msg",
            put(live_inbox_replace).delete(live_inbox_remove),
        )
        .with_state(state)
}

/// Body of `POST /v1/sessions/:id/live-inbox` (queue) and
/// `PUT /v1/sessions/:id/live-inbox/:msg` (replace): the message's block list.
#[derive(serde::Deserialize)]
struct LiveInboxMessageBody {
    content: Vec<ContentBlock>,
}

/// Body of `PUT /v1/sessions/:id/live-inbox/order`: the full permutation of
/// currently queued ids, in the desired consumption order.
#[derive(serde::Deserialize)]
struct LiveInboxOrderBody {
    order: Vec<u64>,
}

/// Response of a successful queue: the id to edit or withdraw the message by.
#[derive(serde::Serialize)]
struct LiveInboxQueuedResponse {
    id: u64,
}

async fn live_inbox_snapshot(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
) -> Result<Json<LiveInboxSnapshot>, (StatusCode, Json<ErrorResponse>)> {
    state
        .live_inbox_snapshot(&id)
        .await
        .map(Json)
        .map_err(error_response)
}

async fn live_inbox_queue(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    ManagedJson(body): ManagedJson<LiveInboxMessageBody>,
) -> Result<Json<LiveInboxQueuedResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .live_inbox_queue(&id, body.content)
        .await
        .map(|id| Json(LiveInboxQueuedResponse { id }))
        .map_err(error_response)
}

async fn live_inbox_remove(
    State(state): State<Arc<ManagedState>>,
    Path((id, msg)): Path<(String, u64)>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    state
        .live_inbox_remove(&id, msg)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(error_response)
}

async fn live_inbox_replace(
    State(state): State<Arc<ManagedState>>,
    Path((id, msg)): Path<(String, u64)>,
    ManagedJson(body): ManagedJson<LiveInboxMessageBody>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    state
        .live_inbox_replace(&id, msg, body.content)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(error_response)
}

async fn live_inbox_reorder(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    ManagedJson(body): ManagedJson<LiveInboxOrderBody>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    state
        .live_inbox_reorder(&id, body.order)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(error_response)
}
