// @formal entrypoint audit.business_action_follows_audit_record
// @formal entrypoint audit.call_identity_is_stable
// @formal entrypoint management_http.crash_preserves_pending_audit
// @formal entrypoint management_http.no_business_write_without_durable_audit
// @formal entrypoint management_http.successful_write_marks_audit_committed
//! `awaken-control` — the authoring / authz plane (Stage B2).
//!
//! This crate owns the management **authoring** surfaces and the embedded IAM
//! guard: the admin config CRUD (catalog / credentials / profiles / MCP defs),
//! Provider model discovery, catalog-and-credential model publication resolution, the
//! webhook-subscription CRUD, the Managed vault front door, user profiles,
//! the config authoring plane (`/v1/config/agents/*`),
//! and the capability snapshot — all optionally gated behind the embedded
//! `ApiToken` guard (ADR-0042/0043 P1). [`control_router`] weaves them into one
//! guarded management router.
//!
//! It is a sibling of the `awaken-coordinator` **data plane** (session surface +
//! protocol adapters + host): the two do NOT depend on each other. The single
//! machine composition root (`awaken-cli`) builds role-scoped handles (stores,
//! vault/env state, the config plane), asks this crate for the authoring router,
//! asks `awaken-coordinator` for the data-plane router, merges them, and applies the
//! guard exactly where it applied before.

pub mod admin_assistant;
mod application_credentials;
mod application_mcp_credentials;
pub use application_credentials::application_credentials_router;
mod mcp_catalog;
mod mcp_probe;
pub use mcp_catalog::McpCatalogService;
#[cfg(feature = "schema")]
pub use mcp_catalog::{
    contract_paths as mcp_contract_paths, contract_schemas as mcp_contract_schemas,
};
pub use mcp_probe::ExtMcpProbe;
pub mod authz;
mod component;
pub mod control_stores;
mod credential_refresh;
pub use credential_refresh::{VaultRefreshFactory, VaultRefresher};
mod agent_revision_visibility;
mod credential_reference;
mod data_subject;
mod managed_agents;
pub mod model_discovery;
pub mod model_publication;
mod registration_supervisor;
mod telemetry_policy;
pub use telemetry_policy::{RepositoryTelemetryNarrowing, TelemetryPolicyService};
pub mod webhook_authoring;
pub mod worker_stores;

#[cfg(test)]
mod application_mcp_credentials_tests;
#[cfg(test)]
mod audit_tests;
#[cfg(test)]
mod managed_org_admission_tests;

use std::sync::Arc;

