//! HTTP CRUD for the admin config plane (ADR-0043 L1): the self-hosted-only
//! management surface the Anthropic Managed wire does not define. It authors the
//! catalog (provider / protocol-endpoint / offering) and enters credentials, then
//! the resolver reads that same catalog to bind a run.
//!
//! The router is a thin adapter over the domain repos (`CatalogRepo`,
//! `CredentialRepo`, `SecretStore`) — it owns no storage of its own. Failures
//! speak RFC-9457 problem details ([`ApiError`]); a credential's secret is
//! write-only (secret-in) and never echoed on a response (secret-free-out).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_api_contract::{ApiError, PROBLEM_JSON_CONTENT_TYPE, REQUEST_ID_HEADER};
use awaken_config_resolver::{
    AgentInputBindingRepository, AgentInputConfig, ConfigRepositoryError, InferenceProfile,
    InferenceProfileStore, ModelTarget, ResolveError, ResolvedInference, SourceLookup,
    cooldown_deadline, get_workspace_profile, put_workspace_profile, resolve_inference,
    resolve_inference_target, resolve_profile, resolve_profile_candidates,
};
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_vault::repo::{
    CredentialMaterialPatch, CredentialRepo, enter_credential, enter_credential_described,
    enter_credential_idempotent_described, enter_credential_idempotent_verified,
    enter_credential_replacement_idempotent_described, rotate_credential_materials_exact,
};
use awaken_credential_vault::{
    AvailabilityLedger, AvailabilityState, CredentialBinding, CredentialCreateParams,
    CredentialError, CredentialKind, CredentialPool, CredentialPoolId, CredentialSource,
    OAuthHelper, SecretStore,
};
use awaken_model_catalog::repo::{CatalogRepo, RepoError};
use awaken_model_catalog::{
    CatalogSyncResult, ModelAttributeSource, ModelAttributes, OfferingSource, OfferingStatus,
};
use awaken_runtime_contract::resilience::Disposition;
use awaken_tenancy::WorkspaceScope as ResourceWorkspace;
use axum::extract::{Extension, FromRef, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};

use crate::provider_connection::{
    ConnectProviderCommand, ModelCatalogDiscovery, ModelCatalogDiscoveryError,
    ProviderConnectionAuthentication, ProviderConnectionError, ProviderConnectionService,
};

mod provider_connections;
pub use provider_connections::{
    ProviderConnectionStatus, ProviderConnectionSummary, ProviderConnectionView,
};
use provider_connections::{list_executable_models, list_provider_connections};

mod credential_wire;
pub use credential_wire::{
    ArchiveCredentialRequest, CredentialSourceView, EnterCredentialRequest, RotateCredentialRequest,
};
use credential_wire::{
    CredentialMaterialInput, credential_material, hosted_credential_matches,
    hosted_credential_source_id, is_hosted_credential, validate_hosted_credential_identity,
};

mod cloud_login;
pub use cloud_login::{CloudLoginApplication, CloudLoginState, CloudLoginStatusView};
use cloud_login::{CloudLoginHandle, cloud_login_router};
mod hosted_credential_request;

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
    /// Per-agent [`AgentInputConfig`] bindings (ADR-0038): which resources an
    /// agent mounts. Rendered into the agent's system prompt at compile (A3a) and
    /// realized into the sandbox at run bind time.
    pub resources: Arc<dyn AgentInputBindingRepository>,
    /// Optional live credential validator. When wired (server-local injects a
    /// provider-genai probe), `POST /credentials/:id/validate` performs a real probe;
    /// otherwise it reports `unknown` (the model SDK never enters this CRUD crate —
    /// it arrives behind this port).
    pub probe: Option<Arc<dyn CredentialProbe>>,
    /// Optional provisioning-side provider model discovery. The HTTP/application
    /// layer passes only an authored endpoint plus a secret-free credential row;
    /// the adapter owns exact credential materialization and provider I/O.
    pub model_discovery: Option<Arc<dyn ModelCatalogDiscovery>>,
    /// Optional authenticated managed-model projection. The adapter may call a
    /// commercial service, but returns only the portable, rebuildable catalog
    /// projection owned by `awaken-model-catalog`.
    pub brokered_catalog: Option<Arc<dyn BrokeredCatalogDiscovery>>,
    /// Credential availability cooldowns (ADR-0043 / E3-4). An operator (or an
    /// external rate-limit signal) cools a source through `POST
    /// /credentials/:id/cooldown`; pool resolution then rotates past it. Shared, so
    /// every route observes the same cooldown state.
    pub availability: Arc<AvailabilityLedger>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IdentityCapabilityView {
    pub mode: String,
    pub cloud_login_enabled: bool,
    pub authenticated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelSupplyCapabilityView {
    pub local_catalog_enabled: bool,
    pub byok_enabled: bool,
    pub cloud_models_enabled: bool,
    pub profile_authoring_enabled: bool,
}

impl Default for ModelSupplyCapabilityView {
    fn default() -> Self {
        Self {
            local_catalog_enabled: true,
            byok_enabled: true,
            cloud_models_enabled: false,
            profile_authoring_enabled: true,
        }
    }
}

/// Product surfaces actually mounted by the same-origin process composition.
/// This is presentation discovery only; mounted routes retain their own PEP.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProductSurfaceCapabilityView {
    pub managed_runtime: bool,
    pub access_management: bool,
}

