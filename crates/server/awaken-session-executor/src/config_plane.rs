//! The config data plane (slice A): author, publish, and install agent configs.
//!
//! `ConfigService` is the config domain's authoring authority — it validates and
//! stores declarative [`AgentConfig`]s in a [`ConfigRegistry`], and on publish
//! compiles one into a content-addressed [`StoredPublication`] and hot-swaps it
//! into the installed catalog. The host then resolves a session's agent to its
//! installed runnable config, so a published agent runs with its own instructions,
//! tools, and plugins (ADR-0031; the config/runtime seam is the compiled snapshot).
//!
//! The runtime never edits config records; it consumes only the compiled config.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_config_resolver::ResourceStore;
use awaken_config_store::{
    AgentConfig, DEFAULT_SCOPE, RunnableConfig, ScopedConfigRegistry, StoredPublication,
    compile_with_resource_prompts,
};
use awaken_tenancy::ScopeId;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{post, put};
use axum::{Extension, Json, Router};
use serde_json::{Value, json};

use crate::tool_catalog::ToolCatalogSource;

/// The config domain service: validate, store, publish, and expose the installed
/// (published) runnable config per agent.
pub struct ConfigService {
    /// The durable, **scope-owned** config store (ADR-0051): reads filter by the
    /// request scope and writes stamp it, so a tenant can neither read nor clobber
    /// another scope's agent by id — and the reserved-scope assistant is invisible to
    /// tenants (ADR-0052 D2).
    registry: Arc<dyn ScopedConfigRegistry>,
    /// The scope-keyed tool catalog (ADR-0052 D3): `compile` binds `tool_ids` against
    /// `catalog.catalog_for(scope)`, so the admin tools are nameable only in the
    /// reserved scope and a tenant naming one fails closed with `UnknownTool`.
    tools: Arc<dyn ToolCatalogSource>,
    /// The installed catalog: agent id → compiled runnable config, hot-swapped on
    /// publish. A run resolves its agent here (awaken-next `set_registry_snapshot`).
    /// This hot cache is keyed by agent id alone (the durable store above is the
    /// tenant-isolated truth); built-in and reserved ids are globally unique, so no
    /// cross-scope collision arises in practice.
    installed: Mutex<HashMap<String, RunnableConfig>>,
    /// Per-agent resource bindings (ADR-0038). When wired, the agent's bound-resource
    /// prompt fragments are appended to its effective system prompt at compile (A3a).
    /// `None` → compilation is byte-identical to an unbound agent.
    resources: Option<Arc<dyn ResourceStore>>,
}

impl ConfigService {
    /// Assemble the config plane over a scope-owned store and a scope-keyed tool
    /// catalog. Callers that never fence by scope pass a
    /// [`StaticToolCatalog`](crate::tool_catalog::StaticToolCatalog).
    pub fn new(registry: Arc<dyn ScopedConfigRegistry>, tools: Arc<dyn ToolCatalogSource>) -> Self {
        Self {
            registry,
            tools,
            installed: Mutex::new(HashMap::new()),
            resources: None,
        }
    }

    /// Wire the per-agent resource-binding store so compiled configs carry their
    /// bound-resource prompts (ADR-0038 A3a). The same store instance is shared with
    /// the admin router's `AdminState.resources`, so an authored binding is visible here.
    #[must_use]
    pub fn with_resources(mut self, resources: Arc<dyn ResourceStore>) -> Self {
        self.resources = Some(resources);
        self
    }

    /// The agent's bound-resource prompt fragments (ADR-0038 A3a). Empty when no
    /// resource store is wired or the agent binds none, so compilation is unchanged.
    fn resource_prompts(&self, agent_id: &str) -> Vec<String> {
        self.resources
            .as_ref()
            .and_then(|store| store.get_agent_resource(agent_id))
            .map(|cfg| awaken_config_resolver::resource_prompts_for(&cfg))
            .unwrap_or_default()
    }

