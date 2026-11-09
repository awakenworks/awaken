// @formal entrypoint resource_binding.draft_audit_and_effect_are_atomic
// @formal entrypoint resource_binding.pending_effect_survives_until_readback
// @formal entrypoint resource_binding.replay_is_idempotent_and_identity_stable
//! Host-side wiring for the management assistant (ADR-0052): the real adapters behind
//! the admin tools' ports, plus the startup seeding that publishes the assistant as an
//! ordinary agent in the reserved scope.
//!
//! The assistant is authored, compiled, and published like any agent (D1) — the
//! seeding here is exactly a `put` + `publish` through `ConfigService`, in the reserved
//! scope (D2), where the scope-keyed catalog makes the six admin tools nameable (D3).

use std::sync::Arc;

use crate::authz::with_composed_workspace;
use async_trait::async_trait;
use awaken_admin_assistant::{
    ADMIN_ASSISTANT_AGENT_ID, CapabilityReader, DraftStore, DraftValidator, InputSpec,
    PlatformCapabilities, PluginInfo, ResourceInventory, admin_assistant_config,
};
use awaken_agent_config::{
    AgentConfig, AgentConfigRevision, AuditedConfigWrite, ManagementEffect, ModelSelection,
};
use awaken_agent_contract::ScopeId;
use awaken_config_resolver::{
    AgentInputBindingRepository, AgentInputConfig, BindingId, FileId, InputBinding,
    InputResourceId, MemoryStoreId, RepositoryId, ResourceAccess,
};
use awaken_config_service::{ConfigPlane, RESERVED_ADMIN_SCOPE};
use awaken_executable_agent_contract::capability::PluginCapability;
use awaken_model_catalog::repo::CatalogRepo;
use awaken_runtime_contract::resolved::ToolDescriptor;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde_json::{Value, json};

/// One explicit local-operator Workspace selector. Hosted execution attempts
/// cannot authenticate a current human management request, so definition ports
/// refuse them rather than treating runtime scope as management authority.
#[derive(Clone)]
enum AssistantWorkspace {
    Fixed(ScopeId),
    CurrentAttempt,
}

impl AssistantWorkspace {
    fn fixed(scope: impl Into<ScopeId>) -> Self {
        Self::Fixed(scope.into())
    }

    fn current_attempt() -> Self {
        Self::CurrentAttempt
    }

    fn operator_scope(&self) -> Result<ScopeId, String> {
        match self {
            Self::Fixed(scope) => Ok(scope.clone()),
            Self::CurrentAttempt => Err("Hosted Agent tools cannot access or change definitions; a current human management request is required".into()),
        }
    }
}

#[derive(Clone)]
struct AdminAssistantLifecycleState {
    plane: ConfigPlane,
    model_selection: Option<ModelSelection>,
}

/// The execution Workspace the trusted edge already resolved. Absence is
/// refused here so the handler below owns no `None` arm; an explicit
/// local/no-login composition supplies its Workspace through the one
/// [`with_composed_workspace`] layer at the management edge instead.
struct RequiredExecutionWorkspace(String);

impl<S> axum::extract::FromRequestParts<S> for RequiredExecutionWorkspace
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<Value>);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        match parts
            .extensions
            .get::<awaken_runtime_contract::runs::scope::WorkspaceScope>()
            .and_then(awaken_runtime_contract::runs::scope::WorkspaceScope::non_empty)
        {
            Some(workspace) => Ok(Self(workspace.to_owned())),
            None => Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "code": "assistant_workspace_unavailable",
                    "title": "Assistant Workspace unavailable",
                    "detail": "no trusted execution Workspace was resolved",
                    "message": "no trusted execution Workspace was resolved",
                })),
            )),
        }
    }
}

/// Idempotent operator-facing recovery for the reserved Assistant publication.
/// Startup already attempts the same ordinary publication path; this route closes
/// the setup-after-startup loop when a runnable model becomes available later.
pub fn admin_assistant_lifecycle_router(
    plane: ConfigPlane,
    fixed_execution_workspace: Option<String>,
    model_selection: Option<ModelSelection>,
) -> Router {
    with_composed_workspace(
        Router::new()
            .route(
                "/v1/config/agents/__admin_assistant/ensure",
                post(ensure_admin_assistant),
            )
            .with_state(AdminAssistantLifecycleState {
                plane,
                model_selection,
            }),
        fixed_execution_workspace,
    )
}

async fn ensure_admin_assistant(
    State(state): State<AdminAssistantLifecycleState>,
    // Cloud IAM/workspace-path middleware owns tenant selection. A hosted
    // request must use that trusted scope; an explicit local/no-login
    // composition supplies its own through `with_composed_workspace`.
    RequiredExecutionWorkspace(execution_workspace): RequiredExecutionWorkspace,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let had_startup_selection = state.model_selection.is_some();
    let selection = state.model_selection.unwrap_or_default();
    let first = seed_admin_assistant(&state.plane, &execution_workspace, selection.clone()).await;
    // A Worker-local ACP binding can become stale after startup, while a provider
    // connection added in the Console is immediately visible to Auto resolution.
    // Preserve an explicit usable selection first, then close that recovery loop.
    let result = if first.is_err() && selection != ModelSelection::Auto {
        seed_admin_assistant(&state.plane, &execution_workspace, ModelSelection::Auto).await
    } else {
        first
    };
    result
        .map(|()| Json(json!({ "status": "ready", "agent_id": ADMIN_ASSISTANT_AGENT_ID })))
        .map_err(|error| {
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "code": if had_startup_selection {
                        "assistant_publication_pending"
                    } else {
                        "assistant_model_unavailable"
                    },
                    "title": if had_startup_selection {
                        "Assistant publication pending"
                    } else {
                        "Assistant model unavailable"
                    },
                    "detail": error.clone(),
                    "message": error,
                })),
            )
        })
}

