//! Anthropic-compatible Files API over the Resources context's one FileCatalog and
//! one content-addressed FileStore. A public `file_...` identity never exposes the
//! private BLAKE3 blob id, and every GET is a pure catalog/blob read.

use std::sync::Arc;

use crate::common::headers::ManagedCapability;
use crate::common::scope::RequiredWorkspaceScope;
use crate::resources::flavor::{
    BetaQueryPolicy, ManagedResourceApiFlavor, resource_api_flavor, without_beta_selector,
};
use crate::types::file::{
    BetaFileListParams, BetaFileMetadata, BetaFileScope, DeletedFile, DeletedFileObjectType,
    FileExpirySeconds, FileListParams, FileMetadata, FileObjectType, FileScopeObjectType,
};
use crate::types::{Page, PageCursor, PageQuery, paginate};
use awaken_resource_contract::{FileApplicationService, FileRecord, ResourcePurgeError};
use axum::extract::{Multipart, Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

use super::managed_resource_error as error;

const DEFAULT_PAGE_SIZE: usize = 20;
const MAX_PAGE_SIZE: usize = 1_000;
pub fn files_router(files: Arc<dyn FileApplicationService>) -> Router {
    Router::new()
        .route("/v1/files", post(upload_file).get(list_files))
        .route("/v1/files/{id}", get(get_file).delete(delete_file))
        .route("/v1/files/{id}/content", get(download_file))
        .with_state(files)
}

fn ga_metadata(record: &FileRecord) -> FileMetadata {
    FileMetadata {
        id: record.id.clone(),
        created_at: record.created_at.clone(),
        filename: record.filename.clone(),
        mime_type: record.mime_type.clone(),
        size_bytes: record.size_bytes,
        kind: FileObjectType::File,
        downloadable: Some(record.downloadable),
        expires_at: record.expires_at.clone(),
    }
}

fn beta_metadata(record: &FileRecord) -> BetaFileMetadata {
    BetaFileMetadata {
        id: record.id.clone(),
        kind: FileObjectType::File,
        filename: record.filename.clone(),
        mime_type: record.mime_type.clone(),
        size_bytes: record.size_bytes,
        created_at: record.created_at.clone(),
        downloadable: Some(record.downloadable),
        scope: record.scope_id.as_ref().map(|id| BetaFileScope {
            id: id.clone(),
            kind: FileScopeObjectType::Session,
        }),
    }
}

async fn list_files(
    State(files): State<Arc<dyn FileApplicationService>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> impl IntoResponse {
    let flavor = match resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Files,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        Ok(flavor) => flavor,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    match flavor {
        ManagedResourceApiFlavor::Beta => list_beta_files(files, &workspace, raw.as_deref()).await,
        ManagedResourceApiFlavor::Ga => list_ga_files(files, &workspace, raw.as_deref()).await,
    }
}

async fn list_beta_files(
    files: Arc<dyn FileApplicationService>,
    workspace: &str,
    raw: Option<&str>,
) -> axum::response::Response {
    let query = match serde_urlencoded::from_str::<BetaFileListParams>(&without_beta_selector(raw))
    {
        Ok(query) => query,
        Err(error_value) => return error(StatusCode::BAD_REQUEST, error_value.to_string()),
    };
    let before_id = query.before_id.as_ref();
    let after_id = query.after_id.as_ref();
    if before_id.is_some() && after_id.is_some() {
        return error(
            StatusCode::BAD_REQUEST,
            "before_id and after_id cannot be used together",
        );
    }
    let limit = query.limit.map_or(DEFAULT_PAGE_SIZE, usize::from);
    if !(1..=MAX_PAGE_SIZE).contains(&limit) {
        return error(
            StatusCode::BAD_REQUEST,
            format!("limit must be between 1 and {MAX_PAGE_SIZE}"),
        );
    }
    let records = match files.list(workspace, query.scope_id.as_deref()).await {
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
    let data = selected.into_iter().map(beta_metadata).collect::<Vec<_>>();
    Json(Page::new(data, has_more, first_id, last_id)).into_response()
}

async fn list_ga_files(
    files: Arc<dyn FileApplicationService>,
    workspace: &str,
    raw: Option<&str>,
) -> axum::response::Response {
    let query = match FileListParams::from_query(&without_beta_selector(raw)) {
        Ok(query) => query,
        Err(error_value) => return error(StatusCode::BAD_REQUEST, error_value.to_string()),
    };
    let records = match files.list(workspace, None).await {
        Ok(records) => records,
        Err(error_value) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, error_value.to_string());
        }
    };
    if let Some(ids) = query.ids {
        if query.page.is_some() || query.limit.is_some() {
            return error(
                StatusCode::BAD_REQUEST,
                "ids[] is mutually exclusive with page and limit",
            );
        }
        let mut ids = ids;
        ids.sort();
        ids.dedup();
        if ids.len() > 100 {
            return error(
                StatusCode::BAD_REQUEST,
                "ids[] accepts at most 100 unique entries",
            );
        }
        let selected = records
            .iter()
            .filter(|record| ids.binary_search(&record.id).is_ok())
            .map(ga_metadata)
            .collect();
        return Json(PageCursor::single(selected)).into_response();
    }
    let page = PageQuery {
        page: query.page,
        limit: query.limit.map(usize::from),
    };
    let data = records.iter().map(ga_metadata).collect::<Vec<_>>();
    Json(paginate(data, &page, |file| file.id.as_str())).into_response()
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
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let flavor = match resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Files,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        Ok(flavor) => flavor,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let mut upload: Option<(String, String, Vec<u8>)> = None;
    let mut expiry_seconds = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("expires_in_seconds") {
            if flavor == ManagedResourceApiFlavor::Beta {
                return error(
                    StatusCode::BAD_REQUEST,
                    "expires_in_seconds is only available in GA Files",
                );
            }
            let value = match field
                .text()
                .await
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
            {
                Some(value) => value,
                None => {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "expires_in_seconds must be an integer",
                    );
                }
            };
            expiry_seconds = match FileExpirySeconds::new(value) {
                Ok(value) => Some(value),
                Err(message) => return error(StatusCode::BAD_REQUEST, message),
            };
            continue;
        }
        if field.name() != Some("file") {
            return error(StatusCode::BAD_REQUEST, "unsupported Files multipart field");
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
    let expires_at = expiry_seconds.map(|expiry| {
        (chrono::Utc::now() + chrono::Duration::seconds(expiry.get() as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    });
    match files
        .create_uploaded_file_with_expiry(&workspace, filename, mime_type, &bytes, expires_at)
        .await
    {
        Ok(record) => match flavor {
            ManagedResourceApiFlavor::Beta => {
                (StatusCode::OK, Json(beta_metadata(&record))).into_response()
            }
            ManagedResourceApiFlavor::Ga => {
                (StatusCode::OK, Json(ga_metadata(&record))).into_response()
            }
        },
        Err(ResourcePurgeError::Invalid(message)) => error(StatusCode::BAD_REQUEST, message),
        Err(error_value) => error(StatusCode::INTERNAL_SERVER_ERROR, error_value.to_string()),
    }
}

async fn get_file(
    State(files): State<Arc<dyn FileApplicationService>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> impl IntoResponse {
    let flavor = match resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Files,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        Ok(flavor) => flavor,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    match files.get(&workspace, &id).await {
        Ok(Some(record)) => match flavor {
            ManagedResourceApiFlavor::Beta => {
                (StatusCode::OK, Json(beta_metadata(&record))).into_response()
            }
            ManagedResourceApiFlavor::Ga => {
                (StatusCode::OK, Json(ga_metadata(&record))).into_response()
            }
        },
        Ok(None) => error(StatusCode::NOT_FOUND, "file not found"),
        Err(error_value) => error(StatusCode::INTERNAL_SERVER_ERROR, error_value.to_string()),
    }
}

async fn delete_file(
    State(files): State<Arc<dyn FileApplicationService>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> impl IntoResponse {
    if let Err(message) = resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Files,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default();
    match files.delete(&workspace, &id, now).await {
        Ok(Some(_)) => (
            StatusCode::OK,
            Json(DeletedFile {
                id,
                kind: DeletedFileObjectType::FileDeleted,
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
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> impl IntoResponse {
    if let Err(message) = resource_api_flavor(
        raw.as_deref(),
        &headers,
        ManagedCapability::Files,
        BetaQueryPolicy::QuerySelectsGa,
    ) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    match files.bytes(&workspace, &id).await {
        Ok(Some((record, _))) if !record.downloadable => {
            error(StatusCode::BAD_REQUEST, "file is not downloadable")
        }
        Ok(Some((record, _)))
            if record.expires_at.as_deref().is_some_and(|expires_at| {
                chrono::DateTime::parse_from_rfc3339(expires_at)
                    .is_ok_and(|expiry| expiry <= chrono::Utc::now())
            }) =>
        {
            error(StatusCode::NOT_FOUND, "file content has expired")
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
        let plain = BetaFileMetadata {
            id: "file_1".into(),
            kind: FileObjectType::File,
            filename: "notes.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 5,
            created_at: "2026-01-01T00:00:00Z".into(),
            downloadable: Some(true),
            scope: None,
        };
        let plain = serde_json::to_value(plain).unwrap();
        assert!(plain["scope"].is_null(), "F1");
        assert_eq!(plain.as_object().unwrap().len(), 8, "F1 exact fields");

        let scoped = BetaFileMetadata {
            id: "file_2".into(),
            kind: FileObjectType::File,
            filename: "notes.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 5,
            created_at: "2026-01-01T00:00:00Z".into(),
            downloadable: Some(true),
            scope: Some(BetaFileScope {
                id: "session_1".into(),
                kind: FileScopeObjectType::Session,
            }),
        };
        assert_eq!(
            serde_json::to_value(scoped).unwrap()["scope"]["type"],
            "session",
            "F2"
        );
        let deleted = serde_json::to_value(DeletedFile {
            id: "file_2".into(),
            kind: DeletedFileObjectType::FileDeleted,
        })
        .unwrap();
        assert_eq!(deleted["type"], "file_deleted", "F3");
        assert_eq!(deleted.as_object().unwrap().len(), 2, "F3 exact fields");

        let ga = serde_json::to_value(FileMetadata {
            id: "file_3".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            filename: "notes.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 5,
            kind: FileObjectType::File,
            downloadable: Some(false),
            expires_at: Some("2026-02-01T00:00:00Z".into()),
        })
        .unwrap();
        assert!(ga.get("scope").is_none(), "F4 GA omits beta scope");
        assert_eq!(ga["expires_at"], "2026-02-01T00:00:00Z", "F4");
    }
}