    /// Validate a config by compiling it against `scope`'s tool catalog (a dry run of
    /// publish); no store write. A config naming a tool absent from its scope's
    /// catalog fails closed with `UnknownTool` (ADR-0052 D3).
    pub fn validate(&self, scope: &ScopeId, config: &AgentConfig) -> Result<(), String> {
        compile_with_resource_prompts(
            config,
            &self.tools.catalog_for(scope),
            &self.resource_prompts(&config.id),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    /// Store a config draft (upsert by id) owned by `scope`.
    pub async fn put(&self, scope: &ScopeId, config: &AgentConfig) -> Result<(), String> {
        self.registry
            .put_config_scoped(scope, config)
            .await
            .map_err(|e| e.to_string())
    }

    /// Publish within `scope`: compile the stored config against the scope's tool
    /// catalog, persist the publication (idempotent by fingerprint), and install it
    /// into the live catalog so new runs use it.
    pub async fn publish(&self, scope: &ScopeId, id: &str) -> Result<StoredPublication, String> {
        let config = self
            .registry
            .get_config_scoped(scope, id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no config stored for agent `{id}`"))?;
        let runnable = compile_with_resource_prompts(
            &config,
            &self.tools.catalog_for(scope),
            &self.resource_prompts(id),
        )
        .map_err(|e| e.to_string())?;
        let publication = StoredPublication::published(runnable.clone(), id);
        self.registry
            .put_publication_scoped(scope, &publication)
            .await
            .map_err(|e| e.to_string())?;
        self.installed
            .lock()
            .unwrap()
            .insert(id.to_string(), runnable);
        Ok(publication)
    }

    /// The installed (published) runnable config for `agent`, if any.
    pub fn installed(&self, agent: &str) -> Option<RunnableConfig> {
        self.installed.lock().unwrap().get(agent).cloned()
    }
}

/// Adapts the config plane to the managed agents registry's projection port
/// ([`awaken_protocol_managed::AgentConfigSource`]): `/v1/agents` reads an agent's
/// model/system/tools from the published config truth ([`ConfigService::installed`])
/// rather than a second copy. This is the host-side half of the "retreat to
/// projection" seam — the managed adapter names only the port, never this type.
pub struct ConfigServiceAgentSource(pub Arc<ConfigService>);

impl awaken_protocol_managed::AgentConfigSource for ConfigServiceAgentSource {
    fn agent_view(&self, agent_id: &str) -> Option<awaken_protocol_managed::AgentConfigView> {
        let runnable = self.0.installed(agent_id)?;
        let spec = &runnable.snapshot().resolved_spec;
        Some(awaken_protocol_managed::AgentConfigView {
            model: Some(spec.model_binding.model_ref.clone()),
            system: (!spec.instructions.is_empty()).then(|| spec.instructions.clone()),
            tool_ids: spec.tool_descriptors.iter().map(|d| d.id.clone()).collect(),
        })
    }
}

/// The config data-plane router: `/v1/config/agents/:id` (author) plus
/// `/validate` and `/publish` (lifecycle).
pub fn config_router(service: Arc<ConfigService>) -> Router {
    Router::new()
        .route("/v1/config/agents/:id/validate", post(validate))
        .route("/v1/config/agents/:id/publish", post(publish))
        .route("/v1/config/agents/:id", put(put_config))
        .with_state(service)
}

/// The request's owner scope, stamped by the workspace-path rewrite
/// (`WorkspaceScope`) or the seeded [`DEFAULT_SCOPE`] for a flat/single-tenant call.
fn request_scope(ext: Option<Extension<awaken_protocol_managed::WorkspaceScope>>) -> ScopeId {
    ext.map(|Extension(w)| ScopeId::from(w.0))
        .unwrap_or_else(|| ScopeId::from(DEFAULT_SCOPE))
}

async fn validate(
    State(svc): State<Arc<ConfigService>>,
    scope: Option<Extension<awaken_protocol_managed::WorkspaceScope>>,
    Path(id): Path<String>,
    Json(mut config): Json<AgentConfig>,
) -> (StatusCode, Json<Value>) {
    config.id = id;
    match svc.validate(&request_scope(scope), &config) {
        Ok(()) => (StatusCode::OK, Json(json!({ "valid": true }))),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "valid": false, "error": error })),
        ),
    }
}

async fn put_config(
    State(svc): State<Arc<ConfigService>>,
    scope: Option<Extension<awaken_protocol_managed::WorkspaceScope>>,
    Path(id): Path<String>,
    Json(mut config): Json<AgentConfig>,
) -> (StatusCode, Json<Value>) {
    config.id = id.clone();
    match svc.put(&request_scope(scope), &config).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "id": id }))),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    }
}

async fn publish(
    State(svc): State<Arc<ConfigService>>,
    scope: Option<Extension<awaken_protocol_managed::WorkspaceScope>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    match svc.publish(&request_scope(scope), &id).await {
        Ok(publication) => (
            StatusCode::OK,
            Json(json!({
                "publication_id": publication.publication_id,
                "fingerprint": publication.fingerprint,
                "agent_id": publication.agent_id,
                "installed": true,
            })),
        ),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    }
}

