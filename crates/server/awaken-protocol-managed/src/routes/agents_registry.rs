//! Managed Agent HTTP adapter (`/v1/agents`).
//!
//! The router is deliberately storage-neutral. Production injects the durable
//! configuration-plane adapter; the in-memory implementation below is only a
//! reference adapter for protocol tests. Workspace partitioning is part of the
//! repository key, so no process-local owner side index can bypass it.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::Value;

use crate::routes::{ManagedJson, WorkspaceScope};
use crate::state::DEFAULT_SCOPE;
use crate::types::agent::{Agent, AgentCreateParams, AgentUpdateParams};
use crate::types::{ErrorResponse, ModelConfig, Page, PageQuery, paginate};

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

// Compatibility re-exports for session consumers. Agent registry persistence no
// longer uses this read-only projection port.
pub use awaken_session_contract::{AgentConfigSource, AgentConfigView, AgentMcpServerView};

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
    async fn retrieve(&self, workspace_id: &str, id: &str) -> Result<Agent, ManagedAgentError>;
    async fn list(&self, workspace_id: &str) -> Result<Vec<Agent>, ManagedAgentError>;
    async fn update(
        &self,
        workspace_id: &str,
        id: &str,
        params: AgentUpdateParams,
    ) -> Result<Agent, ManagedAgentError>;
    async fn archive(&self, workspace_id: &str, id: &str) -> Result<Agent, ManagedAgentError>;
    async fn versions(&self, workspace_id: &str, id: &str)
    -> Result<Vec<Agent>, ManagedAgentError>;
}

/// Router state containing exactly one Agent repository implementation.
pub struct AgentRegistryState {
    repository: Arc<dyn ManagedAgentRepository>,
}

impl AgentRegistryState {
    /// Reference in-memory adapter for protocol-only embeddings and tests.
    #[must_use]
    pub fn new() -> Self {
        Self::from_repository(Arc::new(InMemoryManagedAgentRepository::default()))
    }

    #[must_use]
    pub fn from_repository(repository: Arc<dyn ManagedAgentRepository>) -> Self {
        Self { repository }
    }
}

impl Default for AgentRegistryState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
struct Record {
    name: String,
    description: Option<String>,
    model: ModelConfig,
    system: Option<String>,
    metadata: BTreeMap<String, String>,
    mcp_servers: Vec<Value>,
    skills: Vec<Value>,
    tools: Vec<Value>,
    multiagent: Option<Value>,
    version: u64,
    archived_at: Option<String>,
    history: Vec<Agent>,
}

impl Record {
    fn project(&self, id: &str) -> Agent {
        Agent {
            id: id.to_string(),
            object_type: "agent",
            archived_at: self.archived_at.clone(),
            created_at: OBJECT_AT.to_string(),
            updated_at: OBJECT_AT.to_string(),
            name: self.name.clone(),
            description: self.description.clone(),
            model: self.model.clone(),
            system: self.system.clone(),
            metadata: self.metadata.clone(),
            mcp_servers: self.mcp_servers.clone(),
            skills: self.skills.clone(),
            tools: self.tools.clone(),
            multiagent: self.multiagent.clone(),
            version: self.version,
        }
    }
}

/// Minimal reference adapter. The workspace is embedded in the aggregate key;
/// there is no separate owner map whose loss could widen access.
#[derive(Default)]
pub struct InMemoryManagedAgentRepository {
    records: Mutex<HashMap<(String, String), Record>>,
    sequence: AtomicU64,
}

#[async_trait::async_trait]
impl ManagedAgentRepository for InMemoryManagedAgentRepository {
    async fn create(
        &self,
        workspace_id: &str,
        params: AgentCreateParams,
    ) -> Result<Agent, ManagedAgentError> {
        let n = self.sequence.fetch_add(1, Ordering::SeqCst);
        let id = format!("agent_{n:016}");
        let mut record = Record {
            name: params.name,
            description: params.description,
            model: params.model.into_config(),
            system: params.system,
            metadata: params.metadata,
            mcp_servers: params.mcp_servers,
            skills: params.skills,
            tools: params.tools,
            multiagent: params.multiagent.filter(|value| !value.is_null()),
            version: 1,
            archived_at: None,
            history: Vec::new(),
        };
        let agent = record.project(&id);
        record.history.push(agent.clone());
        self.records
            .lock()
            .expect("managed Agent repository")
            .insert((workspace_id.to_string(), id), record);
        Ok(agent)
    }

