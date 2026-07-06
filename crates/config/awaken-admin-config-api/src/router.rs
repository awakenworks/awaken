//! HTTP CRUD for the admin config plane (ADR-0043 L1): the self-hosted-only
//! management surface the Anthropic Managed wire does not define. It authors the
//! catalog (provider / protocol-endpoint / offering) and enters credentials, then
//! the resolver reads that same catalog to bind a run.
//!
//! The router is a thin adapter over the domain repos (`CatalogRepo`,
//! `CredentialRepo`, `SecretStore`) — it owns no storage of its own. Failures
//! speak RFC-9457 problem details ([`ApiError`]); a credential's secret is
//! write-only (secret-in) and never echoed on a response (secret-free-out).

use std::collections::HashMap;
use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_api_contract::{ApiError, PROBLEM_JSON_CONTENT_TYPE, REQUEST_ID_HEADER};
use awaken_config_resolver::{
    AgentMcpConfig, AgentResourceConfig, InferenceProfile, InferenceProfileStore, InvalidProjectId,
    McpServerDef, McpServerId, McpStore, Project, ProjectAgentConfig, ProjectId, ProjectStore,
    ResolveError, ResolvedInference, ResourceStore, SourceLookup, resolve_inference,
    resolve_mcp_servers, resolve_profile,
};
use awaken_credential_vault::repo::{CredentialRepo, enter_credential};
use awaken_credential_vault::{
    CredentialBinding, CredentialCreateParams, CredentialError, CredentialKind, CredentialPool,
    CredentialPoolId, CredentialSource, CredentialSourceId, CredentialStatus, SecretStore,
};
use awaken_model_catalog::repo::{CatalogRepo, RepoError};
use awaken_model_catalog::{Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};

/// The admin config plane's injected stores. The router depends on the domain
/// ports, not a concrete backend, so the same routes serve the in-memory dev
/// wiring and a sqlite/postgres deployment (ADR-0043, split/merge-friendly).
#[derive(Clone)]
pub struct AdminState {
    pub catalog: Arc<dyn CatalogRepo>,
    pub credentials: Arc<dyn CredentialRepo>,
    pub secrets: Arc<dyn SecretStore>,
    /// Authored [`InferenceProfile`]s, keyed by id (the resolver reads these).
    pub profiles: Arc<dyn InferenceProfileStore>,
    /// Authored [`McpServerDef`]s + per-agent [`AgentMcpConfig`] bindings
    /// (ADR-0043 Phase 3; the resolver materializes these at run bind time).
    pub mcp: Arc<dyn McpStore>,
    /// Authored [`Project`]s + per-(project, agent) [`ProjectAgentConfig`]
    /// consumption bindings (a project SELECTS from workspace supply; the
    /// session ingress consults these when a run arrives via `/projects/{id}`).
    pub projects: Arc<dyn ProjectStore>,
    /// Per-agent [`AgentResourceConfig`] bindings (ADR-0038): which resources an
    /// agent mounts. Rendered into the agent's system prompt at compile (A3a) and
    /// realized into the sandbox at run bind time.
    pub resources: Arc<dyn ResourceStore>,
    /// Optional live credential validator. When wired (server-local injects a
    /// provider-genai probe), `POST /credentials/:id/validate` performs a real probe;
    /// otherwise it reports `unknown` (the model SDK never enters this CRUD crate —
    /// it arrives behind this port).
    pub probe: Option<Arc<dyn CredentialProbe>>,
}

/// The result of a live credential probe (secret-free), aligned with the Managed
/// wire's `valid` / `invalid` / `unknown` statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    Valid,
    Invalid,
    Unknown,
}

/// A port that live-probes a resolved credential against its provider endpoint. The
/// implementation (server-local, backed by provider-genai) is the only place the
/// model SDK is named — the CRUD crate stays SDK-free.
#[async_trait::async_trait]
pub trait CredentialProbe: Send + Sync {
    async fn probe(&self, base_url: &str, secret: &RedactedString, model: &str) -> ProbeStatus;
}