#[cfg(test)]
mod resource_prompt_tests {
    use super::*;
    use awaken_config_resolver::{
        AgentResourceConfig, ResourceAccess, ResourceBinding, ResourceKind,
    };
    use awaken_config_store::SqliteConfigStore;
    use awaken_runtime_contract::resolved::ContextPolicy;

    fn agent_config(id: &str) -> AgentConfig {
        AgentConfig {
            id: id.to_string(),
            instructions: "be helpful".to_string(),
            max_steps: 8,
            model_binding: awaken_config_store::ModelSelection::pinned("p", "m", "b"),
            tool_ids: vec![],
            model_candidates: Vec::new(),
            plugin_ids: vec![],
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
            tool_patterns: Vec::new(),
        }
    }

    #[tokio::test]
    async fn publish_injects_bound_resource_prompts_into_the_compiled_instructions() {
        // Author a resource binding for agent-1 in the shared store (ADR-0038 A3a).
        let resources = Arc::new(awaken_config_resolver::InMemoryResourceStore::new());
        resources.put_agent_resource(AgentResourceConfig {
            agent_id: "agent-1".into(),
            resources: vec![ResourceBinding {
                kind: ResourceKind::MemoryStore,
                resource_id: "memstore-7".into(),
                mount_path: "/mnt/memory/prefs".into(),
                access: ResourceAccess::ReadWrite,
                instructions: Some("user preferences".into()),
            }],
            version: 1,
        });

        let registry = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let scope = ScopeId::from(DEFAULT_SCOPE);
        let service = ConfigService::new(
            registry,
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        )
        .with_resources(resources);

        service.put(&scope, &agent_config("agent-1")).await.unwrap();
        service.publish(&scope, "agent-1").await.unwrap();

        // The compiled (installed) config's system prompt carries the base plus the
        // bound memory store's fragment + its per-binding instructions.
        let installed = service.installed("agent-1").unwrap();
        let instructions = &installed.snapshot().resolved_spec.instructions;
        assert!(instructions.starts_with("be helpful"));
        assert!(instructions.contains("/mnt/memory/prefs"));
        assert!(instructions.contains("user preferences"));
    }

    #[tokio::test]
    async fn admin_tools_compile_only_in_the_reserved_scope() {
        use crate::tool_catalog::{RESERVED_ADMIN_SCOPE, ScopedToolCatalog};
        use awaken_runtime_contract::resolved::ToolDescriptor;

        // A catalog where `admin_x` is reserved-scope only (ADR-0052 D3).
        let admin = ToolDescriptor::pinned(
            "admin",
            "admin_x",
            "a management tool",
            serde_json::json!({"type": "object"}),
        );
        let catalog = ScopedToolCatalog::new(Vec::new(), RESERVED_ADMIN_SCOPE, vec![admin]);
        let registry = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let service = ConfigService::new(registry, Arc::new(catalog));

        // A config that names the admin tool.
        let mut cfg = agent_config("mgmt");
        cfg.tool_ids = vec!["admin_x".to_string()];

        // In the reserved scope it compiles: the descriptor is visible there.
        assert!(
            service
                .validate(&ScopeId::from(RESERVED_ADMIN_SCOPE), &cfg)
                .is_ok(),
            "admin tool must resolve in the reserved scope"
        );

        // In any tenant scope it fails closed — the admin tool is not even disclosed,
        // so the same config hits UnknownTool at compile.
        let err = service
            .validate(&ScopeId::from("wrkspc_acme"), &cfg)
            .unwrap_err();
        assert!(
            err.contains("unknown tool") && err.contains("admin_x"),
            "tenant scope must reject the admin tool: {err}"
        );
    }

    #[tokio::test]
    async fn no_resource_store_compiles_byte_identically() {
        // Without a wired resource store, instructions are the base verbatim.
        let registry = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let scope = ScopeId::from(DEFAULT_SCOPE);
        let service = ConfigService::new(
            registry,
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        service.put(&scope, &agent_config("agent-2")).await.unwrap();
        service.publish(&scope, "agent-2").await.unwrap();
        let installed = service.installed("agent-2").unwrap();
        assert_eq!(
            installed.snapshot().resolved_spec.instructions,
            "be helpful"
        );
    }
}
