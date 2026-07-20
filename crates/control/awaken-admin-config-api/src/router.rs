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
    AgentMcpConfig, AgentResourceConfig, InferenceProfile, InferenceProfileStore, McpServerDef,
    McpServerId, McpStore, ResolveError, ResolvedInference, ResourceStore, SourceLookup,
    cooldown_deadline, resolve_inference, resolve_mcp_servers, resolve_profile,
    resolve_profile_candidates,
};
use awaken_credential_vault::repo::{CredentialRepo, enter_credential};
use awaken_credential_vault::{
    AvailabilityLedger, AvailabilityState, CredentialBinding, CredentialCreateParams,
    CredentialError, CredentialKind, CredentialPool, CredentialPoolId, CredentialSource,
    CredentialSourceId, CredentialStatus, SecretStore,
};
use awaken_model_catalog::repo::{CatalogRepo, RepoError};
use awaken_model_catalog::{
    ModelAttributes, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
};
use awaken_runtime_contract::resilience::Disposition;
use axum::extract::{Extension, Path, Query, State};
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
    /// Per-agent [`AgentResourceConfig`] bindings (ADR-0038): which resources an
    /// agent mounts. Rendered into the agent's system prompt at compile (A3a) and
    /// realized into the sandbox at run bind time.
    pub resources: Arc<dyn ResourceStore>,
    /// Optional live credential validator. When wired (server-local injects a
    /// provider-genai probe), `POST /credentials/:id/validate` performs a real probe;
    /// otherwise it reports `unknown` (the model SDK never enters this CRUD crate —
    /// it arrives behind this port).
    pub probe: Option<Arc<dyn CredentialProbe>>,
    /// Credential availability cooldowns (ADR-0043 / E3-4). An operator (or an
    /// external rate-limit signal) cools a source through `POST
    /// /credentials/:id/cooldown`; pool resolution then rotates past it. Shared, so
    /// every route observes the same cooldown state.
    pub availability: Arc<AvailabilityLedger>,
}

/// Trusted workspace coordinate supplied by the composition edge. This adapter
/// owns no tenancy or IAM dependency; it only consumes the already-resolved id.
#[derive(Debug, Clone)]
pub struct ResourceWorkspace(pub String);

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
            "/v1/config/providers/{id}",
            put(put_provider).get(get_provider),
        )
        .route(
            "/v1/config/endpoints/{id}",
            put(put_endpoint).get(get_endpoint),
        )
        .route("/v1/config/offerings", post(post_offering))
        .route(
            "/v1/config/model-attributes/{model_id}",
            put(put_model_attributes),
        )
        .route("/v1/config/catalog", get(get_catalog))
        .route(
            "/v1/config/credentials",
            post(post_credential).get(list_credentials),
        )
        .route("/v1/config/credentials/{id}", get(get_credential))
        .route(
            "/v1/config/credential-pools/{id}",
            put(put_pool).get(get_pool),
        )
        .route(
            "/v1/config/credentials/{id}/archive",
            post(archive_credential),
        )
        .route(
            "/v1/config/credentials/{id}/validate",
            post(validate_credential),
        )
        .route(
            "/v1/config/inference-profiles/{id}",
            put(put_profile).get(get_profile),
        )
        .route(
            "/v1/config/inference-profiles/{id}/resolve",
            post(resolve_profile_route),
        )
        .route(
            "/v1/config/inference-profiles/{id}/resolve-candidates",
            post(resolve_profile_candidates_route),
        )
        .route("/v1/config/inference/resolve", post(resolve_route))
        .route(
            "/v1/config/credentials/{id}/cooldown",
            post(cooldown_credential),
        )
        .route(
            "/v1/config/credentials/{id}/availability",
            get(get_availability),
        )
        .route(
            "/v1/config/credential-pools/{id}/eligible",
            get(get_pool_eligible),
        )
        .route("/v1/config/mcp-servers", get(list_mcp_servers))
        .route(
            "/v1/config/mcp-servers/{id}",
            put(put_mcp_server).get(get_mcp_server),
        )
        .route(
            "/v1/config/agents/{agent_id}/mcp",
            put(put_agent_mcp).get(get_agent_mcp),
        )
        .route(
            "/v1/config/agents/{agent_id}/resources",
            put(put_agent_resource).get(get_agent_resource),
        )
        .route(
            "/v1/config/agents/{agent_id}/mcp/resolve",
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
    // not found); a dialect mismatch that breaks the whole catalog is a 422 invariant.
    state
        .catalog
        .put_offering(offering.clone())
        .await
        .map_err(|e| repo_problem(&e, &req_id(&headers)))?;
    Ok(Json(offering))
}