/// Publish the management assistant into the reserved scope through the ordinary
/// publish path (D1/D2), via the scope edge ([`ConfigPlane`]). Idempotent —
/// re-seeding recompiles to the same content address. Returns the publish error
/// verbatim so a caller can surface a bad setup (e.g. no provider-backed model).
pub async fn seed_admin_assistant(
    plane: &ConfigPlane,
    execution_workspace: &str,
    model_selection: ModelSelection,
) -> Result<(), String> {
    let scope = ScopeId::from(RESERVED_ADMIN_SCOPE);
    let mut config = admin_assistant_config();
    config.model_binding = model_selection;
    plane.put(&scope, &config).await?;
    plane
        .publish_for_execution_workspace(&scope, execution_workspace, ADMIN_ASSISTANT_AGENT_ID)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Reads the redacted, org-shared capability snapshot (D4) LIVE: models + providers
/// from the live catalog repo, MCP servers from Agent authoring, existing agent ids
/// from the config plane, and memory-stores + skills from a data-plane resource
/// inventory (when the composition root can reach it). The advertised (global) tool ids
/// and installable plugins are static (they do not change at run time). Carries only
/// ids/names — never a key, endpoint, or header.
pub struct CatalogCapabilityReader {
    /// The live model catalog (not a frozen seed snapshot) — read on every call so a
    /// model an operator adds AFTER startup is visible.
    catalog: Arc<dyn CatalogRepo>,
    /// The global tool catalog ids — static (the runtime tool registry is fixed).
    tools: Vec<String>,
    /// Installable plugins with their config schemas — static.
    plugins: Vec<PluginInfo>,
    /// The config-authoring plane, used to list existing agent ids in the scope.
    plane: ConfigPlane,
    /// The scope whose agents are listed (the tenant/default scope).
    workspace: AssistantWorkspace,
    /// The data-plane resource inventory (memory stores + skills). `None` when the
    /// composition root cannot cleanly reach the data-plane handles at wire time.
    inventory: Option<Arc<dyn ResourceInventory>>,
}

impl CatalogCapabilityReader {
    pub fn new(
        catalog: Arc<dyn CatalogRepo>,
        global_tools: &[ToolDescriptor],
        plugins: &[PluginCapability],
        plane: ConfigPlane,
        workspace: impl Into<ScopeId>,
        inventory: Option<Arc<dyn ResourceInventory>>,
    ) -> Self {
        Self {
            catalog,
            tools: global_tools.iter().map(|d| d.id.clone()).collect(),
            plugins: plugins
                .iter()
                .map(|p| PluginInfo {
                    id: p.id.clone(),
                    schema_keys: p.schema_keys.clone(),
                    // Carry the plugin's JSON Schema through so the assistant authors a
                    // conformant section rather than guessing its shape.
                    config_schema: p.config_schema.clone(),
                })
                .collect(),
            plane,
            workspace: AssistantWorkspace::fixed(workspace),
            inventory,
        }
    }

    /// Hosted execution has no current human management admission; definition
    /// tools stay closed until such a native request contract is supplied.
    pub fn request_scoped(
        catalog: Arc<dyn CatalogRepo>,
        global_tools: &[ToolDescriptor],
        plugins: &[PluginCapability],
        plane: ConfigPlane,
        inventory: Option<Arc<dyn ResourceInventory>>,
    ) -> Self {
        let mut reader = Self::new(
            catalog,
            global_tools,
            plugins,
            plane,
            ScopeId::from("unused-hosted-workspace"),
            inventory,
        );
        reader.workspace = AssistantWorkspace::current_attempt();
        reader
    }
}

#[async_trait]
impl CapabilityReader for CatalogCapabilityReader {
    fn available(&self) -> Result<(), String> {
        self.workspace.operator_scope().map(|_| ())
    }

    async fn capabilities(&self) -> PlatformCapabilities {
        let Ok(scope) = self.workspace.operator_scope() else {
            return PlatformCapabilities::default();
        };
        // LIVE model catalog: deduped offering model ids (catalog order) + provider keys.
        // A read failure degrades to empty rather than failing the tool call.
        let (models, providers) = match self.catalog.snapshot().await {
            Ok(catalog) => {
                let mut models = Vec::new();
                for offering in &catalog.offerings {
                    if offering.status == awaken_model_catalog::OfferingStatus::Active
                        && !models.contains(&offering.model_id)
                    {
                        models.push(offering.model_id.clone());
                    }
                }
                (models, catalog.providers.keys().cloned().collect())
            }
            Err(_) => (Vec::new(), Vec::new()),
        };
        // AgentConfig is the sole MCP authoring truth. Derive the capability
        // inventory from the same typed bindings instead of a parallel MCP catalog.
        let (agents, mcp_servers) = match self.plane.list(&scope).await {
            Ok(configs) => {
                let mut mcp_servers = Vec::new();
                let agents = configs
                    .into_iter()
                    .filter(|config| config.id != ADMIN_ASSISTANT_AGENT_ID)
                    .map(|config| {
                        for server in config.mcp_servers {
                            if !mcp_servers.contains(&server.name) {
                                mcp_servers.push(server.name);
                            }
                        }
                        config.id
                    })
                    .collect();
                (agents, mcp_servers)
            }
            Err(_) => (Vec::new(), Vec::new()),
        };
        // Data-plane inventory (memory stores + skills) when reachable; else empty.
        let (memory_stores, skills) = match &self.inventory {
            Some(inv) => (
                inv.memory_stores(scope.as_str()).await,
                inv.skills(scope.as_str()).await,
            ),
            None => (Vec::new(), Vec::new()),
        };
        PlatformCapabilities {
            agents,
            models,
            providers,
            tools: self.tools.clone(),
            plugins: self.plugins.clone(),
            skills,
            mcp_servers,
            memory_stores,
        }
    }
}

/// The LIVE data-plane resource inventory (ADR-0038 memory stores + skills) behind the
/// [`ResourceInventory`] port, so the [`CatalogCapabilityReader`] can report real memory
/// store + skill ids without `awaken-control` depending on the runtime host. Memory-store
/// definitions come from the durable [`awaken_resource_contract::ResourceRegistry`]
/// shared by authoring and Session resolution, and skills from the
/// [`awaken_resource_contract::SkillStore`]. Carries only ids — never a secret or policy.
pub struct HostResourceInventory {
    memory: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    skills: Arc<dyn awaken_resource_contract::SkillStore>,
}

impl HostResourceInventory {
    /// Build the inventory from the two live handles. Each read receives the
    /// request-resolved Workspace; the adapter stores no parallel tenant default.
    pub fn new(
        memory: Arc<dyn awaken_resource_contract::ResourceRegistry>,
        skills: Arc<dyn awaken_resource_contract::SkillStore>,
    ) -> Self {
        Self { memory, skills }
    }
}

#[async_trait]
impl ResourceInventory for HostResourceInventory {
    async fn memory_stores(&self, workspace_id: &str) -> Vec<String> {
        // Only executable/candidate store ids belong in assistant capabilities;
        // management inventory intentionally retains archived definitions.
        self.memory
            .list_memory_stores(workspace_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|definition| {
                matches!(
                    definition.state,
                    awaken_resource_contract::ResourceState::Active
                        | awaken_resource_contract::ResourceState::Suspended
                )
            })
            .map(|d| d.id.to_string())
            .collect()
    }

    async fn skills(&self, workspace_id: &str) -> Vec<String> {
        // A repository read failure degrades to empty rather than failing the
        // capability call.
        self.skills
            .list_definitions(workspace_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|definition| definition.id.to_string())
            .collect()
    }
}

/// Validates a drafted config through the ordinary config-plane validate in a target
/// scope (the tenant/default scope — a draft is an ordinary agent, so it is validated
/// against the global catalog, and naming an admin tool in a draft is correctly
/// rejected). This is the same check `/v1/config/agents/validate` runs. It holds the
/// scope edge ([`ConfigPlane`]) so the scope-free service stays untouched.
pub struct ConfigServiceDraftValidator {
    plane: ConfigPlane,
    workspace: AssistantWorkspace,
}

impl ConfigServiceDraftValidator {
    pub fn new(plane: ConfigPlane, scope: impl Into<ScopeId>) -> Self {
        Self {
            plane,
            workspace: AssistantWorkspace::fixed(scope),
        }
    }

    pub fn request_scoped(plane: ConfigPlane) -> Self {
        Self {
            plane,
            workspace: AssistantWorkspace::current_attempt(),
        }
    }
}

#[async_trait::async_trait]
impl DraftValidator for ConfigServiceDraftValidator {
    async fn validate(&self, draft: &AgentConfig) -> Result<(), String> {
        // This consumer only needs a human message; flatten the field-routed issue.
        let scope = self.workspace.operator_scope()?;
        self.plane
            .validate(&scope, draft)
            .await
            .map_err(|issue| issue.message)
    }

    async fn runtime_agent_override_ids(
        &self,
        draft: &AgentConfig,
    ) -> Result<std::collections::BTreeSet<String>, String> {
        let scope = self.workspace.operator_scope()?;
        Ok(self.plane.runtime_agent_override_ids(&scope, draft))
    }
}

/// Persists and reads back the admin assistant's drafts as **unpublished** config
/// agents, through the same [`ConfigPlane`] `put`/`get` the editor's Save + the
/// `/v1/config/agents/:id` GET use, in the tenant/default scope. `put` is the
/// unpublished store write (no `publish`), so a drafted agent appears in the console's
/// agent list awaiting the operator's publish; `get` reads that stored draft back.
pub struct ConfigServiceDraftStore {
    plane: ConfigPlane,
    workspace: AssistantWorkspace,
    /// The SEPARATE data-plane resource store (ADR-0038): which resources an agent
    /// mounts, keyed by workspace + agent id. The assistant authors bindings through the same
    /// `DraftStore` port so a single tool call fills in a whole agent — config plus its
    /// mounted resources — even though the two persist to different stores.
    resources: Arc<dyn AgentInputBindingRepository>,
}

impl ConfigServiceDraftStore {
    pub fn new(
        plane: ConfigPlane,
        scope: impl Into<ScopeId>,
        resources: Arc<dyn AgentInputBindingRepository>,
    ) -> Self {
        Self {
            plane,
            workspace: AssistantWorkspace::fixed(scope),
            resources,
        }
    }

    pub fn request_scoped(
        plane: ConfigPlane,
        resources: Arc<dyn AgentInputBindingRepository>,
    ) -> Self {
        Self {
            plane,
            workspace: AssistantWorkspace::current_attempt(),
            resources,
        }
    }

    /// Reconcile durable resource-binding effects until the owning service
    /// cancels this component task. Construction remains side-effect free.
    pub async fn run_resource_effect_reconciliation(
        self: Arc<Self>,
        cancellation: awaken_runtime_contract::CancellationToken,
    ) -> Result<(), String> {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => break,
                _ = interval.tick() => {}
            }
            // This service only applies effects of already admitted durable
            // management commands; it cannot create a new tool confirmation.
            let scopes = match &self.workspace {
                AssistantWorkspace::Fixed(scope) => vec![scope.clone()],
                AssistantWorkspace::CurrentAttempt => self.plane.authoring_scopes().await?,
            };
            for scope in scopes {
                if let Err(error) =
                    apply_pending_resource_effects(&self.plane, &scope, self.resources.as_ref())
                        .await
                {
                    eprintln!("resource binding reconciliation failed: {error}");
                }
            }
        }
        Ok(())
    }
}

