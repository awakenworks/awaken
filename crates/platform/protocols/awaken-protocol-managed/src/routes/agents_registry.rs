// @formal surface obligation=agent_profile.requested_revision_never_follows_mutable_current
// @formal surface obligation=model_geography.every_attempt_candidate_requires_exact_processing_evidence
// @formal reason: The storage-neutral Agent route preserves explicit immutable versions and invokes the
// @formal reason: current Workspace geography policy before mutation.
//! Managed Agent HTTP adapter (`/v1/agents`).
//!
//! The router is deliberately storage-neutral. Every process startup injects
//! an explicit repository; production uses the durable configuration-plane
//! adapter, so this protocol crate owns no second in-memory Agent aggregate.

use std::sync::Arc;

use crate::common::api_version::{ApiWire, ManagedApiVersion};
use crate::common::scope::RequiredWorkspaceScope;
use crate::routes::{ManagedJson, ManagedQuery};
use crate::types::agent::{
    Agent, AgentCreateParams, AgentListParams, AgentRetrieveParams, AgentUpdateParams,
};
use crate::types::{ErrorResponse, PageCursor, PageQuery, paginate, paginate_by};
use axum::extract::{Path, State};
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

/// Native lifecycle receipt over the same authoritative Agent aggregate. This
/// port value is not the Managed SDK's narrower `Agent` wire representation.
#[derive(Debug, Clone)]
pub struct AgentArchiveReceipt {
    pub id: String,
    pub revision: u64,
    pub archived_at: String,
    pub already_archived: bool,
}

/// Read-only execution facts from the exact immutable publication. These are
/// diagnostic coordinates, never a placement or credential-admission grant.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentExecution {
    pub backend_ref: String,
    pub model_ref: String,
    pub provisioning: AgentModelProvisioning,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentModelProvisioning {
    Provider,
    BackendOwned,
    Remote,
    HostExecutor,
}

/// Safe read-only join keyed by the exact visible publication identity.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentExecutionEvidence {
    pub agent_id: String,
    pub version: u64,
    pub execution: Option<AgentExecution>,
}

/// Durable authoring port for the SDK-facing Agent aggregate.
///
/// `workspace_id` is trusted routing context supplied by the edge. It is an
/// intrinsic repository partition, not an authorization policy or principal.
#[async_trait::async_trait]
pub trait ManagedAgentRepository: Send + Sync {
    /// Read the same visible immutable publications without changing SDK wire.
    async fn execution_evidence(
        &self,
        _workspace_id: &str,
    ) -> Result<Vec<AgentExecutionEvidence>, ManagedAgentError> {
        Err(ManagedAgentError::Invalid(
            "Agent execution evidence is unavailable".into(),
        ))
    }
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
    /// Archive one reviewed revision without requiring a Managed model-id
    /// projection. Production delegates both public representations to one CAS
    /// command. Existing repository implementations fail closed by default.
    async fn archive_at_revision(
        &self,
        _workspace_id: &str,
        _id: &str,
        _expected_revision: u64,
    ) -> Result<AgentArchiveReceipt, ManagedAgentError> {
        Err(ManagedAgentError::Invalid(
            "revision-fenced Agent archive is unavailable for this repository".into(),
        ))
    }
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
    pub async fn execution_evidence(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<AgentExecutionEvidence>, ManagedAgentError> {
        self.repository.execution_evidence(workspace_id).await
    }

    /// Native and Managed HTTP adapters share this state and its one downstream
    /// cascade. The repository remains the sole lifecycle mutation authority.
    pub async fn archive_at_revision(
        &self,
        workspace_id: &str,
        id: &str,
        expected_revision: u64,
    ) -> Result<AgentArchiveReceipt, ManagedAgentError> {
        let receipt = self
            .repository
            .archive_at_revision(workspace_id, id, expected_revision)
            .await?;
        self.archive_dependents(workspace_id, id)
            .await
            .map_err(ManagedAgentError::Storage)?;
        Ok(receipt)
    }

    async fn archive_dependents(&self, workspace_id: &str, id: &str) -> Result<(), String> {
        match &self.archive_cascade {
            Some(cascade) => cascade.archive_agent_dependents(workspace_id, id).await,
            None => Ok(()),
        }
    }
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
    api_version: ManagedApiVersion,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    ManagedJson(params): ManagedJson<AgentCreateParams>,
) -> Result<Json<ApiWire<Agent>>, WireError> {
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
        .map(|agent| Json(api_version.wrap(agent)))
        .map_err(wire_error)
}

async fn retrieve_agent(
    State(state): State<Arc<AgentRegistryState>>,
    api_version: ManagedApiVersion,
    Path(id): Path<String>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    ManagedQuery(query): ManagedQuery<AgentRetrieveParams>,
) -> Result<Json<ApiWire<Agent>>, WireError> {
    state
        .repository
        .retrieve(&scope, &id, query.version)
        .await
        .map(|agent| Json(api_version.wrap(agent)))
        .map_err(wire_error)
}

async fn list_agents(
    State(state): State<Arc<AgentRegistryState>>,
    api_version: ManagedApiVersion,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    ManagedQuery(params): ManagedQuery<AgentListParams>,
) -> Result<Json<ApiWire<PageCursor<Agent>>>, WireError> {
    state
        .repository
        .list(&scope, &params)
        .await
        .map(|agents| {
            Json(
                api_version.wrap(paginate(agents, &params.page_query(), |agent| {
                    agent.id.as_str()
                })),
            )
        })
        .map_err(wire_error)
}

async fn update_agent(
    State(state): State<Arc<AgentRegistryState>>,
    api_version: ManagedApiVersion,
    Path(id): Path<String>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    ManagedJson(params): ManagedJson<AgentUpdateParams>,
) -> Result<Json<ApiWire<Agent>>, WireError> {
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
        .map(|agent| Json(api_version.wrap(agent)))
        .map_err(wire_error)
}

async fn archive_agent(
    State(state): State<Arc<AgentRegistryState>>,
    api_version: ManagedApiVersion,
    Path(id): Path<String>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
) -> Result<Json<ApiWire<Agent>>, WireError> {
    let agent = state
        .repository
        .archive(&scope, &id)
        .await
        .map_err(wire_error)?;
    state
        .archive_dependents(&scope, &id)
        .await
        .map_err(|error| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new("api_error", error)),
            )
        })?;
    Ok(Json(api_version.wrap(agent)))
}

