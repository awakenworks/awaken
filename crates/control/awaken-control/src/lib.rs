//! `awaken-control` — the authoring / authz plane (Stage B2).
//!
//! This crate owns the management **authoring** surfaces and the embedded IAM
//! guard: the admin config CRUD (catalog / credentials / profiles / MCP defs), the
//! webhook-subscription CRUD, the Managed vault front door, user profiles,
//! deployments, environments, the config authoring plane (`/v1/config/agents/*`),
//! and the capability snapshot — all optionally gated behind the embedded
//! `ApiToken` guard (ADR-0042/0043 P1). [`control_router`] weaves them into one
//! guarded management router.
//!
//! It is a sibling of the `awaken-server` **data plane** (session surface +
//! protocol adapters + host): the two do NOT depend on each other. The single
//! machine composition root (`awaken-cli`) builds the shared handles (stores,
//! vault/env state, the config plane), asks this crate for the authoring router,
//! asks `awaken-server` for the data-plane router, merges them, and applies the
//! guard exactly where it applied before.

pub mod admin_assistant;
pub mod authz;
pub mod control_stores;
pub mod resource_owner;
pub mod worker_stores;

use std::sync::Arc;

pub use crate::admin_assistant::{
    CatalogCapabilityReader, ConfigServiceDraftStore, ConfigServiceDraftValidator,
    HostResourceInventory, seed_admin_assistant,
};
// Embedded management-plane IAM (ADR-0042/0043 P1): the authorizer, its boot
// fn, the mint spec (tests / operator embeddings), and the bootstrap constants.
pub use crate::authz::{
    ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, BOOTSTRAP_WORKSPACE, ManagementAuthz, TokenSpec,
    embedded_iam,
};
pub use crate::control_stores::{ControlStoreConfig, StoreBackend};
pub use crate::resource_owner::{ResourceOwners, resource_ownership_guard};
// The database-less worker's shared store subset (Stage C): the catalog + credential
// vault + secret store a drained run resolves its model from, opened the same way the
// Serve composition opens them (Option A, shared-DB).
pub use crate::worker_stores::{
    SharedConfigStores, open_shared_config_stores, open_shared_config_stores_from_env,
};

use awaken_admin_config_api::{
    AdminState, CredentialProbe, InferenceProfileStore, McpStore, WebhookStore, admin_router,
};
use awaken_config_resolver::ResourceStore;
use awaken_config_service::{
    ConfigPlane, ConfigService, ConfigServiceAgentSource, capabilities_router, config_router,
};
use awaken_credential_vault::SecretStore;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_model_catalog::repo::CatalogRepo;
use awaken_protocol_managed::{
    AgentRegistryState, DeploymentState, EnvironmentState, UserProfileState, VaultState,
    agents_router, deployments_router, environments_router, user_profiles_router, vault_router,
};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_webhook_managed::{WebhookLifecycleSink, assemble};
use axum::Router;

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
    /// Authored inference profiles (admin aggregate).
    pub profiles: Arc<dyn InferenceProfileStore>,
    /// Authored MCP server definitions (admin aggregate).
    pub mcp_store: Arc<dyn McpStore>,
    /// Authored webhook endpoints (admin aggregate); the sink is returned for the data plane.
    pub webhook_store: Arc<dyn WebhookStore>,
    /// Per-agent resource bindings, shared with the config service (ADR-0038).
    pub resource_store: Arc<dyn ResourceStore>,
    /// The live credential probe (provider-backed), injected by the composition root.
    pub probe: Arc<dyn CredentialProbe>,
    /// The Managed vault state, shared with the data-plane managed state.
    pub vault_state: Arc<VaultState>,
    /// The environment state, shared with the data-plane managed state.
    pub env_state: Arc<EnvironmentState>,
    /// The config authoring plane (scope edge over the config service).
    pub plane: ConfigPlane,
    /// The scope-free config service (also wired into the host), for the `/v1/agents` projection.
    pub config_service: Arc<ConfigService>,
    /// The host's global tool descriptors, for `GET /v1/capabilities`.
    pub global_tools: Vec<ToolDescriptor>,
    /// The org id stamped on webhook deliveries (`AWAKEN_ORG_ID`).
    pub org_id: Option<String>,
    /// The embedded IAM guard, when enabled (`AWAKEN_MGMT_IAM=embedded`).
    pub iam: Option<Arc<ManagementAuthz>>,
}

