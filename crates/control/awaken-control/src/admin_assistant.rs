//! Host-side wiring for the management assistant (ADR-0052): the real adapters behind
//! the admin tools' ports, plus the startup seeding that publishes the assistant as an
//! ordinary agent in the reserved scope.
//!
//! The assistant is authored, compiled, and published like any agent (D1) — the
//! seeding here is exactly a `put` + `publish` through `ConfigService`, in the reserved
//! scope (D2), where the scope-keyed catalog makes the five admin tools nameable (D3).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_admin_assistant::{
    ADMIN_ASSISTANT_AGENT_ID, CapabilityReader, DraftStore, DraftValidator, EnvironmentAuthor,
    InputSpec, PlatformCapabilities, PluginInfo, ResourceInventory, admin_assistant_config,
};
use awaken_config_resolver::{
    AgentInputBindingRepository, AgentInputConfig, BindingId, FileId, InputBinding,
    InputResourceId, McpStore, MemoryStoreId, RepositoryId, ResourceAccess,
};
use awaken_config_service::{ConfigPlane, RESERVED_ADMIN_SCOPE};
use awaken_config_store::{AgentConfig, DEFAULT_SCOPE, ManagementEffect};
use awaken_model_catalog::repo::CatalogRepo;
use awaken_runtime_contract::capability::PluginCapability;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;

/// Publish the management assistant into the reserved scope through the ordinary
/// publish path (D1/D2), via the scope edge ([`ConfigPlane`]). Idempotent —
/// re-seeding recompiles to the same content address. Returns the publish error
/// verbatim so a caller can surface a bad setup (e.g. no provider-backed model).
pub async fn seed_admin_assistant(plane: &ConfigPlane) -> Result<(), String> {
    let scope = ScopeId::from(RESERVED_ADMIN_SCOPE);
    plane.put(&scope, &admin_assistant_config()).await?;
    plane
        .publish(&scope, ADMIN_ASSISTANT_AGENT_ID)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Reads the redacted, org-shared capability snapshot (D4) LIVE: models + providers
/// from the live catalog repo, MCP servers from the live MCP store, existing agent ids
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
    /// The live authored MCP servers.
    mcp: Arc<dyn McpStore>,
    /// The config-authoring plane, used to list existing agent ids in the scope.
    plane: ConfigPlane,
    /// The scope whose agents are listed (the tenant/default scope).
    scope: ScopeId,
    /// The data-plane resource inventory (memory stores + skills). `None` when the
    /// composition root cannot cleanly reach the data-plane handles at wire time.
    inventory: Option<Arc<dyn ResourceInventory>>,
}

impl CatalogCapabilityReader {
    pub fn new(
        catalog: Arc<dyn CatalogRepo>,
        global_tools: &[ToolDescriptor],
        plugins: &[PluginCapability],
        mcp: Arc<dyn McpStore>,
        plane: ConfigPlane,
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
            mcp,
            plane,
            scope: ScopeId::from(DEFAULT_SCOPE),
            inventory,
        }
    }
}

#[async_trait]
impl CapabilityReader for CatalogCapabilityReader {
    async fn capabilities(&self) -> PlatformCapabilities {
        // LIVE model catalog: deduped offering model ids (catalog order) + provider keys.
        // A read failure degrades to empty rather than failing the tool call.
        let (models, providers) = match self.catalog.snapshot().await {
            Ok(catalog) => {
                let mut models = Vec::new();
                for offering in &catalog.offerings {
                    if !models.contains(&offering.model_id) {
                        models.push(offering.model_id.clone());
                    }
                }
                (models, catalog.providers.keys().cloned().collect())
            }
            Err(_) => (Vec::new(), Vec::new()),
        };
        // LIVE authored MCP servers (sorted ids from the store).
        let mcp_servers = self
            .mcp
            .list_servers()
            .into_iter()
            .map(|s| s.id.0)
            .collect();
        // LIVE existing agents in the scope, minus the reserved admin assistant itself.
        let agents = match self.plane.list(&self.scope).await {
            Ok(configs) => configs
                .into_iter()
                .map(|c| c.id)
                .filter(|id| id != ADMIN_ASSISTANT_AGENT_ID)
                .collect(),
            Err(_) => Vec::new(),
        };
        // Data-plane inventory (memory stores + skills) when reachable; else empty.
        let (memory_stores, skills) = match &self.inventory {
            Some(inv) => (inv.memory_stores().await, inv.skills().await),
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
            // Deployment-level vocabulary for authoring an environment (the same catalog
            // the console renders), so `admin_draft_environment` grounds its runtime +
            // schema-conformant sandbox instead of guessing.
            runtimes: awaken_config_service::runtime_catalog(),
            sandbox: Some(awaken_config_service::sandbox_capability()),
        }
    }
}