pub use crate::admin_assistant::{
    CatalogCapabilityReader, ConfigServiceDraftStore, ConfigServiceDraftValidator,
    HostResourceInventory, admin_assistant_lifecycle_router, seed_admin_assistant,
};
pub use crate::application_mcp_credentials::application_mcp_credentials_router;
// Embedded management-plane IAM (ADR-0042/0043 P1): the authorizer, its boot
// fn, the mint spec (tests / operator embeddings), and the bootstrap constants.
pub use crate::authz::{
    ADMIN_TOKEN_FILE, AWAKEN_WORKSPACE_HOSTED_ADMIN_ROLE, AWAKEN_WORKSPACE_HOSTED_BUILDER_ROLE,
    AWAKEN_WORKSPACE_POLICY_NAMESPACE, AWAKEN_WORKSPACE_PUBLISHER_ROLE,
    AWAKEN_WORKSPACE_TUNNEL_MANAGER_ROLE, AWAKEN_WORKSPACE_USER_ROLE, BOOTSTRAP_PRINCIPAL,
    BOOTSTRAP_WORKSPACE, DEFAULT_ORG_ID, HOSTED_RUNTIME_AGENT_EXECUTOR_ROLE,
    HOSTED_RUNTIME_POLICY_NAMESPACE, HOSTED_RUNTIME_WORKSPACE_ADMIN_ROLE,
    HOSTED_RUNTIME_WORKSPACE_USER_ROLE, HostedRuntimePathMatch, HostedRuntimeRouteProfile,
    ManagementAuthz, ManagementIdentityMode, RemoteManagementAuthz, TokenSpec, embedded_iam,
    embedded_iam_for_tenant, embedded_iam_for_tenant_with_entitlements, embedded_iam_for_workspace,
    hosted_runtime_authorization_profile, hosted_runtime_route_profile,
    workspace_authorization_profile,
};
pub use crate::component::{ControlComponent, ControlDependencies, build_control_component};
pub use crate::control_stores::{ControlStoreConfig, StoreBackend};
pub use crate::credential_reference::CredentialRevisionValidator;
pub use crate::data_subject::{consent_router, erasure_router};
pub use crate::managed_agents::ConfigPlaneManagedAgentRepository;
pub use crate::registration_supervisor::{
    RegistrationHealth, RegistrationHealthSnapshot, RegistrationSupervisorConfig,
    StaticRegistrationSupervisor,
};
mod config_routes;
mod managed_agent;
pub use awaken_config_service::{
    LocalRuntimeCapability, RuntimeCapability, RuntimeCapabilitySource, capabilities_router,
    capabilities_router_with_source, sandbox_execution_policy_capability,
    static_runtime_capabilities,
};
pub use awaken_iam_contract::AccountId;
pub use awaken_iam_host::{LocalBrowserAuth, LocalSetupHandoff};
pub use config_routes::config_router;
pub use managed_agent::{agent_config_from_managed, managed_from_agent_config};
// Trusted local materialization: only the credential vault and secret store
// needed by snapshot-pinned inference access.
pub use crate::worker_stores::{
    InferenceMaterializationStores, open_inference_materialization_stores,
};

use awaken_admin_config_api::{AdminState, CredentialProbe};
use awaken_agent_config::{AuditedConfigWrite, DEFAULT_SCOPE, ManagementAuditRecord};
use awaken_config_resolver::AgentInputBindingRepository;
use awaken_config_resolver::{InferenceProfileStore, WebhookStore};
use awaken_config_service::{ConfigPlane, ManagementAuditPlane};
use awaken_credential_vault::SecretStore;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_deployment_contract::AgentArchiveCascade;
use awaken_executable_agent_contract::capability::PluginCapability;
use awaken_model_catalog::repo::CatalogRepo;
use awaken_protocol_managed::{
    ManagedAgentRepository, VaultState, user_profiles_router, vault_router,
};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;
use axum::body::{Body, to_bytes};
use axum::extract::Extension;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use sha2::{Digest, Sha256};

const MAX_AUDITED_BODY: usize = 2 * 1024 * 1024;

