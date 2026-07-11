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
use awaken_config_store::ModelSelection;
use awaken_config_store::{
    AgentConfig, ConfigRegistry, DEFAULT_SCOPE, RunnableConfig, ScopedConfig, ScopedConfigRegistry,
    StoredPublication, compile_with_resource_prompts,
};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{post, put};
use axum::{Extension, Json, Router};
use serde_json::{Value, json};

use crate::binding_resolver::ModelResolver;
use crate::tool_catalog::ToolCatalogSource;

/// A publish failure, split so the edge can map it to an HTTP status (ADR-0052 D5:
/// an unresolvable auto binding is a 409, not a generic 400).
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("no config stored for agent `{0}`")]
    NotStored(String),
    /// The config's `Auto` model could not be resolved — no provider-backed model in
    /// the catalog. Surfaced as 409 ("configure and publish a model first").
    #[error("cannot resolve an auto model binding: {0}")]
    Unresolvable(String),
    #[error("{0}")]
    Compile(String),
    #[error("{0}")]
    Store(String),
}

/// The config domain service: validate, store, publish, and expose the installed
/// (published) runnable config per agent.
///
/// **Scope-free by design (ADR-0051/0052).** Tenancy is an edge aspect: this service
/// never names a `ScopeId`. The already-scoped collaborators — a scope-bound
/// [`ConfigRegistry`] (via `ScopedConfig`) and the scope's resolved tool catalog
/// (`&[ToolDescriptor]`) — are passed in per call by the edge ([`ConfigPlane`] and
/// the router handlers). The service holds only scope-agnostic state: the installed
/// hot catalog (by agent id), the resource-prompt store, and the model resolver.
#[derive(Default)]
pub struct ConfigService {
    /// The installed catalog: agent id → compiled runnable config, hot-swapped on
    /// publish. A run resolves its agent here (awaken-next `set_registry_snapshot`).
    /// Keyed by agent id alone (the durable, scope-owned store is the tenant-isolated
    /// truth); built-in and reserved ids are globally unique, so no cross-scope
    /// collision arises in practice.
    installed: Mutex<HashMap<String, RunnableConfig>>,
    /// Per-agent resource bindings (ADR-0038). When wired, the agent's bound-resource
    /// prompt fragments are appended to its effective system prompt at compile (A3a).
    /// `None` → compilation is byte-identical to an unbound agent.
    resources: Option<Arc<dyn ResourceStore>>,
    /// Resolves an `Auto` model selection to a concrete binding at publish (ADR-0052
    /// D5). `None` → an `Auto` config cannot publish (fail-closed); a `Pinned` config
    /// is unaffected.
    model_resolver: Option<Arc<dyn ModelResolver>>,
}