/// The [`EnvironmentAuthor`] port backed by the managed-plane [`EnvironmentState`], so
/// the assistant's `admin_draft_environment` persists through the SAME registry the
/// console's New-environment modal drives (`POST /v1/environments`).
pub struct EnvironmentStateAuthor {
    env_state: Arc<awaken_protocol_managed::EnvironmentState>,
}

impl EnvironmentStateAuthor {
    #[must_use]
    pub fn new(env_state: Arc<awaken_protocol_managed::EnvironmentState>) -> Self {
        Self { env_state }
    }
}

#[async_trait]
impl EnvironmentAuthor for EnvironmentStateAuthor {
    async fn create(&self, name: &str, config: serde_json::Value) -> Result<String, String> {
        Ok(self.env_state.author(name, config).await)
    }
}

/// The LIVE data-plane resource inventory (ADR-0038 memory stores + skills) behind the
/// [`ResourceInventory`] port, so the [`CatalogCapabilityReader`] can report real memory
/// store + skill ids without `awaken-control` depending on the runtime host. Memory-store
/// definitions come from the durable [`awaken_protocol_managed::ResourceCatalog`]
/// shared by authoring and Session resolution, and skills from the
/// [`awaken_skill_store::SkillStore`]. Carries only ids — never a secret or policy.
pub struct HostResourceInventory {
    memory: Arc<dyn awaken_protocol_managed::ResourceCatalog>,
    skills: Arc<dyn awaken_skill_store::SkillStore>,
    /// Platform-provisioned workspace used to address the skill catalog.
    skill_workspace: String,
}

impl HostResourceInventory {
    /// Build the inventory from the two live handles and an edge-provisioned
    /// workspace coordinate. The adapter never invents a tenant.
    pub fn new(
        memory: Arc<dyn awaken_protocol_managed::ResourceCatalog>,
        skills: Arc<dyn awaken_skill_store::SkillStore>,
        skill_workspace: impl Into<String>,
    ) -> Self {
        Self {
            memory,
            skills,
            skill_workspace: skill_workspace.into(),
        }
    }
}

#[async_trait]
impl ResourceInventory for HostResourceInventory {
    async fn memory_stores(&self) -> Vec<String> {
        // Active/suspended store ids from the durable catalog (sorted by id).
        self.memory
            .list_memory_stores(&self.skill_workspace)
            .into_iter()
            .map(|d| d.id)
            .collect()
    }

    async fn skills(&self) -> Vec<String> {
        // A repository read failure degrades to empty rather than failing the
        // capability call.
        self.skills
            .list_definitions(&self.skill_workspace)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|definition| definition.id)
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
    scope: ScopeId,
}

impl ConfigServiceDraftValidator {
    pub fn new(plane: ConfigPlane, scope: impl Into<ScopeId>) -> Self {
        Self {
            plane,
            scope: scope.into(),
        }
    }
}

impl DraftValidator for ConfigServiceDraftValidator {
    fn validate(&self, draft: &AgentConfig) -> Result<(), String> {
        // This consumer only needs a human message; flatten the field-routed issue.
        self.plane
            .validate(&self.scope, draft)
            .map_err(|issue| issue.message)
    }
}

/// Persists and reads back the admin assistant's drafts as **unpublished** config
/// agents, through the same [`ConfigPlane`] `put`/`get` the editor's Save + the
/// `/v1/config/agents/:id` GET use, in the tenant/default scope. `put` is the
/// unpublished store write (no `publish`), so a drafted agent appears in the console's
/// agent list awaiting the operator's publish; `get` reads that stored draft back.
pub struct ConfigServiceDraftStore {
    plane: ConfigPlane,
    scope: ScopeId,
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
        let store = Self {
            plane,
            scope: scope.into(),
            resources,
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let plane = store.plane.clone();
            let scope = store.scope.clone();
            let resources = store.resources.clone();
            handle.spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    if let Err(error) =
                        apply_pending_resource_effects(&plane, &scope, resources.as_ref()).await
                    {
                        eprintln!("resource binding reconciliation failed: {error}");
                    }
                }
            });
        }
        store
    }
}