fn human_coordinate(value: &str, fallback: &str) -> String {
    let value = value
        .strip_prefix("awaken:personal:")
        .or_else(|| value.strip_prefix("workspace_"))
        .or_else(|| value.strip_prefix("wrkspc_"))
        .or_else(|| value.strip_prefix("org_"))
        .or_else(|| value.strip_prefix("acct_"))
        .unwrap_or(value);
    let looks_opaque = value.len() > 32
        || (value.len() >= 20 && value.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    if value.is_empty() || looks_opaque {
        return fallback.to_owned();
    }
    let words = value
        .split(['-', '_', ':'])
        .filter(|part| !part.is_empty() && *part != "default" && *part != "local")
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>();
    if words.is_empty() {
        fallback.to_owned()
    } else {
        words.join(" ")
    }
}

fn workspace_display_name(id: &str) -> String {
    if id.starts_with("workspace_local_") || id.starts_with("wrkspc_local_") {
        return "Local Workspace".into();
    }
    if id == DEFAULT_SCOPE || id == "default" || id.contains(":personal:") {
        return if id.contains(":personal:") {
            "Personal Workspace"
        } else {
            "Default Workspace"
        }
        .into();
    }
    let name = human_coordinate(id, "Workspace");
    if name.to_ascii_lowercase().contains("workspace") {
        name
    } else {
        format!("{name} Workspace")
    }
}

fn organization_display_name(id: &str) -> String {
    if id == "org_default" || id == "default" || id == DEFAULT_SCOPE {
        return "Default Organization".into();
    }
    let name = human_coordinate(id, "Organization");
    if name.to_ascii_lowercase().contains("organization") {
        name
    } else {
        format!("{name} Organization")
    }
}

fn principal_display_name(
    principal: Option<&awaken_iam_contract::PrincipalRef>,
    identity_mode: &str,
) -> String {
    match principal {
        Some(awaken_iam_contract::PrincipalRef::Account { account_id })
            if account_id.0 == "local-console-admin" =>
        {
            "Local administrator".into()
        }
        Some(awaken_iam_contract::PrincipalRef::Account { account_id }) => {
            human_coordinate(&account_id.0, "Account user")
        }
        Some(awaken_iam_contract::PrincipalRef::Service { service_id }) => {
            human_coordinate(service_id, "Service account")
        }
        Some(awaken_iam_contract::PrincipalRef::ApiToken { .. }) => "API client".into(),
        None if identity_mode == "no-login" => "Local operator".into(),
        None if identity_mode == "awaken-cloud" => "Awaken Cloud user".into(),
        None => "Workspace user".into(),
    }
}

async fn durable_management_audit(
    axum::extract::State(plane): axum::extract::State<ManagementAuditPlane>,
    request: Request<Body>,
    next: axum::middleware::Next,
) -> Response {
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, MAX_AUDITED_BODY).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("audit body: {error}"),
            )
                .into_response();
        }
    };
    let call_id = parts
        .headers
        // `Idempotency-Key` belongs to the domain command behind this edge. In
        // particular, Managed Session replacement owns replay and response
        // semantics in its root aggregate. Reinterpreting that key as an audit
        // call identity creates a second dedupe authority and rejects a valid
        // domain replay before its handler can project the committed response.
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        // An audit call identity needs uniqueness, not a time: draw it from a
        // random source rather than composing a clock reading with a counter.
        .unwrap_or_else(|| format!("generated-{}", uuid::Uuid::new_v4().simple()));
    // The audit record is filed AGAINST a tenant, so an unstamped request has
    // no record to file. Filing it under a compiled-in Workspace would attach a
    // real principal's write to a tenant that never made it, and would let the
    // durable idempotency key of one tenant fence another's retry. Refuse
    // instead; the composition root stamps every anonymous single-machine
    // request before this layer runs.
    let Some(scope) = parts
        .extensions
        .get::<awaken_tenancy::WorkspaceScope>()
        .and_then(awaken_tenancy::WorkspaceScope::non_empty)
        .map(ScopeId::from)
    else {
        return (
            StatusCode::NOT_FOUND,
            "no Workspace was resolved for this management request",
        )
            .into_response();
    };
    // Write-only application material must leave no plaintext-derived digest
    // in the audit store (weak passwords are vulnerable to offline guessing).
    // The audit call, tenant, route and completion remain durable.
    let body_hash = if parts.uri.path() == "/v1/config/application-credentials/enter" {
        "redacted-application-material".to_string()
    } else {
        format!("{:x}", Sha256::digest(&bytes))
    };
    let audit = ManagementAuditRecord::new(
        format!("http:{}:{}", parts.method, parts.uri.path()),
        call_id,
        format!("body_sha256={body_hash}"),
    );
    let admission = match plane.record(&scope, &audit).await {
        Ok(admission) if admission.permits_business_write() => admission,
        Ok(AuditedConfigWrite::Replayed) => {
            // The prior attempt may have committed business state and crashed
            // before recording completion. Never execute an ambiguous retry.
            return (
                StatusCode::CONFLICT,
                "management request id was already admitted; inspect its durable audit record",
            )
                .into_response();
        }
        Ok(AuditedConfigWrite::Conflict { current_revision }) => {
            return (
                StatusCode::CONFLICT,
                format!(
                    "management audit changed concurrently (current revision: {current_revision:?})"
                ),
            )
                .into_response();
        }
        Ok(AuditedConfigWrite::Applied) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "durable management audit returned an inconsistent admission",
            )
                .into_response();
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("durable management audit failed: {error}"),
            )
                .into_response();
        }
    };
    let response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    let (tool, call_id) = audit.identity();
    if admission.requires_audit_completion(response.status().is_success())
        && let Err(error) = plane.mark_committed(&scope, tool, call_id).await
    {
        // The business write already succeeded. Keep the durable pending audit
        // visible for reconciliation instead of masking the real response.
        eprintln!("management audit completion failed: {error}");
    }
    response
}

