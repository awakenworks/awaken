//! Managed Agent HTTP adapter (`/v1/agents`).
//!
//! The router is deliberately storage-neutral. Every composition root injects
//! an explicit repository; production uses the durable configuration-plane
//! adapter, so this protocol crate owns no second in-memory Agent aggregate.

use std::sync::Arc;

use crate::routes::{ManagedJson, WorkspaceScope};
use crate::state::DEFAULT_SCOPE;
use crate::types::agent::{
    Agent, AgentCreateParams, AgentListParams, AgentRetrieveParams, AgentUpdateParams,
};
use crate::types::{ErrorResponse, Page, PageQuery, paginate, paginate_by};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};

// Compatibility re-exports for session consumers. Agent registry persistence no
// longer uses this read-only projection port.
pub use awaken_session_contract::{
    AgentClientToolView, AgentConfigSource, AgentConfigView, AgentMcpServerView,
};

/// Storage-neutral failures exposed by the Managed Agent repository port.
#[derive(Debug, thiserror::Error)]
pub enum ManagedAgentError {
    #[error("agent not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Storage(String),
}

/// Durable authoring port for the SDK-facing Agent aggregate.
///
/// `workspace_id` is trusted routing context supplied by the edge. It is an
/// intrinsic repository partition, not an authorization policy or principal.
#[async_trait::async_trait]
pub trait ManagedAgentRepository: Send + Sync {
    async fn create(
        &self,
        workspace_id: &str,
        params: AgentCreateParams,
    ) -> Result<Agent, ManagedAgentError>;
    async fn retrieve(
        &self,
        workspace_id: &str,
        id: &str,
        version: Option<u64>,
    ) -> Result<Agent, ManagedAgentError>;
    async fn list(
        &self,
        workspace_id: &str,
        params: &AgentListParams,
    ) -> Result<Vec<Agent>, ManagedAgentError>;
    async fn update(
        &self,
        workspace_id: &str,
        id: &str,
        params: AgentUpdateParams,
    ) -> Result<Agent, ManagedAgentError>;
    async fn disable(&self, workspace_id: &str, id: &str) -> Result<Agent, ManagedAgentError>;
    async fn archive(&self, workspace_id: &str, id: &str) -> Result<Agent, ManagedAgentError>;
    async fn versions(&self, workspace_id: &str, id: &str)
    -> Result<Vec<Agent>, ManagedAgentError>;
}

/// Router state containing exactly one Agent repository implementation.
pub struct AgentRegistryState {
    repository: Arc<dyn ManagedAgentRepository>,
    deployments: Option<Arc<crate::routes::deployments::DeploymentState>>,
}

impl AgentRegistryState {
    #[must_use]
    pub fn from_repository(repository: Arc<dyn ManagedAgentRepository>) -> Self {
        Self {
            repository,
            deployments: None,
        }
    }

    /// Bind the existing Deployment aggregate so archiving a primary Agent can
    /// terminally archive its deployments in the same request operation.
    #[must_use]
    pub fn with_deployments(
        mut self,
        deployments: Arc<crate::routes::deployments::DeploymentState>,
    ) -> Self {
        self.deployments = Some(deployments);
        self
    }
}

fn request_scope(scope: &Option<Extension<WorkspaceScope>>) -> String {
    scope.as_ref().map_or_else(
        || DEFAULT_SCOPE.to_string(),
        |workspace| workspace.0.0.clone(),
    )
}

pub fn agents_router(state: Arc<AgentRegistryState>) -> Router {
    Router::new()
        .route("/v1/agents", post(create_agent).get(list_agents))
        .route("/v1/agents/{id}", get(retrieve_agent).post(update_agent))
        .route("/v1/agents/{id}/disable", post(disable_agent))
        .route("/v1/agents/{id}/archive", post(archive_agent))
        .route("/v1/agents/{id}/versions", get(list_versions))
        .with_state(state)
}

type WireError = (StatusCode, Json<ErrorResponse>);

fn wire_error(error: ManagedAgentError) -> WireError {
    match error {
        ManagedAgentError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("not_found_error", "agent not found")),
        ),
        ManagedAgentError::Conflict(message) => (
            StatusCode::CONFLICT,
            Json(ErrorResponse::new("invalid_request_error", message)),
        ),
        ManagedAgentError::Invalid(message) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("invalid_request_error", message)),
        ),
        ManagedAgentError::Storage(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new("api_error", message)),
        ),
    }
}

async fn create_agent(
    State(state): State<Arc<AgentRegistryState>>,
    scope: Option<Extension<WorkspaceScope>>,
    ManagedJson(params): ManagedJson<AgentCreateParams>,
) -> Result<Json<Agent>, WireError> {
    state
        .repository
        .create(&request_scope(&scope), params)
        .await
        .map(Json)
        .map_err(wire_error)
}

async fn retrieve_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
    Query(query): Query<AgentRetrieveParams>,
) -> Result<Json<Agent>, WireError> {
    state
        .repository
        .retrieve(&request_scope(&scope), &id, query.version)
        .await
        .map(Json)
        .map_err(wire_error)
}

async fn list_agents(
    State(state): State<Arc<AgentRegistryState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Query(params): Query<AgentListParams>,
) -> Result<Json<Page<Agent>>, WireError> {
    state
        .repository
        .list(&request_scope(&scope), &params)
        .await
        .map(|agents| {
            Json(paginate(agents, &params.page_query(), |agent| {
                agent.id.as_str()
            }))
        })
        .map_err(wire_error)
}

async fn update_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
    ManagedJson(params): ManagedJson<AgentUpdateParams>,
) -> Result<Json<Agent>, WireError> {
    state
        .repository
        .update(&request_scope(&scope), &id, params)
        .await
        .map(Json)
        .map_err(wire_error)
}

async fn disable_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<Agent>, WireError> {
    state
        .repository
        .disable(&request_scope(&scope), &id)
        .await
        .map(Json)
        .map_err(wire_error)
}

async fn archive_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<Agent>, WireError> {
    let scope = request_scope(&scope);
    let agent = state
        .repository
        .archive(&scope, &id)
        .await
        .map_err(wire_error)?;
    if let Some(deployments) = &state.deployments {
        deployments
            .archive_for_agent(&scope, &id)
            .await
            .map_err(|error| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ErrorResponse::new("api_error", error.to_string())),
                )
            })?;
    }
    Ok(Json(agent))
}

async fn list_versions(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
    Query(page): Query<PageQuery>,
) -> Result<Json<Page<Agent>>, WireError> {
    state
        .repository
        .versions(&request_scope(&scope), &id)
        .await
        .map(|versions| {
            Json(paginate_by(versions, &page, |agent| {
                agent.version.to_string()
            }))
        })
        .map_err(wire_error)
}