const RESOURCE_EFFECT_KIND: &str = "agent_resource_binding";

async fn apply_pending_resource_effects(
    plane: &ConfigPlane,
    scope: &ScopeId,
    resources: &dyn AgentInputBindingRepository,
) -> Result<usize, String> {
    let effects = plane.pending_management_effects(scope).await?;
    let mut completed = 0;
    for effect in effects {
        if effect.kind != RESOURCE_EFFECT_KIND {
            continue;
        }
        let config: AgentInputConfig =
            serde_json::from_value(effect.payload).map_err(|error| error.to_string())?;
        resources
            .put_agent_inputs(scope.as_str(), config.clone())
            .map_err(|error| error.to_string())?;
        if resources
            .get_agent_inputs(scope.as_str(), &config.agent_id)
            .as_ref()
            != Some(&config)
        {
            return Err(format!(
                "resource store did not durably read back binding `{}`",
                config.agent_id
            ));
        }
        plane
            .complete_management_effect(scope, &effect.kind, &effect.key)
            .await?;
        completed += 1;
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

fn kind_str(target: &InputResourceId) -> &'static str {
    match target {
        InputResourceId::File(_) => "file",
        InputResourceId::MemoryStore(_) => "memory_store",
        InputResourceId::Repository(_) => "repository",
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

/// The per-kind default mount path when the operator omits `mount_path` — mirrors the
/// console editor's defaults so an assistant-authored binding matches a hand-authored one.
fn default_mount_path(target: &InputResourceId) -> &'static str {
    match target {
        InputResourceId::MemoryStore(_) => "/mnt/memory",
        InputResourceId::File(_) => "/mnt/files/data",
        InputResourceId::Repository(_) => "/workspace/repo",
    }
}

fn resource_config(
    agent_id: &str,
    resources: Vec<InputSpec>,
    revision: i64,
) -> Result<AgentInputConfig, String> {
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
            .unwrap_or_else(|| default_mount_path(&target).to_string());
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
        inputs: bindings,
        revision,
    })
}

#[async_trait]
impl DraftStore for ConfigServiceDraftStore {
    async fn put(&self, draft: &AgentConfig) -> Result<(), String> {
        self.plane.put(&self.scope, draft).await
    }

    async fn put_audited(
        &self,
        draft: &AgentConfig,
        audit: &awaken_admin_assistant::AdminAuditEvent,
    ) -> Result<(), String> {
        self.plane
            .put_with_audit(&self.scope, draft, audit)
            .await
            .map(|_| ())
    }

    async fn put_audited_with_resources(
        &self,
        draft: &AgentConfig,
        audit: &awaken_admin_assistant::AdminAuditEvent,
        resources: Option<Vec<InputSpec>>,
    ) -> Result<(), String> {
        let revision = self
            .resources
            .get_agent_inputs(self.scope.as_str(), &draft.id)
            .map_or(1, |current| current.revision + 1);
        let resource_config = resources
            .map(|resources| resource_config(&draft.id, resources, revision))
            .transpose()?;
        let effect = match resource_config.as_ref() {
            Some(config) => Some(ManagementEffect {
                kind: RESOURCE_EFFECT_KIND.to_string(),
                key: config.agent_id.clone(),
                payload: serde_json::to_value(config).map_err(|error| error.to_string())?,
            }),
            None => None,
        };
        self.plane
            .put_with_audit_effect(&self.scope, draft, audit, effect.as_ref())
            .await?;
        apply_pending_resource_effects(&self.plane, &self.scope, self.resources.as_ref())
            .await
            .map_err(|error| format!("resources could not be bound: {error}"))?;
        Ok(())
    }

    async fn record_audit(
        &self,
        audit: &awaken_admin_assistant::AdminAuditEvent,
    ) -> Result<(), String> {
        self.plane
            .record_management_audit(&self.scope, audit)
            .await
            .map(|_| ())
    }