/// Stable frontend/SDK feature discovery; callers never infer deployment
/// posture from a failed Cloud request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConfigCapabilitiesView {
    pub identity: IdentityCapabilityView,
    pub models: ModelSupplyCapabilityView,
    pub surfaces: ProductSurfaceCapabilityView,
}

impl Default for ConfigCapabilitiesView {
    fn default() -> Self {
        Self {
            identity: IdentityCapabilityView {
                mode: "no-login".into(),
                cloud_login_enabled: false,
                authenticated: false,
            },
            models: ModelSupplyCapabilityView::default(),
            surfaces: ProductSurfaceCapabilityView::default(),
        }
    }
}

/// Live presentation projection for deployment and identity capabilities.
/// The source owns no authentication protocol; it derives status from the
/// composition root's canonical identity owner on every request.
pub trait ConfigCapabilitiesSource: Send + Sync {
    fn current(&self) -> ConfigCapabilitiesView;
}

#[derive(Clone)]
struct StaticConfigCapabilities(ConfigCapabilitiesView);

impl ConfigCapabilitiesSource for StaticConfigCapabilities {
    fn current(&self) -> ConfigCapabilitiesView {
        self.0.clone()
    }
}

#[derive(Clone)]
struct AdminRouterState {
    admin: AdminState,
    capabilities: Arc<dyn ConfigCapabilitiesSource>,
    cloud_login: CloudLoginHandle,
}

impl FromRef<AdminRouterState> for AdminState {
    fn from_ref(state: &AdminRouterState) -> Self {
        state.admin.clone()
    }
}

impl FromRef<AdminRouterState> for ConfigCapabilitiesView {
    fn from_ref(state: &AdminRouterState) -> Self {
        state.capabilities.current()
    }
}

