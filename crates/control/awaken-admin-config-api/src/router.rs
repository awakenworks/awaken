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
use awaken_config_resolver::{ResolveError, SourceLookup, resolve_inference};
use awaken_credential_vault::repo::{CredentialRepo, enter_credential};
use awaken_credential_vault::{
    CredentialBinding, CredentialCreateParams, CredentialError, CredentialKind, CredentialPool,
    CredentialPoolId, CredentialSource, CredentialSourceId, SecretStore,
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
        .route("/v1/config/inference/resolve", post(resolve_route))
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
    // Snapshot the workspace's credential sources + pools into a lookup for the
    // resolver (pools carry the failover members).
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    for source in state
        .credentials
        .list(&request.workspace_id)
        .await
        .map_err(|e| cred_problem(&e, &rid))?
    {
        sources.insert(source.id.0.clone(), source);
    }
    let mut pools: HashMap<String, CredentialPool> = HashMap::new();
    for pool in state
        .credentials
        .list_pools(&request.workspace_id)
        .await
        .map_err(|e| cred_problem(&e, &rid))?
    {
        pools.insert(pool.id.0.clone(), pool);
    }
    let lookup = WorkspaceLookup { sources, pools };
    let resolved = resolve_inference(
        &catalog,
        &request.model_id,
        &request.binding,
        &lookup,
        &*state.secrets,
    )
    .await
    .map_err(|e| resolve_problem(&e, &rid))?;
    Ok(Json(ResolvedInferenceView {
        model_id: resolved.triple.model_id,
        provider_id: resolved.triple.provider_id,
        protocol_endpoint_id: resolved.triple.protocol_endpoint_id,
        adapter_kind: resolved.adapter_kind.to_string(),
        base_url: resolved.base_url,
        credential_present: resolved.credential.is_some(),
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