impl ConfigService {
    /// A scope-free config service. Wire the model resolver and resource store with
    /// the chainable builders; the scoped registry + tool catalog are supplied per
    /// call by the edge, never held here.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire the model resolver so an `Auto`-bound config resolves to a first-offering
    /// at publish (ADR-0052 D5).
    #[must_use]
    pub fn with_model_resolver(mut self, resolver: Arc<dyn ModelResolver>) -> Self {
        self.model_resolver = Some(resolver);
        self
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

    /// Validate a config by compiling it against the caller-supplied tool `catalog`
    /// (a dry run of publish); no store write. Mirrors publish: an `Auto` model is
    /// resolved first (D5) so a draft with the default binding validates, and a config
    /// naming a tool absent from that catalog fails closed with `UnknownTool` (D3 —
    /// the edge resolves the catalog for the request scope).
    pub fn validate(&self, config: &AgentConfig, catalog: &[ToolDescriptor]) -> Result<(), String> {
        let compile_input = self
            .resolve_for_compile(config.clone())
            .map_err(|e| e.to_string())?;
        compile_with_resource_prompts(&compile_input, catalog, &self.resource_prompts(&config.id))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Store a config draft (upsert by id) in the caller-supplied scope-bound
    /// `registry` (a `ScopedConfig` the edge bound to the request scope).
    pub async fn put(
        &self,
        registry: &dyn ConfigRegistry,
        config: &AgentConfig,
    ) -> Result<(), String> {
        registry.put_config(config).await.map_err(|e| e.to_string())
    }

    /// Publish: resolve an `Auto` model to a concrete binding (D5), compile the stored
    /// config against the caller-supplied `catalog`, persist the publication
    /// (idempotent by fingerprint) into the scope-bound `registry`, and install it into
    /// the live catalog so new runs use it. The stored source config is left untouched —
    /// its `Auto` selection persists so the reconciler can re-resolve it later.
    pub async fn publish(
        &self,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
    ) -> Result<StoredPublication, PublishError> {
        let config = registry
            .get_config(id)
            .await
            .map_err(|e| PublishError::Store(e.to_string()))?
            .ok_or_else(|| PublishError::NotStored(id.to_string()))?;

        // Resolve the model *before* compile (compile requires a concrete binding).
        // `Auto` → first-offering via the resolver; `Pinned` → the authored binding
        // and its authored candidates.
        let compile_input = self.resolve_for_compile(config)?;

        let runnable =
            compile_with_resource_prompts(&compile_input, catalog, &self.resource_prompts(id))
                .map_err(|e| PublishError::Compile(e.to_string()))?;
        let publication = StoredPublication::published(runnable.clone(), id);
        registry
            .put_publication(&publication)
            .await
            .map_err(|e| PublishError::Store(e.to_string()))?;
        self.installed
            .lock()
            .unwrap()
            .insert(id.to_string(), runnable);
        Ok(publication)
    }

    /// Produce the concrete config `compile` consumes: an `Auto` selection is
    /// resolved to a first-offering (D5), a `Pinned` selection is used as authored.
    /// Only the *returned* config is concrete; the stored source keeps its `Auto`.
    fn resolve_for_compile(&self, mut config: AgentConfig) -> Result<AgentConfig, PublishError> {
        if crate::binding_resolver::needs_resolution(&config.model_binding) {
            let resolver = self
                .model_resolver
                .as_ref()
                .ok_or_else(|| PublishError::Unresolvable("no model resolver wired".into()))?;
            let resolved = resolver
                .resolve_auto()
                .map_err(PublishError::Unresolvable)?;
            config.model_binding = ModelSelection::Pinned(resolved.primary);
            config.model_candidates = resolved.candidates;
        }
        Ok(config)
    }

    /// Re-resolve and re-publish an `Auto`-bound agent (ADR-0052 D5), reading and
    /// writing through the caller-supplied scope-bound `registry`. Returns `true` if it
    /// re-published (a `Pinned` agent is skipped; a missing one is skipped). Idempotent
    /// by content address, so a retry after a catalog change is safe. This is what
    /// [`ConfigServiceReconciler`](crate::binding_resolver::ConfigServiceReconciler)
    /// drives from the catalog write path.
    pub async fn reconcile(
        &self,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
    ) -> Result<bool, String> {
        let stored = registry.get_config(id).await.map_err(|e| e.to_string())?;
        match stored {
            // Only auto bindings are re-resolved; an operator pin is authoritative.
            Some(config) if config.model_binding.is_auto() => {
                self.publish(registry, id, catalog)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// The installed (published) runnable config for `agent`, if any.
    pub fn installed(&self, agent: &str) -> Option<RunnableConfig> {
        self.installed.lock().unwrap().get(agent).cloned()
    }
}

/// The scope-aware **edge** over the scope-free [`ConfigService`]: it holds the
/// durable scope-owned store and the scope-keyed tool catalog, and for a given
/// request scope binds a [`ScopedConfig`] registry + resolves the scope's tool
/// catalog, then delegates to the service. This is where — and the only place where —
/// the config plane names a [`ScopeId`] (ADR-0051: tenancy is an edge aspect).
#[derive(Clone)]
pub struct ConfigPlane {
    service: Arc<ConfigService>,
    store: Arc<dyn ScopedConfigRegistry>,
    tools: Arc<dyn ToolCatalogSource>,
}

impl ConfigPlane {
    pub fn new(
        service: Arc<ConfigService>,
        store: Arc<dyn ScopedConfigRegistry>,
        tools: Arc<dyn ToolCatalogSource>,
    ) -> Self {
        Self {
            service,
            store,
            tools,
        }
    }

    /// The scope's tool catalog (D3): the descriptors a config in `scope` may name.
    pub fn catalog_for(&self, scope: &ScopeId) -> Vec<ToolDescriptor> {
        self.tools.catalog_for(scope)
    }

    /// A scope-bound registry (a `ScopedConfig` decorator, ADR-0051): every read
    /// filters by `scope` and every write stamps it, so the service stays scope-free.
    pub fn registry_for(&self, scope: &ScopeId) -> ScopedConfig<dyn ScopedConfigRegistry> {
        ScopedConfig::new(self.store.clone(), scope.clone())
    }

    /// Validate a config in `scope` (compile dry-run against the scope's catalog).
    pub fn validate(&self, scope: &ScopeId, config: &AgentConfig) -> Result<(), String> {
        self.service.validate(config, &self.catalog_for(scope))
    }

    /// Store a config draft owned by `scope`.
    pub async fn put(&self, scope: &ScopeId, config: &AgentConfig) -> Result<(), String> {
        self.service.put(&self.registry_for(scope), config).await
    }

    /// Publish a stored config in `scope`.
    pub async fn publish(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<StoredPublication, PublishError> {
        self.service
            .publish(&self.registry_for(scope), id, &self.catalog_for(scope))
            .await
    }

    /// Re-resolve and re-publish an `Auto`-bound agent in `scope` (ADR-0052 D5).
    pub async fn reconcile(&self, scope: &ScopeId, id: &str) -> Result<bool, String> {
        self.service
            .reconcile(&self.registry_for(scope), id, &self.catalog_for(scope))
            .await
    }

    /// The scope-free service (for the installed-projection reads).
    #[must_use]
    pub fn service(&self) -> &Arc<ConfigService> {
        &self.service
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
/// `/validate` and `/publish` (lifecycle). The [`ConfigPlane`] state is the scope
/// edge — it binds the request scope onto the scope-free service.
pub fn config_router(plane: ConfigPlane) -> Router {
    Router::new()
        .route("/v1/config/agents/:id/validate", post(validate))
        .route("/v1/config/agents/:id/publish", post(publish))
        .route("/v1/config/agents/:id", put(put_config))
        .with_state(plane)
}

/// The request's owner scope, stamped by the workspace-path rewrite
/// (`WorkspaceScope`) or the seeded [`DEFAULT_SCOPE`] for a flat/single-tenant call.
fn request_scope(ext: Option<Extension<awaken_protocol_managed::WorkspaceScope>>) -> ScopeId {
    ext.map(|Extension(w)| ScopeId::from(w.0))
        .unwrap_or_else(|| ScopeId::from(DEFAULT_SCOPE))
}

async fn validate(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_protocol_managed::WorkspaceScope>>,
    Path(id): Path<String>,
    Json(mut config): Json<AgentConfig>,
) -> (StatusCode, Json<Value>) {
    config.id = id;
    match plane.validate(&request_scope(scope), &config) {
        Ok(()) => (StatusCode::OK, Json(json!({ "valid": true }))),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "valid": false, "error": error })),
        ),
    }
}

async fn put_config(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_protocol_managed::WorkspaceScope>>,
    Path(id): Path<String>,
    Json(mut config): Json<AgentConfig>,
) -> (StatusCode, Json<Value>) {
    config.id = id.clone();
    match plane.put(&request_scope(scope), &config).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "id": id }))),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    }
}

