//! Managed Agent HTTP adapter (`/v1/agents`).
//!
//! The router is deliberately storage-neutral. Every process startup injects
//! an explicit repository; production uses the durable configuration-plane
//! adapter, so this protocol crate owns no second in-memory Agent aggregate.

use std::sync::Arc;

use crate::common::scope::RequiredWorkspaceScope;
use crate::routes::ManagedJson;
use crate::types::agent::{
    Agent, AgentCreateParams, AgentListParams, AgentRetrieveParams, AgentUpdateParams,
};
use crate::types::{ErrorResponse, PageCursor, PageQuery, paginate, paginate_by};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

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
    async fn archive(&self, workspace_id: &str, id: &str) -> Result<Agent, ManagedAgentError>;
    async fn versions(&self, workspace_id: &str, id: &str)
    -> Result<Vec<Agent>, ManagedAgentError>;
}

/// Lifecycle command edge invoked after the authoritative Agent repository has
/// archived an Agent. The protocol adapter does not know which downstream
/// aggregate consumes the command; AllInOne binds the Coordinator-owned
/// implementation and split Control leaves it absent.
/// Router state containing exactly one Agent repository implementation.
pub struct AgentRegistryState {
    repository: Arc<dyn ManagedAgentRepository>,
    archive_cascade: Option<Arc<dyn awaken_deployment_contract::AgentArchiveCascade>>,
    inference_geo_policy: Option<Arc<dyn crate::ManagedInferenceGeoPolicy>>,
}

impl AgentRegistryState {
    #[must_use]
    pub fn from_repository(repository: Arc<dyn ManagedAgentRepository>) -> Self {
        Self {
            repository,
            archive_cascade: None,
            inference_geo_policy: None,
        }
    }

    #[must_use]
    pub fn with_inference_geo_policy(
        mut self,
        policy: Arc<dyn crate::ManagedInferenceGeoPolicy>,
    ) -> Self {
        self.inference_geo_policy = Some(policy);
        self
    }

    /// Bind one lifecycle command edge so archiving an Agent can synchronously
    /// terminalize its dependents in the same request operation.
    #[must_use]
    pub fn with_archive_cascade(
        mut self,
        archive_cascade: Arc<dyn awaken_deployment_contract::AgentArchiveCascade>,
    ) -> Self {
        self.archive_cascade = Some(archive_cascade);
        self
    }
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

fn inference_policy_error(error: crate::InferenceGeoPolicyError) -> WireError {
    match error {
        crate::InferenceGeoPolicyError::Denied { .. } => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                error.to_string(),
            )),
        ),
        crate::InferenceGeoPolicyError::Unavailable(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("api_error", error.to_string())),
        ),
    }
}

async fn authorize_agent_geo(
    state: &AgentRegistryState,
    workspace_id: &str,
    inference_geo: Option<crate::types::ModelInferenceGeo>,
) -> Result<(), WireError> {
    match &state.inference_geo_policy {
        Some(policy) => policy
            .authorize(
                workspace_id,
                inference_geo,
                crate::InferenceGeoCheckpoint::AgentSave,
            )
            .await
            .map_err(inference_policy_error),
        None => Ok(()),
    }
}

async fn create_agent(
    State(state): State<Arc<AgentRegistryState>>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    ManagedJson(params): ManagedJson<AgentCreateParams>,
) -> Result<Json<Agent>, WireError> {
    authorize_agent_geo(
        &state,
        &scope,
        params.model.clone().into_config().inference_geo,
    )
    .await?;
    state
        .repository
        .create(&scope, params)
        .await
        .map(Json)
        .map_err(wire_error)
}

async fn retrieve_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    Query(query): Query<AgentRetrieveParams>,
) -> Result<Json<Agent>, WireError> {
    state
        .repository
        .retrieve(&scope, &id, query.version)
        .await
        .map(Json)
        .map_err(wire_error)
}

async fn list_agents(
    State(state): State<Arc<AgentRegistryState>>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    Query(params): Query<AgentListParams>,
) -> Result<Json<PageCursor<Agent>>, WireError> {
    state
        .repository
        .list(&scope, &params)
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
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    ManagedJson(params): ManagedJson<AgentUpdateParams>,
) -> Result<Json<Agent>, WireError> {
    let inference_geo = match params.model.as_ref() {
        Some(model) => model.clone().into_config().inference_geo,
        None => {
            state
                .repository
                .retrieve(&scope, &id, None)
                .await
                .map_err(wire_error)?
                .model
                .inference_geo
        }
    };
    authorize_agent_geo(&state, &scope, inference_geo).await?;
    state
        .repository
        .update(&scope, &id, params)
        .await
        .map(Json)
        .map_err(wire_error)
}

async fn archive_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
) -> Result<Json<Agent>, WireError> {
    let agent = state
        .repository
        .archive(&scope, &id)
        .await
        .map_err(wire_error)?;
    if let Some(cascade) = &state.archive_cascade {
        cascade
            .archive_agent_dependents(&scope, &id)
            .await
            .map_err(|error| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ErrorResponse::new("api_error", error)),
                )
            })?;
    }
    Ok(Json(agent))
}

