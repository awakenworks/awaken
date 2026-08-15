//! Anthropic-compatible Files API over the Resources context's one FileCatalog and
//! one content-addressed FileStore. A public `file_...` identity never exposes the
//! private BLAKE3 blob id, and every GET is a pure catalog/blob read.

use std::sync::Arc;

use awaken_resource_contract::{FileApplicationService, FileRecord, ResourcePurgeError};
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;

use crate::common::scope::RequiredWorkspaceScope;
use crate::types::{ErrorResponse, Page};

const DEFAULT_PAGE_SIZE: usize = 20;
const MAX_PAGE_SIZE: usize = 1_000;

pub fn files_router(files: Arc<dyn FileApplicationService>) -> Router {
    Router::new()
        .route("/v1/files", post(upload_file).get(list_files))
        .route("/v1/files/{id}", get(get_file).delete(delete_file))
        .route("/v1/files/{id}/content", get(download_file))
        .with_state(files)
}

#[derive(Debug, Serialize)]
struct FileScope<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct FileMetadata<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    filename: &'a str,
    mime_type: &'a str,
    size_bytes: u64,
    created_at: &'a str,
    downloadable: bool,
    scope: Option<FileScope<'a>>,
}

#[derive(Debug, Serialize)]
struct DeletedFile {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
}

fn metadata(record: &FileRecord) -> FileMetadata<'_> {
    FileMetadata {
        id: &record.id,
        kind: "file",
        filename: &record.filename,
        mime_type: &record.mime_type,
        size_bytes: record.size_bytes,
        created_at: &record.created_at,
        downloadable: record.downloadable,
        scope: record.scope_id.as_deref().map(|id| FileScope {
            id,
            kind: "session",
        }),
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> axum::response::Response {
    (
        status,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
        .into_response()
}

async fn list_files(
    State(files): State<Arc<dyn FileApplicationService>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if let Some(name) = query.keys().find(|name| {
        !matches!(
            name.as_str(),
            "before_id" | "after_id" | "limit" | "scope_id" | "beta"
        )
    }) {
        return error(
            StatusCode::BAD_REQUEST,
            format!("unsupported Files list parameter `{name}`"),
        );
    }
    let before_id = query.get("before_id");
    let after_id = query.get("after_id");
    if before_id.is_some() && after_id.is_some() {
        return error(
            StatusCode::BAD_REQUEST,
            "before_id and after_id cannot be used together",
        );
    }
    let limit = match query.get("limit") {
        Some(value) => match value.parse::<usize>() {
            Ok(limit) => limit,
            Err(_) => return error(StatusCode::BAD_REQUEST, "limit must be an integer"),
        },
        None => DEFAULT_PAGE_SIZE,
    };
    if !(1..=MAX_PAGE_SIZE).contains(&limit) {
        return error(
            StatusCode::BAD_REQUEST,
            format!("limit must be between 1 and {MAX_PAGE_SIZE}"),
        );
    }
    let records = match files
        .list(&workspace, query.get("scope_id").map(String::as_str))
        .await
    {
        Ok(records) => records,
        Err(error_value) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, error_value.to_string());
        }
    };
    let start = if let Some(cursor) = after_id.map(String::as_str) {
        match records.iter().position(|record| record.id == cursor) {
            Some(position) => position + 1,
            None => return error(StatusCode::BAD_REQUEST, "after_id was not found"),
        }
    } else {
        0
    };
    let end_bound = if let Some(cursor) = before_id.map(String::as_str) {
        match records.iter().position(|record| record.id == cursor) {
            Some(position) => position,
            None => return error(StatusCode::BAD_REQUEST, "before_id was not found"),
        }
    } else {
        records.len()
    };
    if start > end_bound {
        return error(StatusCode::BAD_REQUEST, "cursor range is empty");
    }
    let selected = records[start..end_bound]
        .iter()
        .take(limit)
        .collect::<Vec<_>>();
    let has_more = start + selected.len() < end_bound;
    let first_id = selected.first().map(|record| record.id.clone());
    let last_id = selected.last().map(|record| record.id.clone());
    let data = selected.into_iter().map(metadata).collect::<Vec<_>>();
    Json(Page::new(data, has_more, first_id, last_id)).into_response()
}

fn valid_filename(filename: &str) -> bool {
    let char_count = filename.chars().count();
    (1..=255).contains(&char_count)
        && !filename
            .chars()
            .any(|character| character.is_control() || "<>:\"|?*\\/".contains(character))
}