    async fn get(&self, id: &str) -> Result<Option<AgentConfig>, String> {
        self.plane.get(&self.scope, id).await
    }

    async fn put_resources(&self, agent_id: &str, resources: Vec<InputSpec>) -> Result<(), String> {
        let revision = self
            .resources
            .get_agent_inputs(self.scope.as_str(), agent_id)
            .map_or(1, |current| current.revision + 1);
        self.resources
            .put_agent_inputs(
                self.scope.as_str(),
                resource_config(agent_id, resources, revision)?,
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn get_resources(&self, agent_id: &str) -> Result<Vec<InputSpec>, String> {
        let Some(cfg) = self
            .resources
            .get_agent_inputs(self.scope.as_str(), agent_id)
        else {
            return Ok(Vec::new());
        };
        Ok(cfg
            .inputs
            .into_iter()
            .map(|b| InputSpec {
                kind: kind_str(&b.target).to_string(),
                resource_id: b.target.id().to_string(),
                mount_path: Some(b.mount_path),
                access: Some(match b.access {
                    ResourceAccess::ReadOnly => "read_only".to_string(),
                    ResourceAccess::ReadWrite => "read_write".to_string(),
                }),
                instructions: b.instructions,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_config_resolver::{
        InMemoryAgentInputBindingRepository, InMemoryMcpStore, McpServerDef, McpServerId,
    };
    use awaken_config_service::{
        ConfigPlane, ConfigService, ModelResolver, ResolvedModel, ScopedToolCatalog,
        StaticToolCatalog,
    };
    use awaken_config_store::{DEFAULT_SCOPE, ModelSelection, SqliteConfigStore};
    use awaken_credential_vault::CredentialBinding;
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
        })
        .await
        .unwrap();
        Arc::new(repo)
    }

    fn tool(id: &str) -> ToolDescriptor {
        ToolDescriptor::pinned("t", id, "d", serde_json::json!({"type": "object"}))
    }

    /// A minimal `ModelResolver` for the test: resolves `Auto` to the catalog's
    /// first offering (the data-plane's `CatalogModelResolver` lives in the server
    /// crate; this crate needs only a stub to publish the seeded assistant).
    struct FirstOfferingResolver(ProviderCatalog);

    impl ModelResolver for FirstOfferingResolver {
        fn resolve_auto(&self) -> Result<ResolvedModel, String> {
            let mut offerings = self.0.offerings.iter();
            let primary = offerings
                .next()
                .ok_or_else(|| "no provider-backed model in the catalog".to_string())?;
            Ok(ResolvedModel {
                primary: ModelBinding::new("default", primary.model_id.clone(), "default"),
                candidates: offerings
                    .map(|o| ModelBinding::new("default", o.model_id.clone(), "default"))
                    .collect(),
            })
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
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn draft_resource_bindings_keep_equal_agent_ids_in_their_workspace() {
        let plane = ConfigPlane::new(
            Arc::new(ConfigService::new()),
            Arc::new(SqliteConfigStore::open_in_memory().unwrap()),
            Arc::new(StaticToolCatalog(Vec::new())),
        );
        let resources = Arc::new(InMemoryAgentInputBindingRepository::new());
        let workspace_a =
            ConfigServiceDraftStore::new(plane.clone(), "workspace-a", resources.clone());
        let workspace_b = ConfigServiceDraftStore::new(plane, "workspace-b", resources);
        let binding = |resource_id: &str| InputSpec {
            kind: "file".into(),
            resource_id: resource_id.into(),
            mount_path: None,
            access: Some("read_only".into()),
            instructions: None,
        };

        workspace_a
            .put_resources("shared-agent", vec![binding("file-a")])
            .await
            .unwrap();
        workspace_b
            .put_resources("shared-agent", vec![binding("file-b")])
            .await
            .unwrap();

        assert_eq!(
            workspace_a.get_resources("shared-agent").await.unwrap()[0].resource_id,
            "file-a"
        );
        assert_eq!(
            workspace_b.get_resources("shared-agent").await.unwrap()[0].resource_id,
            "file-b"
        );
    }

    #[tokio::test]
    async fn seeding_publishes_the_assistant_into_the_reserved_scope_only() {
        // A scope-keyed catalog + a resolver so the Auto assistant can publish.
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let tools = Arc::new(ScopedToolCatalog::new(
            Vec::new(),
            RESERVED_ADMIN_SCOPE,
            awaken_admin_assistant::admin_tool_descriptors(),
        ));
        let service = Arc::new(
            ConfigService::new()
                .with_model_resolver(Arc::new(FirstOfferingResolver(catalog("m-1")))),
        );
        let plane = ConfigPlane::new(service.clone(), store, tools);

        seed_admin_assistant(&plane).await.expect("seed");

        // Published + installed under the reserved id, model auto-resolved.
        let installed = service
            .installed(ADMIN_ASSISTANT_AGENT_ID)
            .expect("installed");
        let spec = &installed.resolved_spec;
        assert_eq!(spec.model_binding.model_ref, "m-1");
        // It carries the admin tool descriptors (nameable because it published in
        // the reserved scope): capabilities/draft/patch/validate/explain + draft-environment.
        assert_eq!(spec.tool_descriptors.len(), 6);
    }

    #[tokio::test]
    async fn capability_reader_reports_live_ids_no_secrets() {
        // A live MCP store with one authored server.
        let mcp = Arc::new(InMemoryMcpStore::new());
        mcp.put_server(McpServerDef {
            workspace_id: DEFAULT_SCOPE.into(),
            id: McpServerId("github".into()),
            display_name: "GitHub".into(),
            url: "https://mcp.example".into(),
            credential_binding: CredentialBinding::None,
            version: 1,
        });
        // A config plane over an empty store → no existing agents.
        let plane = ConfigPlane::new(
            Arc::new(ConfigService::new()),
            Arc::new(SqliteConfigStore::open_in_memory().unwrap()),
            Arc::new(StaticToolCatalog(vec![tool("read")])),
        );
        let reader =
            CatalogCapabilityReader::new(repo("m-1").await, &[tool("read")], &[], mcp, plane, None);
        let caps = reader.capabilities().await;
        // LIVE model catalog → real model id + provider key.
        assert_eq!(caps.models, vec!["m-1"]);
        assert_eq!(caps.tools, vec!["read"]);
        assert_eq!(caps.providers, vec!["anthropic"]);
        // LIVE MCP servers.
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
        use awaken_config_resolver::InMemoryResourceCatalog;
        use awaken_protocol_managed::resource_plane::{
            ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, ResourceCatalog,
            ResourceState,
        };
        use awaken_skill_store::{
            InMemorySkillStore, SkillBundleFile, SkillDefinition, SkillStore, SkillVersion,
            bundle_sha256,
        };

        let registry = Arc::new(InMemoryResourceCatalog::new());
        for (id, state) in [
            ("mem-1", ResourceState::Active),
            ("mem-gone", ResourceState::Archived),
        ] {
            registry
                .create_memory_store(
                    MemoryStoreDefinition {
                        id: id.into(),
                        workspace_id: DEFAULT_SCOPE.into(),
                        name: "Prefs".into(),
                        description: String::new(),
                        metadata: std::collections::BTreeMap::new(),
                        state,
                        current_config_version: ConfigVersion::INITIAL,
                    },
                    MemoryStoreConfigVersion {
                        memory_store_id: id.into(),
                        version: ConfigVersion::INITIAL,
                        recall_policy: Default::default(),
                        extraction_policy: Default::default(),
                        retention_policy: Default::default(),
                    },
                )
                .unwrap();
        }
        let skills = Arc::new(InMemorySkillStore::new());
        let files = vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"# greet".to_vec(),
        }];
        skills
            .create(
                SkillDefinition {
                    id: "greet".into(),
                    workspace_id: DEFAULT_SCOPE.into(),
                    display_title: None,
                    latest_version: 1,
                    last_version: 1,
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
                },
            )
            .await
            .unwrap();

        let inv = HostResourceInventory::new(registry, skills, DEFAULT_SCOPE);
        assert_eq!(inv.memory_stores().await, vec!["mem-1".to_string()]);
        assert_eq!(inv.skills().await, vec!["greet".to_string()]);
    }

    /// The reader lists existing agent ids in the scope, excluding the reserved admin
    /// assistant id, from the LIVE config plane.
    #[tokio::test]
    async fn capability_reader_lists_existing_agents_excluding_the_assistant() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let plane = ConfigPlane::new(
            Arc::new(ConfigService::new()),
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
            Arc::new(InMemoryMcpStore::new()),
            plane,
            None,
        );
        let caps = reader.capabilities().await;
        assert_eq!(caps.agents, vec!["support"]);
        assert!(!caps.agents.contains(&ADMIN_ASSISTANT_AGENT_ID.to_string()));
    }

    #[tokio::test]
    async fn draft_validator_rejects_an_unknown_tool_in_a_tenant_draft() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let plane = ConfigPlane::new(
            Arc::new(ConfigService::new()),
            store,
            Arc::new(StaticToolCatalog(vec![tool("read")])),
        );
        let validator = ConfigServiceDraftValidator::new(plane, DEFAULT_SCOPE);

        let mut good = admin_assistant_config();
        good.model_binding = ModelSelection::pinned("p", "m", "b");
        good.tool_ids = vec!["read".to_string()];
        assert!(validator.validate(&good).is_ok());

        let mut bad = good.clone();
        bad.tool_ids = vec!["ghost".to_string()];
        assert!(validator.validate(&bad).is_err());
    }

    // ---- CEG §10 additions -------------------------------------------------

    /// F35(b): when no provider-backed model resolves, `publish` fails and
    /// `seed_admin_assistant` surfaces that error string verbatim.
    #[tokio::test]
    async fn seed_returns_the_publish_error_when_no_model_resolves() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let tools = Arc::new(ScopedToolCatalog::new(
            Vec::new(),
            RESERVED_ADMIN_SCOPE,
            awaken_admin_assistant::admin_tool_descriptors(),
        ));
        // An EMPTY catalog: the Auto assistant cannot resolve a model, so publish
        // is Unresolvable and seed returns the error.
        let service = Arc::new(
            ConfigService::new()
                .with_model_resolver(Arc::new(FirstOfferingResolver(ProviderCatalog::default()))),
        );
        let plane = ConfigPlane::new(service, store, tools);
        let err = seed_admin_assistant(&plane)
            .await
            .expect_err("no provider-backed model → publish fails");
        assert!(!err.is_empty(), "the publish error is surfaced: {err}");
    }

