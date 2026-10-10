// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Static owner: Repository method adapters over RepositoryApplicationService;
// @formal reason: awaken-protocol-awaken owns the single extension route factory and request/response
// @formal reason: DTOs. Dynamic path: the injected methods require the trusted Workspace extension and
// @formal reason: idempotency key, delegate create/list/get to the existing Resource application, and
// @formal reason: preserve pagination and error projection. The adjacent repository_api_http suite uses
// @formal reason: the real composed router and SQLite registry with credential-decision fixtures. This
// @formal reason: product classification does not prove upstream IAM, real Vault admission, remote Git
// @formal reason: access or universal async/SQL behavior.
//! Awaken-owned reusable Repository authoring, not an Anthropic SDK endpoint.
//! Trusted Workspace and IAM admission come from the existing edge; no secrets
//! enter this adapter, and authoring success does not assert remote Git access.

use super::managed_resource_error as err;
use crate::awaken_extensions::{CreateRepository, RepositoryObject};
use crate::{
    common::{headers::parse_idempotency_key_header, scope::RequiredWorkspaceScope},
    routes::{ManagedJson, ManagedQuery},
    types::{PageCursor, PageQuery, paginate},
};
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
        ResourceRegistryError::IdentityRejected { detail: message }
        | ResourceRegistryError::ConfigurationRejected { detail: message }
        | ResourceRegistryError::AdmissionRejected { detail: message } => {
            err(StatusCode::BAD_REQUEST, message)
        }
        ResourceRegistryError::VersionExhausted { id } => err(
            StatusCode::CONFLICT,
            format!("resource config version space is exhausted for `{id}`"),
        ),
        ResourceRegistryError::EncodingFailure { detail } => {
            err(StatusCode::INTERNAL_SERVER_ERROR, detail)
        }
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
        ResourceRegistryError::AuthorityUnavailable { .. }
        | ResourceRegistryError::Indeterminate { .. } => err(
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
    crate::awaken_extensions::repositories_router(list, create, retrieve).with_state(application)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_failure_status_uses_the_typed_registry_cause() {
        // Cause/effect table: identity/config/admission -> 400; exhausted or
        // concurrent state -> 409; missing -> 404; unavailable/indeterminate
        // -> 503; corrupt/encoding -> 500. All diagnostics are identical so
        // status selection cannot regress to message parsing.
        for (cause, expected) in [
            (
                ResourceRegistryError::IdentityRejected {
                    detail: "same".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                ResourceRegistryError::ConfigurationRejected {
                    detail: "same".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                ResourceRegistryError::AdmissionRejected {
                    detail: "same".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                ResourceRegistryError::VersionExhausted { id: "same".into() },
                StatusCode::CONFLICT,
            ),
            (
                ResourceRegistryError::NotFound("same".into()),
                StatusCode::NOT_FOUND,
            ),
            (
                ResourceRegistryError::AuthorityUnavailable {
                    detail: "same".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ResourceRegistryError::Indeterminate {
                    detail: "same".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ResourceRegistryError::CorruptData("same".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                ResourceRegistryError::EncodingFailure {
                    detail: "same".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            assert_eq!(failure(cause).status(), expected);
        }
    }
}