async fn put_model_attributes(
    State(state): State<AdminState>,
    Path(model_id): Path<String>,
    headers: HeaderMap,
    Json(attrs): Json<ModelAttributes>,
) -> Result<Json<ModelAttributes>, Problem> {
    // Model attributes publish independently of offerings — they carry no
    // provider/endpoint reference (`ProviderCatalog::validate` leaves them
    // unconstrained), so the only failure surface is a whole-catalog invariant (422).
    state
        .catalog
        .put_model_attributes(model_id, attrs.clone())
        .await
        .map_err(|e| repo_problem(&e, &req_id(&headers)))?;
    Ok(Json(attrs))
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
        ResolveError::IncompatibleCredential { .. } => (422, "incompatible_credential"),
        // External error code string stays `pool_exhausted` for wire stability even
        // though the internal variant is now the clearer NoEligibleCredential.
        ResolveError::NoEligibleCredential { .. } => (409, "pool_exhausted"),
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

/// The ordered candidate list a profile resolves to (E3-2): one secret-free view per
/// model in the profile's axis, in failover order.
#[derive(serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResolvedCandidatesView {
    candidates: Vec<ResolvedInferenceView>,
}

/// Dry-run a stored profile's whole model axis (`AxisBinding` pin/pool) into its
/// ordered `(model × credential)` candidate list — the failover order a run would
/// use. An unresolvable model is skipped; all-unresolvable is fail-closed.
async fn resolve_profile_candidates_route(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ResolveProfileRequest>,
) -> Result<Json<ResolvedCandidatesView>, Problem> {
    let rid = req_id(&headers);
    let profile = state
        .profiles
        .get(&id)
        .ok_or_else(|| profile_missing(&id, &rid))?;
    let scoped_workspace = scope.map(|Extension(scope)| scope.0);
    let workspace = scoped_workspace.clone().unwrap_or(request.workspace_id);
    if scoped_workspace.is_some() && profile.workspace_id != workspace {
        return Err(profile_missing(&id, &rid));
    }
    let catalog = state
        .catalog
        .snapshot()
        .await
        .map_err(|e| repo_problem(&e, &rid))?;
    let lookup = workspace_lookup(&state, &workspace, &rid).await?;
    let resolved = resolve_profile_candidates(&catalog, &profile, &lookup, &*state.secrets)
        .await
        .map_err(|e| resolve_problem(&e, &rid))?;
    Ok(Json(ResolvedCandidatesView {
        candidates: resolved.into_iter().map(view_of).collect(),
    }))
}

/// Wall-clock milliseconds since the epoch — the `now` the availability ledger reads.
/// Only the HTTP layer touches the clock; the ledger itself stays time-argument pure.
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A cooldown signal an operator (or an external rate-limit integration) records
/// against a credential source. `kind` maps to a failure
/// [`Disposition`](awaken_runtime_contract::resilience::Disposition): `quota` cools
/// until `retry_after_secs` (or a default window); `exhausted` cools until cleared;
/// `available` / `clear` lifts any cooldown; `transient` / `permanent` are no-ops on
/// availability (they are retry/next-binding decisions, not identity cooldowns).
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CooldownRequest {
    kind: String,
    #[serde(default)]
    retry_after_secs: Option<u64>,
}

async fn cooldown_credential(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(request): Json<CooldownRequest>,
) -> Json<AvailabilityState> {
    let source = CredentialSourceId(id);
    let now = now_ms();
    match request.kind.as_str() {
        "quota" => {
            let disposition = Disposition::Quota {
                retry_after: request.retry_after_secs.map(std::time::Duration::from_secs),
            };
            if let Some(deadline) = cooldown_deadline(disposition, now) {
                state.availability.cool_down(&source, deadline);
            }
        }
        "exhausted" => state.availability.exhaust(&source),
        "available" | "clear" => state.availability.clear(&source),
        // A transient / permanent failure is a retry / next-binding decision, not an
        // identity cooldown — its disposition yields no deadline.
        other => {
            let disposition = match other {
                "permanent" => Disposition::Permanent,
                _ => Disposition::Transient,
            };
            let _ = cooldown_deadline(disposition, now);
        }
    }
    Json(state.availability.state(&source, now))
}