    /// F35(c): re-seeding recompiles to the same installed content (idempotent).
    #[tokio::test]
    async fn seeding_is_idempotent_recompiling_to_the_same_content() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let tools = Arc::new(ScopedToolCatalog::new(
            Vec::new(),
            RESERVED_ADMIN_SCOPE,
            awaken_admin_assistant::admin_tool_descriptors(),
        ));
        let service = Arc::new(
            ConfigService::new()
                .with_model_resolver(Arc::new(FirstOfferingResolver(catalog("m-1")))),
        );
        let plane = ConfigPlane::new(service.clone(), store, tools);

        seed_admin_assistant(&plane).await.expect("first seed");
        let first_model = service
            .installed(ADMIN_ASSISTANT_AGENT_ID)
            .unwrap()
            .resolved_spec
            .model_binding
            .model_ref
            .clone();
        let first_tools = service
            .installed(ADMIN_ASSISTANT_AGENT_ID)
            .unwrap()
            .resolved_spec
            .tool_descriptors
            .len();

        seed_admin_assistant(&plane)
            .await
            .expect("re-seed is idempotent");
        let handle = service.installed(ADMIN_ASSISTANT_AGENT_ID).unwrap();
        let second = handle;
        assert_eq!(second.resolved_spec.model_binding.model_ref, first_model);
        assert_eq!(second.resolved_spec.tool_descriptors.len(), first_tools);
    }

    /// F36(b): the validator flattens the field-routed issue to its human message.
    #[tokio::test]
    async fn draft_validator_error_carries_the_issue_message() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let plane = ConfigPlane::new(
            Arc::new(ConfigService::new()),
            store,
            Arc::new(StaticToolCatalog(vec![tool("read")])),
        );
        let validator = ConfigServiceDraftValidator::new(plane, DEFAULT_SCOPE);
        let mut bad = admin_assistant_config();
        bad.model_binding = ModelSelection::pinned("p", "m", "b");
        bad.tool_ids = vec!["ghost".to_string()];
        let err = validator
            .validate(&bad)
            .expect_err("an unknown tool is rejected");
        assert!(
            !err.is_empty(),
            "carries the flattened issue message: {err}"
        );
    }
}