async fn apply_pending_resource_effects(
    plane: &ConfigPlane,
    scope: &ScopeId,
    resources: &dyn AgentInputBindingRepository,
) -> Result<usize, String> {
    let effects = plane.pending_management_effects(scope).await?;
    let mut completed = 0;
    for effect in effects {
        let ManagementEffect::UpsertAgentInputs { config } = &effect;
        resources
            .put_agent_inputs(scope.as_str(), config.clone())
            .await
            .map_err(|error| error.to_string())?;
        let observed = resources
            .get_agent_inputs(scope.as_str(), &config.agent_id)
            .await
            .map_err(|error| error.to_string())?;
        let receipt = effect
            .confirm_readback(scope, observed.as_ref())
            .ok_or_else(|| {
                format!(
                    "resource store did not durably read back binding `{}`",
                    config.agent_id
                )
            })?;
        completed += usize::from(plane.complete_management_effect(&receipt).await?);
    }
    Ok(completed)
}

/// Map the assistant's transport string to the canonical input identity.
fn parse_target(kind: &str, resource_id: String) -> Result<InputResourceId, String> {
    if resource_id.trim().is_empty() {
        return Err(format!("resource id is required for `{kind}`"));
    }
    match kind {
        "file" => Ok(InputResourceId::File(FileId::from(resource_id))),
        "memory_store" => Ok(InputResourceId::MemoryStore(MemoryStoreId::from(
            resource_id,
        ))),
        "github_repository" | "repository" => {
            Ok(InputResourceId::Repository(RepositoryId::from(resource_id)))
        }
        "outputs" => Err("outputs are configured by the Environment, not as Agent inputs".into()),
        "skill" => Err("skills are configured through Agent skills, not as inputs".into()),
        other => Err(format!("unknown resource kind `{other}`")),
    }
}

/// Map the neutral `access` string to a [`ResourceAccess`], defaulting to read/write
/// when the operator omits it (mirrors the editor's default).
fn parse_access(access: Option<&str>) -> Result<ResourceAccess, String> {
    match access {
        None | Some("read_write") => Ok(ResourceAccess::ReadWrite),
        Some("read_only") => Ok(ResourceAccess::ReadOnly),
        Some(other) => Err(format!("unknown resource access `{other}`")),
    }
}

fn resource_config(
    agent_id: &str,
    resources: Vec<InputSpec>,
    environment: Option<awaken_config_resolver::AgentEnvironmentBinding>,
    revision: i64,
) -> Result<AgentInputConfig, String> {
    let default_mounts = awaken_provisioning_contract::resource_input_default_mounts();
    let mut bindings = Vec::with_capacity(resources.len());
    for (index, spec) in resources.into_iter().enumerate() {
        let target = parse_target(&spec.kind, spec.resource_id)?;
        let requested_access = parse_access(spec.access.as_deref())?;
        let access = if matches!(&target, InputResourceId::File(_)) {
            ResourceAccess::ReadOnly
        } else {
            requested_access
        };
        let mount_path = spec
            .mount_path
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| default_mounts.mount_path(&target).to_string());
        bindings.push(InputBinding {
            binding_id: BindingId::new(format!("agent:{agent_id}:input:{index}")),
            target,
            mount_path,
            access,
            instructions: spec.instructions,
        });
    }
    Ok(AgentInputConfig {
        agent_id: agent_id.to_string(),
        environment,
        inputs: bindings,
        revision,
    })
}

#[async_trait]
impl DraftStore for ConfigServiceDraftStore {
    async fn put_audited_with_resources(
        &self,
        draft: &AgentConfig,
        expected_revision: u64,
        audit: &awaken_admin_assistant::AdminAuditEvent,
        resources: Option<Vec<InputSpec>>,
    ) -> Result<(), String> {
        let scope = self.workspace.operator_scope()?;
        let current = self
            .resources
            .get_agent_inputs(scope.as_str(), &draft.id)
            .await
            .map_err(|error| error.to_string())?;
        let revision = current.as_ref().map_or(1, |current| current.revision + 1);
        let environment = current.and_then(|current| current.environment);
        let resource_config = resources
            .map(|resources| resource_config(&draft.id, resources, environment, revision))
            .transpose()?;
        let effect = resource_config.map(|config| ManagementEffect::UpsertAgentInputs { config });
        let write = self
            .plane
            .put_with_audit_effect(&scope, draft, expected_revision, audit, effect.as_ref())
            .await?;
        accept_audited_config_write(write)?;
        apply_pending_resource_effects(&self.plane, &scope, self.resources.as_ref())
            .await
            .map_err(|error| format!("resources could not be bound: {error}"))?;
        Ok(())
    }

    async fn record_audit(
        &self,
        audit: &awaken_admin_assistant::AdminAuditEvent,
    ) -> Result<AuditedConfigWrite, String> {
        self.plane
            .record_management_audit(&self.workspace.operator_scope()?, audit)
            .await
    }

