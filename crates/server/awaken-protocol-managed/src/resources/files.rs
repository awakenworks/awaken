//! Anthropic-compatible Files API over the Resources context's one FileCatalog and
//! one content-addressed FileStore. A public `file_...` identity never exposes the
//! private BLAKE3 blob id, and every GET is a pure catalog/blob read.

use std::sync::Arc;

use crate::common::headers::ManagedCapability;
use crate::common::scope::RequiredWorkspaceScope;
use crate::resources::flavor::{
    ManagedResourceApiSurface, resource_api_surface, without_beta_selector,
};
use crate::routes::ManagedMultipart;
use crate::types::file::{
    BetaFileCursorListParams, BetaFileCursorMetadata, BetaFileListParams, BetaFileMetadata,
    BetaFileScope, DeletedFile, DeletedFileObjectType, FileExpirySeconds, FileListParams,
    FileMetadata, FileObjectType, FileScopeObjectType,
};
use crate::types::page::paginate_id_page;
use crate::types::{PageCursor, PageQuery, paginate};
use awaken_resource_contract::{FileApplicationService, FileRecord, ResourcePurgeError};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

use super::managed_resource_error as error;

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

fn beta_cursor_metadata(record: &FileRecord) -> BetaFileCursorMetadata {
    BetaFileCursorMetadata {
        id: record.id.clone(),
        created_at: record.created_at.clone(),
        filename: record.filename.clone(),
        mime_type: record.mime_type.clone(),
        size_bytes: record.size_bytes,
        kind: FileObjectType::File,
        downloadable: Some(record.downloadable),
        expires_at: record.expires_at.clone(),
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
    let surface = match resource_api_surface(raw.as_deref(), &headers, ManagedCapability::Files) {
        Ok(surface) => surface,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    match surface {
        ManagedResourceApiSurface::CapabilityBeta => {
            list_beta_files(files, &workspace, raw.as_deref()).await
        }
        ManagedResourceApiSurface::QueryBeta => {
            list_beta_cursor_files(files, &workspace, raw.as_deref()).await
        }
        ManagedResourceApiSurface::Ga => list_ga_files(files, &workspace, raw.as_deref()).await,
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
    let records = match files.list(workspace, query.scope_id.as_deref()).await {
        Ok(records) => records,
        Err(error_value) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, error_value.to_string());
        }
    };
    let page = match paginate_id_page(
        &records,
        query.before_id.as_deref(),
        query.after_id.as_deref(),
        query.limit.map(usize::from),
        |record| record.id.as_str(),
    ) {
        Ok(page) => page,
        Err(error_value) => return error(StatusCode::BAD_REQUEST, error_value.to_string()),
    };
    Json(crate::types::Page::new(
        page.data.iter().map(beta_metadata).collect(),
        page.has_more,
        page.first_id,
        page.last_id,
    ))
    .into_response()
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
    let page =
        match cursor_file_page(files, workspace, None, query.page, query.limit, query.ids).await {
            Ok(page) => page,
            Err(response) => return response,
        };
    Json(PageCursor {
        data: page.data.iter().map(ga_metadata).collect(),
        next_page: page.next_page,
    })
    .into_response()
}

async fn list_beta_cursor_files(
    files: Arc<dyn FileApplicationService>,
    workspace: &str,
    raw: Option<&str>,
) -> axum::response::Response {
    let query = match BetaFileCursorListParams::from_query(&without_beta_selector(raw)) {
        Ok(query) => query,
        Err(error_value) => return error(StatusCode::BAD_REQUEST, error_value),
    };
    let page = match cursor_file_page(
        files,
        workspace,
        query.scope_id.as_deref(),
        query.page,
        query.limit,
        query.ids,
    )
    .await
    {
        Ok(page) => page,
        Err(response) => return response,
    };
    Json(PageCursor {
        data: page.data.iter().map(beta_cursor_metadata).collect(),
        next_page: page.next_page,
    })
    .into_response()
}

async fn cursor_file_page(
    files: Arc<dyn FileApplicationService>,
    workspace: &str,
    scope_id: Option<&str>,
    page: Option<String>,
    limit: Option<u16>,
    ids: Option<Vec<String>>,
) -> Result<PageCursor<FileRecord>, axum::response::Response> {
    let records = match files.list(workspace, scope_id).await {
        Ok(records) => records,
        Err(error_value) => {
            return Err(error(
                StatusCode::INTERNAL_SERVER_ERROR,
                error_value.to_string(),
            ));
        }
    };
    if let Some(ids) = ids {
        if page.is_some() || limit.is_some() {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "ids[] is mutually exclusive with page and limit",
            ));
        }
        let mut ids = ids;
        ids.sort();
        ids.dedup();
        if ids.len() > 100 {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "ids[] accepts at most 100 unique entries",
            ));
        }
        let selected = records
            .into_iter()
            .filter(|record| ids.binary_search(&record.id).is_ok())
            .collect();
        return Ok(PageCursor::single(selected));
    }
    Ok(paginate(
        records,
        &PageQuery {
            page,
            limit: limit.map(usize::from),
        },
        |file| file.id.as_str(),
    ))
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
    mut multipart: ManagedMultipart,
) -> impl IntoResponse {
    let surface = match resource_api_surface(raw.as_deref(), &headers, ManagedCapability::Files) {
        Ok(surface) => surface,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let mut upload: Option<(String, String, Vec<u8>)> = None;
    let mut expiry_seconds = None;
    while let Some(field) = match multipart.0.next_field().await {
        Ok(field) => field,
        Err(multipart_error) => {
            return error(StatusCode::BAD_REQUEST, multipart_error.to_string());
        }
    } {
        if field.name() == Some("expires_in_seconds") {
            if surface == ManagedResourceApiSurface::CapabilityBeta {
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
        Ok(record) => match surface {
            ManagedResourceApiSurface::CapabilityBeta => {
                (StatusCode::OK, Json(beta_metadata(&record))).into_response()
            }
            ManagedResourceApiSurface::QueryBeta => {
                (StatusCode::OK, Json(beta_cursor_metadata(&record))).into_response()
            }
            ManagedResourceApiSurface::Ga => {
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
    let surface = match resource_api_surface(raw.as_deref(), &headers, ManagedCapability::Files) {
        Ok(surface) => surface,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    match files.get(&workspace, &id).await {
        Ok(Some(record)) => match surface {
            ManagedResourceApiSurface::CapabilityBeta => {
                (StatusCode::OK, Json(beta_metadata(&record))).into_response()
            }
            ManagedResourceApiSurface::QueryBeta => {
                (StatusCode::OK, Json(beta_cursor_metadata(&record))).into_response()
            }
            ManagedResourceApiSurface::Ga => {
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
    if let Err(message) = resource_api_surface(raw.as_deref(), &headers, ManagedCapability::Files) {
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
    if let Err(message) = resource_api_surface(raw.as_deref(), &headers, ManagedCapability::Files) {
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
        // exact delete receipt; F4 top-level GA -> expiry without scope; F5
        // post-GA Beta -> both expiry and scope. In every rule the DTO owns the
        // fixed field set, so a manually assembled alternate envelope cannot
        // drift into the API.
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

        let query_beta = serde_json::to_value(BetaFileCursorMetadata {
            id: "file_4".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            filename: "notes.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 5,
            kind: FileObjectType::File,
            downloadable: Some(true),
            expires_at: Some("2026-02-01T00:00:00Z".into()),
            scope: Some(BetaFileScope {
                id: "session_1".into(),
                kind: FileScopeObjectType::Session,
            }),
        })
        .unwrap();
        assert_eq!(query_beta.as_object().unwrap().len(), 9, "F5 exact fields");
        assert_eq!(query_beta["scope"]["type"], "session", "F5");
        assert_eq!(query_beta["expires_at"], "2026-02-01T00:00:00Z", "F5");
    }
}
