//! Awaken-owned reusable Repository authoring, not an Anthropic SDK endpoint.
//! Trusted Workspace and IAM admission come from the existing edge; no secrets
//! enter this adapter, and authoring success does not assert remote Git access.

use super::managed_resource_error as err;
use crate::{
    common::{headers::parse_idempotency_key_header, scope::RequiredWorkspaceScope},
    routes::{ManagedJson, ManagedQuery},
    types::{PageCursor, PageQuery, paginate},
};
use awaken_protocol_awaken::{CreateRepository, RepositoryObject};
use awaken_resource_contract::{
    CreateRepositoryCommand, RepositoryApplicationService, ResourceRegistryError,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use std::sync::Arc;

fn failure(error: ResourceRegistryError) -> axum::response::Response {
    match error {
        ResourceRegistryError::Invalid(message) => err(StatusCode::BAD_REQUEST, message),
        ResourceRegistryError::NotFound(_) | ResourceRegistryError::ConfigNotFound { .. } => err(
            StatusCode::NOT_FOUND,
            "Repository was not found in this Workspace",
        ),
        ResourceRegistryError::AlreadyRegistered(_)
        | ResourceRegistryError::NotActive { .. }
        | ResourceRegistryError::ConfigConflict { .. }
        | ResourceRegistryError::ConcurrentModification(_) => err(
            StatusCode::CONFLICT,
            "Repository changed or this request key was used for different settings; check the existing repository",
        ),
        ResourceRegistryError::Unavailable(_) => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Repository configuration service is unavailable; retry the same request",
        ),
        ResourceRegistryError::CorruptData(_) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Repository configuration could not be loaded",
        ),
    }
}

pub fn repositories_router(application: Arc<dyn RepositoryApplicationService>) -> Router {
    awaken_protocol_awaken::repositories_router(list, create, retrieve).with_state(application)
}

async fn list(
    State(application): State<Arc<dyn RepositoryApplicationService>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    ManagedQuery(query): ManagedQuery<PageQuery>,
) -> axum::response::Response {
    match application.list(&workspace).await {
        Ok(definitions) => {
            let page = paginate(definitions, &query, |definition| definition.id.as_str());
            Json(PageCursor {
                data: page.data,
                next_page: page.next_page,
            })
            .into_response()
        }
        Err(error) => failure(error),
    }
}

async fn retrieve(
    State(application): State<Arc<dyn RepositoryApplicationService>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> axum::response::Response {
    match application.get(&workspace, &id).await {
        Ok(Some(details)) => Json(RepositoryObject::from(details)).into_response(),
        Ok(None) => err(
            StatusCode::NOT_FOUND,
            "Repository was not found in this Workspace",
        ),
        Err(error) => failure(error),
    }
}

async fn create(
    State(application): State<Arc<dyn RepositoryApplicationService>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    headers: HeaderMap,
    ManagedJson(body): ManagedJson<CreateRepository>,
) -> axum::response::Response {
    let key = match parse_idempotency_key_header(&headers) {
        Ok(Some(key)) => key,
        Ok(None) => return err(StatusCode::BAD_REQUEST, "Idempotency-Key is required"),
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let command = CreateRepositoryCommand {
        workspace_id: workspace,
        name: body.name,
        description: body.description,
        remote_url: body.remote_url,
        credential_binding: body.credential_binding,
        initial_branch: body.initial_branch,
        initial_commit: body.initial_commit,
        clone_policy: body.clone_policy,
    };
    match application.create_idempotent(command, &key).await {
        Ok(details) => (StatusCode::CREATED, Json(RepositoryObject::from(details))).into_response(),
        Err(error) => failure(error),
    }
}