/// The stores + shared handles [`control_router`] needs to build the authoring
/// plane. The composition root (`awaken-cli`) builds these, hands clones here for
/// the authoring routers, and keeps its own for the data-plane host — one instance
/// of each port is shared across both planes, exactly as before the split.
pub struct ControlRouterInput {
    /// The model catalog (providers / endpoints / offerings) the admin CRUD authors.
    pub catalog: Arc<dyn CatalogRepo>,
    /// The credential repo (secret-free source/pool rows) the admin + vault surfaces read.
    pub credentials: Arc<dyn CredentialRepo>,
    /// The sealed-secret store the admin + vault surfaces + webhook sealing use.
    pub secrets: Arc<dyn SecretStore>,
    /// Workspace MCP configuration and inspection over the same Vault owner.
    pub mcp_catalog: mcp_catalog::McpCatalogService,
    /// Workspace narrowing of the deployment content-capture ceiling.
    pub telemetry_policy: TelemetryPolicyService,
    /// Authored inference profiles (admin aggregate).
    pub profiles: Arc<dyn InferenceProfileStore>,
    /// Authored webhook endpoints (admin aggregate).
    pub webhook_store: Arc<dyn WebhookStore>,
    /// Per-agent resource bindings, shared with the config service (ADR-0038).
    pub resource_store: Arc<dyn AgentInputBindingRepository>,
    /// The live credential probe (provider-backed), injected by the composition root.
    pub probe: Arc<dyn CredentialProbe>,
    /// Provisioning-side provider model discovery, injected by composition.
    pub model_discovery: Arc<dyn awaken_admin_config_api::ModelCatalogDiscovery>,
    /// Signed-in managed model projection, absent outside Awaken Cloud mode.
    pub brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    /// One deployment posture for every model-supply command and projection.
    /// The admin router uses the same value for server enforcement and client
    /// capability discovery; composition must not maintain a hidden second mode.
    pub model_supply: awaken_admin_config_api::ModelSupplyCapabilityView,
    /// Whether this browser-serving origin reaches the canonical Managed
    /// runtime/resource surfaces. The composition root derives this from a
    /// local mount or an explicit hosted same-origin route.
    pub managed_runtime_available: bool,
    /// The Managed vault state, shared with the data-plane managed state.
    pub vault_state: Arc<VaultState>,
    pub credential_execution_authority:
        Option<Arc<dyn awaken_credential_contract::CredentialExecutionAuthority>>,
    /// The one authoring projection used by `/v1/agents`. Execution-owned
    /// consumers receive the same port from composition instead of reconstructing
    /// a second repository adapter.
    pub agent_repository: Arc<dyn ManagedAgentRepository>,
    /// Optional local lifecycle command edge used after Agent archive. Split
    /// Control leaves this absent; the composition root owns its implementation.
    pub agent_archive_cascade: Option<Arc<dyn AgentArchiveCascade>>,
    /// The config authoring plane (scope edge over the config service).
    pub plane: ConfigPlane,
    /// Model selection used to publish or recover the reserved Console Assistant.
    pub assistant_model_selection: Option<awaken_agent_config::ModelSelection>,
    /// The host's global tool descriptors, for `GET /v1/capabilities`.
    pub global_tools: Vec<ToolDescriptor>,
    /// The exact installable plugin catalog composed by the runtime host.
    pub plugins: Vec<PluginCapability>,
    /// Runtime capabilities projected by the composition root from the one
    /// executable catalog.
    pub runtimes: Arc<dyn RuntimeCapabilitySource>,
    /// Process-configured workspace used only when no authenticated scope is
    /// stamped (the explicit no-login deployment mode).
    pub platform_workspace: String,
    /// One neutral Data Subject application shared by profile/consent/erasure adapters.
    pub data_subject_application: Arc<awaken_data_subject_application::DataSubjectApplication>,
    /// Controller partition used for every subject access on this Control surface.
    pub data_subject_org: String,
    /// Control-owned Environment definition and sandbox-policy routes.
    pub environment_router: Router,
    /// Cloud-only Tunnel command/query port. Kept optional so open/self-hosted
    /// compositions expose no local Tunnel behavior.
    pub managed_tunnel_application:
        Option<Arc<dyn awaken_protocol_managed::ManagedTunnelApplication>>,
    pub inference_geo_policy: Option<Arc<dyn awaken_protocol_managed::ManagedInferenceGeoPolicy>>,
    /// Optional admission port installed inside IAM and audit.
    pub managed_request_limiter:
        Option<Arc<dyn awaken_tenancy::org_admission::OrgRequestAdmission>>,
    /// The embedded IAM guard, when enabled by typed deployment identity mode.
    pub iam: Option<Arc<ManagementAuthz>>,
    /// Canonical local setup/session routes, mounted outside the protected
    /// surface so an unauthenticated browser can exchange its one-time token.
    pub local_browser_auth: Option<awaken_iam_host::LocalBrowserAuth>,
    /// Awaken Cloud identity adapter. Mutually exclusive with `iam`.
    pub remote_iam: Option<Arc<RemoteManagementAuthz>>,
    /// Optional application coordinator for IAM's canonical desktop OAuth
    /// operation. Present only in interactive local Cloud composition.
    pub cloud_login: Option<Arc<dyn awaken_admin_config_api::CloudLoginApplication>>,
    /// The process's one wall clock, read by the admin surfaces that stamp or
    /// judge time.
    pub clock: Arc<dyn awaken_time::WallClock>,
}

