//! The Files API (`/v1/files`) the official `@anthropic-ai/sdk` drives via
//! `client.beta.files.upload` / `.download`. Bytes live in the host's
//! content-addressed blob store (`awaken-file-store`, BLAKE3 id), shared with
//! file-resource mounts (ADR-0038): an uploaded `file_id` a session references in
//! `resources[]` resolves to these same bytes at mount time.

use std::sync::Arc;

use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::host::SharedHost;

/// Mount the Files API over the host's blob store.
pub fn files_router(host: Arc<SharedHost>) -> Router {
    Router::new()
        .route("/v1/files", post(upload_file).get(list_files))
        .route("/v1/files/:id", get(get_file))
        .route("/v1/files/:id/content", get(download_file))
        .with_state(host)
}

/// `GET /v1/files?scope_id=<session>` — the session's output artifacts (ADR-0038):
/// files the agent wrote under `outputs/`, harvested into the blob store. Without a
/// `scope_id` the list is empty (this server scopes files to a session, not globally).
async fn list_files(
    State(host): State<Arc<SharedHost>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let data: Vec<_> = match q.get("scope_id").cloned() {
        Some(session) => host
            .session_artifacts(&session)
            .await
            .into_iter()
            .map(|(id, path)| {
                json!({
                    "id": id,
                    "type": "file",
                    "filename": path,
                    "downloadable": true,
                })
            })
            .collect(),
        None => Vec::new(),
    };
    Json(json!({ "data": data, "has_more": false }))
}

/// `POST /v1/files` (multipart, `purpose=agent`): store the `file` part's bytes and
/// return their content id as `FileMetadata`. Idempotent (equal bytes → same id).
async fn upload_file(
    State(host): State<Arc<SharedHost>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut filename = "upload".to_string();
    let mut bytes: Option<Vec<u8>> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("file") {
            if let Some(name) = field.file_name() {
                filename = name.to_string();
            }
            bytes = field.bytes().await.ok().map(|b| b.to_vec());
        }
        // Other parts (`purpose`, …) are tolerated and ignored.
    }
    let Some(bytes) = bytes else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "multipart request has no `file` part" })),
        )
            .into_response();
    };
    let size = bytes.len();
    match host.file_store().put(&bytes).await {
        Ok(id) => (
            StatusCode::OK,
            Json(json!({
                "id": id,
                "type": "file",
                "filename": filename,
                "mime_type": "application/octet-stream",
                "size_bytes": size,
                "created_at": "1970-01-01T00:00:00Z",
                "downloadable": true,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `GET /v1/files/{id}` — metadata (presence + size).
async fn get_file(
    State(host): State<Arc<SharedHost>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match host.file_store().get(&id).await {
        Ok(Some(bytes)) => (
            StatusCode::OK,
            Json(json!({
                "id": id,
                "type": "file",
                "size_bytes": bytes.len(),
                "downloadable": true,
            })),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("file `{id}` not found") })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `GET /v1/files/{id}/content` — the raw bytes (what `files.download` reads).
async fn download_file(
    State(host): State<Arc<SharedHost>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match host.file_store().get(&id).await {
        Ok(Some(bytes)) => (StatusCode::OK, bytes).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