/// The current availability of a credential source (cooldown auto-resumes by time).
async fn get_availability(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Json<AvailabilityState> {
    Json(state.availability.state(&CredentialSourceId(id), now_ms()))
}

/// Which members of a pool are selectable right now — `selection_order` with cooled
/// members dropped (`eligible_order`). The ops view of the mid-run rotation.
#[derive(serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PoolEligibleView {
    eligible: Vec<String>,
    cooled: Vec<String>,
}

async fn get_pool_eligible(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<PoolEligibleView>, Problem> {
    let rid = req_id(&headers);
    let pool = state
        .credentials
        .get_pool(&CredentialPoolId(id))
        .await
        .map_err(|e| cred_problem(&e, &rid))?;
    let now = now_ms();
    let eligible: Vec<String> = pool
        .eligible_order(&state.availability, now)
        .iter()
        .map(|m| m.credential_source_id.0.clone())
        .collect();
    let cooled: Vec<String> = pool
        .selection_order()
        .iter()
        .filter(|m| {
            !state
                .availability
                .is_available(&m.credential_source_id, now)
        })
        .map(|m| m.credential_source_id.0.clone())
        .collect();
    Ok(Json(PoolEligibleView { eligible, cooled }))
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
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut profile): Json<InferenceProfile>,
) -> Result<Json<InferenceProfile>, Problem> {
    if let Some(Extension(scope)) = scope {
        if state
            .profiles
            .get(&id)
            .is_some_and(|current| current.workspace_id != scope.0)
        {
            return Err(profile_missing(&id, &req_id(&headers)));
        }
        profile.workspace_id = scope.0;
    }
    state.profiles.put(id, profile.clone());
    Ok(Json(profile))
}

async fn get_profile(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<InferenceProfile>, Problem> {
    let profile = state
        .profiles
        .get(&id)
        .ok_or_else(|| profile_missing(&id, &req_id(&headers)))?;
    if scope.is_some_and(|Extension(scope)| profile.workspace_id != scope.0) {
        return Err(profile_missing(&id, &req_id(&headers)));
    }
    Ok(Json(profile))
}

/// Resolve an authored profile within a workspace's credential scope.
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResolveProfileRequest {
    workspace_id: String,
}

/// Resolve an authored profile: the same resolution as `inference/resolve`, but the
/// model + binding + disabled endpoints come from the stored [`InferenceProfile`].
async fn resolve_profile_route(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ResolveProfileRequest>,
) -> Result<Json<ResolvedInferenceView>, Problem> {
    let rid = req_id(&headers);
    let profile = state
        .profiles
        .get(&id)
        .ok_or_else(|| profile_missing(&id, &rid))?;
    let scoped_workspace = scope.map(|Extension(scope)| scope.0);
    let workspace = scoped_workspace.clone().unwrap_or(request.workspace_id);
    if scoped_workspace.is_some() && profile.workspace_id != workspace {
        return Err(profile_missing(&id, &rid));
    }
    let catalog = state
        .catalog
        .snapshot()
        .await
        .map_err(|e| repo_problem(&e, &rid))?;
    let lookup = workspace_lookup(&state, &workspace, &rid).await?;
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
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut def): Json<McpServerDef>,
) -> Result<Json<McpServerDef>, Problem> {
    let rid = req_id(&headers);
    def.id = McpServerId(id);
    if let Some(Extension(scope)) = scope {
        if state
            .mcp
            .get_server(&def.id.0)
            .is_some_and(|current| current.workspace_id != scope.0)
        {
            return Err(mcp_server_missing(&def.id.0, &rid));
        }
        def.workspace_id = scope.0;
    }
    match &def.credential_binding {
        CredentialBinding::None => {}
        CredentialBinding::Exact {
            credential_source_id,
        } => {
            let source = state
                .credentials
                .get(credential_source_id)
                .await
                .map_err(|e| cred_problem(&e, &rid))?;
            if !def.workspace_id.is_empty() && source.workspace_id != def.workspace_id {
                return Err(mcp_server_missing(&def.id.0, &rid));
            }
        }
        CredentialBinding::OneOfCredentialPool { credential_pool_id } => {
            let pool = state
                .credentials
                .get_pool(credential_pool_id)
                .await
                .map_err(|e| cred_problem(&e, &rid))?;
            if !def.workspace_id.is_empty() && pool.workspace_id != def.workspace_id {
                return Err(mcp_server_missing(&def.id.0, &rid));
            }
        }
    }
    state.mcp.put_server(def.clone());
    Ok(Json(def))
}