async fn publish(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_protocol_managed::WorkspaceScope>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    match plane.publish(&request_scope(scope), &id).await {
        Ok(publication) => (
            StatusCode::OK,
            Json(json!({
                "publication_id": publication.publication_id,
                "fingerprint": publication.fingerprint,
                "agent_id": publication.agent_id,
                "installed": true,
            })),
        ),
        // An unresolvable auto model is a 409 ("configure a model first"), ADR-0052 D5;
        // every other publish failure stays a 400.
        Err(err @ PublishError::Unresolvable(_)) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": err.to_string() })),
        ),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": error.to_string() })),
        ),
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

    use crate::binding_resolver::{ModelResolver, ResolvedModel};
    use awaken_runtime_contract::resolved::ModelBinding;

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

    fn auto_config(id: &str) -> AgentConfig {
        let mut cfg = agent_config(id);
        cfg.model_binding = ModelSelection::Auto;
        cfg
    }

    struct FakeResolver;
    impl ModelResolver for FakeResolver {
        fn resolve_auto(&self) -> Result<ResolvedModel, String> {
            Ok(ResolvedModel {
                primary: ModelBinding::new("openai", "m-first", "genai"),
                candidates: vec![ModelBinding::new("openai", "m-second", "genai")],
            })
        }
    }

    /// A config plane (the scope edge) over a fresh in-memory store, an optional
    /// resolver, an optional resource store, and the given tool catalog.
    fn plane_with(
        tools: Arc<dyn ToolCatalogSource>,
        resolver: Option<Arc<dyn ModelResolver>>,
        resources: Option<Arc<dyn ResourceStore>>,
    ) -> ConfigPlane {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let mut service = ConfigService::new();
        if let Some(resolver) = resolver {
            service = service.with_model_resolver(resolver);
        }
        if let Some(resources) = resources {
            service = service.with_resources(resources);
        }
        ConfigPlane::new(Arc::new(service), store, tools)
    }

    fn static_plane(resolver: Option<Arc<dyn ModelResolver>>) -> ConfigPlane {
        plane_with(
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
            resolver,
            None,
        )
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

        let scope = ScopeId::from(DEFAULT_SCOPE);
        let plane = plane_with(
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
            None,
            Some(resources),
        );
        plane.put(&scope, &agent_config("agent-1")).await.unwrap();
        plane.publish(&scope, "agent-1").await.unwrap();

        // The compiled (installed) config's system prompt carries the base plus the
        // bound memory store's fragment + its per-binding instructions.
        let installed = plane.service().installed("agent-1").unwrap();
        let instructions = &installed.snapshot().resolved_spec.instructions;
        assert!(instructions.starts_with("be helpful"));
        assert!(instructions.contains("/mnt/memory/prefs"));
        assert!(instructions.contains("user preferences"));
    }

    #[tokio::test]
    async fn publish_resolves_auto_to_the_first_offering_and_keeps_the_source_auto() {
        let plane = static_plane(Some(Arc::new(FakeResolver)));
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &auto_config("mgmt")).await.unwrap();
        plane.publish(&scope, "mgmt").await.unwrap();

        // The compiled (installed) config carries the resolved concrete binding +
        // the remaining offerings as pool candidates (ADR-0052 D5).
        let installed = plane.service().installed("mgmt").unwrap();
        let spec = &installed.snapshot().resolved_spec;
        assert_eq!(spec.model_binding.model_ref, "m-first");
        assert_eq!(spec.model_candidates.len(), 1);
        assert_eq!(spec.model_candidates[0].model_ref, "m-second");

        // The stored *source* config is still Auto — so a later catalog change can
        // re-resolve it (reconcile returns true only for an Auto source).
        assert!(plane.reconcile(&scope, "mgmt").await.unwrap());
    }

    #[tokio::test]
    async fn auto_without_a_resolver_is_a_conflict() {
        let plane = static_plane(None);
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &auto_config("mgmt")).await.unwrap();
        let err = plane.publish(&scope, "mgmt").await.unwrap_err();
        assert!(matches!(err, PublishError::Unresolvable(_)));
    }

    #[tokio::test]
    async fn reconcile_re_publishes_auto_but_skips_pinned() {
        let plane = static_plane(Some(Arc::new(FakeResolver)));
        let scope = ScopeId::from(DEFAULT_SCOPE);

        // A pinned agent: reconcile is a no-op (operator pin is authoritative).
        plane.put(&scope, &agent_config("pinned")).await.unwrap();
        plane.publish(&scope, "pinned").await.unwrap();
        assert!(!plane.reconcile(&scope, "pinned").await.unwrap());

        // An auto agent: reconcile re-publishes (idempotent by content address).
        plane.put(&scope, &auto_config("auto")).await.unwrap();
        plane.publish(&scope, "auto").await.unwrap();
        assert!(plane.reconcile(&scope, "auto").await.unwrap());

        // A missing agent: skipped, not an error.
        assert!(!plane.reconcile(&scope, "ghost").await.unwrap());
    }

    #[tokio::test]
    async fn reconciler_adapter_republishes_the_named_auto_agents() {
        use crate::binding_resolver::{AssistantBindingReconciler, ConfigServiceReconciler};

        let plane = static_plane(Some(Arc::new(FakeResolver)));
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &auto_config("assistant")).await.unwrap();
        plane.publish(&scope, "assistant").await.unwrap();
        plane.put(&scope, &agent_config("pinned")).await.unwrap();
        plane.publish(&scope, "pinned").await.unwrap();

        // The catalog-write path drives one seam over a fixed id set; only the auto
        // one is re-published.
        let reconciler = ConfigServiceReconciler::new(
            plane.clone(),
            DEFAULT_SCOPE,
            vec!["assistant".to_string(), "pinned".to_string()],
        );
        assert_eq!(reconciler.reconcile().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn pinned_publishes_without_a_resolver() {
        let plane = static_plane(None);
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &agent_config("pinned")).await.unwrap();
        plane.publish(&scope, "pinned").await.unwrap();
        let installed = plane.service().installed("pinned").unwrap();
        assert_eq!(
            installed.snapshot().resolved_spec.model_binding.model_ref,
            "m"
        );
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
        let plane = plane_with(Arc::new(catalog), None, None);

        // A config that names the admin tool.
        let mut cfg = agent_config("mgmt");
        cfg.tool_ids = vec!["admin_x".to_string()];

        // In the reserved scope it compiles: the descriptor is visible there.
        assert!(
            plane
                .validate(&ScopeId::from(RESERVED_ADMIN_SCOPE), &cfg)
                .is_ok(),
            "admin tool must resolve in the reserved scope"
        );

        // In any tenant scope it fails closed — the admin tool is not even disclosed,
        // so the same config hits UnknownTool at compile.
        let err = plane
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
        let scope = ScopeId::from(DEFAULT_SCOPE);
        let plane = static_plane(None);
        plane.put(&scope, &agent_config("agent-2")).await.unwrap();
        plane.publish(&scope, "agent-2").await.unwrap();
        let installed = plane.service().installed("agent-2").unwrap();
        assert_eq!(
            installed.snapshot().resolved_spec.instructions,
            "be helpful"
        );
    }
}