/// Build the authoring / authz management router over the shared handles, and
/// return the webhook lifecycle sink the data plane feeds. The guard (when
/// present) wraps ONLY the admin + vault surfaces: an axum layer binds to the
/// routes present when applied, so merging the guarded sub-router later leaves
/// every other surface untouched (ADR-0043). Behavior is identical to the
/// pre-split `management_router_over` authoring half.
pub fn control_router(input: ControlRouterInput) -> (Router, Arc<WebhookLifecycleSink>) {
    let ControlRouterInput {
        catalog,
        credentials,
        secrets,
        profiles,
        mcp_store,
        webhook_store,
        resource_store,
        probe,
        vault_state,
        env_state,
        plane,
        config_service,
        global_tools,
        org_id,
        iam,
    } = input;

    // ONE MCP store across the admin router and the ManagedHost, and ONE
    // credential repo + secret store across admin, vaults, and sessions: a
    // credential or MCP config entered through any surface is the same row a
    // session's prepare reads (ADR-0043 Phase 3).
    let admin = admin_router(AdminState {
        catalog,
        credentials: credentials.clone(),
        secrets: secrets.clone(),
        profiles,
        mcp: mcp_store.clone(),
        // Per-agent resource bindings (ADR-0038).
        resources: resource_store.clone(),
        // The live credential probe is backed by provider-genai in the composition
        // root — the only place the model SDK is named; the admin CRUD crate stays
        // SDK-free.
        probe: Some(probe),
        // Shared credential-availability cooldowns (E3-4).
        availability: Default::default(),
    });
    // The webhook plane (ADR-0048): subscriptions are an id-addressed config resource
    // in the same admin store, their `whsec_` secret sealed in the shared vault. Merge
    // the front door into the admin router BEFORE the ownership fence so
    // `/v1/config/webhook-subscriptions/{id}` is tenant-fenced like profiles/MCP; the
    // sink (fed the same store + secrets) fans committed session facts out-of-band.
    let (webhook_sink, webhook_crud) = assemble(webhook_store, secrets.clone(), org_id);
    let admin = admin.merge(webhook_crud);
    // Tenant ownership for the id-addressed config resources (ADR-0051): MCP server
    // defs, inference profiles, and webhook subscriptions are fenced by the authoring
    // scope. The shared catalog is intentionally uncovered (org/deployment-level
    // config). Wraps the admin router only; these are matched routes, so a route
    // `layer` runs correctly.
    let admin = admin.layer(axum::middleware::from_fn_with_state(
        ResourceOwners::new(),
        resource_ownership_guard,
    ));
    let vaults = vault_router(vault_state);
    // The user-profiles front door (`/v1/user_profiles`) over its own in-mem store.
    let user_profiles = user_profiles_router(Arc::new(UserProfileState::new()));
    // Deployments + deployment runs (`/v1/deployments`, `/v1/deployment_runs`).
    let deployments = deployments_router(Arc::new(DeploymentState::new()));
    // Environments + work queue (`/v1/environments`, single-worker open cap). Shared
    // with the session state so `POST /v1/sessions` resolves an environment's
    // networking policy (egress on/off) at creation.
    let environments = environments_router(env_state);
    // The config authoring plane (`/v1/config/agents/*`): the console authors the
    // rich `AgentConfig` here and `publish` compiles + installs it so sessions run it.
    let config_plane = config_router(plane);
    // `/v1/agents` projects the config plane it hosts: an agent published via
    // `/v1/config/agents` is retrievable as a managed-wire projection of that single
    // truth (no second store), which is how the console probes the assistant.
    let agents = agents_router(Arc::new(
        AgentRegistryState::new()
            .with_config_source(Arc::new(ConfigServiceAgentSource(config_service))),
    ));
    // Capability snapshot (`GET /v1/capabilities`): the host's tool descriptors +
    // installable plugins (with config schema) so the console authors data-driven.
    let capabilities = capabilities_router(global_tools);

    // The IAM guard (when enabled) wraps the admin + vault routers only. An
    // axum layer binds to the routes present when it is applied, so merging
    // the guarded sub-router later leaves every other surface untouched. The
    // token-management routes exist ONLY under the guard (they authorize
    // against the same embedded IAM the guard authenticates with), and they
    // are merged before the layer so the guard authenticates them first.
    let mut mgmt = admin
        .merge(vaults)
        .merge(user_profiles)
        .merge(agents)
        .merge(deployments)
        .merge(environments)
        .merge(config_plane)
        .merge(capabilities);
    if let Some(iam) = iam {
        mgmt = mgmt.merge(crate::authz::token_router(iam.clone()));
        mgmt = mgmt.layer(axum::middleware::from_fn_with_state(
            iam,
            crate::authz::management_guard,
        ));
    }
    (mgmt, webhook_sink)
}