    async fn get_audit(
        &self,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<awaken_agent_config::ManagementAuditEntry>, String> {
        self.plane
            .get_management_audit(&self.workspace.operator_scope()?, tool, call_id)
            .await
    }

    async fn reconcile_pending_effects(&self) -> Result<(), String> {
        let scope = self.workspace.operator_scope()?;
        apply_pending_resource_effects(&self.plane, &scope, self.resources.as_ref())
            .await
            .map(|_| ())
            .map_err(|error| format!("resources could not be bound: {error}"))
    }

    async fn get_versioned(&self, id: &str) -> Result<Option<AgentConfigRevision>, String> {
        self.plane
            .get_versioned(&self.workspace.operator_scope()?, id)
            .await
    }
}

fn accept_audited_config_write(write: AuditedConfigWrite) -> Result<(), String> {
    match write {
        AuditedConfigWrite::Applied | AuditedConfigWrite::Replayed => Ok(()),
        AuditedConfigWrite::Conflict { current_revision } => Err(format!(
            "Agent changed concurrently (current revision: {current_revision:?})"
        )),
    }
}

#[cfg(test)]
mod tests {
    mod hosted_definition_tests;
    use super::*;
    use awaken_agent_config::{DEFAULT_SCOPE, ModelSelection};
    use awaken_config_resolver::InMemoryAgentInputBindingRepository;
    use awaken_config_service::{
        ConfigPlane, ConfigService, ModelPublicationResolver, ResolvedPublicationModels,
        ScopedToolCatalog, StaticToolCatalog,
    };
    use awaken_config_store::SqliteConfigStore;
    use awaken_executable_agent_catalog::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};
    use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
    use awaken_model_catalog::{
        ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderCatalog,
        ProviderId,
    };
    use awaken_runtime_contract::resolved::ModelBinding;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    /// Build a LIVE in-memory catalog repo with one provider + endpoint + offering, so
    /// `snapshot()` yields the given `model` under provider `anthropic`.
    async fn repo(model: &str) -> Arc<dyn CatalogRepo> {
        let repo = InMemoryCatalogRepo::new();
        repo.put_provider(Provider {
            id: ProviderId::new("anthropic"),
            slug: "anthropic".into(),
            display_name: "Anthropic".into(),
            version: 1,
        })
        .await
        .unwrap();
        repo.put_endpoint(ProtocolEndpoint {
            workspace_id: None,
            id: ProtocolEndpointId::new("ep1"),
            provider_id: ProviderId::new("anthropic"),
            dialect: ApiDialect::AnthropicMessages,
            base_url: None,
            timeout_secs: 30,
            display_name: "ep".into(),
            version: 1,
        })
        .await
        .unwrap();
        repo.put_offering(Offering {
            model_id: model.to_string(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: None,
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
        })
        .await
        .unwrap();
        Arc::new(repo)
    }

    fn tool(id: &str) -> ToolDescriptor {
        ToolDescriptor::pinned("t", id, "d", serde_json::json!({"type": "object"}))
    }

    /// A minimal host-executor publication resolver for this control-plane test.
    struct FirstOfferingResolver(ProviderCatalog);

    #[async_trait::async_trait]
    impl ModelPublicationResolver for FirstOfferingResolver {
        async fn resolve_models(
            &self,
            _workspace: &awaken_agent_contract::ScopeId,
            selection: &ModelSelection,
            fallbacks: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, awaken_config_service::PublicationResolutionError>
        {
            let (primary, fallbacks) = if let Some(primary) = selection.resolved() {
                (primary.clone(), fallbacks.to_vec())
            } else {
                let mut offerings = self.0.offerings.iter().filter(|offering| {
                    offering.status == awaken_model_catalog::OfferingStatus::Active
                });
                let primary = offerings
                    .next()
                    .ok_or(awaken_config_service::PublicationResolutionError::MissingPrimary)?;
                let binding = |offering: &Offering| {
                    ModelBinding::new(&offering.provider_id.0, &offering.model_id, "genai")
                };
                (binding(primary), offerings.map(binding).collect())
            };
            Ok(ResolvedPublicationModels::host(
                primary, fallbacks, None, None,
            ))
        }
    }

    fn catalog(model: &str) -> ProviderCatalog {
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".to_string(),
            Provider {
                id: ProviderId::new("anthropic"),
                slug: "anthropic".into(),
                display_name: "Anthropic".into(),
                version: 1,
            },
        );
        ProviderCatalog {
            providers,
            offerings: vec![Offering {
                model_id: model.to_string(),
                provider_id: ProviderId::new("anthropic"),
                protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
                dialect: ApiDialect::AnthropicMessages,
                upstream_model: None,
                source: Default::default(),
                status: Default::default(),
                last_seen_at_unix_ms: None,
            }],
            ..Default::default()
        }
    }

    fn test_config_service() -> ConfigService {
        ConfigService::new(
            Arc::new(FirstOfferingResolver(catalog("m-1"))),
            Arc::new(LocalExecutableAgentRegistrar::new(Arc::new(
                ExecutableAgentCatalog::new(),
            ))),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            Vec::new(),
            |_| true,
        )
    }

    fn test_config_service_with_catalog() -> (ConfigService, Arc<ExecutableAgentCatalog>) {
        let executable = Arc::new(ExecutableAgentCatalog::new());
        (
            ConfigService::new(
                Arc::new(FirstOfferingResolver(catalog("m-1"))),
                Arc::new(LocalExecutableAgentRegistrar::new(executable.clone())),
                std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
                Vec::new(),
                |_| true,
            ),
            executable,
        )
    }

    enum ReadbackHook {
        Pause(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        ),
        Missing,
        Changed,
        Failed,
    }

    struct ObservedInputRepository {
        inner: InMemoryAgentInputBindingRepository,
        hook: std::sync::Mutex<Option<ReadbackHook>>,
    }

    #[async_trait::async_trait]
    impl AgentInputBindingRepository for ObservedInputRepository {
        async fn put_agent_inputs(
            &self,
            workspace: &str,
            config: AgentInputConfig,
        ) -> Result<(), awaken_config_resolver::AgentInputRepositoryError> {
            self.inner.put_agent_inputs(workspace, config).await
        }

        async fn get_agent_inputs(
            &self,
            workspace: &str,
            agent: &str,
        ) -> Result<Option<AgentInputConfig>, awaken_config_resolver::AgentInputRepositoryError>
        {
            let mut observed = self.inner.get_agent_inputs(workspace, agent).await?;
            let hook = self.hook.lock().unwrap().take();
            match hook {
                Some(ReadbackHook::Pause(ready, resume)) => {
                    ready.send(()).unwrap();
                    tokio::time::timeout(std::time::Duration::from_secs(10), resume)
                        .await
                        .map_err(|e| {
                            awaken_config_resolver::AgentInputRepositoryError::Storage(
                                e.to_string(),
                            )
                        })?
                        .map_err(|e| {
                            awaken_config_resolver::AgentInputRepositoryError::Storage(
                                e.to_string(),
                            )
                        })?;
                }
                Some(ReadbackHook::Missing) => observed = None,
                Some(ReadbackHook::Changed) => {
                    if let Some(config) = &mut observed {
                        config.agent_id.push('x');
                    }
                }
                Some(ReadbackHook::Failed) => {
                    return Err(awaken_config_resolver::AgentInputRepositoryError::Storage(
                        "readback unavailable".into(),
                    ));
                }
                None => {}
            }
            Ok(observed)
        }

        async fn list_agent_inputs(
            &self,
            workspace: &str,
        ) -> Result<Vec<AgentInputConfig>, awaken_config_resolver::AgentInputRepositoryError>
        {
            self.inner.list_agent_inputs(workspace).await
        }
    }

    async fn queue_test_effect(
        plane: &ConfigPlane,
        scope: &ScopeId,
        revision: u64,
    ) -> ManagementEffect {
        let draft = AgentConfig {
            id: "effect-agent".into(),
            instructions: "effect receipt".into(),
            model_binding: ModelSelection::Auto,
            ..Default::default()
        };
        let audit = awaken_agent_config::ManagementAuditRecord {
            tool: "admin_patch_agent".into(),
            call_id: format!("effect-{revision}"),
            summary: format!("revision={revision}"),
        };
        let effect = ManagementEffect::UpsertAgentInputs {
            config: resource_config(&draft.id, Vec::new(), None, revision as i64).unwrap(),
        };
        plane.record_management_audit(scope, &audit).await.unwrap();
        assert_eq!(
            plane
                .put_with_audit_effect(scope, &draft, revision - 1, &audit, Some(&effect))
                .await
                .unwrap(),
            AuditedConfigWrite::Applied
        );
        effect
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn management_effect_late_readback_cannot_retire_the_next_intent() {
        // C1 B applies/reads revision 1 but pauses before returning readback;
        // C2 A retires that exact effect; C3 a new audited write queues revision 2;
        // C4 B resumes with old evidence. Effects: E1 B retires zero rows, E2 the
        // successor remains durable, E3 next reconciliation applies/reads/retires
        // revision 2. Real SQLite + the existing input reducer own both stores.
        let plane = ConfigPlane::new(
            Arc::new(test_config_service()),
            Arc::new(SqliteConfigStore::open_in_memory().await.unwrap()),
            Arc::new(StaticToolCatalog(Vec::new())),
        );
        let scope = ScopeId::from("late-readback");
        let old = queue_test_effect(&plane, &scope, 1).await;
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let resources = Arc::new(ObservedInputRepository {
            inner: InMemoryAgentInputBindingRepository::new(),
            hook: std::sync::Mutex::new(Some(ReadbackHook::Pause(ready_tx, resume_rx))),
        });
        let late = tokio::spawn({
            let plane = plane.clone();
            let scope = scope.clone();
            let resources = resources.clone();
            async move { apply_pending_resource_effects(&plane, &scope, resources.as_ref()).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx)
            .await
            .unwrap()
            .unwrap();
        let observed = resources
            .inner
            .get_agent_inputs(scope.as_str(), old.key())
            .await
            .unwrap();
        let receipt = old.confirm_readback(&scope, observed.as_ref()).unwrap();
        assert!(plane.complete_management_effect(&receipt).await.unwrap());
        let next = queue_test_effect(&plane, &scope, 2).await;
        resume_tx.send(()).unwrap();
        assert_eq!(late.await.unwrap().unwrap(), 0, "E1");
        assert_eq!(
            plane.pending_management_effects(&scope).await.unwrap(),
            vec![next.clone()],
            "E2"
        );
        assert_eq!(
            apply_pending_resource_effects(&plane, &scope, resources.as_ref())
                .await
                .unwrap(),
            1,
            "E3"
        );
        let actual = resources
            .inner
            .get_agent_inputs(scope.as_str(), next.key())
            .await
            .unwrap();
        assert!(next.confirm_readback(&scope, actual.as_ref()).is_some());
        assert!(
            plane
                .pending_management_effects(&scope)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn management_effect_failed_readback_retains_the_durable_intent() {
        // Decision table: physical put succeeded x readback missing/changed/error
        // => no receipt and no retirement. An exact retry then converges through
        // the same reconciler, rather than a separate compensation write path.
        for hook in [
            ReadbackHook::Missing,
            ReadbackHook::Changed,
            ReadbackHook::Failed,
        ] {
            let plane = ConfigPlane::new(
                Arc::new(test_config_service()),
                Arc::new(SqliteConfigStore::open_in_memory().await.unwrap()),
                Arc::new(StaticToolCatalog(Vec::new())),
            );
            let scope = ScopeId::from("readback-failure");
            let expected = queue_test_effect(&plane, &scope, 1).await;
            let resources = ObservedInputRepository {
                inner: InMemoryAgentInputBindingRepository::new(),
                hook: std::sync::Mutex::new(Some(hook)),
            };
            assert!(
                apply_pending_resource_effects(&plane, &scope, &resources)
                    .await
                    .is_err()
            );
            assert_eq!(
                plane.pending_management_effects(&scope).await.unwrap(),
                vec![expected]
            );
            assert_eq!(
                apply_pending_resource_effects(&plane, &scope, &resources)
                    .await
                    .unwrap(),
                1
            );
            assert!(
                plane
                    .pending_management_effects(&scope)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn assistant_resource_defaults_and_overrides_are_total_and_file_is_read_only() {
        // Assistant Resource cause/effect decision table:
        // | Rule | kind | mount input | requested access | Effect |
        // | A1 | memory_store | omitted | omitted | canonical Memory default + RW |
        // | A2 | file | empty | RW | canonical File default + forced RO |
        // | A3 | repository | omitted | omitted | canonical Repository default + RW |
        // | A4 | repository | explicit canonical path | RO | exact path + RO |
        // Constraints: omitted and empty both select the provisioning authority;
        // an explicit path is never rewritten; immutable File access never widens.
        let config = resource_config(
            "agent-defaults",
            vec![
                InputSpec {
                    kind: "memory_store".into(),
                    resource_id: "memory".into(),
                    mount_path: None,
                    access: None,
                    instructions: None,
                },
                InputSpec {
                    kind: "file".into(),
                    resource_id: "file".into(),
                    mount_path: Some(String::new()),
                    access: Some("read_write".into()),
                    instructions: None,
                },
                InputSpec {
                    kind: "repository".into(),
                    resource_id: "repository-default".into(),
                    mount_path: None,
                    access: None,
                    instructions: None,
                },
                InputSpec {
                    kind: "repository".into(),
                    resource_id: "repository-explicit".into(),
                    mount_path: Some("/workspace/custom".into()),
                    access: Some("read_only".into()),
                    instructions: None,
                },
            ],
            None,
            1,
        )
        .unwrap();
        let defaults = awaken_provisioning_contract::resource_input_default_mounts();
        assert_eq!(config.inputs[0].mount_path, defaults.memory_store, "A1");
        assert_eq!(config.inputs[0].access, ResourceAccess::ReadWrite, "A1");
        assert_eq!(config.inputs[1].mount_path, defaults.file, "A2");
        assert_eq!(config.inputs[1].access, ResourceAccess::ReadOnly, "A2");
        assert_eq!(config.inputs[2].mount_path, defaults.repository, "A3");
        assert_eq!(config.inputs[2].access, ResourceAccess::ReadWrite, "A3");
        assert_eq!(config.inputs[3].mount_path, "/workspace/custom", "A4");
        assert_eq!(config.inputs[3].access, ResourceAccess::ReadOnly, "A4");
    }

    #[tokio::test]
    async fn draft_resource_bindings_keep_equal_agent_ids_in_their_workspace() {
        // Causes: C1 equal Agent id under distinct Workspace scopes; C2 an
        // already-cancelled service token starts reconciliation. Effects: E1
        // each scope reads only its binding; E2 reconciliation exits without a
        // detached timer. Rules: R1=C1 -> E1; R2=C2 -> E2.
        let plane = ConfigPlane::new(
            Arc::new(test_config_service()),
            Arc::new(SqliteConfigStore::open_in_memory().await.unwrap()),
            Arc::new(StaticToolCatalog(Vec::new())),
        );
        let resources = Arc::new(InMemoryAgentInputBindingRepository::new());
        let workspace_a = Arc::new(ConfigServiceDraftStore::new(
            plane.clone(),
            "workspace-a",
            resources.clone(),
        ));
        let workspace_b = Arc::new(ConfigServiceDraftStore::new(
            plane,
            "workspace-b",
            resources.clone(),
        ));
        let binding = |resource_id: &str| InputSpec {
            kind: "file".into(),
            resource_id: resource_id.into(),
            mount_path: None,
            access: Some("read_only".into()),
            instructions: None,
        };

        let draft = AgentConfig {
            id: "shared-agent".into(),
            instructions: "workspace-isolated resource binding".into(),
            max_steps: Some(8),
            model_binding: ModelSelection::Auto,
            ..Default::default()
        };
        for (store, call_id, resource_id) in [
            (workspace_a.as_ref(), "resource-a", "file-a"),
            (workspace_b.as_ref(), "resource-b", "file-b"),
        ] {
            let audit = awaken_agent_config::ManagementAuditRecord {
                tool: awaken_admin_assistant::CREATE_DRAFT_TOOL.into(),
                call_id: call_id.into(),
                summary: format!("workspace resource `{resource_id}`"),
            };
            assert_eq!(
                store.record_audit(&audit).await.unwrap(),
                AuditedConfigWrite::Applied,
                "sole audit begin"
            );
            store
                .put_audited_with_resources(&draft, 0, &audit, Some(vec![binding(resource_id)]))
                .await
                .unwrap();
        }

        assert_eq!(
            resources
                .get_agent_inputs("workspace-a", "shared-agent")
                .await
                .unwrap()
                .unwrap()
                .inputs[0]
                .target
                .id(),
            "file-a"
        );
        assert_eq!(
            resources
                .get_agent_inputs("workspace-b", "shared-agent")
                .await
                .unwrap()
                .unwrap()
                .inputs[0]
                .target
                .id(),
            "file-b"
        );
        let cancellation = awaken_runtime_contract::CancellationToken::new();
        cancellation.cancel();
        workspace_a
            .run_resource_effect_reconciliation(cancellation)
            .await
            .expect("R2 cancelled reconciler exits");
    }

    #[tokio::test]
    async fn committed_replay_projection_applies_the_existing_resource_effect() {
        // Response-loss effect table:
        // | rule | config+audit | resource effect | retry projection | effect |
        // | E1   | committed    | pending         | reconcile        | readback present; journal empty |
        // The Assistant replay test owns invocation of this narrow projection; this
        // adapter test proves it delegates to the one existing effect reconciler.
        let raw_store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
        let plane = ConfigPlane::new(
            Arc::new(test_config_service()),
            raw_store,
            Arc::new(StaticToolCatalog(Vec::new())),
        );
        let resources = Arc::new(InMemoryAgentInputBindingRepository::new());
        let scope = ScopeId::from("effect-replay");
        let adapter = ConfigServiceDraftStore::new(plane.clone(), scope.clone(), resources.clone());
        let draft = AgentConfig {
            id: "effect-agent".into(),
            instructions: "resource effect".into(),
            max_steps: Some(8),
            model_binding: ModelSelection::Auto,
            ..Default::default()
        };
        let audit = awaken_agent_config::ManagementAuditRecord {
            tool: awaken_admin_assistant::CREATE_DRAFT_TOOL.into(),
            call_id: "effect-response-loss".into(),
            summary: "draft agent `effect-agent`".into(),
        };
        let effect = ManagementEffect::UpsertAgentInputs {
            config: resource_config(
                &draft.id,
                vec![InputSpec {
                    kind: "file".into(),
                    resource_id: "file-replayed".into(),
                    mount_path: None,
                    access: Some("read_only".into()),
                    instructions: None,
                }],
                None,
                1,
            )
            .unwrap(),
        };
        assert_eq!(
            plane.record_management_audit(&scope, &audit).await.unwrap(),
            AuditedConfigWrite::Applied,
            "E1 sole audit begin"
        );
        assert_eq!(
            plane
                .put_with_audit_effect(&scope, &draft, 0, &audit, Some(&effect))
                .await
                .unwrap(),
            AuditedConfigWrite::Applied,
            "E1 setup"
        );
        assert!(
            resources
                .get_agent_inputs(scope.as_str(), &draft.id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            plane
                .pending_management_effects(&scope)
                .await
                .unwrap()
                .len(),
            1
        );

        adapter.reconcile_pending_effects().await.unwrap();

        assert_eq!(
            resources
                .get_agent_inputs(scope.as_str(), &draft.id)
                .await
                .unwrap()
                .unwrap()
                .inputs[0]
                .target
                .id(),
            "file-replayed",
            "E1"
        );
        assert!(
            plane
                .pending_management_effects(&scope)
                .await
                .unwrap()
                .is_empty(),
            "E1"
        );
    }

    #[tokio::test]
    async fn hosted_execution_scope_does_not_authorize_definition_effects() {
        // A trusted execution Workspace is necessary for runtime execution, but
        // it proves neither a human requester nor current definition authority.
        // H1 present scope and H2 no scope both refuse before any definition,
        // management audit, resource binding or pending effect is written.
        let plane = ConfigPlane::new(
            Arc::new(test_config_service()),
            Arc::new(SqliteConfigStore::open_in_memory().await.unwrap()),
            Arc::new(StaticToolCatalog(Vec::new())),
        );
        let resources = Arc::new(InMemoryAgentInputBindingRepository::new());
        let store = ConfigServiceDraftStore::request_scoped(plane.clone(), resources.clone());
        let scope = ScopeId::from("workspace-tenant");
        let mut draft = admin_assistant_config();
        draft.id = "tenant-draft".into();
        let audit = awaken_agent_config::ManagementAuditRecord {
            tool: awaken_admin_assistant::CREATE_DRAFT_TOOL.into(),
            call_id: "hosted-draft".into(),
            summary: "not a human confirmation".into(),
        };
        let context = awaken_tool_contract::tool::ToolOperationContext {
            run_id: Some(awaken_agent_contract::agent::run::Id("run-tenant".into())),
            thread_id: None,
            operation_id: "operation-tenant".into(),
            call_id: None,
            execution_scope: Some(awaken_agent_contract::ExecutionScopeRef(scope.clone())),
        };
        awaken_tool_contract::tool::with_tool_operation_context(context, async {
            assert!(
                store.record_audit(&audit).await.is_err(),
                "H1 no audit begin"
            );
            assert!(
                store
                    .put_audited_with_resources(&draft, 0, &audit, None)
                    .await
                    .is_err(),
                "H1 no direct write"
            );
        })
        .await;
        assert!(store.record_audit(&audit).await.is_err(), "H2 no fallback");
        assert!(
            store
                .put_audited_with_resources(&draft, 0, &audit, None)
                .await
                .is_err(),
            "H2 no direct write"
        );
        assert!(plane.get(&scope, &draft.id).await.unwrap().is_none());
        assert!(
            plane
                .get_management_audit(&scope, &audit.tool, &audit.call_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            plane
                .pending_management_effects(&scope)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            resources
                .get_agent_inputs(scope.as_str(), &draft.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn seeding_uses_reserved_configuration_and_explicit_execution_workspace() {
        // A scope-keyed catalog + a resolver so the Auto assistant can publish.
        let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
        let tools = Arc::new(ScopedToolCatalog::new(
            Vec::new(),
            RESERVED_ADMIN_SCOPE,
            awaken_admin_assistant::admin_tool_descriptors(),
        ));
        let (service, executable) = test_config_service_with_catalog();
        let service = Arc::new(service);
        let plane = ConfigPlane::new(service.clone(), store, tools);

        let execution_workspace = "wrkspc_live";
        seed_admin_assistant(&plane, execution_workspace, ModelSelection::Auto)
            .await
            .expect("seed");

        // The draft/publication belongs to the reserved configuration namespace,
        // while the executable is installed only in the real resource/credential
        // Workspace. The reserved namespace never becomes a synthetic Workspace.
        assert!(
            executable
                .current(RESERVED_ADMIN_SCOPE, ADMIN_ASSISTANT_AGENT_ID)
                .is_none()
        );
        let installed = executable
            .current(execution_workspace, ADMIN_ASSISTANT_AGENT_ID)
            .expect("installed");
        let spec = &installed.snapshot.resolved_spec;
        assert_eq!(spec.model_binding.identity().1, "m-1");
        // It carries the admin tool descriptors (nameable because it published in
        // the reserved scope): capabilities/draft/patch/validate/explain + draft-environment.
        assert_eq!(spec.tool_descriptors.len(), 6);
    }

    #[tokio::test]
    async fn lifecycle_ensure_publishes_after_setup_becomes_available() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
        let tools = Arc::new(ScopedToolCatalog::new(
            Vec::new(),
            RESERVED_ADMIN_SCOPE,
            awaken_admin_assistant::admin_tool_descriptors(),
        ));
        let (service, executable) = test_config_service_with_catalog();
        let plane = ConfigPlane::new(Arc::new(service), store, tools);
        let state = AdminAssistantLifecycleState {
            plane,
            model_selection: Some(ModelSelection::Auto),
        };

        let Json(result) = ensure_admin_assistant(
            State(state),
            RequiredExecutionWorkspace("workspace-live".into()),
        )
        .await
        .expect("ensure");
        assert_eq!(result["status"], "ready");
        assert!(
            executable
                .current("workspace-live", ADMIN_ASSISTANT_AGENT_ID)
                .is_some()
        );
    }

    #[tokio::test]
    async fn lifecycle_ensure_uses_only_the_trusted_request_workspace_when_hosted() {
        // Cause/effect graph: a trusted request scope selects the exact hosted
        // execution Workspace; absent scope may use an explicit local fallback;
        // absent scope + absent fallback fails before publication. A process-local
        // coordinate can never override tenant selection.
        //
        // Decision table:
        // | Rule | request scope | local fallback | effect |
        // | H1 | tenant | local | publish only into tenant |
        // | H2 | absent | local | publish into local (local composition) |
        // | H3 | absent | absent | reject without publication |
        let make_state = || async {
            let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
            let tools = Arc::new(ScopedToolCatalog::new(
                Vec::new(),
                RESERVED_ADMIN_SCOPE,
                awaken_admin_assistant::admin_tool_descriptors(),
            ));
            let (service, executable) = test_config_service_with_catalog();
            (
                AdminAssistantLifecycleState {
                    plane: ConfigPlane::new(Arc::new(service), store, tools),
                    model_selection: Some(ModelSelection::Auto),
                },
                executable,
            )
        };

        let (state, executable) = make_state().await;
        let Json(result) = ensure_admin_assistant(
            State(state),
            RequiredExecutionWorkspace("workspace-tenant".into()),
        )
        .await
        .expect("H1 tenant publication");
        assert_eq!(result["status"], "ready", "H1");
        assert!(
            executable
                .current("workspace-tenant", ADMIN_ASSISTANT_AGENT_ID)
                .is_some(),
            "H1"
        );
        assert!(
            executable
                .current("workspace-local", ADMIN_ASSISTANT_AGENT_ID)
                .is_none(),
            "H1"
        );

        // H2: the composition's own Workspace, supplied by the router layer.
        let (state, executable) = make_state().await;
        let Json(result) = ensure_admin_assistant(
            State(state),
            RequiredExecutionWorkspace("workspace-local".into()),
        )
        .await
        .expect("H2 local publication");
        assert_eq!(result["status"], "ready", "H2");
        assert!(
            executable
                .current("workspace-local", ADMIN_ASSISTANT_AGENT_ID)
                .is_some(),
            "H2"
        );

        // H3: no edge stamp AND no composed Workspace. The router — not the
        // handler — is the only place a composition may supply one, so this
        // drives the real mount with `None` and expects the extractor's refusal.
        let (state, executable) = make_state().await;
        let response = tower::ServiceExt::oneshot(
            with_composed_workspace(
                Router::new()
                    .route(
                        "/v1/config/agents/__admin_assistant/ensure",
                        post(ensure_admin_assistant),
                    )
                    .with_state(state),
                None,
            ),
            axum::http::Request::post("/v1/config/agents/__admin_assistant/ensure")
                .body(axum::body::Body::empty())
                .expect("H3 request"),
        )
        .await
        .expect("H3 response");
        let status = response.status();
        let problem: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("H3 body"),
        )
        .expect("H3 envelope");
        assert_eq!(status, StatusCode::BAD_REQUEST, "H3");
        assert_eq!(problem["code"], "assistant_workspace_unavailable", "H3");
        assert!(
            executable
                .current("workspace-local", ADMIN_ASSISTANT_AGENT_ID)
                .is_none(),
            "H3"
        );
    }

    #[tokio::test]
    async fn lifecycle_ensure_reports_the_model_prerequisite() {
        let plane = ConfigPlane::new(
            Arc::new(test_config_service()),
            Arc::new(SqliteConfigStore::open_in_memory().await.unwrap()),
            Arc::new(StaticToolCatalog(Vec::new())),
        );
        let state = AdminAssistantLifecycleState {
            plane,
            model_selection: None,
        };

        let (status, Json(problem)) = ensure_admin_assistant(
            State(state),
            RequiredExecutionWorkspace(DEFAULT_SCOPE.into()),
        )
        .await
        .expect_err("an empty live catalog must fail closed");
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(problem["code"], "assistant_model_unavailable");
        assert!(
            problem["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty())
        );
        assert_eq!(problem["detail"], problem["message"]);
    }

    #[tokio::test]
    async fn lifecycle_ensure_recovers_from_a_stale_startup_backend() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
        let tools = Arc::new(ScopedToolCatalog::new(
            Vec::new(),
            RESERVED_ADMIN_SCOPE,
            awaken_admin_assistant::admin_tool_descriptors(),
        ));
        let (service, executable) = test_config_service_with_catalog();
        let plane = ConfigPlane::new(Arc::new(service), store, tools);
        let state = AdminAssistantLifecycleState {
            plane,
            model_selection: Some(
                ModelSelection::try_backend_default("acp:stale", Default::default())
                    .expect("exact ACP backend"),
            ),
        };

        let Json(result) = ensure_admin_assistant(
            State(state),
            RequiredExecutionWorkspace("workspace-recovered".into()),
        )
        .await
        .expect("auto fallback");
        assert_eq!(result["status"], "ready");
        assert_eq!(
            executable
                .current("workspace-recovered", ADMIN_ASSISTANT_AGENT_ID)
                .expect("installed through current provider catalog")
                .snapshot
                .resolved_spec
                .model_binding
                .identity()
                .1,
            "m-1",
        );
    }

    #[tokio::test]
    async fn capability_reader_reports_live_ids_no_secrets() {
        // The Agent aggregate is the live MCP authoring truth.
        let plane = ConfigPlane::new(
            Arc::new(test_config_service()),
            Arc::new(SqliteConfigStore::open_in_memory().await.unwrap()),
            Arc::new(StaticToolCatalog(vec![tool("read")])),
        );
        let mut support = admin_assistant_config();
        support.id = "support".into();
        support.mcp_servers = vec![
            awaken_runtime_contract::agent_bindings::AgentMcpServerBinding {
                name: "github".into(),
                transport: awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::http(
                    "https://mcp.example",
                ),
                credential: None,
                prompts_as_skills: false,
            },
        ];
        plane
            .put(&ScopeId::from(DEFAULT_SCOPE), &support)
            .await
            .unwrap();
        let reader = CatalogCapabilityReader::new(
            repo("m-1").await,
            &[tool("read")],
            &[],
            plane,
            DEFAULT_SCOPE,
            None,
        );
        let caps = reader.capabilities().await;
        // LIVE model catalog → real model id + provider key.
        assert_eq!(caps.models, vec!["m-1"]);
        assert_eq!(caps.tools, vec!["read"]);
        assert_eq!(caps.providers, vec!["anthropic"]);
        // LIVE MCP servers derived from AgentConfig.
        assert_eq!(caps.mcp_servers, vec!["github"]);
        // No inventory wired → memory stores empty.
        assert!(caps.memory_stores.is_empty());
        let json = serde_json::to_string(&caps).unwrap();
        assert!(!json.contains("key") && !json.contains("secret"));
    }

    /// The LIVE inventory reports a put memory store's id and a delivered skill's id — the
    /// two data-plane sources the `CatalogCapabilityReader` folds in when wired.
    #[tokio::test]
    async fn host_inventory_reports_put_memory_stores_and_skills() {
        // Cause/effect graph: C1 an active Memory Store and Skill belong to the
        // request Workspace; C2 an archived Memory Store is present; C3 the
        // caller selects another Workspace. Effects: E1 return the active IDs;
        // E2 omit archived IDs; E3 return no cross-Workspace IDs. Decision rules:
        // R1=C1+!C3->E1, R2=C2->E2, R3=C1+C3->E3.
        // FMECA: retaining a constructor-time Workspace beside the request scope
        // can either leak inventory across tenants or hide valid capabilities;
        // removing that redundant coordinate leaves one request-owned boundary.
        use awaken_resource_contract::{
            ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, RegisterMemoryStore,
            ResourceAdministration as _, ResourceState,
        };
        use awaken_skill_store::{
            InMemorySkillStore, SkillBundleFile, SkillDefinition, SkillStore, SkillVersion,
            bundle_sha256,
        };

        let storage = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .await
                .expect("open ephemeral Resource Registry"),
        );
        let registry = Arc::new(awaken_resource_application::RegistryApplication::new(
            storage,
            awaken_time::system_clock(),
        ));
        for (id, state) in [
            ("mem-1", ResourceState::Active),
            ("mem-gone", ResourceState::Archived),
        ] {
            registry
                .register_memory_store(RegisterMemoryStore {
                    definition: MemoryStoreDefinition {
                        id: id.into(),
                        workspace_id: DEFAULT_SCOPE.into(),
                        name: "Prefs".into(),
                        description: String::new(),
                        metadata: std::collections::BTreeMap::new(),
                        state,
                        current_config_version: ConfigVersion::INITIAL,
                        timestamps: Default::default(),
                    },
                    initial_config: MemoryStoreConfigVersion {
                        memory_store_id: id.into(),
                        version: ConfigVersion::INITIAL,
                        retention_policy: Default::default(),
                    },
                })
                .await
                .expect("register inventory fixture");
        }
        let skills = Arc::new(InMemorySkillStore::new());
        let files = vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"# greet".to_vec(),
            executable: false,
        }];
        skills
            .create(
                SkillDefinition {
                    id: "greet".into(),
                    workspace_id: DEFAULT_SCOPE.into(),
                    display_title: None,
                    latest_version: 1,
                    last_version: 1,
                    timestamps: Default::default(),
                },
                SkillVersion {
                    id: "skver-greet-1".into(),
                    skill_id: "greet".into(),
                    version: 1,
                    name: "greet".into(),
                    description: String::new(),
                    directory: "/skills/greet".into(),
                    bundle_sha256: bundle_sha256(&files),
                    files,
                    created_unix_nanos: 0,
                },
            )
            .await
            .unwrap();

        let inv = HostResourceInventory::new(registry, skills);
        assert_eq!(
            inv.memory_stores(DEFAULT_SCOPE).await,
            vec!["mem-1".to_string()],
            "R1/R2"
        );
        assert_eq!(
            inv.skills(DEFAULT_SCOPE).await,
            vec!["greet".to_string()],
            "R1"
        );
        assert!(inv.memory_stores("other-workspace").await.is_empty(), "R3");
        assert!(inv.skills("other-workspace").await.is_empty(), "R3");
    }

    /// The reader lists existing agent ids in the scope, excluding the reserved admin
    /// assistant id, from the LIVE config plane.
    #[tokio::test]
    async fn capability_reader_lists_existing_agents_excluding_the_assistant() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
        let plane = ConfigPlane::new(
            Arc::new(test_config_service()),
            store,
            Arc::new(StaticToolCatalog(vec![tool("read")])),
        );
        let scope = ScopeId::from(DEFAULT_SCOPE);
        // Persist an ordinary agent and the reserved assistant into the scope.
        let mut support = admin_assistant_config();
        support.id = "support".into();
        plane.put(&scope, &support).await.unwrap();
        plane.put(&scope, &admin_assistant_config()).await.unwrap();

        let reader = CatalogCapabilityReader::new(
            repo("m-1").await,
            &[tool("read")],
            &[],
            plane,
            DEFAULT_SCOPE,
            None,
        );
        let caps = reader.capabilities().await;
        assert_eq!(caps.agents, vec!["support"]);
        assert!(!caps.agents.contains(&ADMIN_ASSISTANT_AGENT_ID.to_string()));
    }

    #[tokio::test]
    async fn capability_reader_lists_only_its_selected_execution_workspace() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
        let plane = ConfigPlane::new(
            Arc::new(test_config_service()),
            store,
            Arc::new(StaticToolCatalog(vec![tool("read")])),
        );
        let mut default_agent = admin_assistant_config();
        default_agent.id = "default-agent".into();
        plane
            .put(&ScopeId::from(DEFAULT_SCOPE), &default_agent)
            .await
            .unwrap();
        let mut workspace_agent = admin_assistant_config();
        workspace_agent.id = "workspace-agent".into();
        plane
            .put(&ScopeId::from("workspace-local"), &workspace_agent)
            .await
            .unwrap();

        let reader = CatalogCapabilityReader::new(
            repo("m-1").await,
            &[tool("read")],
            &[],
            plane,
            "workspace-local",
            None,
        );
        let caps = reader.capabilities().await;
        assert_eq!(caps.agents, vec!["workspace-agent"]);
    }

    #[tokio::test]
    async fn draft_validator_rejects_an_unknown_tool_in_a_tenant_draft() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
        let plane = ConfigPlane::new(
            Arc::new(test_config_service()),
            store,
            Arc::new(StaticToolCatalog(vec![tool("read")])),
        );
        let validator = ConfigServiceDraftValidator::new(plane, DEFAULT_SCOPE);

        let mut good = admin_assistant_config();
        good.model_binding = ModelSelection::pinned("p", "m", "b").expect("literal coordinates");
        good.tool_ids = vec!["read".to_string()];
        assert!(validator.validate(&good).await.is_ok());

        let mut bad = good.clone();
        bad.tool_ids = vec!["ghost".to_string()];
        assert!(validator.validate(&bad).await.is_err());
    }

    // ---- CEG §10 additions -------------------------------------------------

    /// F35(b): when no provider-backed model resolves, `publish` fails and
    /// `seed_admin_assistant` surfaces that error string verbatim.
    #[tokio::test]
    async fn seed_returns_the_publish_error_when_no_model_resolves() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
        let tools = Arc::new(ScopedToolCatalog::new(
            Vec::new(),
            RESERVED_ADMIN_SCOPE,
            awaken_admin_assistant::admin_tool_descriptors(),
        ));
        // An EMPTY catalog: the Auto assistant cannot resolve a model, so publish
        // is Unresolvable and seed returns the error.
        let service = Arc::new(ConfigService::new(
            Arc::new(FirstOfferingResolver(ProviderCatalog::default())),
            Arc::new(LocalExecutableAgentRegistrar::new(Arc::new(
                ExecutableAgentCatalog::new(),
            ))),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            Vec::new(),
            |_| true,
        ));
        let plane = ConfigPlane::new(service, store, tools);
        let err = seed_admin_assistant(&plane, DEFAULT_SCOPE, ModelSelection::Auto)
            .await
            .expect_err("no provider-backed model → publish fails");
        assert!(!err.is_empty(), "the publish error is surfaced: {err}");
    }

    /// F35(c): re-seeding recompiles to the same installed content (idempotent).
    #[tokio::test]
    async fn seeding_is_idempotent_recompiling_to_the_same_content() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
        let tools = Arc::new(ScopedToolCatalog::new(
            Vec::new(),
            RESERVED_ADMIN_SCOPE,
            awaken_admin_assistant::admin_tool_descriptors(),
        ));
        let (service, executable) = test_config_service_with_catalog();
        let service = Arc::new(service);
        let plane = ConfigPlane::new(service.clone(), store, tools);

        seed_admin_assistant(&plane, DEFAULT_SCOPE, ModelSelection::Auto)
            .await
            .expect("first seed");
        let first_model = executable
            .current(DEFAULT_SCOPE, ADMIN_ASSISTANT_AGENT_ID)
            .unwrap()
            .snapshot
            .resolved_spec
            .model_binding
            .identity()
            .1
            .to_owned();
        let first_tools = executable
            .current(DEFAULT_SCOPE, ADMIN_ASSISTANT_AGENT_ID)
            .unwrap()
            .snapshot
            .resolved_spec
            .tool_descriptors
            .len();

        seed_admin_assistant(&plane, DEFAULT_SCOPE, ModelSelection::Auto)
            .await
            .expect("re-seed is idempotent");
        let handle = executable
            .current(DEFAULT_SCOPE, ADMIN_ASSISTANT_AGENT_ID)
            .unwrap();
        let second = handle.snapshot;
        assert_eq!(second.resolved_spec.model_binding.identity().1, first_model);
        assert_eq!(second.resolved_spec.tool_descriptors.len(), first_tools);
    }

    /// F36(b): the validator flattens the field-routed issue to its human message.
    #[tokio::test]
    async fn draft_validator_error_carries_the_issue_message() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
        let plane = ConfigPlane::new(
            Arc::new(test_config_service()),
            store,
            Arc::new(StaticToolCatalog(vec![tool("read")])),
        );
        let validator = ConfigServiceDraftValidator::new(plane, DEFAULT_SCOPE);
        let mut bad = admin_assistant_config();
        bad.model_binding = ModelSelection::pinned("p", "m", "b").expect("literal coordinates");
        bad.tool_ids = vec!["ghost".to_string()];
        let err = validator
            .validate(&bad)
            .await
            .expect_err("an unknown tool is rejected");
        assert!(
            !err.is_empty(),
            "carries the flattened issue message: {err}"
        );
    }
}