struct LiveControlCapabilities {
    identity_mode: String,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    cloud_login_enabled: bool,
    local_iam: bool,
    model_supply: awaken_admin_config_api::ModelSupplyCapabilityView,
    managed_runtime_available: bool,
}

impl awaken_admin_config_api::ConfigCapabilitiesSource for LiveControlCapabilities {
    fn current(&self) -> awaken_admin_config_api::ConfigCapabilitiesView {
        awaken_admin_config_api::ConfigCapabilitiesView {
            identity: awaken_admin_config_api::IdentityCapabilityView {
                mode: self.identity_mode.clone(),
                cloud_login_enabled: self.cloud_login_enabled,
                authenticated: self.remote_iam.as_ref().is_some_and(|authz| {
                    authz.cloud_user_token().is_ok_and(|token| token.is_some())
                }),
            },
            models: self.model_supply.clone(),
            surfaces: awaken_admin_config_api::ProductSurfaceCapabilityView {
                managed_runtime: self.managed_runtime_available,
                access_management: self.local_iam,
            },
        }
    }
}

fn application_credential_routes(
    vault: Arc<VaultState>,
    authority: Option<Arc<dyn awaken_credential_contract::CredentialExecutionAuthority>>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Router {
    let external = std::env::var_os("AWAKEN_APPLICATION_CREDENTIAL_CONSUMERS_FILE")
        .map(|path| application_credentials_router(vault.clone(), path.into(), authority, clock))
        .unwrap_or_default();
    application_mcp_credentials_router(vault).merge(external)
}

/// Build the authoring / authz management router over the shared handles. The
/// lifecycle sink is assembled separately by its Managed Execution consumer, so
/// split Control never needs a Session repository merely to expose webhook CRUD.
/// The guard (when present) wraps only the management surfaces (ADR-0043).
pub fn control_router(input: ControlRouterInput) -> Router {
    let ControlRouterInput {
        catalog,
        credentials,
        secrets,
        profiles,
        mcp_catalog,
        telemetry_policy,
        webhook_store,
        resource_store,
        probe,
        model_discovery,
        brokered_catalog,
        model_supply,
        managed_runtime_available,
        vault_state,
        credential_execution_authority,
        agent_repository,
        agent_archive_cascade,
        plane,
        assistant_model_selection,
        global_tools,
        plugins,
        runtimes,
        platform_workspace,
        data_subject_application,
        data_subject_org,
        environment_router,
        managed_tunnel_application,
        inference_geo_policy,
        managed_request_limiter,
        iam,
        local_browser_auth,
        remote_iam,
        cloud_login,
        clock,
    } = input;

    let identity_mode = if remote_iam.is_some() {
        "awaken-cloud"
    } else if iam.is_some() {
        "self-managed"
    } else {
        "no-login"
    };
    let cloud_login_enabled = cloud_login.is_some();
    let capabilities: Arc<dyn awaken_admin_config_api::ConfigCapabilitiesSource> =
        Arc::new(LiveControlCapabilities {
            identity_mode: identity_mode.into(),
            remote_iam: remote_iam.clone(),
            cloud_login_enabled,
            local_iam: iam.is_some(),
            model_supply,
            managed_runtime_available,
        });
    let admin = awaken_admin_config_api::admin_router_with_runtime_capabilities(
        AdminState {
            catalog,
            credentials: credentials.clone(),
            secrets: secrets.clone(),
            profiles,
            // Per-agent resource bindings (ADR-0038).
            resources: resource_store.clone(),
            // The live credential probe is backed by provider-genai in the composition
            // root — the only place the model SDK is named; the admin CRUD crate stays
            // SDK-free.
            probe: Some(probe),
            model_discovery: Some(model_discovery),
            brokered_catalog,
            // Shared credential-availability cooldowns (E3-4).
            availability: Default::default(),
            clock: clock.clone(),
        },
        capabilities,
        cloud_login,
    );
    // The webhook plane (ADR-0048): subscriptions are an id-addressed config resource
    // in the same admin store, their `whsec_` secret sealed in the shared vault. Merge
    // the front door into the admin router BEFORE the ownership fence so
    // `/v1/config/webhook-subscriptions/{id}` is tenant-fenced like profiles/MCP; the
    // sink (fed the same store + secrets) fans committed session facts out-of-band.
    let webhook_crud = webhook_authoring::webhook_config_router(webhook_store, secrets.clone());
    let admin = admin.merge(webhook_crud);
    let application_credentials =
        application_credential_routes(vault_state.clone(), credential_execution_authority, clock);
    let presentation_org = data_subject_org.clone();
    let user_profiles = user_profiles_router(data_subject_application, data_subject_org);
    // The config authoring plane (`/v1/config/agents/*`): the console authors the
    // rich `AgentConfig` here and `publish` compiles + installs it so sessions run it.
    let audit_plane = plane.management_audit_plane();
    let assistant_lifecycle =
        crate::admin_assistant_lifecycle_router(plane.clone(), None, assistant_model_selection);
    let config_plane = config_router(plane);
    // `/v1/agents` projects the config plane it hosts: an agent published via
    // `/v1/config/agents` is retrievable as a managed-wire projection of that single
    // truth (no second store), which is how the console probes the assistant.
    let agents = config_routes::agent_registry_routers(
        agent_repository,
        inference_geo_policy,
        agent_archive_cascade,
    );
    // Capability snapshot (`GET /v1/capabilities`): the host's tool descriptors +
    // installable plugins (with config schema) so the console authors data-driven.
    let capabilities = crate::capabilities_router_with_source(global_tools, plugins, runtimes);
    let presentation_identity_mode = identity_mode;
    // The presentation route reads the Workspace the edge resolved. The
    // installation's own Workspace is filled in by ONE layer below, never by
    // the handler, so this projection can never invent a tenant name.
    let workspace_context = workspace_context_router(presentation_org, presentation_identity_mode);

    // The IAM guard (when enabled) wraps the admin + vault routers only. An
    // axum layer binds to the routes present when it is applied, so merging
    // the guarded sub-router later leaves every other surface untouched. The
    // token-management routes exist ONLY under the guard (they authorize
    // against the same embedded IAM the guard authenticates with), and they
    // are merged before the layer so the guard authenticates them first.
    let mut mgmt = admin
        .merge(application_credentials)
        .merge(mcp_catalog::router(mcp_catalog))
        .merge(telemetry_policy::router(telemetry_policy))
        .merge(vault_router(vault_state))
        .merge(user_profiles)
        .merge(agents)
        .merge(config_plane)
        .merge(assistant_lifecycle)
        .merge(workspace_context)
        .merge(capabilities)
        .merge(environment_router);
    if let Some(application) = managed_tunnel_application {
        mgmt = mgmt.merge(awaken_protocol_managed::tunnels_router(application));
    }
    if let Some(iam) = iam.as_ref() {
        mgmt = mgmt.merge(crate::authz::token_router(iam.clone()));
    }
    let no_login = iam.is_none() && remote_iam.is_none();
    mgmt = awaken_protocol_managed::with_managed_response_context(protect_management_router(
        mgmt,
        audit_plane,
        iam,
        remote_iam,
        managed_request_limiter,
    ));
    if no_login {
        mgmt = crate::authz::with_composed_workspace(mgmt, Some(platform_workspace));
    }
    if let Some(local_browser_auth) = local_browser_auth {
        mgmt = mgmt.merge(awaken_iam_host::local_browser_router(local_browser_auth));
    }
    mgmt
}

/// The Managed protocol's Org admission: its route classification and its
/// Anthropic encoding of every outcome, over the access edge's bucket decision.
pub const MANAGED_ORG_ADMISSION: awaken_authz_enforce::OrgAdmissionEncoding =
    awaken_authz_enforce::OrgAdmissionEncoding {
        classify: awaken_protocol_managed::managed_org_admission_class,
        scope_missing: awaken_protocol_managed::managed_admission_scope_missing,
        unavailable: awaken_protocol_managed::managed_admission_unavailable,
        rejected: awaken_protocol_managed::managed_admission_rejected,
        annotate: Some(awaken_protocol_managed::annotate_managed_rate_limit),
    };

/// Apply the canonical management audit and IAM edge to a domain router.
///
/// Deployment is execution-owned, but its public management API must retain the
/// same authorization and audit semantics as the authoring surfaces. Keeping the
/// edge here prevents a second interpretation of management identity in the CLI.
pub fn protect_management_router(
    mut router: Router,
    audit_plane: ManagementAuditPlane,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    request_limiter: Option<Arc<dyn awaken_tenancy::org_admission::OrgRequestAdmission>>,
) -> Router {
    if let Some(request_limiter) = request_limiter {
        // Axum layers wrap in reverse installation order. Install admission
        // first so IAM and then audit are outside it; only an authenticated
        // WorkspaceScope can reach the organization resolver.
        router = router.layer(axum::middleware::from_fn_with_state(
            awaken_authz_enforce::OrgAdmissionEdge::new(request_limiter, MANAGED_ORG_ADMISSION),
            awaken_authz_enforce::admit_by_org_bucket,
        ));
    }
    // Layer order is outside-in in reverse application order: audit is
    // installed first, then an optional IAM edge wraps it. Thus unauthenticated
    // requests never create audit intents, while admitted requests carry the
    // authenticated WorkspaceScope into durable audit and ownership guards.
    router = router.layer(axum::middleware::from_fn_with_state(
        audit_plane,
        durable_management_audit,
    ));
    protect_authorized_router(router, iam, remote_iam)
}

/// Apply the one IAM authentication/authorization edge without adding
/// management audit or organization admission. Runtime protocol routes use
/// this projection; authoring and management routes use
/// [`protect_management_router`], which delegates to the same edge after its
/// additional policies are installed.
pub fn protect_authorized_router(
    mut router: Router,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
) -> Router {
    if let Some(iam) = iam {
        router = router.layer(axum::middleware::from_fn_with_state(
            iam,
            crate::authz::management_guard,
        ));
    } else if let Some(remote_iam) = remote_iam {
        router = router.layer(axum::middleware::from_fn_with_state(
            remote_iam,
            crate::authz::cloud_management_guard,
        ));
    }
    router
}

/// Compose the Coordinator's two authenticated public protocol domains.
///
/// Service/runtime routes enter the deployment IAM edge. AI SDK and AG-UI are
/// supplied separately because Coordinator has already installed the canonical
/// application-token guard on them. Merging the application router only after
/// layering prevents the standard `Authorization` header from being consumed
/// once as a Cloud/service JWT and again as an application capability.
pub fn protect_runtime_protocol_routers(
    service_router: Router,
    application_router: Router,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
) -> Router {
    protect_authorized_router(service_router, iam, remote_iam).merge(application_router)
}

#[cfg(test)]
mod presentation_tests {
    use super::{organization_display_name, workspace_display_name};

    #[test]
    fn installation_coordinates_never_become_product_chrome_copy() {
        assert_eq!(
            workspace_display_name("workspace_local_126cc_18cbbe83d0355538"),
            "Local Workspace"
        );
        assert_eq!(
            organization_display_name("org_default"),
            "Default Organization"
        );
        assert_eq!(
            workspace_display_name("awaken:personal:acct_0123456789"),
            "Personal Workspace"
        );
    }
}

fn workspace_context_router(organization: String, identity_mode: &'static str) -> Router {
    Router::new().route(
        "/v1/config/workspace-context",
        get(
            move |Extension(scope): Extension<awaken_tenancy::WorkspaceScope>,
                  principal: Option<Extension<crate::authz::AuthedPrincipal>>| {
                let organization_id = organization.clone();
                async move {
                    let workspace_id = scope.0;
                    let user_display_name = principal_display_name(
                        principal.as_ref().map(|Extension(principal)| &principal.0),
                        identity_mode,
                    );
                    let workspace_name = workspace_display_name(&workspace_id);
                    let organization_name = if identity_mode == "no-login" {
                        "Local Organization".into()
                    } else {
                        organization_display_name(&organization_id)
                    };
                    Json(serde_json::json!({
                        "workspace_id": workspace_id,
                        "workspace_display_name": workspace_name,
                        "organization_id": organization_id,
                        "organization_display_name": organization_name,
                        "user_display_name": user_display_name,
                        "principal_scope_key": principal_scope_key(principal.as_ref().map(|Extension(p)| &p.0), identity_mode),
                    }))
                }
            },
        ),
    )
}

fn principal_scope_key(
    principal: Option<&awaken_iam_contract::PrincipalRef>,
    identity_mode: &str,
) -> Option<String> {
    if principal.is_none() && identity_mode != "no-login" {
        return None;
    }
    let bytes =
        serde_json::to_vec(&(identity_mode, principal)).expect("principal reference serialization");
    Some(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod workspace_context_tests {
    use super::*;
    #[test]
    fn draft_scope_uses_only_trusted_actor_identity_and_explicit_local_mode() {
        // DT-ACTOR: no principal + no-login -> stable local scope; no principal
        // + authenticated mode -> absent; distinct trusted principals ->
        // distinct scopes. The opaque value contains no credential or clear id
        // and is never consumed by command admission.
        assert!(principal_scope_key(None, "no-login").is_some());
        assert!(principal_scope_key(None, "awaken-cloud").is_none());
        let a = awaken_iam_contract::PrincipalRef::Service {
            service_id: "service-a".into(),
        };
        let b = awaken_iam_contract::PrincipalRef::Service {
            service_id: "service-b".into(),
        };
        let first = principal_scope_key(Some(&a), "self-managed");
        assert_eq!(first, principal_scope_key(Some(&a), "self-managed"));
        assert_ne!(first, principal_scope_key(Some(&b), "self-managed"));
        assert!(!first.unwrap().contains("service-a"));
    }
}