/// Author the model catalog and enter credentials. The resolver consumes the same
/// catalog snapshot (`GET /v1/config/catalog`) to bind a run.
pub fn admin_router(state: AdminState) -> Router {
    Router::new()
        .route(
            "/v1/config/providers/:id",
            put(put_provider).get(get_provider),
        )
        .route(
            "/v1/config/endpoints/:id",
            put(put_endpoint).get(get_endpoint),
        )
        .route("/v1/config/offerings", post(post_offering))
        .route("/v1/config/catalog", get(get_catalog))
        .route(
            "/v1/config/credentials",
            post(post_credential).get(list_credentials),
        )
        .route("/v1/config/credentials/:id", get(get_credential))
        .route(
            "/v1/config/credential-pools/:id",
            put(put_pool).get(get_pool),
        )
        .route(
            "/v1/config/credentials/:id/archive",
            post(archive_credential),
        )
        .route(
            "/v1/config/credentials/:id/validate",
            post(validate_credential),
        )
        .route(
            "/v1/config/inference-profiles/:id",
            put(put_profile).get(get_profile),
        )
        .route(
            "/v1/config/inference-profiles/:id/resolve",
            post(resolve_profile_route),
        )
        .route("/v1/config/inference/resolve", post(resolve_route))
        .route("/v1/config/mcp-servers", get(list_mcp_servers))
        .route(
            "/v1/config/mcp-servers/:id",
            put(put_mcp_server).get(get_mcp_server),
        )
        .route("/v1/config/projects", get(list_projects))
        .route("/v1/config/projects/:id", put(put_project).get(get_project))
        .route(
            "/v1/config/projects/:project_id/agents/:agent_id/mcp",
            put(put_project_agent_mcp).get(get_project_agent_mcp),
        )
        .route(
            "/v1/config/agents/:agent_id/mcp",
            put(put_agent_mcp).get(get_agent_mcp),
        )
        .route(
            "/v1/config/agents/:agent_id/resources",
            put(put_agent_resource).get(get_agent_resource),
        )
        .route(
            "/v1/config/agents/:agent_id/mcp/resolve",
            post(resolve_agent_mcp),
        )
        .with_state(state)
}

/// An [`ApiError`] rendered as an RFC-9457 `application/problem+json` response.
struct Problem(ApiError);

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            status,
            [(CONTENT_TYPE, PROBLEM_JSON_CONTENT_TYPE)],
            Json(self.0),
        )
            .into_response()
    }
}

/// The request correlation id echoed on errors, from the request header (else `-`).
fn req_id(headers: &HeaderMap) -> String {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string()
}

fn repo_problem(error: &RepoError, rid: &str) -> Problem {
    let (status, code) = match error {
        RepoError::ProviderNotFound(_) | RepoError::EndpointNotFound(_) => (404, "not_found"),
        RepoError::Invariant(_) => (422, "catalog_invariant"),
    };
    Problem(ApiError::new(
        status,
        code,
        "Catalog error",
        error.to_string(),
        rid,
    ))
}

fn cred_problem(error: &CredentialError, rid: &str) -> Problem {
    let (status, code) = match error {
        CredentialError::SourceNotFound(_)
        | CredentialError::SecretNotFound(_)
        | CredentialError::PoolNotFound(_) => (404, "not_found"),
        CredentialError::NoCredential => (422, "no_credential"),
        CredentialError::NotActive(_) => (409, "credential_inactive"),
        _ => (422, "credential_invalid"),
    };
    Problem(ApiError::new(
        status,
        code,
        "Credential error",
        error.to_string(),
        rid,
    ))
}

async fn put_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut provider): Json<Provider>,
) -> Result<Json<Provider>, Problem> {
    // The path id is authoritative, so a client cannot upsert under a mismatched id.
    provider.id = ProviderId::new(id);
    state
        .catalog
        .put_provider(provider.clone())
        .await
        .map_err(|e| repo_problem(&e, &req_id(&headers)))?;
    Ok(Json(provider))
}

async fn get_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Provider>, Problem> {
    state
        .catalog
        .get_provider(&ProviderId::new(id))
        .await
        .map(Json)
        .map_err(|e| repo_problem(&e, &req_id(&headers)))
}