    async fn retrieve(&self, workspace_id: &str, id: &str) -> Result<Agent, ManagedAgentError> {
        self.records
            .lock()
            .expect("managed Agent repository")
            .get(&(workspace_id.to_string(), id.to_string()))
            .map(|record| record.project(id))
            .ok_or(ManagedAgentError::NotFound)
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<Agent>, ManagedAgentError> {
        let records = self.records.lock().expect("managed Agent repository");
        let mut agents: Vec<_> = records
            .iter()
            .filter(|((workspace, _), _)| workspace == workspace_id)
            .map(|((_, id), record)| record.project(id))
            .collect();
        agents.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(agents)
    }

    async fn update(
        &self,
        workspace_id: &str,
        id: &str,
        params: AgentUpdateParams,
    ) -> Result<Agent, ManagedAgentError> {
        let mut records = self.records.lock().expect("managed Agent repository");
        let record = records
            .get_mut(&(workspace_id.to_string(), id.to_string()))
            .ok_or(ManagedAgentError::NotFound)?;
        if params.version != record.version {
            return Err(ManagedAgentError::Conflict(format!(
                "version mismatch: expected {}, got {}",
                record.version, params.version
            )));
        }
        if let Some(name) = params.name {
            record.name = name;
        }
        if let Some(model) = params.model {
            record.model = model.into_config();
        }
        if let Some(description) = params.description {
            record.description = Some(description);
        }
        if let Some(system) = params.system {
            record.system = Some(system);
        }
        if let Some(metadata) = params.metadata {
            record.metadata = metadata;
        }
        if let Some(mcp_servers) = params.mcp_servers {
            record.mcp_servers = mcp_servers;
        }
        if let Some(skills) = params.skills {
            record.skills = skills;
        }
        if let Some(tools) = params.tools {
            record.tools = tools;
        }
        if let Some(multiagent) = params.multiagent {
            record.multiagent = Some(multiagent).filter(|value| !value.is_null());
        }
        record.version += 1;
        let agent = record.project(id);
        record.history.push(agent.clone());
        Ok(agent)
    }

    async fn archive(&self, workspace_id: &str, id: &str) -> Result<Agent, ManagedAgentError> {
        let mut records = self.records.lock().expect("managed Agent repository");
        let record = records
            .get_mut(&(workspace_id.to_string(), id.to_string()))
            .ok_or(ManagedAgentError::NotFound)?;
        record.archived_at = Some(OBJECT_AT.to_string());
        record.version += 1;
        let agent = record.project(id);
        record.history.push(agent.clone());
        Ok(agent)
    }

    async fn versions(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Vec<Agent>, ManagedAgentError> {
        self.records
            .lock()
            .expect("managed Agent repository")
            .get(&(workspace_id.to_string(), id.to_string()))
            .map(|record| record.history.clone())
            .ok_or(ManagedAgentError::NotFound)
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
) -> Result<Json<Agent>, WireError> {
    state
        .repository
        .retrieve(&request_scope(&scope), &id)
        .await
        .map(Json)
        .map_err(wire_error)
}

async fn list_agents(
    State(state): State<Arc<AgentRegistryState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Query(page): Query<PageQuery>,
) -> Result<Json<Page<Agent>>, WireError> {
    state
        .repository
        .list(&request_scope(&scope))
        .await
        .map(|agents| Json(paginate(agents, &page, |agent| agent.id.as_str())))
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

async fn archive_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<Agent>, WireError> {
    state
        .repository
        .archive(&request_scope(&scope), &id)
        .await
        .map(Json)
        .map_err(wire_error)
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
        .map(|versions| Json(paginate(versions, &page, |agent| agent.id.as_str())))
        .map_err(wire_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    async fn create_owned(app: &Router, scope: &str) -> String {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/agents")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({ "name": "a", "model": "kimi" })).unwrap(),
            ))
            .unwrap();
        request
            .extensions_mut()
            .insert(WorkspaceScope(scope.to_string()));
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<Value>(&bytes).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn call(app: &Router, method: &str, path: &str, scope: Option<&str>) -> StatusCode {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        if let Some(scope) = scope {
            request
                .extensions_mut()
                .insert(WorkspaceScope(scope.to_string()));
        }
        app.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn workspace_is_part_of_the_repository_key() {
        let app = agents_router(Arc::new(AgentRegistryState::new()));
        let id = create_owned(&app, "workspace-a").await;
        let path = format!("/v1/agents/{id}");
        assert_eq!(
            call(&app, "GET", &path, Some("workspace-a")).await,
            StatusCode::OK
        );
        assert_eq!(
            call(&app, "GET", &path, Some("workspace-b")).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(call(&app, "GET", &path, None).await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_archive_and_history_share_one_aggregate() {
        let app = agents_router(Arc::new(AgentRegistryState::new()));
        let id = create_owned(&app, DEFAULT_SCOPE).await;
        let update = json!({ "version": 1, "name": "renamed" });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/agents/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&update).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            call(&app, "POST", &format!("/v1/agents/{id}/archive"), None).await,
            StatusCode::OK
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/agents/{id}/versions"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["data"].as_array().unwrap().len(), 3);
    }
}