async fn list_versions(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    Query(page): Query<PageQuery>,
) -> Result<Json<PageCursor<Agent>>, WireError> {
    state
        .repository
        .versions(&scope, &id)
        .await
        .map(|versions| {
            Json(paginate_by(versions, &page, |agent| {
                agent.version.to_string()
            }))
        })
        .map_err(wire_error)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use serde_json::json;
    use tower::ServiceExt as _;

    use super::*;
    use crate::types::{ModelConfig, ModelInferenceGeo};

    struct FakeRepository {
        agent: Mutex<Option<Agent>>,
        writes: AtomicUsize,
    }

    impl FakeRepository {
        fn new() -> Self {
            Self {
                agent: Mutex::new(None),
                writes: AtomicUsize::new(0),
            }
        }

        fn from_params(params: AgentCreateParams) -> Agent {
            Agent {
                id: "agent-policy".into(),
                object_type: "agent",
                archived_at: None,
                created_at: "2026-08-18T00:00:00Z".into(),
                updated_at: "2026-08-18T00:00:00Z".into(),
                name: params.name,
                description: params.description,
                model: params.model.into_config().into_resolved(),
                system: params.system,
                metadata: params.metadata,
                mcp_servers: params.mcp_servers,
                skills: params.skills,
                tools: params.tools,
                multiagent: params.multiagent,
                version: 1,
            }
        }
    }

    #[async_trait::async_trait]
    impl ManagedAgentRepository for FakeRepository {
        async fn create(
            &self,
            _: &str,
            params: AgentCreateParams,
        ) -> Result<Agent, ManagedAgentError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            let agent = Self::from_params(params);
            *self.agent.lock().unwrap() = Some(agent.clone());
            Ok(agent)
        }

        async fn retrieve(
            &self,
            _: &str,
            _: &str,
            _: Option<u64>,
        ) -> Result<Agent, ManagedAgentError> {
            self.agent
                .lock()
                .unwrap()
                .clone()
                .ok_or(ManagedAgentError::NotFound)
        }

        async fn list(
            &self,
            _: &str,
            _: &AgentListParams,
        ) -> Result<Vec<Agent>, ManagedAgentError> {
            Ok(self.agent.lock().unwrap().clone().into_iter().collect())
        }

        async fn update(
            &self,
            _: &str,
            _: &str,
            params: AgentUpdateParams,
        ) -> Result<Agent, ManagedAgentError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            let mut guard = self.agent.lock().unwrap();
            let agent = guard.as_mut().ok_or(ManagedAgentError::NotFound)?;
            if let Some(name) = params.name {
                agent.name = name;
            }
            if let Some(model) = params.model {
                agent.model = model.into_config().into_resolved();
            }
            agent.version += 1;
            Ok(agent.clone())
        }

        async fn archive(&self, _: &str, _: &str) -> Result<Agent, ManagedAgentError> {
            Err(ManagedAgentError::NotFound)
        }

        async fn versions(&self, _: &str, _: &str) -> Result<Vec<Agent>, ManagedAgentError> {
            Ok(Vec::new())
        }
    }

    struct Policy {
        allow: AtomicBool,
        unavailable: AtomicBool,
    }

    #[async_trait::async_trait]
    impl crate::ManagedInferenceGeoPolicy for Policy {
        async fn authorize(
            &self,
            workspace_id: &str,
            inference_geo: Option<ModelInferenceGeo>,
            _: crate::InferenceGeoCheckpoint,
        ) -> Result<(), crate::InferenceGeoPolicyError> {
            if self.unavailable.load(Ordering::SeqCst) {
                return Err(crate::InferenceGeoPolicyError::Unavailable(
                    "offline".into(),
                ));
            }
            if self.allow.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(crate::InferenceGeoPolicyError::Denied {
                    workspace_id: workspace_id.into(),
                    geo: crate::inference_geo_name(inference_geo),
                })
            }
        }
    }

    async fn request(app: &Router, body: serde_json::Value, uri: &str) -> StatusCode {
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        request
            .extensions_mut()
            .insert(awaken_tenancy::WorkspaceScope("workspace-policy".into()));
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let _ = response.into_body().collect().await.unwrap();
        status
    }

    #[tokio::test]
    async fn agent_save_checks_current_workspace_policy_before_repository_write() {
        // C1 denied create -> 400/no write; C2 allowed create -> write; C3 policy
        // narrows and a name-only update retains the pinned geo -> 400/no write;
        // C4 policy authority outage -> 503/no write.
        let repository = Arc::new(FakeRepository::new());
        let policy = Arc::new(Policy {
            allow: AtomicBool::new(false),
            unavailable: AtomicBool::new(false),
        });
        let app = agents_router(Arc::new(
            AgentRegistryState::from_repository(repository.clone())
                .with_inference_geo_policy(policy.clone()),
        ));
        let create = json!({
            "name": "geo",
            "model": {"id": "model", "inference_geo": "us"}
        });
        assert_eq!(
            request(&app, create.clone(), "/v1/agents").await,
            StatusCode::BAD_REQUEST,
            "C1"
        );
        assert_eq!(repository.writes.load(Ordering::SeqCst), 0, "C1");

        policy.allow.store(true, Ordering::SeqCst);
        assert_eq!(
            request(&app, create, "/v1/agents").await,
            StatusCode::OK,
            "C2"
        );
        assert_eq!(repository.writes.load(Ordering::SeqCst), 1, "C2");

        policy.allow.store(false, Ordering::SeqCst);
        assert_eq!(
            request(
                &app,
                json!({"name": "still pinned"}),
                "/v1/agents/agent-policy",
            )
            .await,
            StatusCode::BAD_REQUEST,
            "C3"
        );
        assert_eq!(repository.writes.load(Ordering::SeqCst), 1, "C3");

        policy.unavailable.store(true, Ordering::SeqCst);
        assert_eq!(
            request(
                &app,
                json!({"model": "model", "metadata": BTreeMap::<String, String>::new()}),
                "/v1/agents/agent-policy",
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE,
            "C4"
        );
        assert_eq!(repository.writes.load(Ordering::SeqCst), 1, "C4");
    }
}