async fn list_versions(
    State(state): State<Arc<AgentRegistryState>>,
    api_version: ManagedApiVersion,
    Path(id): Path<String>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    ManagedQuery(page): ManagedQuery<PageQuery>,
) -> Result<Json<ApiWire<PageCursor<Agent>>>, WireError> {
    state
        .repository
        .versions(&scope, &id)
        .await
        .map(|versions| {
            Json(api_version.wrap(paginate_by(versions, &page, |agent| {
                agent.version.to_string()
            })))
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
    use crate::types::ModelInferenceGeo;

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
            self.writes.fetch_add(1, Ordering::SeqCst);
            let mut guard = self.agent.lock().unwrap();
            let agent = guard.as_mut().ok_or(ManagedAgentError::NotFound)?;
            agent.archived_at = Some("2026-08-18T00:00:01Z".into());
            agent.version += 1;
            Ok(agent.clone())
        }

        async fn versions(&self, _: &str, _: &str) -> Result<Vec<Agent>, ManagedAgentError> {
            Ok(self.agent.lock().unwrap().clone().into_iter().collect())
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
            .insert(awaken_runtime_contract::runs::scope::WorkspaceScope(
                "workspace-policy".into(),
            ));
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let _ = response.into_body().collect().await.unwrap();
        status
    }

    async fn response(app: &Router, method: &str, uri: &str) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(awaken_runtime_contract::runs::scope::WorkspaceScope(
                "workspace-policy".into(),
            ));
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn agent_api_date_and_execution_identity_are_checked_before_repository_effects() {
        // Causes: C1 absent/legacy/current/invalid date; C2 omitted/null/default/
        // AWS/unknown/extra-field identity. Effects: D1 supported dates and
        // default identities -> one write; D2 only current response carries
        // identity; D3 invalid dates/unsupported identity -> 400 + zero writes.
        // The Cartesian rows isolate each selector and prove a positive witness
        // alongside every refusal. Read/list/version/archive use the same DTO.
        for date in [
            None,
            Some("2023-06-01"),
            Some("2026-10-21"),
            Some("2099-01-01"),
        ] {
            for (identity, admitted) in [
                (None, true),
                (Some(json!(null)), true),
                (Some(json!({"type":"service_account"})), true),
                (
                    Some(
                        json!({"type":"aws_role", "role_arn":"arn:aws:iam::123456789012:role/unsupported"}),
                    ),
                    false,
                ),
                (Some(json!({"type":"future"})), false),
                (
                    Some(json!({"type":"service_account", "role_arn":"not-ignored"})),
                    false,
                ),
            ] {
                let repository = Arc::new(FakeRepository::new());
                let app = agents_router(Arc::new(AgentRegistryState::from_repository(
                    repository.clone(),
                )));
                let mut body = json!({"name":"date", "model":"model"});
                if let Some(identity) = identity {
                    body["execution_identity"] = identity;
                }
                let mut builder = Request::post("/v1/agents")
                    .header("content-type", "application/json")
                    .extension(awaken_runtime_contract::runs::scope::WorkspaceScope(
                        "workspace-policy".into(),
                    ));
                if let Some(date) = date {
                    builder = builder.header("anthropic-version", date);
                }
                let response = app
                    .clone()
                    .oneshot(builder.body(Body::from(body.to_string())).unwrap())
                    .await
                    .unwrap();
                let allows = admitted && date != Some("2099-01-01");
                assert_eq!(
                    response.status(),
                    if allows {
                        StatusCode::OK
                    } else {
                        StatusCode::BAD_REQUEST
                    },
                    "date={date:?} body={body}"
                );
                assert_eq!(
                    repository.writes.load(Ordering::SeqCst),
                    usize::from(allows)
                );
                let value: serde_json::Value = serde_json::from_slice(
                    &response.into_body().collect().await.unwrap().to_bytes(),
                )
                .unwrap();
                if allows {
                    assert_eq!(
                        value.get("execution_identity").is_some(),
                        date == Some("2026-10-21")
                    );
                    for path in [
                        "/v1/agents",
                        "/v1/agents/agent-policy",
                        "/v1/agents/agent-policy/versions",
                    ] {
                        let mut builder = Request::get(path).extension(
                            awaken_runtime_contract::runs::scope::WorkspaceScope(
                                "workspace-policy".into(),
                            ),
                        );
                        if let Some(date) = date {
                            builder = builder.header("anthropic-version", date);
                        }
                        let response = app
                            .clone()
                            .oneshot(builder.body(Body::empty()).unwrap())
                            .await
                            .unwrap();
                        assert_eq!(response.status(), StatusCode::OK);
                        let value: serde_json::Value = serde_json::from_slice(
                            &response.into_body().collect().await.unwrap().to_bytes(),
                        )
                        .unwrap();
                        let row = value.get("data").map(|data| &data[0]).unwrap_or(&value);
                        assert_eq!(
                            row.get("execution_identity").is_some(),
                            date == Some("2026-10-21")
                        );
                    }
                } else {
                    assert_eq!(value["error"]["type"], "invalid_request_error");
                }
            }
        }
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

    // Test design: agent_lifecycle_projects_every_official_sdk_operation
    // Cause/effect graph: create establishes revision 1; retrieve
    // and list project that identity without mutation; versions exposes the
    // immutable revision; archive advances once and returns the terminal
    // projection. Every official Agent route participates in this chain.
    // Decision table: absent->create; active->read/update/archive; archived->terminal projection.
    #[tokio::test]
    async fn agent_lifecycle_projects_every_official_sdk_operation() {
        let repository = Arc::new(FakeRepository::new());
        let policy = Arc::new(Policy {
            allow: AtomicBool::new(true),
            unavailable: AtomicBool::new(false),
        });
        let app = agents_router(Arc::new(
            AgentRegistryState::from_repository(repository.clone())
                .with_inference_geo_policy(policy),
        ));
        assert_eq!(
            request(
                &app,
                json!({"name": "lifecycle", "model": "model"}),
                "/v1/agents",
            )
            .await,
            StatusCode::OK,
            "A1/create"
        );

        let (status, retrieved) = response(&app, "GET", "/v1/agents/agent-policy").await;
        assert_eq!(status, StatusCode::OK, "A1/retrieve");
        assert_eq!(retrieved["id"], "agent-policy", "A1/retrieve");
        let (status, listed) = response(&app, "GET", "/v1/agents").await;
        assert_eq!(status, StatusCode::OK, "A1/list");
        assert_eq!(listed["data"][0]["id"], "agent-policy", "A1/list");
        assert_eq!(
            request(
                &app,
                json!({"name": "updated lifecycle"}),
                "/v1/agents/agent-policy",
            )
            .await,
            StatusCode::OK,
            "A1/update"
        );
        let (status, versions) = response(&app, "GET", "/v1/agents/agent-policy/versions").await;
        assert_eq!(status, StatusCode::OK, "A1/versions");
        assert_eq!(versions["data"][0]["version"], 2, "A1/versions");
        let (status, archived) = response(&app, "POST", "/v1/agents/agent-policy/archive").await;
        assert_eq!(status, StatusCode::OK, "A1/archive");
        assert!(archived["archived_at"].is_string(), "A1/archive");
        assert_eq!(repository.writes.load(Ordering::SeqCst), 3, "A1");
    }
}