async fn put_endpoint(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut endpoint): Json<ProtocolEndpoint>,
) -> Result<Json<ProtocolEndpoint>, Problem> {
    endpoint.id = ProtocolEndpointId::new(id);
    state
        .catalog
        .put_endpoint(endpoint.clone())
        .await
        .map_err(|e| repo_problem(&e, &req_id(&headers)))?;
    Ok(Json(endpoint))
}

async fn get_endpoint(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ProtocolEndpoint>, Problem> {
    state
        .catalog
        .get_endpoint(&ProtocolEndpointId::new(id))
        .await
        .map(Json)
        .map_err(|e| repo_problem(&e, &req_id(&headers)))
}

async fn post_offering(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(offering): Json<Offering>,
) -> Result<Json<Offering>, Problem> {
    // Fail-closed reference integrity (offering → provider/endpoint) lives in the
    // repo's put_offering: a dangling endpoint ref is a 404 (referenced resource
    // not found); a flavor mismatch that breaks the whole catalog is a 422 invariant.
    state
        .catalog
        .put_offering(offering.clone())
        .await
        .map_err(|e| repo_problem(&e, &req_id(&headers)))?;
    Ok(Json(offering))
}

async fn get_catalog(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<awaken_model_catalog::ProviderCatalog>, Problem> {
    state
        .catalog
        .snapshot()
        .await
        .map(Json)
        .map_err(|e| repo_problem(&e, &req_id(&headers)))
}

async fn put_pool(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut pool): Json<CredentialPool>,
) -> Result<Json<CredentialPool>, Problem> {
    // The path id is authoritative.
    pool.id = CredentialPoolId(id);
    state
        .credentials
        .put_pool(pool.clone())
        .await
        .map_err(|e| cred_problem(&e, &req_id(&headers)))?;
    Ok(Json(pool))
}

async fn get_pool(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CredentialPool>, Problem> {
    state
        .credentials
        .get_pool(&CredentialPoolId(id))
        .await
        .map(Json)
        .map_err(|e| cred_problem(&e, &req_id(&headers)))
}

/// A resolver lookup backed by a workspace snapshot: the credential sources **and**
/// pools the resolver may bind to. `get_pool` is what makes the `OneOfCredentialPool`
/// binding (with failover) resolvable over the admin API.
struct WorkspaceLookup {
    sources: HashMap<String, CredentialSource>,
    pools: HashMap<String, CredentialPool>,
}

impl SourceLookup for WorkspaceLookup {
    fn get(&self, id: &str) -> Option<&CredentialSource> {
        self.sources.get(id)
    }
    fn get_pool(&self, id: &str) -> Option<&CredentialPool> {
        self.pools.get(id)
    }
}

fn resolve_problem(error: &ResolveError, rid: &str) -> Problem {
    let (status, code) = match error {
        ResolveError::ModelUnresolved(_) => (404, "model_unresolved"),
        ResolveError::EndpointMissing(_) => (422, "endpoint_missing"),
        ResolveError::SourceMissing(_) | ResolveError::PoolMissing(_) => (404, "not_found"),
        ResolveError::PoolExhausted(_) => (409, "pool_exhausted"),
        ResolveError::Credential(_) => (422, "credential_invalid"),
    };
    Problem(ApiError::new(
        status,
        code,
        "Resolution error",
        error.to_string(),
        rid,
    ))
}

/// A dry-run resolve request: bind `model_id` (+ credential `binding`) against the
/// authored catalog. The workspace scopes which credential sources are visible.
#[derive(serde::Deserialize)]
pub struct ResolveRequest {
    workspace_id: String,
    model_id: String,
    binding: CredentialBinding,
}

/// The **secret-free** result of a resolve (ADR-0043): the execution triple + the
/// adapter/endpoint it binds to, and whether a credential resolved — never the
/// secret itself. This is what an operator's "test binding" call sees.
#[derive(serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResolvedInferenceView {
    model_id: String,
    provider_id: String,
    protocol_endpoint_id: String,
    adapter_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    /// Whether a credential was materialized (never the value).
    credential_present: bool,
}

/// Dry-run a binding through the resolver against the authored catalog, returning
/// the secret-free resolved triple. This exercises the same `resolve_inference`
/// path a run uses, so an operator can validate a provider/endpoint/model +
/// credential wiring before creating an agent.
async fn resolve_route(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<ResolvedInferenceView>, Problem> {
    let rid = req_id(&headers);
    let catalog = state
        .catalog
        .snapshot()
        .await
        .map_err(|e| repo_problem(&e, &rid))?;
    let lookup = workspace_lookup(&state, &request.workspace_id, &rid).await?;
    let resolved = resolve_inference(
        &catalog,
        &request.model_id,
        &request.binding,
        &lookup,
        &*state.secrets,
    )
    .await
    .map_err(|e| resolve_problem(&e, &rid))?;
    Ok(Json(view_of(resolved)))
}

/// The secret-free projection of a [`ResolvedInference`].
fn view_of(resolved: ResolvedInference) -> ResolvedInferenceView {
    ResolvedInferenceView {
        model_id: resolved.triple.model_id,
        provider_id: resolved.triple.provider_id,
        protocol_endpoint_id: resolved.triple.protocol_endpoint_id,
        adapter_kind: resolved.adapter_kind.to_string(),
        base_url: resolved.base_url,
        credential_present: resolved.credential.is_some(),
    }
}

/// Snapshot a workspace's credential sources + pools into a resolver lookup.
async fn workspace_lookup(
    state: &AdminState,
    workspace_id: &str,
    rid: &str,
) -> Result<WorkspaceLookup, Problem> {
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    for source in state
        .credentials
        .list(workspace_id)
        .await
        .map_err(|e| cred_problem(&e, rid))?
    {
        sources.insert(source.id.0.clone(), source);
    }
    let mut pools: HashMap<String, CredentialPool> = HashMap::new();
    for pool in state
        .credentials
        .list_pools(workspace_id)
        .await
        .map_err(|e| cred_problem(&e, rid))?
    {
        pools.insert(pool.id.0.clone(), pool);
    }
    Ok(WorkspaceLookup { sources, pools })
}

async fn put_profile(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(profile): Json<InferenceProfile>,
) -> Result<Json<InferenceProfile>, Problem> {
    state.profiles.put(id, profile.clone());
    Ok(Json(profile))
}

async fn get_profile(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<InferenceProfile>, Problem> {
    state
        .profiles
        .get(&id)
        .map(Json)
        .ok_or_else(|| profile_missing(&id, &req_id(&headers)))
}

#[derive(serde::Deserialize)]
struct ResolveProfileRequest {
    workspace_id: String,
}

/// Resolve an authored profile: the same resolution as `inference/resolve`, but the
/// model + binding + disabled endpoints come from the stored [`InferenceProfile`].
async fn resolve_profile_route(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ResolveProfileRequest>,
) -> Result<Json<ResolvedInferenceView>, Problem> {
    let rid = req_id(&headers);
    let profile = state
        .profiles
        .get(&id)
        .ok_or_else(|| profile_missing(&id, &rid))?;
    let catalog = state
        .catalog
        .snapshot()
        .await
        .map_err(|e| repo_problem(&e, &rid))?;
    let lookup = workspace_lookup(&state, &request.workspace_id, &rid).await?;
    let resolved = resolve_profile(&catalog, &profile, &lookup, &*state.secrets)
        .await
        .map_err(|e| resolve_problem(&e, &rid))?;
    Ok(Json(view_of(resolved)))
}

fn profile_missing(id: &str, rid: &str) -> Problem {
    Problem(ApiError::new(
        404,
        "not_found",
        "Profile not found",
        format!("no inference profile `{id}`"),
        rid,
    ))
}

/// Author an MCP server definition. The path id is authoritative, and the
/// credential binding is validated fail-closed on write: an `Exact` binding
/// referencing an unknown credential source (or a pool binding referencing an
/// unknown pool) is a 404, so a def that can never resolve is never stored.
async fn put_mcp_server(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut def): Json<McpServerDef>,
) -> Result<Json<McpServerDef>, Problem> {
    let rid = req_id(&headers);
    def.id = McpServerId(id);
    match &def.credential_binding {
        CredentialBinding::None => {}
        CredentialBinding::Exact {
            credential_source_id,
        } => {
            state
                .credentials
                .get(credential_source_id)
                .await
                .map_err(|e| cred_problem(&e, &rid))?;
        }
        CredentialBinding::OneOfCredentialPool { credential_pool_id } => {
            state
                .credentials
                .get_pool(credential_pool_id)
                .await
                .map_err(|e| cred_problem(&e, &rid))?;
        }
    }
    state.mcp.put_server(def.clone());
    Ok(Json(def))
}

async fn get_mcp_server(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<McpServerDef>, Problem> {
    state
        .mcp
        .get_server(&id)
        .map(Json)
        .ok_or_else(|| mcp_server_missing(&id, &req_id(&headers)))
}

async fn list_mcp_servers(State(state): State<AdminState>) -> Json<Vec<McpServerDef>> {
    Json(state.mcp.list_servers())
}

/// Bind which MCP servers an agent uses. The path agent id is authoritative, and
/// every referenced server id must already be authored (fail-closed: a binding to
/// an unknown server is a 404, never a dangling reference).
async fn put_agent_mcp(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    Json(mut config): Json<AgentMcpConfig>,
) -> Result<Json<AgentMcpConfig>, Problem> {
    let rid = req_id(&headers);
    config.agent_id = agent_id;
    for server_id in &config.mcp_server_ids {
        if state.mcp.get_server(&server_id.0).is_none() {
            return Err(mcp_server_missing(&server_id.0, &rid));
        }
    }
    state.mcp.put_agent_config(config.clone());
    Ok(Json(config))
}

async fn get_agent_mcp(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<AgentMcpConfig>, Problem> {
    state
        .mcp
        .get_agent_config(&agent_id)
        .map(Json)
        .ok_or_else(|| agent_mcp_missing(&agent_id, &req_id(&headers)))
}

/// Bind which resources an agent mounts (ADR-0038). The path agent id is
/// authoritative; the binding set is stored whole (upsert by agent id).
async fn put_agent_resource(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    Json(mut config): Json<AgentResourceConfig>,
) -> Json<AgentResourceConfig> {
    config.agent_id = agent_id;
    state.resources.put_agent_resource(config.clone());
    Json(config)
}

async fn get_agent_resource(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<AgentResourceConfig>, Problem> {
    state
        .resources
        .get_agent_resource(&agent_id)
        .map(Json)
        .ok_or_else(|| agent_resource_missing(&agent_id, &req_id(&headers)))
}

fn agent_resource_missing(agent_id: &str, rid: &str) -> Problem {
    Problem(ApiError::new(
        404,
        "not_found",
        "Agent resource config not found",
        format!("no resource config for agent `{agent_id}`"),
        rid,
    ))
}

#[derive(serde::Deserialize)]
struct ResolveAgentMcpRequest {
    workspace_id: String,
}

/// The **secret-free** projection of a resolved MCP server (ADR-0043): what the
/// agent's binding materializes to, and whether a credential resolved — never the
/// secret itself. This is what an operator's "test binding" call sees.
#[derive(serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResolvedMcpServerView {
    name: String,
    url: String,
    /// Whether a credential was materialized (never the value).
    credential_present: bool,
}

/// Dry-run an agent's MCP binding through the resolver: load the agent's
/// [`AgentMcpConfig`], collect the referenced [`McpServerDef`]s, and materialize
/// each credential binding against the workspace's sources/pools — the same
/// `resolve_mcp_servers` path a run uses — returning the secret-free views.
async fn resolve_agent_mcp(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ResolveAgentMcpRequest>,
) -> Result<Json<Vec<ResolvedMcpServerView>>, Problem> {
    let rid = req_id(&headers);
    let config = state
        .mcp
        .get_agent_config(&agent_id)
        .ok_or_else(|| agent_mcp_missing(&agent_id, &rid))?;
    let mut defs = Vec::with_capacity(config.mcp_server_ids.len());
    for server_id in &config.mcp_server_ids {
        defs.push(
            state
                .mcp
                .get_server(&server_id.0)
                .ok_or_else(|| mcp_server_missing(&server_id.0, &rid))?,
        );
    }
    let lookup = workspace_lookup(&state, &request.workspace_id, &rid).await?;
    let resolved = resolve_mcp_servers(&defs, &lookup, &*state.secrets)
        .await
        .map_err(|e| resolve_problem(&e, &rid))?;
    Ok(Json(
        resolved
            .into_iter()
            .map(|s| ResolvedMcpServerView {
                name: s.name,
                url: s.url,
                credential_present: s.credential.is_some(),
            })
            .collect(),
    ))
}

/// Author a project. The path id is authoritative and doubles as the ingress
/// address segment, so it must be a DNS-safe lowercase slug (the shared tenancy
/// rule, [`ProjectId::parse`]); fail closed on anything else.
async fn put_project(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut project): Json<Project>,
) -> Result<Json<Project>, Problem> {
    let rid = req_id(&headers);
    let project_id = ProjectId::parse(id).map_err(|InvalidProjectId(id)| {
        Problem(ApiError::new(
            422,
            "invalid_project_id",
            "Invalid project id",
            format!(
                "project id `{id}` must be a DNS-safe lowercase slug ([a-z0-9-], 1-50 chars, no leading/trailing `-`)"
            ),
            &rid,
        ))
    })?;
    if project.workspace_id.is_empty() {
        return Err(Problem(ApiError::new(
            422,
            "invalid_project",
            "Invalid project",
            "workspace_id must not be empty".to_string(),
            &rid,
        )));
    }
    project.id = project_id;
    state.projects.put_project(project.clone());
    Ok(Json(project))
}

async fn get_project(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Project>, Problem> {
    state
        .projects
        .get_project(&id)
        .map(Json)
        .ok_or_else(|| project_missing(&id, &req_id(&headers)))
}

async fn list_projects(State(state): State<AdminState>) -> Json<Vec<Project>> {
    Json(state.projects.list_projects())
}

/// Bind which MCP servers an agent uses WITHIN one project. Path ids are
/// authoritative; the project and every referenced server must already be
/// authored (fail-closed — a project binding can only select existing supply).
async fn put_project_agent_mcp(
    State(state): State<AdminState>,
    Path((project_id, agent_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(mut config): Json<ProjectAgentConfig>,
) -> Result<Json<ProjectAgentConfig>, Problem> {
    let rid = req_id(&headers);
    if state.projects.get_project(&project_id).is_none() {
        return Err(project_missing(&project_id, &rid));
    }
    for server_id in &config.mcp_server_ids {
        if state.mcp.get_server(&server_id.0).is_none() {
            return Err(mcp_server_missing(&server_id.0, &rid));
        }
    }
    config.project_id = ProjectId(project_id);
    config.agent_id = agent_id;
    state.projects.put_project_agent(config.clone());
    Ok(Json(config))
}

async fn get_project_agent_mcp(
    State(state): State<AdminState>,
    Path((project_id, agent_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<ProjectAgentConfig>, Problem> {
    state
        .projects
        .get_project_agent(&project_id, &agent_id)
        .map(Json)
        .ok_or_else(|| project_agent_mcp_missing(&project_id, &agent_id, &req_id(&headers)))
}

fn project_missing(id: &str, rid: &str) -> Problem {
    Problem(ApiError::new(
        404,
        "not_found",
        "Project not found",
        format!("no project `{id}`"),
        rid,
    ))
}

fn project_agent_mcp_missing(project_id: &str, agent_id: &str, rid: &str) -> Problem {
    Problem(ApiError::new(
        404,
        "not_found",
        "Project agent MCP config not found",
        format!("no mcp config for agent `{agent_id}` in project `{project_id}`"),
        rid,
    ))
}

fn mcp_server_missing(id: &str, rid: &str) -> Problem {
    Problem(ApiError::new(
        404,
        "not_found",
        "MCP server not found",
        format!("no mcp server `{id}`"),
        rid,
    ))
}

fn agent_mcp_missing(agent_id: &str, rid: &str) -> Problem {
    Problem(ApiError::new(
        404,
        "not_found",
        "Agent MCP config not found",
        format!("no mcp config for agent `{agent_id}`"),
        rid,
    ))
}

/// Disable a credential (soft archive): a disabled source fails closed at
/// materialization, so a leaked/rotated key can be pulled without deleting the row.
async fn archive_credential(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CredentialSource>, Problem> {
    let rid = req_id(&headers);
    let mut source = state
        .credentials
        .get(&CredentialSourceId(id))
        .await
        .map_err(|e| cred_problem(&e, &rid))?;
    source.status = CredentialStatus::Disabled;
    source.version += 1;
    state
        .credentials
        .put(source.clone())
        .await
        .map_err(|e| cred_problem(&e, &rid))?;
    Ok(Json(source))
}

#[derive(serde::Deserialize)]
struct ValidateRequest {
    workspace_id: String,
    model_id: String,
}

/// The secret-free result of a live credential probe.
#[derive(serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CredentialValidation {
    pub status: ProbeStatus,
    pub adapter_kind: String,
}

/// Live-validate a credential: resolve it (Exact binding) to get the endpoint +
/// materialized secret, then probe the provider through the injected port. Reports
/// `unknown` when no probe is wired or the adapter is one the probe can't reach.
async fn validate_credential(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ValidateRequest>,
) -> Result<Json<CredentialValidation>, Problem> {
    let rid = req_id(&headers);
    let catalog = state
        .catalog
        .snapshot()
        .await
        .map_err(|e| repo_problem(&e, &rid))?;
    let lookup = workspace_lookup(&state, &request.workspace_id, &rid).await?;
    let resolved = resolve_inference(
        &catalog,
        &request.model_id,
        &CredentialBinding::Exact {
            credential_source_id: CredentialSourceId(id),
        },
        &lookup,
        &*state.secrets,
    )
    .await
    .map_err(|e| resolve_problem(&e, &rid))?;

    let status = match (&state.probe, resolved.adapter_kind, &resolved.credential) {
        (Some(probe), "anthropic", Some(secret)) => {
            probe
                .probe(
                    resolved.base_url.as_deref().unwrap_or_default(),
                    secret,
                    &request.model_id,
                )
                .await
        }
        _ => ProbeStatus::Unknown,
    };
    Ok(Json(CredentialValidation {
        status,
        adapter_kind: resolved.adapter_kind.to_string(),
    }))
}

/// The credential-entry wire body. `secret` is write-only: it is sealed into the
/// [`SecretStore`] and never appears on any response (the returned row is
/// secret-free). `RedactedString` is intentionally not `Deserialize`, so the raw
/// secret crosses the wire exactly once, here.
#[derive(serde::Deserialize)]
struct EnterCredentialRequest {
    workspace_id: String,
    kind: CredentialKind,
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default)]
    env_key: Option<String>,
    /// The secret to seal — required for `vault`, unused for `env` (which reads a
    /// host variable at materialization), so it defaults to empty.
    #[serde(default)]
    secret: String,
}

async fn post_credential(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(body): Json<EnterCredentialRequest>,
) -> Result<(StatusCode, Json<awaken_credential_vault::CredentialSource>), Problem> {
    let params = CredentialCreateParams {
        workspace_id: body.workspace_id,
        kind: body.kind,
        provider_id: body.provider_id,
        env_key: body.env_key,
        secret: Some(RedactedString::new(body.secret)),
    };
    let source = enter_credential(params, &*state.secrets, &*state.credentials)
        .await
        .map_err(|e| cred_problem(&e, &req_id(&headers)))?;
    Ok((StatusCode::CREATED, Json(source)))
}

async fn get_credential(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<awaken_credential_vault::CredentialSource>, Problem> {
    state
        .credentials
        .get(&CredentialSourceId(id))
        .await
        .map(Json)
        .map_err(|e| cred_problem(&e, &req_id(&headers)))
}

#[derive(serde::Deserialize)]
struct ListCredentialsQuery {
    workspace_id: String,
}

async fn list_credentials(
    State(state): State<AdminState>,
    Query(query): Query<ListCredentialsQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<awaken_credential_vault::CredentialSource>>, Problem> {
    state
        .credentials
        .list(&query.workspace_id)
        .await
        .map(Json)
        .map_err(|e| cred_problem(&e, &req_id(&headers)))
}
