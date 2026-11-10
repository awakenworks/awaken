// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: This native HTTP projection binds trusted Workspace and a reviewed revision to
// @formal reason: the existing Agent repository archive command and its existing Deployment cascade.
// @formal reason: It owns no lifecycle persistence, registration, model resolution or retry authority.
//! Native receipt representation for the existing Agent lifecycle command.

use super::{RequiredScope, problem_details};
use awaken_protocol_managed::{
    AgentRegistryState, ManagedAgentError, ManagedAgentRepository, ManagedInferenceGeoPolicy,
    agents_router,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveRequest {
    generation: u64,
}

pub(crate) fn configuration_lifecycle_router(state: Arc<AgentRegistryState>) -> Router {
    Router::new()
        .route("/v1/config/agents/{id}/archive", post(archive))
        .route(
            "/v1/awaken/agents/execution-evidence",
            get(execution_evidence),
        )
        .with_state(state)
}

/// Bind both HTTP representations to the one repository and cascade instance.
pub(crate) fn agent_registry_routers(
    repository: Arc<dyn ManagedAgentRepository>,
    inference_geo: Option<Arc<dyn ManagedInferenceGeoPolicy>>,
    cascade: Option<Arc<dyn awaken_deployment_contract::AgentArchiveCascade>>,
) -> Router {
    let state = AgentRegistryState::from_repository(repository);
    let state = match inference_geo {
        Some(policy) => state.with_inference_geo_policy(policy),
        None => state,
    };
    let state = match cascade {
        Some(cascade) => state.with_archive_cascade(cascade),
        None => state,
    };
    let state = Arc::new(state);
    agents_router(state.clone()).merge(configuration_lifecycle_router(state))
}

async fn archive(
    State(state): State<Arc<AgentRegistryState>>,
    RequiredScope(scope): RequiredScope,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let request = match serde_json::from_value::<ArchiveRequest>(body) {
        Ok(request) if request.generation > 0 => request,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                problem_details(
                    "agent_archive_invalid",
                    "Agent archive requires a reviewed version",
                    "Provide only generation as a positive integer; lifecycle timestamps are server-owned.",
                ),
            );
        }
    };
    match state
        .archive_at_revision(scope.as_str(), &id, request.generation)
        .await
    {
        Ok(receipt) => (
            StatusCode::OK,
            Json(json!({
                "id": receipt.id, "generation": receipt.revision,
                "archived_at": receipt.archived_at, "already_archived": receipt.already_archived,
            })),
        ),
        Err(error) => {
            let (status, code, title) = match &error {
                ManagedAgentError::NotFound => {
                    (StatusCode::NOT_FOUND, "agent_not_found", "Agent not found")
                }
                ManagedAgentError::Conflict(_) => (
                    StatusCode::CONFLICT,
                    "agent_archive_conflict",
                    "Agent version changed",
                ),
                ManagedAgentError::Invalid(_) => (
                    StatusCode::BAD_REQUEST,
                    "agent_archive_invalid",
                    "Agent archive cannot proceed",
                ),
                ManagedAgentError::Storage(_) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "agent_archive_unavailable",
                    "Read the Agent before continuing this archive",
                ),
            };
            (status, problem_details(code, title, error.to_string()))
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionEvidenceQuery {
    agent_id: Option<String>,
    version: Option<u64>,
}

async fn execution_evidence(
    State(state): State<Arc<AgentRegistryState>>,
    RequiredScope(scope): RequiredScope,
    Query(query): Query<ExecutionEvidenceQuery>,
) -> (StatusCode, Json<Value>) {
    let publication = match (query.agent_id.as_deref(), query.version) {
        (None, None) => None,
        (Some(id), Some(version)) if !id.trim().is_empty() && version > 0 => Some((id, version)),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                problem_details(
                    "invalid_publication_selector",
                    "Select one exact Agent publication",
                    "agent_id and a positive version must be supplied together",
                ),
            );
        }
    };
    match state.execution_evidence(scope.as_str(), publication).await {
        Ok(data) => (StatusCode::OK, Json(json!({"data": data}))),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            problem_details(
                "agent_execution_evidence_unavailable",
                "Agent execution evidence is unavailable",
                error.to_string(),
            ),
        ),
    }
}
