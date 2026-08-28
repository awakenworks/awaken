//! Anthropic-compatible Dreams routes over the dream application.

use std::sync::Arc;

use awaken_session_contract::{Dream, DreamCreateParams, DreamListParams, DreamPage, DreamStatus};
use awaken_tenancy::WorkspaceScope;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::routes::ManagedJson;
use crate::types::ErrorResponse;
use awaken_dream_application::{DreamApiError, DreamApplication};

pub fn dreams_router(state: Arc<DreamApplication>) -> Router {
    Router::new()
        .route("/v1/dreams", post(create).get(list))
        .route("/v1/dreams/{id}", get(retrieve))
        .route("/v1/dreams/{id}/cancel", post(cancel))
        .route("/v1/dreams/{id}/archive", post(archive))
        .with_state(state)
}

fn scope(workspace: Option<axum::Extension<WorkspaceScope>>) -> String {
    workspace
        .map(|workspace| workspace.0.0.clone())
        .unwrap_or_else(|| "default".into())
}

async fn create(
    State(state): State<Arc<DreamApplication>>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
    ManagedJson(params): ManagedJson<DreamCreateParams>,
) -> Result<Json<Dream>, (StatusCode, Json<ErrorResponse>)> {
    state
        .create(&scope(workspace), params)
        .await
        .map(Json)
        .map_err(error_response)
}

async fn retrieve(
    State(state): State<Arc<DreamApplication>>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
    Path(id): Path<String>,
) -> Result<Json<Dream>, (StatusCode, Json<ErrorResponse>)> {
    state
        .retrieve(&scope(workspace), &id)
        .await
        .map(Json)
        .map_err(error_response)
}

async fn list(
    State(state): State<Arc<DreamApplication>>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
    RawQuery(query): RawQuery,
) -> Result<Json<DreamPage>, (StatusCode, Json<ErrorResponse>)> {
    let params = parse_list_query(query.as_deref()).map_err(error_response)?;
    state
        .list(&scope(workspace), params)
        .await
        .map(Json)
        .map_err(error_response)
}

fn parse_list_query(raw: Option<&str>) -> Result<DreamListParams, DreamApiError> {
    let mut params = DreamListParams::default();
    for (key, value) in form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "created_at[gt]" if !value.is_empty() => {
                params.created_after = Some(value.into_owned());
            }
            "created_at[lt]" if !value.is_empty() => {
                params.created_before = Some(value.into_owned());
            }
            "include_archived" => {
                params.include_archived = value.parse().map_err(|_| {
                    DreamApiError::BadRequest("include_archived must be a boolean".into())
                })?;
            }
            "statuses" => params.statuses.push(match value.as_ref() {
                "pending" => DreamStatus::Pending,
                "running" => DreamStatus::Running,
                "completed" => DreamStatus::Completed,
                "failed" => DreamStatus::Failed,
                "canceled" => DreamStatus::Canceled,
                _ => {
                    return Err(DreamApiError::BadRequest(
                        "unknown Dream status filter".into(),
                    ));
                }
            }),
            "limit" => {
                let limit: usize = value
                    .parse()
                    .map_err(|_| DreamApiError::BadRequest("limit must be an integer".into()))?;
                if !(1..=100).contains(&limit) {
                    return Err(DreamApiError::BadRequest(
                        "limit must be between 1 and 100".into(),
                    ));
                }
                params.limit = Some(limit);
            }
            "page" if value.is_empty() => params.page = None,
            "page" => params.page = Some(value.into_owned()),
            _ => {}
        }
    }
    Ok(params)
}

async fn cancel(
    State(state): State<Arc<DreamApplication>>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
    Path(id): Path<String>,
) -> Result<Json<Dream>, (StatusCode, Json<ErrorResponse>)> {
    state
        .cancel(&scope(workspace), &id)
        .await
        .map(Json)
        .map_err(error_response)
}

async fn archive(
    State(state): State<Arc<DreamApplication>>,
    workspace: Option<axum::Extension<WorkspaceScope>>,
    Path(id): Path<String>,
) -> Result<Json<Dream>, (StatusCode, Json<ErrorResponse>)> {
    state
        .archive(&scope(workspace), &id)
        .await
        .map(Json)
        .map_err(error_response)
}

fn error_response(error: DreamApiError) -> (StatusCode, Json<ErrorResponse>) {
    let (status, kind) = match error {
        DreamApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "invalid_request_error"),
        DreamApiError::NotFound => (StatusCode::NOT_FOUND, "not_found_error"),
        DreamApiError::Conflict(_) => (StatusCode::CONFLICT, "invalid_request_error"),
        DreamApiError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "api_error"),
    };
    (status, Json(ErrorResponse::new(kind, error.to_string())))
}

#[cfg(test)]
mod tests {
    use super::parse_list_query;

    #[test]
    fn empty_dream_filters_match_python_omission() {
        // Metamorphic relation: replacing Python's omitted optional filters
        // with TypeScript's empty pairs preserves DreamListParams; each real
        // value remains present. This crosses page and both timestamp branches.
        let omitted = parse_list_query(None).unwrap();
        let typescript_empty =
            parse_list_query(Some("page=&created_at%5Bgt%5D=&created_at%5Blt%5D=")).unwrap();
        let populated = parse_list_query(Some(
            "page=dream_1&created_at%5Bgt%5D=2026-01-01T00%3A00%3A00Z",
        ))
        .unwrap();
        assert_eq!(typescript_empty.page, omitted.page);
        assert_eq!(typescript_empty.created_after, omitted.created_after);
        assert_eq!(typescript_empty.created_before, omitted.created_before);
        assert_eq!(populated.page.as_deref(), Some("dream_1"));
        assert_eq!(
            populated.created_after.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
    }
}