async fn get_mcp_server(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<McpServerDef>, Problem> {
    let def = state
        .mcp
        .get_server(&id)
        .ok_or_else(|| mcp_server_missing(&id, &req_id(&headers)))?;
    if scope.is_some_and(|Extension(scope)| def.workspace_id != scope.0) {
        return Err(mcp_server_missing(&id, &req_id(&headers)));
    }
    Ok(Json(def))
}

async fn list_mcp_servers(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
) -> Json<Vec<McpServerDef>> {
    let mut servers = state.mcp.list_servers();
    if let Some(Extension(scope)) = scope {
        servers.retain(|server| server.workspace_id == scope.0);
    }
    Json(servers)
}

/// Bind which MCP servers an agent uses. The path agent id is authoritative, and
/// every referenced server id must already be authored (fail-closed: a binding to
/// an unknown server is a 404, never a dangling reference).
async fn put_agent_mcp(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    Json(mut config): Json<AgentMcpConfig>,
) -> Result<Json<AgentMcpConfig>, Problem> {
    let rid = req_id(&headers);
    config.agent_id = agent_id;
    let workspace = scope.map(|Extension(scope)| scope.0);
    for server_id in &config.mcp_server_ids {
        if state.mcp.get_server(&server_id.0).is_none_or(|server| {
            workspace
                .as_ref()
                .is_some_and(|workspace| server.workspace_id != *workspace)
        }) {
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
    scope: Option<Extension<ResourceWorkspace>>,
    Path(agent_id): Path<String>,
    Json(mut config): Json<AgentResourceConfig>,
) -> Json<AgentResourceConfig> {
    config.agent_id = agent_id;
    let workspace = scope.map_or_else(String::new, |Extension(scope)| scope.0);
    state
        .resources
        .put_agent_resource_in(&workspace, config.clone());
    Json(config)
}

async fn get_agent_resource(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<AgentResourceConfig>, Problem> {
    let workspace = scope.map_or_else(String::new, |Extension(scope)| scope.0);
    state
        .resources
        .get_agent_resource_in(&workspace, &agent_id)
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

/// Resolve an agent's MCP binding within a workspace's credential scope.
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResolveAgentMcpRequest {
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
    scope: Option<Extension<ResourceWorkspace>>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ResolveAgentMcpRequest>,
) -> Result<Json<Vec<ResolvedMcpServerView>>, Problem> {
    let rid = req_id(&headers);
    let config = state
        .mcp
        .get_agent_config(&agent_id)
        .ok_or_else(|| agent_mcp_missing(&agent_id, &rid))?;
    let scoped_workspace = scope.map(|Extension(scope)| scope.0);
    let workspace = scoped_workspace.clone().unwrap_or(request.workspace_id);
    let mut defs = Vec::with_capacity(config.mcp_server_ids.len());
    for server_id in &config.mcp_server_ids {
        let def = state
            .mcp
            .get_server(&server_id.0)
            .ok_or_else(|| mcp_server_missing(&server_id.0, &rid))?;
        if scoped_workspace.is_some() && def.workspace_id != workspace {
            return Err(mcp_server_missing(&server_id.0, &rid));
        }
        defs.push(def);
    }
    let lookup = workspace_lookup(&state, &workspace, &rid).await?;
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

/// Live-validate a credential against a model's resolved provider endpoint.
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ValidateCredentialRequest {
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
    Json(request): Json<ValidateCredentialRequest>,
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EnterCredentialRequest {
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
        oauth_command: None,
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

#[cfg(test)]
mod tests {
    //! Unit coverage of the pure problem-detail mappers (CEG 09: repo_problem /
    //! cred_problem / resolve_problem). Every error variant is asserted to the
    //! status + code the causal-graph spec pins, including the catch-all arms —
    //! the fail-closed default must never widen to a permissive status.
    use super::*;
    use awaken_model_catalog::CatalogError;

    // --- repo_problem: (a) NotFound→404; (b) Invariant→422 -------------------
    #[test]
    fn repo_problem_provider_not_found_is_404() {
        let p = repo_problem(&RepoError::ProviderNotFound("x".into()), "rid");
        assert_eq!(p.0.status, 404);
        assert_eq!(p.0.code, "not_found");
    }

    #[test]
    fn repo_problem_endpoint_not_found_is_404() {
        let p = repo_problem(&RepoError::EndpointNotFound("x".into()), "rid");
        assert_eq!(p.0.status, 404);
        assert_eq!(p.0.code, "not_found");
    }

    #[test]
    fn repo_problem_invariant_is_422() {
        let p = repo_problem(
            &RepoError::Invariant(CatalogError::OfferingEndpointUnknown {
                model: "m".into(),
                endpoint: "e".into(),
            }),
            "rid",
        );
        assert_eq!(p.0.status, 422);
        assert_eq!(p.0.code, "catalog_invariant");
    }

    // --- cred_problem: (a) NotFound→404; (b) NoCredential→422;
    //     (c) NotActive→409; (d) `_`→422 ---------------------------------------
    #[test]
    fn cred_problem_not_found_family_is_404() {
        for e in [
            CredentialError::SourceNotFound("s".into()),
            CredentialError::SecretNotFound("s".into()),
            CredentialError::PoolNotFound("p".into()),
        ] {
            let p = cred_problem(&e, "rid");
            assert_eq!(p.0.status, 404, "{e:?}");
            assert_eq!(p.0.code, "not_found", "{e:?}");
        }
    }

    #[test]
    fn cred_problem_no_credential_is_422() {
        let p = cred_problem(&CredentialError::NoCredential, "rid");
        assert_eq!(p.0.status, 422);
        assert_eq!(p.0.code, "no_credential");
    }

    #[test]
    fn cred_problem_not_active_is_409() {
        let p = cred_problem(&CredentialError::NotActive("s".into()), "rid");
        assert_eq!(p.0.status, 409);
        assert_eq!(p.0.code, "credential_inactive");
    }

    #[test]
    fn cred_problem_catch_all_is_422_credential_invalid() {
        // The `_` arm must fail closed to 422 for every non-enumerated variant.
        for e in [
            CredentialError::MissingMaterialRef("s".into()),
            CredentialError::MissingEnv("s".into()),
            CredentialError::Seal,
            CredentialError::OAuth("boom".into()),
            CredentialError::Storage("io".into()),
        ] {
            let p = cred_problem(&e, "rid");
            assert_eq!(p.0.status, 422, "{e:?}");
            assert_eq!(p.0.code, "credential_invalid", "{e:?}");
        }
    }

    // --- resolve_problem: (a) ModelUnresolved→404; (b) EndpointMissing→422;
    //     (c) Source/PoolMissing→404; (d) Incompatible→422; (e) NoEligible→409;
    //     (f) Credential→422 ----------------------------------------------------
    #[test]
    fn resolve_problem_model_unresolved_is_404() {
        let p = resolve_problem(&ResolveError::ModelUnresolved("m".into()), "rid");
        assert_eq!(p.0.status, 404);
        assert_eq!(p.0.code, "model_unresolved");
    }

    #[test]
    fn resolve_problem_endpoint_missing_is_422() {
        let p = resolve_problem(&ResolveError::EndpointMissing("e".into()), "rid");
        assert_eq!(p.0.status, 422);
        assert_eq!(p.0.code, "endpoint_missing");
    }

    #[test]
    fn resolve_problem_source_and_pool_missing_are_404() {
        for e in [
            ResolveError::SourceMissing("s".into()),
            ResolveError::PoolMissing("p".into()),
        ] {
            let p = resolve_problem(&e, "rid");
            assert_eq!(p.0.status, 404, "{e:?}");
            assert_eq!(p.0.code, "not_found", "{e:?}");
        }
    }

    #[test]
    fn resolve_problem_incompatible_credential_is_422() {
        let p = resolve_problem(
            &ResolveError::IncompatibleCredential {
                source_id: "s".into(),
                provider_id: "anthropic".into(),
            },
            "rid",
        );
        assert_eq!(p.0.status, 422);
        assert_eq!(p.0.code, "incompatible_credential");
    }

    #[test]
    fn resolve_problem_no_eligible_credential_is_409_pool_exhausted() {
        // Wire code stays `pool_exhausted` even though the variant is NoEligibleCredential.
        let p = resolve_problem(
            &ResolveError::NoEligibleCredential {
                pool_id: "p".into(),
                total: 2,
                cooled: 1,
                over_capacity: 0,
            },
            "rid",
        );
        assert_eq!(p.0.status, 409);
        assert_eq!(p.0.code, "pool_exhausted");
    }

    #[test]
    fn resolve_problem_credential_is_422() {
        let p = resolve_problem(&ResolveError::Credential(CredentialError::Seal), "rid");
        assert_eq!(p.0.status, 422);
        assert_eq!(p.0.code, "credential_invalid");
    }
}