async fn upload_file(
    State(files): State<Arc<dyn FileApplicationService>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut upload: Option<(String, String, Vec<u8>)> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() != Some("file") {
            continue;
        }
        let filename = field.file_name().unwrap_or("upload").to_string();
        let mime_type = field
            .content_type()
            .map(str::to_string)
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let Ok(bytes) = field.bytes().await else {
            return error(StatusCode::BAD_REQUEST, "could not read multipart file");
        };
        upload = Some((filename, mime_type, bytes.to_vec()));
    }
    let Some((filename, mime_type, bytes)) = upload else {
        return error(
            StatusCode::BAD_REQUEST,
            "multipart request has no `file` part",
        );
    };
    if !valid_filename(&filename) {
        return error(StatusCode::BAD_REQUEST, "filename is invalid");
    }
    match files
        .create_uploaded_file(&workspace, filename, mime_type, &bytes)
        .await
    {
        Ok(record) => (StatusCode::OK, Json(metadata(&record))).into_response(),
        Err(ResourcePurgeError::Invalid(message)) => error(StatusCode::BAD_REQUEST, message),
        Err(error_value) => error(StatusCode::INTERNAL_SERVER_ERROR, error_value.to_string()),
    }
}

async fn get_file(
    State(files): State<Arc<dyn FileApplicationService>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match files.get(&workspace, &id).await {
        Ok(Some(record)) => (StatusCode::OK, Json(metadata(&record))).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "file not found"),
        Err(error_value) => error(StatusCode::INTERNAL_SERVER_ERROR, error_value.to_string()),
    }
}

async fn delete_file(
    State(files): State<Arc<dyn FileApplicationService>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default();
    match files.delete(&workspace, &id, now).await {
        Ok(Some(_)) => (
            StatusCode::OK,
            Json(DeletedFile {
                id,
                kind: "file_deleted",
            }),
        )
            .into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "file not found"),
        Err(error_value) => error(StatusCode::INTERNAL_SERVER_ERROR, error_value.to_string()),
    }
}

async fn download_file(
    State(files): State<Arc<dyn FileApplicationService>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match files.bytes(&workspace, &id).await {
        Ok(Some((record, _))) if !record.downloadable => {
            error(StatusCode::BAD_REQUEST, "file is not downloadable")
        }
        Ok(Some((record, bytes))) => {
            let mut response = (StatusCode::OK, bytes).into_response();
            if let Ok(value) = HeaderValue::from_str(&record.mime_type) {
                response.headers_mut().insert(header::CONTENT_TYPE, value);
            }
            response
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "file not found"),
        Err(error_value) => error(StatusCode::INTERNAL_SERVER_ERROR, error_value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_file_responses_are_owned_by_typed_dtos() {
        // Cause/effect decision table: F1 unscoped metadata -> `scope:null`;
        // F2 session-scoped metadata -> exact `{id,type}` scope; F3 deletion ->
        // exact delete receipt. In every rule the DTO owns the fixed field set,
        // so a manually assembled alternate envelope cannot drift into the API.
        let plain = FileMetadata {
            id: "file_1",
            kind: "file",
            filename: "notes.txt",
            mime_type: "text/plain",
            size_bytes: 5,
            created_at: "2026-01-01T00:00:00Z",
            downloadable: true,
            scope: None,
        };
        let plain = serde_json::to_value(plain).unwrap();
        assert!(plain["scope"].is_null(), "F1");
        assert_eq!(plain.as_object().unwrap().len(), 8, "F1 exact fields");

        let scoped = FileMetadata {
            id: "file_2",
            kind: "file",
            filename: "notes.txt",
            mime_type: "text/plain",
            size_bytes: 5,
            created_at: "2026-01-01T00:00:00Z",
            downloadable: true,
            scope: Some(FileScope {
                id: "session_1",
                kind: "session",
            }),
        };
        assert_eq!(
            serde_json::to_value(scoped).unwrap()["scope"]["type"],
            "session",
            "F2"
        );
        let deleted = serde_json::to_value(DeletedFile {
            id: "file_2".into(),
            kind: "file_deleted",
        })
        .unwrap();
        assert_eq!(deleted["type"], "file_deleted", "F3");
        assert_eq!(deleted.as_object().unwrap().len(), 2, "F3 exact fields");
    }
}