impl FromRef<AdminRouterState> for CloudLoginHandle {
    fn from_ref(state: &AdminRouterState) -> Self {
        state.cloud_login.clone()
    }
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

#[async_trait::async_trait]
pub trait BrokeredCatalogDiscovery: Send + Sync {
    async fn projection(&self) -> Result<awaken_model_catalog::BrokeredCatalogProjection, String>;
}

/// Reconcile one complete managed-model observation through the existing
/// Catalog authority. HTTP refresh and hosted automatic convergence share this
/// application operation; neither transport owns a second synchronization path.
pub async fn reconcile_brokered_catalog(
    discovery: &dyn BrokeredCatalogDiscovery,
    catalog: &dyn CatalogRepo,
) -> Result<CatalogSyncResult, String> {
    let projection = discovery.projection().await?;
    catalog
        .reconcile_brokered_projection(projection)
        .await
        .map_err(|error| error.to_string())
}

/// Author the model catalog and enter credentials. The resolver consumes the same
/// catalog snapshot (`GET /v1/config/catalog`) to bind a run.
pub fn admin_router(state: AdminState) -> Router {
    admin_router_with_capabilities(state, ConfigCapabilitiesView::default())
}

pub fn admin_router_with_capabilities(
    state: AdminState,
    capabilities: ConfigCapabilitiesView,
) -> Router {
    admin_router_with_runtime_capabilities(
        state,
        Arc::new(StaticConfigCapabilities(capabilities)),
        None,
    )
}

pub fn admin_router_with_runtime_capabilities(
    state: AdminState,
    capabilities: Arc<dyn ConfigCapabilitiesSource>,
    cloud_login: Option<Arc<dyn CloudLoginApplication>>,
) -> Router {
    Router::new()
        .merge(cloud_login_router())
        .route("/v1/config/capabilities", get(get_config_capabilities))
        .route(
            "/v1/config/provider-descriptors",
            get(get_provider_descriptors),
        )
        .route(
            "/v1/config/provider-connections",
            post(test_and_save_provider_connection).get(list_provider_connections),
        )
        .route("/v1/config/executable-models", get(list_executable_models))
        .route(
            "/v1/config/model-attributes/{model_id}",
            put(put_model_attributes),
        )
        .route("/v1/config/catalog", get(get_catalog))
        .route(
            "/v1/config/brokered-models/refresh",
            post(refresh_brokered_models),
        )
        .route(
            "/v1/config/credentials",
            post(post_credential).get(list_credentials),
        )
        .route("/v1/config/credentials/{id}", get(get_credential))
        .route(
            "/v1/config/credentials/{id}/rotate",
            post(rotate_credential),
        )
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
        .route(
            "/v1/config/agents/{agent_id}/resources",
            put(put_agent_inputs).get(get_agent_inputs),
        )
        .with_state(AdminRouterState {
            admin: state,
            capabilities,
            cloud_login: CloudLoginHandle(cloud_login),
        })
}

async fn get_config_capabilities(
    State(capabilities): State<ConfigCapabilitiesView>,
) -> Json<ConfigCapabilitiesView> {
    Json(capabilities)
}

async fn refresh_brokered_models(
    State(state): State<AdminState>,
    State(capabilities): State<ConfigCapabilitiesView>,
    headers: HeaderMap,
) -> Result<Json<CatalogSyncResult>, Problem> {
    let rid = req_id(&headers);
    if !capabilities.models.local_catalog_enabled {
        return Err(model_supply_managed(&rid));
    }
    if !capabilities.models.cloud_models_enabled {
        return Err(Problem(ApiError::new(
            409,
            "cloud_models_disabled",
            "Cloud models disabled",
            "enable cloud_models with Awaken Cloud identity before refreshing managed models",
            &rid,
        )));
    }
    if !capabilities.identity.authenticated {
        return Err(Problem(ApiError::new(
            401,
            "cloud_sign_in_required",
            "Awaken Cloud sign-in required",
            "Sign in to Awaken Cloud before refreshing managed models",
            &rid,
        )));
    }
    let discovery = state.brokered_catalog.as_ref().ok_or_else(|| {
        Problem(ApiError::new(
            401,
            "cloud_sign_in_required",
            "Awaken Cloud sign-in required",
            "Sign in to Awaken Cloud before refreshing managed models",
            &rid,
        ))
    })?;
    let result = reconcile_brokered_catalog(discovery.as_ref(), state.catalog.as_ref())
        .await
        .map_err(|detail| {
            Problem(ApiError::new(
                503,
                "brokered_catalog_unavailable",
                "Managed model catalog unavailable",
                detail,
                &rid,
            ))
        })?;
    Ok(Json(result))
}

fn model_supply_managed(rid: &str) -> Problem {
    Problem(ApiError::new(
        403,
        "model_supply_managed",
        "Model supply is managed by the deployment",
        "this deployment exposes read-only managed models and does not accept tenant model-supply configuration",
        rid,
    ))
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

fn model_discovery_problem(error: &ModelCatalogDiscoveryError, rid: &str) -> Problem {
    let (status, code) = match error {
        ModelCatalogDiscoveryError::CredentialUnavailable(_) => {
            (409, "discovery_credential_unavailable")
        }
        ModelCatalogDiscoveryError::Provider(_) => (502, "model_discovery_failed"),
    };
    Problem(ApiError::new(
        status,
        code,
        "Provider model discovery failed",
        error.to_string(),
        rid,
    ))
}

fn provider_connection_problem(error: &ProviderConnectionError, rid: &str) -> Problem {
    match error {
        ProviderConnectionError::Credential(error) => cred_problem(error, rid),
        ProviderConnectionError::Discovery(error) => model_discovery_problem(error, rid),
        ProviderConnectionError::Catalog(error) => repo_problem(error, rid),
        ProviderConnectionError::UnsupportedDialect { .. } => Problem(ApiError::new(
            422,
            "dialect_unsupported",
            "Unsupported provider protocol",
            error.to_string(),
            rid,
        )),
        ProviderConnectionError::UnsupportedAuthentication { .. } => Problem(ApiError::new(
            422,
            "connection_auth_unsupported",
            "Unsupported authentication method",
            error.to_string(),
            rid,
        )),
        ProviderConnectionError::Invalid(_) => Problem(ApiError::new(
            422,
            "connection_auth_invalid",
            "Invalid provider connection",
            error.to_string(),
            rid,
        )),
        ProviderConnectionError::NoModelsDiscovered => Problem(ApiError::new(
            422,
            "no_models_discovered",
            "No models discovered",
            error.to_string(),
            rid,
        )),
    }
}

fn config_repository_problem(error: &ConfigRepositoryError, rid: &str) -> Problem {
    Problem(ApiError::new(
        500,
        "repository_unavailable",
        "Configuration repository unavailable",
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
        CredentialError::MutationConflict(_) => (409, "credential_version_conflict"),
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

async fn get_provider_descriptors() -> Json<Vec<awaken_model_catalog::ProviderDriverDescriptor>> {
    Json(awaken_model_catalog::provider_driver_descriptors())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SaveProviderConnectionRequest {
    pub idempotency_key: String,
    pub workspace_id: String,
    pub provider_id: String,
    pub display_name: String,
    /// Optional human label embedded into the generated credential source id.
    /// It is non-secret and does not replace the stable idempotency fingerprint.
    #[serde(default)]
    pub credential_name: Option<String>,
    pub dialect: awaken_model_catalog::ApiDialect,
    /// Optional stable qualifier for a second endpoint using the same dialect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub configuration: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_connection_timeout")]
    pub timeout_secs: u64,
    /// Write-only API key. Exactly one of `secret`, `oauth_helper`, or
    /// `credential_source_id` must be supplied; the connection command is the
    /// one authoring path for every supported credential source.
    #[serde(default)]
    pub secret: Option<String>,
    /// Server-owned OAuth helper used to mint a short-lived token for both the
    /// pre-save discovery and the persisted credential source.
    #[serde(default)]
    pub oauth_helper: Option<OAuthHelper>,
    /// Reuse one active Workspace credential instead of creating a duplicate.
    #[serde(default)]
    pub credential_source_id: Option<CredentialSourceId>,
}

const fn default_connection_timeout() -> u64 {
    60
}

async fn test_and_save_provider_connection(
    State(state): State<AdminState>,
    State(capabilities): State<ConfigCapabilitiesView>,
    scope: Option<Extension<ResourceWorkspace>>,
    headers: HeaderMap,
    Json(body): Json<SaveProviderConnectionRequest>,
) -> Result<(StatusCode, Json<ProviderConnectionView>), Problem> {
    let rid = req_id(&headers);
    if !capabilities.models.byok_enabled {
        return Err(model_supply_managed(&rid));
    }
    let workspace_id = scope.map_or(body.workspace_id.clone(), |Extension(scope)| scope.0);
    let supplied_auth = usize::from(body.secret.is_some())
        + usize::from(body.oauth_helper.is_some())
        + usize::from(body.credential_source_id.is_some());
    if supplied_auth != 1 {
        return Err(Problem(ApiError::new(
            422,
            "connection_auth_invalid",
            "Choose one authentication method",
            "exactly one of API key, OAuth helper, or existing credential is required",
            &rid,
        )));
    }
    let discovery = state.model_discovery.as_ref().ok_or_else(|| {
        Problem(ApiError::new(
            503,
            "connection_test_unavailable",
            "Connection testing unavailable",
            "no provider discovery adapter is installed",
            &rid,
        ))
    })?;
    let authentication = match (body.secret, body.oauth_helper, body.credential_source_id) {
        (Some(secret), None, None) => {
            ProviderConnectionAuthentication::ApiKey(RedactedString::new(secret))
        }
        (None, Some(helper), None) => ProviderConnectionAuthentication::OAuth(helper),
        (None, None, Some(id)) => ProviderConnectionAuthentication::Existing(id),
        _ => unreachable!("authentication cardinality checked above"),
    };
    let service = ProviderConnectionService::new(
        state.catalog.clone(),
        state.credentials.clone(),
        state.secrets.clone(),
        discovery.clone(),
    );
    let connected = service
        .connect(ConnectProviderCommand {
            idempotency_key: body.idempotency_key,
            workspace_id,
            provider_id: body.provider_id,
            display_name: body.display_name,
            credential_name: body.credential_name,
            dialect: body.dialect,
            endpoint_name: body.endpoint_name,
            base_url: body.base_url,
            configuration: body.configuration,
            timeout_secs: body.timeout_secs,
            authentication,
        })
        .await
        .map_err(|error| provider_connection_problem(&error, &rid))?;
    Ok((
        StatusCode::CREATED,
        Json(ProviderConnectionView {
            provider: connected.provider,
            endpoint: connected.endpoint,
            credential: connected.credential.into(),
            sync: connected.sync,
        }),
    ))
}

fn unix_time_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn put_model_attributes(
    State(state): State<AdminState>,
    State(capabilities): State<ConfigCapabilitiesView>,
    Path(model_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<PutModelAttributesRequest>,
) -> Result<Json<ModelAttributes>, Problem> {
    if !capabilities.models.local_catalog_enabled {
        return Err(model_supply_managed(&req_id(&headers)));
    }
    // Model attributes publish independently of offerings — they carry no
    // provider/endpoint reference (`ProviderCatalog::validate` leaves them
    // unconstrained), so the only failure surface is a whole-catalog invariant (422).
    let attrs = ModelAttributes {
        context_window: request.context_window,
        max_output_tokens: request.max_output_tokens,
        provenance: Default::default(),
    }
    .stamped(ModelAttributeSource::Manual, unix_time_ms());
    state
        .catalog
        .put_model_attributes(model_id, attrs.clone())
        .await
        .map_err(|e| repo_problem(&e, &req_id(&headers)))?;
    Ok(Json(attrs))
}

/// Secret-free authoring input. Provenance is intentionally absent: external
/// callers cannot claim that a manual value came from a provider or curated source.
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PutModelAttributesRequest {
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

async fn get_catalog(
    State(state): State<AdminState>,
    State(capabilities): State<ConfigCapabilitiesView>,
    headers: HeaderMap,
) -> Result<Json<awaken_model_catalog::ProviderCatalog>, Problem> {
    let mut catalog = state
        .catalog
        .snapshot()
        .await
        .map_err(|e| repo_problem(&e, &req_id(&headers)))?;
    if !capabilities.models.cloud_models_enabled {
        for offering in &mut catalog.offerings {
            if offering.source == OfferingSource::Brokered {
                offering.status = OfferingStatus::Unavailable;
            }
        }
    }
    Ok(Json(catalog))
}

async fn put_pool(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut pool): Json<CredentialPool>,
) -> Result<Json<CredentialPool>, Problem> {
    // The path id is authoritative.
    pool.id = CredentialPoolId(id);
    if let Some(Extension(scope)) = scope {
        if state
            .credentials
            .get_pool(&pool.id)
            .await
            .is_ok_and(|current| current.workspace_id != scope.0)
        {
            return Err(cred_problem(
                &CredentialError::PoolNotFound(pool.id.0.clone()),
                &req_id(&headers),
            ));
        }
        pool.workspace_id = scope.0;
    }
    state
        .credentials
        .put_pool(pool.clone())
        .await
        .map_err(|e| cred_problem(&e, &req_id(&headers)))?;
    Ok(Json(pool))
}

async fn get_pool(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CredentialPool>, Problem> {
    let pool = state
        .credentials
        .get_pool(&CredentialPoolId(id))
        .await
        .map_err(|e| cred_problem(&e, &req_id(&headers)))?;
    if scope.is_some_and(|Extension(scope)| pool.workspace_id != scope.0) {
        return Err(cred_problem(
            &CredentialError::PoolNotFound(pool.id.0.clone()),
            &req_id(&headers),
        ));
    }
    Ok(Json(pool))
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
        ResolveError::ModelAmbiguous { .. } => (409, "model_ambiguous"),
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

/// A dry-run resolve request. An unqualified target is valid when the model id
/// identifies exactly one active offering; callers always use the same target shape.
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    workspace_id: String,
    target: ModelTarget,
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
    scope: Option<Extension<ResourceWorkspace>>,
    headers: HeaderMap,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<ResolvedInferenceView>, Problem> {
    let rid = req_id(&headers);
    let catalog = state
        .catalog
        .snapshot()
        .await
        .map_err(|e| repo_problem(&e, &rid))?;
    let workspace = scope.map_or_else(|| request.workspace_id.clone(), |Extension(scope)| scope.0);
    let lookup = workspace_lookup(&state, &workspace, &rid).await?;
    let resolved = resolve_inference_target(
        &catalog,
        &request.target,
        &[],
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
    let scoped_workspace = scope.map(|Extension(scope)| scope.0);
    let workspace = scoped_workspace.clone().unwrap_or(request.workspace_id);
    let profile = if scoped_workspace.is_some() {
        get_workspace_profile(state.profiles.as_ref(), &workspace, &id)
    } else {
        state.profiles.get(&id)
    }
    .map_err(|error| config_repository_problem(&error, &rid))?
    .ok_or_else(|| profile_missing(&id, &rid))?;
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

async fn credential_in_scope(
    state: &AdminState,
    id: &CredentialSourceId,
    scope: Option<&Extension<ResourceWorkspace>>,
    rid: &str,
) -> Result<CredentialSource, Problem> {
    let source = state
        .credentials
        .get(id)
        .await
        .map_err(|error| cred_problem(&error, rid))?;
    if scope.is_some_and(|Extension(scope)| source.workspace_id != scope.0) {
        return Err(cred_problem(
            &CredentialError::SourceNotFound(id.0.clone()),
            rid,
        ));
    }
    Ok(source)
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
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<CooldownRequest>,
) -> Result<Json<AvailabilityState>, Problem> {
    let source = CredentialSourceId(id);
    if scope.is_some() {
        credential_in_scope(&state, &source, scope.as_ref(), &req_id(&headers)).await?;
    }
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
    Ok(Json(state.availability.state(&source, now)))
}

/// The current availability of a credential source (cooldown auto-resumes by time).
async fn get_availability(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<AvailabilityState>, Problem> {
    let source = CredentialSourceId(id);
    if scope.is_some() {
        credential_in_scope(&state, &source, scope.as_ref(), &req_id(&headers)).await?;
    }
    Ok(Json(state.availability.state(&source, now_ms())))
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
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<PoolEligibleView>, Problem> {
    let rid = req_id(&headers);
    let pool = state
        .credentials
        .get_pool(&CredentialPoolId(id))
        .await
        .map_err(|e| cred_problem(&e, &rid))?;
    if scope.is_some_and(|Extension(scope)| pool.workspace_id != scope.0) {
        return Err(cred_problem(
            &CredentialError::PoolNotFound(pool.id.0.clone()),
            &rid,
        ));
    }
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
    State(capabilities): State<ConfigCapabilitiesView>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut profile): Json<InferenceProfile>,
) -> Result<Json<InferenceProfile>, Problem> {
    let rid = req_id(&headers);
    if !capabilities.models.profile_authoring_enabled {
        return Err(model_supply_managed(&rid));
    }
    let scoped_workspace = scope.map(|Extension(scope)| scope.0);
    if let Some(workspace) = &scoped_workspace {
        profile.workspace_id.clone_from(workspace);
    }
    validate_profile(&profile).map_err(|detail| {
        Problem(ApiError::new(
            422,
            "invalid_inference_profile",
            "Invalid inference profile",
            detail,
            &rid,
        ))
    })?;
    if let Some(workspace) = &scoped_workspace {
        put_workspace_profile(state.profiles.as_ref(), workspace, &id, profile.clone())
    } else {
        state.profiles.put(id, profile.clone())
    }
    .map_err(|error| config_repository_problem(&error, &rid))?;
    Ok(Json(profile))
}

fn validate_profile(profile: &InferenceProfile) -> Result<(), String> {
    const MAX_FALLBACKS: usize = 8;
    if profile.primary.target.model_id.trim().is_empty() {
        return Err("primary.model_id must not be empty".into());
    }
    if profile.fallbacks.len() > MAX_FALLBACKS {
        return Err(format!(
            "at most {MAX_FALLBACKS} fallback targets are allowed"
        ));
    }
    let key = |target: &ModelTarget| {
        (
            target.model_id.trim().to_owned(),
            target.provider_id.as_deref().unwrap_or_default().to_owned(),
            target
                .protocol_endpoint_id
                .as_deref()
                .unwrap_or_default()
                .to_owned(),
        )
    };
    let mut seen = HashSet::from([key(&profile.primary.target)]);
    for (index, candidate) in profile.fallbacks.iter().enumerate() {
        let target = &candidate.target;
        if target.model_id.trim().is_empty() {
            return Err(format!("fallbacks[{index}].model_id must not be empty"));
        }
        if !seen.insert(key(target)) {
            return Err(format!(
                "fallbacks[{index}] duplicates an earlier model target"
            ));
        }
    }
    Ok(())
}

async fn get_profile(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<InferenceProfile>, Problem> {
    let workspace = scope.map(|Extension(scope)| scope.0);
    let profile = if let Some(workspace) = &workspace {
        get_workspace_profile(state.profiles.as_ref(), workspace, &id)
    } else {
        state.profiles.get(&id)
    }
    .map_err(|error| config_repository_problem(&error, &req_id(&headers)))?
    .ok_or_else(|| profile_missing(&id, &req_id(&headers)))?;
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
    let scoped_workspace = scope.map(|Extension(scope)| scope.0);
    let workspace = scoped_workspace.clone().unwrap_or(request.workspace_id);
    let profile = if scoped_workspace.is_some() {
        get_workspace_profile(state.profiles.as_ref(), &workspace, &id)
    } else {
        state.profiles.get(&id)
    }
    .map_err(|error| config_repository_problem(&error, &rid))?
    .ok_or_else(|| profile_missing(&id, &rid))?;
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

/// Bind which resources an agent mounts (ADR-0038). The path agent id is
/// authoritative; the binding set is stored whole (upsert by agent id).
async fn put_agent_inputs(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    Json(mut config): Json<AgentInputConfig>,
) -> Result<Json<AgentInputConfig>, Problem> {
    config.agent_id = agent_id;
    let rid = req_id(&headers);
    validate_agent_input_mount_paths(&config)
        .map_err(|error| agent_input_mount_problem(&error, &rid))?;
    let workspace = resource_workspace(scope).map_err(|_| resource_workspace_problem(&rid))?;
    state
        .resources
        .put_agent_inputs(&workspace, config.clone())
        .map_err(|error| agent_input_write_problem(error, &rid))?;
    Ok(Json(config))
}

fn validate_agent_input_mount_paths(
    config: &AgentInputConfig,
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    config
        .inputs
        .iter()
        .filter(|input| {
            matches!(
                &input.target,
                awaken_config_resolver::InputResourceId::Repository(_)
            )
        })
        .try_for_each(|input| {
            awaken_provisioning_contract::validate_repository_mount_path(&input.mount_path)
        })
}

fn agent_input_mount_problem(
    error: &awaken_provisioning_contract::SandboxError,
    rid: &str,
) -> Problem {
    Problem(ApiError::new(
        422,
        "invalid_mount_path",
        "Invalid Agent input mount path",
        error.to_string(),
        rid,
    ))
}

async fn get_agent_inputs(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<AgentInputConfig>, Problem> {
    let rid = req_id(&headers);
    let workspace = resource_workspace(scope).map_err(|_| resource_workspace_problem(&rid))?;
    state
        .resources
        .get_agent_inputs(&workspace, &agent_id)
        .map_err(|error| agent_input_write_problem(error, &rid))?
        .map(Json)
        .ok_or_else(|| agent_inputs_missing(&agent_id, &rid))
}

#[derive(Debug, Clone, Copy)]
struct MissingResourceWorkspace;

fn resource_workspace(
    scope: Option<Extension<ResourceWorkspace>>,
) -> Result<String, MissingResourceWorkspace> {
    scope
        .as_ref()
        .and_then(|Extension(scope)| scope.non_empty())
        .map(str::to_owned)
        .ok_or(MissingResourceWorkspace)
}

fn resource_workspace_problem(rid: &str) -> Problem {
    Problem(ApiError::new(
        404,
        "not_found",
        "Workspace not found",
        "no trusted Workspace was resolved for the request",
        rid,
    ))
}

fn agent_inputs_missing(agent_id: &str, rid: &str) -> Problem {
    Problem(ApiError::new(
        404,
        "not_found",
        "Agent input config not found",
        format!("no input config for agent `{agent_id}`"),
        rid,
    ))
}

fn agent_input_write_problem(
    error: awaken_config_resolver::AgentInputRepositoryError,
    rid: &str,
) -> Problem {
    use awaken_config_resolver::AgentInputRepositoryError as Error;
    let (status, code, title) = match error {
        Error::InvalidRevision(_) => (422, "invalid_revision", "Invalid Agent input revision"),
        Error::RevisionConflict { .. } => {
            (409, "revision_conflict", "Agent input revision conflict")
        }
        Error::Storage(_) => (500, "storage_error", "Agent input storage failed"),
    };
    Problem(ApiError::new(status, code, title, error.to_string(), rid))
}

/// Resolve an agent's MCP binding within a workspace's credential scope.
/// Disable a credential (soft archive): a disabled source fails closed at
/// materialization, so a leaked/rotated key can be pulled without deleting the row.
async fn archive_credential(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ArchiveCredentialRequest>,
) -> Result<Json<CredentialSourceView>, Problem> {
    let rid = req_id(&headers);
    if body.expected_version < 1 {
        return Err(cred_problem(
            &CredentialError::InvalidSource("expected_version must be greater than zero".into()),
            &rid,
        ));
    }
    let source = credential_in_scope(&state, &CredentialSourceId(id), scope.as_ref(), &rid).await?;
    let source = awaken_credential_vault::repo::revoke_credential_exact(
        &source.id,
        body.expected_version,
        awaken_credential_vault::repo::CredentialRetirement::Disable,
        state.secrets.as_ref(),
        state.credentials.as_ref(),
    )
    .await
    .map_err(|e| cred_problem(&e, &rid))?;
    Ok(Json(source.into()))
}

/// Live-validate a credential against a model's resolved provider endpoint.
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ValidateCredentialRequest {
    workspace_id: String,
    #[serde(default)]
    model_id: Option<String>,
    /// Generic hosted-governance validation mode. The durable source id is the
    /// path identity, so this field is mutually exclusive with `model_id`.
    #[serde(default)]
    provider_ref: Option<String>,
}

/// The secret-free result of a live credential probe.
#[derive(serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CredentialValidation {
    pub status: ProbeStatus,
    pub adapter_kind: String,
    /// Current durable revision for an exact hosted reference validation.
    /// Model live-probe responses omit it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_version: Option<i64>,
}

/// Live-validate a credential: resolve it (Exact binding) to get the endpoint +
/// materialized secret, then probe the provider through the injected port. Reports
/// `unknown` when no probe is wired or the adapter is one the probe can't reach.
async fn validate_credential(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ValidateCredentialRequest>,
) -> Result<Json<CredentialValidation>, Problem> {
    let rid = req_id(&headers);
    let workspace = scope.as_ref().map_or_else(
        || request.workspace_id.clone(),
        |Extension(scope)| scope.0.clone(),
    );
    if let (None, Some(provider_ref)) = (request.model_id.as_ref(), request.provider_ref.as_deref())
    {
        if workspace.trim().is_empty() || provider_ref.trim().is_empty() {
            return Err(cred_problem(
                &CredentialError::InvalidSource(
                    "hosted credential validation requires a Workspace and provider_ref".into(),
                ),
                &rid,
            ));
        }
        let source_id = CredentialSourceId(id.clone());
        let source = credential_in_scope(&state, &source_id, scope.as_ref(), &rid).await?;
        if !is_hosted_credential(&source)
            || !hosted_credential_matches(&source, &workspace, provider_ref)
            || source.version < 1
        {
            return Err(cred_problem(&CredentialError::SourceNotFound(id), &rid));
        }
        return Ok(Json(CredentialValidation {
            status: ProbeStatus::Valid,
            adapter_kind: "credential_reference".into(),
            credential_version: Some(source.version),
        }));
    }
    let model_id = match (request.model_id, request.provider_ref) {
        (Some(model_id), None) if !model_id.trim().is_empty() => model_id,
        _ => {
            return Err(cred_problem(
                &CredentialError::InvalidSource(
                    "choose either model_id or the provider_ref validation mode".into(),
                ),
                &rid,
            ));
        }
    };
    let catalog = state
        .catalog
        .snapshot()
        .await
        .map_err(|e| repo_problem(&e, &rid))?;
    let lookup = workspace_lookup(&state, &workspace, &rid).await?;
    let resolved = resolve_inference(
        &catalog,
        &model_id,
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
                    &model_id,
                )
                .await
        }
        _ => ProbeStatus::Unknown,
    };
    Ok(Json(CredentialValidation {
        status,
        adapter_kind: resolved.adapter_kind.to_string(),
        credential_version: None,
    }))
}

async fn post_credential(
    State(state): State<AdminState>,
    State(capabilities): State<ConfigCapabilitiesView>,
    scope: Option<Extension<ResourceWorkspace>>,
    headers: HeaderMap,
    Json(body): Json<EnterCredentialRequest>,
) -> Result<(StatusCode, Json<CredentialSourceView>), Problem> {
    let rid = req_id(&headers);
    if body.idempotency_key.is_none()
        && !capabilities.models.byok_enabled
        && body.descriptor.is_none()
        && body.provider_id.is_some()
    {
        return Err(model_supply_managed(&rid));
    }
    let secret = credential_material(body.secret, body.material)
        .map_err(|error| cred_problem(&error, &rid))?;
    let oauth_command = match (body.kind, body.oauth_helper) {
        (CredentialKind::Oauth, Some(helper)) => Some(helper.command()),
        (CredentialKind::Oauth, None) => {
            return Err(cred_problem(
                &CredentialError::OAuth("oauth credentials require oauth_helper".into()),
                &rid,
            ));
        }
        (_, Some(_)) => {
            return Err(cred_problem(
                &CredentialError::OAuth("oauth_helper is only valid for oauth credentials".into()),
                &rid,
            ));
        }
        (_, None) => None,
    };
    let workspace_id = scope.map_or(body.workspace_id, |Extension(scope)| scope.0);
    let canonical_provider = body
        .descriptor
        .as_ref()
        .map(|descriptor| descriptor.provider.0.as_str())
        .or(body.provider_id.as_deref());
    let idempotent_id = match (body.idempotency_key.as_deref(), canonical_provider) {
        (Some(key), Some(provider_ref))
            if body.kind == CredentialKind::Vault
                && secret.is_some()
                && oauth_command.is_none() =>
        {
            validate_hosted_credential_identity(&workspace_id, provider_ref, key)
                .map_err(|error| cred_problem(&error, &rid))?;
            Some(hosted_credential_source_id(
                &workspace_id,
                provider_ref,
                key,
            ))
        }
        (Some(_), _) => {
            return Err(cred_problem(
                &CredentialError::InvalidSource(
                    "idempotent hosted credentials require a canonical provider, Vault kind, and material"
                        .into(),
                ),
                &rid,
            ));
        }
        (None, _) => None,
    };
    let params = CredentialCreateParams {
        workspace_id,
        kind: body.kind,
        provider_id: body.provider_id,
        env_key: body.env_key,
        secret,
        oauth_command,
    };
    let source = match (idempotent_id, body.descriptor, body.replacement_of) {
        (Some(id), Some(descriptor), Some(replacement_of)) => {
            enter_credential_replacement_idempotent_described(
                &replacement_of,
                id,
                params,
                None,
                descriptor,
                state.secrets.as_ref(),
                state.credentials.as_ref(),
            )
            .await
            .map(|entry| entry.source)
        }
        (Some(id), Some(descriptor), None) => enter_credential_idempotent_described(
            id,
            params,
            None,
            descriptor,
            state.secrets.as_ref(),
            state.credentials.as_ref(),
        )
        .await
        .map(|entry| entry.source),
        (Some(id), None, None) => enter_credential_idempotent_verified(
            id,
            params,
            None,
            state.secrets.as_ref(),
            state.credentials.as_ref(),
        )
        .await
        .map(|entry| entry.source),
        (None, Some(descriptor), None) => {
            enter_credential_described(params, descriptor, &*state.secrets, &*state.credentials)
                .await
        }
        (None, None, None) => enter_credential(params, &*state.secrets, &*state.credentials).await,
        (_, None, Some(_)) | (None, Some(_), Some(_)) => Err(CredentialError::InvalidSource(
            "replacement_of requires a described idempotent hosted credential request".into(),
        )),
    }
    .map_err(|e| cred_problem(&e, &rid))?;
    Ok((StatusCode::CREATED, Json(source.into())))
}

async fn rotate_credential(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<RotateCredentialRequest>,
) -> Result<Json<CredentialSourceView>, Problem> {
    let rid = req_id(&headers);
    if body.expected_version < 1 {
        return Err(cred_problem(
            &CredentialError::InvalidSource("expected_version must be greater than zero".into()),
            &rid,
        ));
    }
    let replacement = credential_material(body.secret, body.material)
        .map_err(|error| cred_problem(&error, &rid))?;
    if replacement.is_none() && body.descriptor.is_none() {
        return Err(cred_problem(
            &CredentialError::InvalidSource("rotation material or descriptor is required".into()),
            &rid,
        ));
    }
    let id = CredentialSourceId(id);
    credential_in_scope(&state, &id, scope.as_ref(), &rid).await?;
    let source = rotate_credential_materials_exact(
        &id,
        body.expected_version,
        CredentialMaterialPatch {
            primary: replacement,
            auxiliary: Default::default(),
            descriptor: body.descriptor,
        },
        state.secrets.as_ref(),
        state.credentials.as_ref(),
    )
    .await
    .map_err(|error| cred_problem(&error, &rid))?;
    Ok(Json(source.into()))
}

async fn get_credential(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CredentialSourceView>, Problem> {
    credential_in_scope(
        &state,
        &CredentialSourceId(id),
        scope.as_ref(),
        &req_id(&headers),
    )
    .await
    .map(|source| Json(source.into()))
}

#[derive(serde::Deserialize)]
struct ListCredentialsQuery {
    workspace_id: String,
    #[serde(default)]
    provider_ref: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    hosted_only: bool,
}

async fn list_credentials(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Query(query): Query<ListCredentialsQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<CredentialSourceView>>, Problem> {
    let workspace = scope.map_or(query.workspace_id, |Extension(scope)| scope.0);
    if query.provider_ref.is_some() != query.idempotency_key.is_some() {
        return Err(cred_problem(
            &CredentialError::InvalidSource(
                "provider_ref and idempotency_key must be supplied together for operation lookup"
                    .into(),
            ),
            &req_id(&headers),
        ));
    }
    if let (Some(provider_ref), Some(idempotency_key)) = (
        query.provider_ref.as_deref(),
        query.idempotency_key.as_deref(),
    ) {
        validate_hosted_credential_identity(&workspace, provider_ref, idempotency_key)
            .map_err(|error| cred_problem(&error, &req_id(&headers)))?;
        let id = hosted_credential_source_id(&workspace, provider_ref, idempotency_key);
        return match state.credentials.get(&id).await {
            Ok(source) if hosted_credential_matches(&source, &workspace, provider_ref) => {
                Ok(Json(vec![source.into()]))
            }
            Ok(_) => Err(cred_problem(
                &CredentialError::MutationConflict(
                    "hosted credential operation identity conflicts with durable metadata".into(),
                ),
                &req_id(&headers),
            )),
            Err(CredentialError::SourceNotFound(_)) => Ok(Json(Vec::new())),
            Err(error) => Err(cred_problem(&error, &req_id(&headers))),
        };
    }
    let sources = state
        .credentials
        .list(&workspace)
        .await
        .map_err(|e| cred_problem(&e, &req_id(&headers)))?;
    Ok(Json(
        sources
            .into_iter()
            .filter(|source| !query.hosted_only || is_hosted_credential(source))
            .map(Into::into)
            .collect(),
    ))
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
            CredentialError::EnvironmentSourceUnsupported("s".into()),
            CredentialError::WorkerLocalSourceUnsupported("s".into()),
            CredentialError::InvalidSource("bad source".into()),
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
    fn resolve_problem_model_ambiguous_is_409() {
        let p = resolve_problem(
            &ResolveError::ModelAmbiguous {
                model_id: "m".into(),
                candidates: vec!["p/ep1".into(), "p/ep2".into()],
            },
            "rid",
        );
        assert_eq!(p.0.status, 409);
        assert_eq!(p.0.code, "model_ambiguous");
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
