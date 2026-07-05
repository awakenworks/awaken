//! The memory-store API (`/v1/memory_stores`) behind the ADR-0038 MemoryStore
//! resource family. Unlike the content-addressed Files API, a memory store has a
//! stable, mutable id: a session mounts it read-write (`resources[{type:
//! "memory_store", id}]`), the agent edits the realized file, and the host harvests
//! the write back under the same id — so memory written in one session is visible to
//! the next. Bytes live in the host's in-memory `memory_stores` map (one process).
//!
//! The official `@anthropic-ai/sdk` has no typed binding for these, so a TS client
//! drives them through the low-level `client.post` / `client.get` request methods.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::host::SharedHost;

/// Mount the memory-store API over the host's mutable memory stores.
pub fn memory_stores_router(host: Arc<SharedHost>) -> Router {
    Router::new()
        .route("/v1/memory_stores", post(create_memory_store))
        .route("/v1/memory_stores/:id", get(get_memory_store))
        .with_state(host)
}

/// `POST /v1/memory_stores` — create a new, empty memory store and return its id.
/// The id is what a session references in `resources[{type:"memory_store", id}]`.
async fn create_memory_store(State(host): State<Arc<SharedHost>>) -> impl IntoResponse {
    let id = host.create_memory_store();
    (
        StatusCode::OK,
        Json(json!({ "id": id, "type": "memory_store" })),
    )
}

/// `GET /v1/memory_stores/{id}` — the store's current bytes as UTF-8 `content`,
/// reflecting any harvested write-back. 404 when the id is unknown.
async fn get_memory_store(
    State(host): State<Arc<SharedHost>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match host.memory_get(&id) {
        Some(bytes) => (
            StatusCode::OK,
            Json(json!({
                "id": id,
                "type": "memory_store",
                "content": String::from_utf8_lossy(&bytes),
                "size_bytes": bytes.len(),
            })),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("memory_store `{id}` not found") })),
        )
            .into_response(),
    }
}
