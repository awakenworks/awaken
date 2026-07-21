//! The config data plane (slice A): author, publish, and install agent configs.
//!
//! `ConfigService` is the config domain's authoring authority — it validates and
//! stores declarative [`AgentConfig`]s in a [`ConfigRegistry`], and on publish
//! compiles one into a content-addressed [`StoredPublication`] and hot-swaps it
//! into the installed catalog. The host then resolves a session's agent to its
//! installed executable snapshot, so a published agent runs with its own instructions,
//! tools, and plugins (ADR-0031; the config/runtime seam is the compiled snapshot).
//!
//! The runtime never edits config records; it consumes only the compiled config.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_config_resolver::{AgentInputBindingRepository, InferenceAccessPublisher};
use awaken_config_store::{
    AgentConfig, AgentConfigRevision, AuditedConfigWrite, ConfigRegistry, ConfigWrite,
    DEFAULT_SCOPE, ExecutableAgentSnapshot, ManagementAuditEntry, ManagementAuditRecord,
    ManagementEffect, ModelSelection, ScopedConfig, ScopedConfigRegistry, StoredPublication,
    ToolOverride,
};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{Value, json};

use crate::binding_resolver::ModelResolver;
use crate::compaction::apply_compaction;
use crate::publication::{PublishError, ResolvedAgentConfig, ValidationIssue, snapshot_metadata};
use crate::tool_catalog::ToolCatalogSource;

/// The config domain service: validate, store, publish, and expose the installed
/// published executable snapshot per agent.
///
/// **Scope-free by design (ADR-0051/0052).** Tenancy is an edge aspect: this service
/// never names a `ScopeId`. The already-scoped collaborators — a scope-bound
/// [`ConfigRegistry`] (via `ScopedConfig`) and the scope's resolved tool catalog
/// (`&[ToolDescriptor]`) — are passed in per call by the edge ([`ConfigPlane`] and
/// the router handlers). The service holds only scope-agnostic state: the installed
/// hot catalog (by agent id), the resource-prompt store, and the model resolver.
#[derive(Clone)]
struct InstalledEntry {
    source_revision: u64,
    snapshot: ExecutableAgentSnapshot,
}

#[derive(Default)]
pub struct ConfigService {
    /// The installed catalog: agent id → executable snapshot, hot-swapped on
    /// publish. A run resolves its agent here (awaken-next `set_registry_snapshot`).
    /// Keyed by agent id alone (the durable, scope-owned store is the tenant-isolated
    /// truth); built-in and reserved ids are globally unique, so no cross-scope
    /// collision arises in practice.
    installed: Mutex<HashMap<String, InstalledEntry>>,
    /// Per-agent resource bindings (ADR-0038). When wired, the agent's bound-resource
    /// prompt fragments are appended to its effective system prompt at compile (A3a).
    /// `None` → compilation is byte-identical to an unbound agent.
    pub(crate) resources: Option<Arc<dyn AgentInputBindingRepository>>,
    /// Resolves an `Auto` model selection to a concrete binding at publish (ADR-0052
    /// D5). `None` → an `Auto` config cannot publish (fail-closed); a `Pinned` config
    /// is unaffected.
    model_resolver: Option<Arc<dyn ModelResolver>>,
    /// Resolves provider access once at publication. Kept as an inward port so the
    /// config domain does not depend on vault/catalog adapters.
    inference_access_publisher: Option<Arc<dyn InferenceAccessPublisher>>,
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

    #[must_use]
    pub fn with_inference_access_publisher(
        mut self,
        publisher: Arc<dyn InferenceAccessPublisher>,
    ) -> Self {
        self.inference_access_publisher = Some(publisher);
        self
    }

    /// Wire the per-Agent input binding repository used by Session projections.
    /// Resource inputs are deliberately not compiled into the Agent snapshot: the
    /// Session resolver composes current defaults with temporary attachments once.
    #[must_use]
    pub fn with_resources(mut self, resources: Arc<dyn AgentInputBindingRepository>) -> Self {
        self.resources = Some(resources);
        self
    }

    /// Validate a config by compiling it against the caller-supplied tool `catalog`
    /// (a dry run of publish); no store write. Mirrors publish: an `Auto` model is
    /// resolved first (D5) so a draft with the default binding validates, and a config
    /// naming a tool absent from that catalog fails closed with `UnknownTool` (D3 —
    /// the edge resolves the catalog for the request scope).
    pub fn validate(
        &self,
        workspace: &str,
        config: &AgentConfig,
        catalog: &[ToolDescriptor],
    ) -> Result<(), ValidationIssue> {
        // The config domain owns validation truth; it also owns *which field* failed
        // (`CompileError::field_path`), so the UI projects the issue to the right section
        // instead of parsing a free-text string. An auto-model that can't resolve is a
        // `model` issue; a compile failure carries its own field.
        let resolved = self
            .resolve_agent_config(
                workspace,
                AgentConfigRevision {
                    config: config.clone(),
                    revision: 0,
                },
            )
            .map_err(|e| ValidationIssue {
                path: "model".to_string(),
                message: e.to_string(),
            })?;
        awaken_config_store::compile_resolved(
            &resolved.config,
            catalog,
            snapshot_metadata(&resolved),
        )
        .map(|_| ())
        .map_err(|e| ValidationIssue {
            path: e.field_path().to_string(),
            message: e.to_string(),
        })
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

    pub async fn put_if_revision(
        &self,
        registry: &dyn ConfigRegistry,
        config: &AgentConfig,
        expected_generation: u64,
    ) -> Result<ConfigWrite, String> {
        registry
            .put_config_if_revision(config, expected_generation)
            .await
            .map_err(|e| e.to_string())
    }

    /// Load a stored config draft by id from the scope-bound `registry`.
    pub async fn get(
        &self,
        registry: &dyn ConfigRegistry,
        id: &str,
    ) -> Result<Option<AgentConfig>, String> {
        registry.get_config(id).await.map_err(|e| e.to_string())
    }

    pub async fn get_versioned(
        &self,
        registry: &dyn ConfigRegistry,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, String> {
        registry
            .get_config_revision(id)
            .await
            .map_err(|e| e.to_string())
    }

    /// Every stored config draft in the scope-bound `registry` (the console's list).
    pub async fn list(&self, registry: &dyn ConfigRegistry) -> Result<Vec<AgentConfig>, String> {
        registry.list_configs().await.map_err(|e| e.to_string())
    }

    /// Publish: resolve an `Auto` model to a concrete binding (D5), compile the stored
    /// config against the caller-supplied `catalog`, persist the publication
    /// (idempotent by fingerprint) into the scope-bound `registry`, and install it into
    /// the live catalog so new runs use it. The stored source config is left untouched —
    /// its `Auto` selection persists so the reconciler can re-resolve it later.
    pub async fn publish(
        &self,
        workspace: &str,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
    ) -> Result<StoredPublication, PublishError> {
        let versioned = registry
            .get_config_revision(id)
            .await
            .map_err(|e| PublishError::Store(e.to_string()))?
            .ok_or_else(|| PublishError::NotStored(id.to_string()))?;
        let source_revision = versioned.revision;
        let mut resolved = self.resolve_agent_config(workspace, versioned)?;
        if let Some(publisher) = &self.inference_access_publisher {
            let models = resolved
                .config
                .model_binding
                .resolved()
                .into_iter()
                .chain(resolved.config.model_candidates.iter())
                .cloned()
                .collect::<Vec<_>>();
            resolved.inference_access = Some(
                publisher
                    .resolve_access(workspace, &models)
                    .await
                    .map_err(PublishError::Unresolvable)?,
            );
        }
        let snapshot = awaken_config_store::compile_resolved(
            &resolved.config,
            catalog,
            snapshot_metadata(&resolved),
        )
        .map_err(|e| PublishError::Compile(e.to_string()))?;
        let publication =
            StoredPublication::published_at_revision(snapshot.clone(), id, source_revision);
        let write = registry
            .put_publication_if_config_revision(&publication, source_revision)
            .await
            .map_err(|e| PublishError::Store(e.to_string()))?;
        if let ConfigWrite::Conflict { current_revision } = write {
            return Err(PublishError::StaleRevision(current_revision));
        }
        let mut installed = self.installed.lock().unwrap();
        let replace = installed
            .get(id)
            .is_none_or(|current| source_revision >= current.source_revision);
        if replace {
            installed.insert(
                id.to_string(),
                InstalledEntry {
                    source_revision,
                    snapshot,
                },
            );
        }
        Ok(publication)
    }

    /// Read every configuration input once and pin the exact result. No runtime or
    /// worker path is allowed to repeat this work.
    fn resolve_agent_config(
        &self,
        _workspace: &str,
        source: AgentConfigRevision,
    ) -> Result<ResolvedAgentConfig, PublishError> {
        let source_revision = source.revision;
        let mut config = source.config;
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
        // Compaction is the AGENT's policy over the model's capability: derive the effective
        // trigger from the resolved model (`context_window` − output headroom, the agent may
        // override) and stamp it into both realizations (native compact ext + ACP CLI). Baked
        // into the content-addressed config at publish, re-baked by the reconciler on a catalog
        // change. An unresolved model / no window simply leaves the authored config untouched.
        if let Some(resolver) = self.model_resolver.as_ref()
            && let Some(model_id) = config.model_binding.resolved().map(|b| b.model_ref.clone())
        {
            let strategy = config.compaction.clone().unwrap_or_default();
            apply_compaction(
                &mut config.plugin_config,
                &strategy,
                resolver.context_window(&model_id),
                resolver.max_output_tokens(&model_id),
            );
        }
        let mut inputs = vec![awaken_runtime_contract::ResolvedInputRef {
            kind: "agent_config".into(),
            id: config.id.clone(),
            version: awaken_runtime_contract::ResolvedInputVersion::Revision(source_revision),
        }];
        let model_bytes = serde_json::to_vec(&(
            config.model_binding.resolved(),
            &config.model_candidates,
            &config.compaction,
        ))
        .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
        inputs.push(awaken_runtime_contract::ResolvedInputRef {
            kind: "model_binding".into(),
            id: config
                .model_binding
                .resolved()
                .map(|binding| binding.model_ref.clone())
                .unwrap_or_default(),
            version: awaken_runtime_contract::ResolvedInputVersion::ContentHash(
                awaken_runtime_contract::content_fingerprint(&model_bytes)
                    .map_err(|error| PublishError::Unresolvable(error.to_string()))?,
            ),
        });
        let manifest = awaken_runtime_contract::ResolutionManifest::new(inputs)
            .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
        Ok(ResolvedAgentConfig {
            source: awaken_runtime_contract::AgentConfigRevisionRef {
                agent_id: awaken_runtime_contract::snapshot::AgentId(config.id.clone()),
                revision: source_revision,
            },
            config,
            manifest,
            inference_access: None,
        })
    }

    /// Re-resolve and re-publish an `Auto`-bound agent (ADR-0052 D5), reading and
    /// writing through the caller-supplied scope-bound `registry`. Returns `true` if it
    /// re-published (a `Pinned` agent is skipped; a missing one is skipped). Idempotent
    /// by content address, so a retry after a catalog change is safe. This is what
    /// [`ConfigServiceReconciler`](crate::binding_resolver::ConfigServiceReconciler)
    /// drives from the catalog write path.
    pub async fn reconcile(
        &self,
        workspace: &str,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
    ) -> Result<bool, String> {
        let stored = registry.get_config(id).await.map_err(|e| e.to_string())?;
        match stored {
            // Only auto bindings are re-resolved; an operator pin is authoritative.
            Some(config) if config.model_binding.is_auto() => {
                self.publish(workspace, registry, id, catalog)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// The installed (published) executable snapshot for `agent`, if any.
    pub fn installed(&self, agent: &str) -> Option<ExecutableAgentSnapshot> {
        self.installed
            .lock()
            .unwrap()
            .get(agent)
            .map(|entry| entry.snapshot.clone())
    }

    /// Warm-load the installed catalog from a durable registry's published configs
    /// (scope-bound). `installed` is otherwise populated only at publish time and
    /// held in-memory, so a fresh process — after a restart, or a server that did
    /// not author the publish itself — would resolve a published agent to the seed
    /// model. This rehydrates it from the store; the latest publication per agent
    /// wins (rows arrive oldest-first). Returns how many agents were installed.
    pub async fn warm_install(
        &self,
        registry: &dyn ScopedConfigRegistry,
        scope: &ScopeId,
    ) -> usize {
        let pubs = match registry.list_published_scoped(scope).await {
            Ok(pubs) => pubs,
            Err(_) => return 0,
        };
        let mut installed = self.installed.lock().unwrap();
        let n = pubs.len();
        for p in pubs {
            let replace = installed
                .get(&p.agent_id)
                .is_none_or(|current| p.source_revision >= current.source_revision);
            if replace {
                installed.insert(
                    p.agent_id,
                    InstalledEntry {
                        source_revision: p.source_revision,
                        snapshot: p.snapshot,
                    },
                );
            }
        }
        n
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
    pub fn validate(&self, scope: &ScopeId, config: &AgentConfig) -> Result<(), ValidationIssue> {
        self.service
            .validate(scope.as_str(), config, &self.catalog_for(scope))
    }

    /// Store a config draft owned by `scope`.
    pub async fn put(&self, scope: &ScopeId, config: &AgentConfig) -> Result<(), String> {
        self.service.put(&self.registry_for(scope), config).await
    }

    pub async fn put_with_audit(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, String> {
        self.store
            .put_config_with_audit_scoped(scope, config, audit)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn put_with_audit_effect(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
        audit: &ManagementAuditRecord,
        effect: Option<&ManagementEffect>,
    ) -> Result<AuditedConfigWrite, String> {
        self.store
            .put_config_with_audit_effect_scoped(scope, config, audit, effect)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn pending_management_effects(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<ManagementEffect>, String> {
        self.store
            .pending_management_effects_scoped(scope)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn complete_management_effect(
        &self,
        scope: &ScopeId,
        kind: &str,
        key: &str,
    ) -> Result<(), String> {
        self.store
            .complete_management_effect_scoped(scope, kind, key)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn record_management_audit(
        &self,
        scope: &ScopeId,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, String> {
        self.store
            .record_management_audit_scoped(scope, audit)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn get_management_audit(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, String> {
        self.store
            .get_management_audit_scoped(scope, tool, call_id)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn mark_management_audit_committed(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<(), String> {
        self.store
            .mark_management_audit_committed_scoped(scope, tool, call_id)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn put_if_revision(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
        expected_generation: u64,
    ) -> Result<ConfigWrite, String> {
        self.service
            .put_if_revision(&self.registry_for(scope), config, expected_generation)
            .await
    }

    /// Load one stored config draft owned by `scope`.
    pub async fn get(&self, scope: &ScopeId, id: &str) -> Result<Option<AgentConfig>, String> {
        self.service.get(&self.registry_for(scope), id).await
    }

    pub async fn get_versioned(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, String> {
        self.service
            .get_versioned(&self.registry_for(scope), id)
            .await
    }

    /// Every stored config draft owned by `scope`.
    pub async fn list(&self, scope: &ScopeId) -> Result<Vec<AgentConfig>, String> {
        self.service.list(&self.registry_for(scope)).await
    }

    /// Publish a stored config in `scope`.
    pub async fn publish(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<StoredPublication, PublishError> {
        self.service
            .publish(
                scope.as_str(),
                &self.registry_for(scope),
                id,
                &self.catalog_for(scope),
            )
            .await
    }

    /// Re-resolve and re-publish an `Auto`-bound agent in `scope` (ADR-0052 D5).
    pub async fn reconcile(&self, scope: &ScopeId, id: &str) -> Result<bool, String> {
        self.service
            .reconcile(
                scope.as_str(),
                &self.registry_for(scope),
                id,
                &self.catalog_for(scope),
            )
            .await
    }

    /// The scope-free service (for the installed-projection reads).
    #[must_use]
    pub fn service(&self) -> &Arc<ConfigService> {
        &self.service
    }
}

/// The config data-plane router: `/v1/config/agents/:id` (author) plus
/// `/validate` and `/publish` (lifecycle). The [`ConfigPlane`] state is the scope
/// edge — it binds the request scope onto the scope-free service.
pub fn config_router(plane: ConfigPlane) -> Router {
    Router::new()
        .route("/v1/config/agents", get(list_configs))
        .route("/v1/config/agents/{id}/validate", post(validate))
        .route("/v1/config/agents/{id}/publish", post(publish))
        .route("/v1/config/agents/{id}", get(get_config).put(put_config))
        .with_state(plane)
}

/// The request's owner scope, stamped by the workspace-path rewrite
/// (`WorkspaceScope`) or the seeded [`DEFAULT_SCOPE`] for a flat/single-tenant call.
fn request_scope(ext: Option<Extension<awaken_tenancy::WorkspaceScope>>) -> ScopeId {
    ext.map(|Extension(w)| ScopeId::from(w.0))
        .unwrap_or_else(|| ScopeId::from(DEFAULT_SCOPE))
}

/// `GET /v1/config/agents` — every stored draft in the request's scope, each tagged
/// `published` when a compiled config is currently installed for it.
async fn list_configs(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
) -> (StatusCode, Json<Value>) {
    let scope = request_scope(scope);
    match plane.list(&scope).await {
        Ok(configs) => {
            let data: Vec<Value> = configs
                .into_iter()
                .map(|config| {
                    let published = plane.service().installed(&config.id).is_some();
                    managed_from_agent_config(&config, published)
                })
                .collect();
            (StatusCode::OK, Json(json!({ "data": data })))
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

/// `GET /v1/config/agents/:id` — one stored draft within the request's scope
/// (`404` when absent, or owned by another scope — never disclose cross-tenant).
async fn get_config(
    State(plane): State<ConfigPlane>,
    Path(id): Path<String>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
) -> (StatusCode, Json<Value>) {
    let scope = request_scope(scope);
    match plane.get_versioned(&scope, &id).await {
        Ok(Some(versioned)) => {
            let published = plane.service().installed(&id).is_some();
            let mut body = managed_from_agent_config(&versioned.config, published);
            body.as_object_mut()
                .expect("managed config is an object")
                .insert("generation".to_string(), json!(versioned.revision));
            (StatusCode::OK, Json(body))
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no config stored for agent `{id}`") })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

async fn validate(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let config = match agent_config_from_managed(id, &body) {
        Ok(c) => c,
        // A body the projection can't even parse is a whole-config issue (path "").
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "valid": false,
                    "issues": [{ "path": "", "message": error, "severity": "error" }],
                })),
            );
        }
    };
    // Validation is a query — the request succeeds even when the config is invalid, so
    // both outcomes are 200 with `{ valid, issues }`. Structured, field-routed issues
    // (ADR-0053): the UI highlights the section, never re-derives the rule. Compile fails
    // fast, so there is at most one issue today.
    match plane.validate(&request_scope(scope), &config) {
        Ok(()) => (StatusCode::OK, Json(json!({ "valid": true, "issues": [] }))),
        Err(issue) => (
            StatusCode::OK,
            Json(json!({
                "valid": false,
                "issues": [{ "path": issue.path, "message": issue.message, "severity": "error" }],
            })),
        ),
    }
}

async fn put_config(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let config = match agent_config_from_managed(id.clone(), &body) {
        Ok(c) => c,
        Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    };
    let scope = request_scope(scope);
    if let Some(expected) = body.get("generation").and_then(Value::as_u64) {
        return match plane.put_if_revision(&scope, &config, expected).await {
            Ok(ConfigWrite::Applied { revision }) => (
                StatusCode::OK,
                Json(json!({ "id": id, "generation": revision })),
            ),
            Ok(ConfigWrite::Conflict { current_revision }) => (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "config generation conflict",
                    "current_revision": current_revision,
                })),
            ),
            Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
        };
    }
    match plane.put(&scope, &config).await {
        Ok(()) => match plane.get_versioned(&scope, &id).await {
            Ok(Some(current)) => (
                StatusCode::OK,
                Json(json!({ "id": id, "generation": current.revision })),
            ),
            _ => (StatusCode::OK, Json(json!({ "id": id }))),
        },
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    }
}

// ---- object model: the managed Agent object (+ our extensions) ----
// The config plane's agent object is the SDK `/v1/agents` object shape — name,
// model {id}, system, tools, mcp_servers, skills, multiagent, metadata — plus the
// extension block that carries our differentiated value (plugins, plugin_config,
// context_policy, max_steps). The runtime `AgentConfig` is the compile-input the
// object maps to; these two functions are the only translation seam.

fn managed_tool_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o
            .get("id")
            .or_else(|| o.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// Parse the managed-shaped agent object into the runtime compile-input. The
/// `model` object carries only `{id}` (managed); it becomes a `Pinned` selection
/// (provider/backend resolve downstream from the model id), so the console can
/// author + publish without a model resolver wired into this server.
fn agent_config_from_managed(id: String, body: &Value) -> Result<AgentConfig, String> {
    let string = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
    let model_ref = match body.get("model") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => o
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    };
    let array = |k: &str| {
        body.get(k)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let context_policy = match body.get("context_policy").cloned() {
        Some(v) => serde_json::from_value(v).map_err(|e| e.to_string())?,
        None => Default::default(),
    };
    // Tool presentation overrides (ADR-0053): alias / description / defer per tool.
    let tool_overrides: Vec<ToolOverride> = match body.get("tool_overrides").cloned() {
        Some(v) => serde_json::from_value(v).map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let metadata = body
        .get("metadata")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok(AgentConfig {
        id,
        instructions: string("system").unwrap_or_default(),
        max_steps: body.get("max_steps").and_then(Value::as_u64).unwrap_or(8) as usize,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::pinned("", model_ref, ""),
        tool_ids: array("tools").iter().filter_map(managed_tool_id).collect(),
        plugin_ids: array("plugins")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        plugin_config: body
            .get("plugin_config")
            .and_then(Value::as_object)
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
        context_policy,
        tool_patterns: Vec::new(),
        model_candidates: Vec::new(),
        name: string("name"),
        description: string("description"),
        metadata,
        mcp_servers: array("mcp_servers"),
        skills: array("skills"),
        multiagent: body.get("multiagent").filter(|v| !v.is_null()).cloned(),
        tool_overrides,
        recovery_policies: body
            .get("recovery_policies")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_default(),
        compaction: body
            .get("compaction")
            .filter(|v| !v.is_null())
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
    })
}

/// Project the stored config back into the managed-shaped object (+ extensions +
/// live `published` flag), so a read round-trips to the same object the SDK sees.
fn managed_from_agent_config(cfg: &AgentConfig, published: bool) -> Value {
    json!({
        "id": cfg.id,
        "type": "agent",
        "name": cfg.name,
        "description": cfg.description,
        "model": { "id": cfg.model_binding.resolved().map(|b| b.model_ref.clone()).unwrap_or_default() },
        "system": cfg.instructions,
        "metadata": cfg.metadata,
        "tools": cfg.tool_ids,
        "recovery_policies": cfg.recovery_policies,
        "mcp_servers": cfg.mcp_servers,
        "skills": cfg.skills,
        "multiagent": cfg.multiagent,
        // extensions (our differentiated value, additive to the managed object):
        "max_steps": cfg.max_steps,
        "plugins": cfg.plugin_ids,
        "plugin_config": cfg.plugin_config,
        "context_policy": cfg.context_policy,
        "tool_overrides": cfg.tool_overrides,
        "compaction": cfg.compaction,
        "published": published,
    })
}

async fn publish(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
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
        Err(err @ (PublishError::Unresolvable(_) | PublishError::StaleRevision(_))) => (
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
        AgentInputConfig, BindingId, InputBinding, InputResourceId, MemoryStoreId, ResourceAccess,
    };
    use awaken_config_store::{ConfigStoreError, SqliteConfigStore};
    use awaken_runtime_contract::resolved::ContextPolicy;

    use crate::binding_resolver::{ModelResolver, ResolvedModel};
    use awaken_runtime_contract::InferenceAccess;
    use awaken_runtime_contract::resolved::ModelBinding;
    use awaken_tenancy::WorkspaceScope;

    fn agent_config(id: &str) -> AgentConfig {
        AgentConfig {
            id: id.to_string(),
            instructions: "be helpful".to_string(),
            max_steps: 8,
            delegation_limits: Default::default(),
            model_binding: awaken_config_store::ModelSelection::pinned("p", "m", "b"),
            tool_ids: vec![],
            model_candidates: Vec::new(),
            plugin_ids: vec![],
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
            tool_patterns: Vec::new(),
            ..Default::default()
        }
    }

    fn auto_config(id: &str) -> AgentConfig {
        let mut cfg = agent_config(id);
        cfg.model_binding = ModelSelection::Auto;
        cfg
    }

    fn resolve_config(service: &ConfigService, config: AgentConfig) -> AgentConfig {
        service
            .resolve_agent_config(
                DEFAULT_SCOPE,
                AgentConfigRevision {
                    config,
                    revision: 1,
                },
            )
            .unwrap()
            .config
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

    struct FakeAccessPublisher;

    impl InferenceAccessPublisher for FakeAccessPublisher {
        fn resolve_access<'a>(
            &'a self,
            scope: &'a str,
            models: &'a [ModelBinding],
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<InferenceAccess, String>> + Send + 'a>,
        > {
            Box::pin(async move {
                let model = models.first().ok_or_else(|| "missing model".to_string())?;
                Ok(InferenceAccess::resolved_credential(
                    format!("credential-{scope}"),
                    3,
                    scope,
                    "provider@2",
                    "endpoint@4",
                    awaken_runtime_contract::InferenceEndpoint {
                        adapter_kind: "openai".into(),
                        base_url: "https://example.invalid/v1".into(),
                        upstream_model: model.model_ref.clone(),
                    },
                ))
            })
        }
    }

    /// A config plane (the scope edge) over a fresh in-memory store, an optional
    /// resolver, an optional resource store, and the given tool catalog.
    fn plane_with(
        tools: Arc<dyn ToolCatalogSource>,
        resolver: Option<Arc<dyn ModelResolver>>,
        resources: Option<Arc<dyn AgentInputBindingRepository>>,
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

    #[tokio::test]
    async fn publish_resolves_scope_access_once_into_the_persisted_snapshot() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let service = Arc::new(
            ConfigService::new().with_inference_access_publisher(Arc::new(FakeAccessPublisher)),
        );
        let plane = ConfigPlane::new(
            service,
            store,
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let scope = ScopeId::from("workspace-a");
        plane.put(&scope, &agent_config("agent-a")).await.unwrap();
        let publication = plane.publish(&scope, "agent-a").await.unwrap();
        let access = publication
            .snapshot
            .metadata
            .inference_access
            .as_ref()
            .expect("publication carries resolved inference access");
        assert_eq!(access.scope_id.as_deref(), Some("workspace-a"));
        assert_eq!(access.reference, "credential-workspace-a");
        assert_eq!(publication.fingerprint, publication.snapshot.fingerprint.0);
    }

    fn static_plane(resolver: Option<Arc<dyn ModelResolver>>) -> ConfigPlane {
        plane_with(
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
            resolver,
            None,
        )
    }

    // --- store-error paths (CEG P1 / P6 / reconcile-a / F19c / F21b) ----------
    // A real SqliteConfigStore never fails its ops deterministically, so the
    // fail-closed error arms need test doubles that return `ConfigStoreError`.

    /// Every operation fails — drives the read-failure arms.
    struct FailingRegistry;
    #[async_trait::async_trait]
    impl ConfigRegistry for FailingRegistry {
        async fn put_config(&self, _c: &AgentConfig) -> Result<(), ConfigStoreError> {
            Err(ConfigStoreError("boom".into()))
        }
        async fn get_config(&self, _id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
            Err(ConfigStoreError("boom".into()))
        }
        async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
            Err(ConfigStoreError("boom".into()))
        }
        async fn put_publication(&self, _p: &StoredPublication) -> Result<(), ConfigStoreError> {
            Err(ConfigStoreError("boom".into()))
        }
        async fn get_publication(
            &self,
            _fp: &str,
        ) -> Result<Option<StoredPublication>, ConfigStoreError> {
            Err(ConfigStoreError("boom".into()))
        }
    }

    /// Reads a stored pinned config fine, but fails when persisting the publication
    /// — isolates the `put_publication` → `Store` branch (P6).
    struct PublishFailRegistry;
    #[async_trait::async_trait]
    impl ConfigRegistry for PublishFailRegistry {
        async fn put_config(&self, _c: &AgentConfig) -> Result<(), ConfigStoreError> {
            Ok(())
        }
        async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
            Ok(Some(agent_config(id)))
        }
        async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
            Ok(vec![])
        }
        async fn put_publication(&self, _p: &StoredPublication) -> Result<(), ConfigStoreError> {
            Err(ConfigStoreError("publication store down".into()))
        }
        async fn get_publication(
            &self,
            _fp: &str,
        ) -> Result<Option<StoredPublication>, ConfigStoreError> {
            Ok(None)
        }
    }

    /// Simulates an authoring write that wins after publish reads generation 7
    /// but before it tries to persist/install the compiled artifact.
    struct StalePublishRegistry;

    #[async_trait::async_trait]
    impl ConfigRegistry for StalePublishRegistry {
        async fn put_config(&self, _c: &AgentConfig) -> Result<(), ConfigStoreError> {
            Ok(())
        }

        async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
            Ok(Some(agent_config(id)))
        }

        async fn get_config_revision(
            &self,
            id: &str,
        ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
            Ok(Some(AgentConfigRevision {
                config: agent_config(id),
                revision: 7,
            }))
        }

        async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
            Ok(Vec::new())
        }

        async fn put_publication(&self, _p: &StoredPublication) -> Result<(), ConfigStoreError> {
            panic!("generation-fenced publish must use the atomic method")
        }

        async fn put_publication_if_config_revision(
            &self,
            publication: &StoredPublication,
            expected_generation: u64,
        ) -> Result<ConfigWrite, ConfigStoreError> {
            assert_eq!(publication.source_revision, 7);
            assert_eq!(expected_generation, 7);
            Ok(ConfigWrite::Conflict {
                current_revision: Some(8),
            })
        }

        async fn get_publication(
            &self,
            _fp: &str,
        ) -> Result<Option<StoredPublication>, ConfigStoreError> {
            Ok(None)
        }
    }

    /// A scope-bound registry whose reads and writes fail, so the HTTP handlers hit
    /// their 500 / 400 error arms (F19c / F21b).
    struct FailingScopedRegistry;
    #[async_trait::async_trait]
    impl ScopedConfigRegistry for FailingScopedRegistry {
        async fn put_config_scoped(
            &self,
            _s: &ScopeId,
            _c: &AgentConfig,
        ) -> Result<(), ConfigStoreError> {
            Err(ConfigStoreError("boom".into()))
        }
        async fn get_config_scoped(
            &self,
            _s: &ScopeId,
            _id: &str,
        ) -> Result<Option<AgentConfig>, ConfigStoreError> {
            Err(ConfigStoreError("boom".into()))
        }
        async fn list_configs_scoped(
            &self,
            _s: &ScopeId,
        ) -> Result<Vec<AgentConfig>, ConfigStoreError> {
            Err(ConfigStoreError("boom".into()))
        }
        async fn put_publication_scoped(
            &self,
            _s: &ScopeId,
            _p: &StoredPublication,
        ) -> Result<(), ConfigStoreError> {
            Err(ConfigStoreError("boom".into()))
        }
        async fn get_publication_scoped(
            &self,
            _s: &ScopeId,
            _fp: &str,
        ) -> Result<Option<StoredPublication>, ConfigStoreError> {
            Err(ConfigStoreError("boom".into()))
        }
        async fn list_published_scoped(
            &self,
            _s: &ScopeId,
        ) -> Result<Vec<StoredPublication>, ConfigStoreError> {
            Err(ConfigStoreError("boom".into()))
        }
    }

    fn failing_scoped_plane() -> ConfigPlane {
        ConfigPlane::new(
            Arc::new(ConfigService::new()),
            Arc::new(FailingScopedRegistry),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        )
    }

    // P1: a registry read failure on publish surfaces as `PublishError::Store`.
    #[tokio::test]
    async fn publish_maps_a_registry_read_failure_to_store() {
        let err = ConfigService::new()
            .publish(DEFAULT_SCOPE, &FailingRegistry, "a", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, PublishError::Store(_)), "got {err:?}");
    }

    // P6: a publication-persist failure (after a clean read + compile) is `Store`.
    #[tokio::test]
    async fn publish_maps_a_publication_persist_failure_to_store() {
        let err = ConfigService::new()
            .publish(DEFAULT_SCOPE, &PublishFailRegistry, "a", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, PublishError::Store(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn publish_never_installs_an_artifact_from_a_stale_source_revision() {
        let service = ConfigService::new();
        let err = service
            .publish(DEFAULT_SCOPE, &StalePublishRegistry, "a", &[])
            .await
            .unwrap_err();

        assert!(matches!(err, PublishError::StaleRevision(Some(8))));
        assert!(
            service.installed("a").is_none(),
            "a stale publication must not enter the live catalog"
        );
    }

    // reconcile-a: a registry read failure on reconcile is a returned `Err`.
    #[tokio::test]
    async fn reconcile_propagates_a_registry_read_failure() {
        assert!(
            ConfigService::new()
                .reconcile(DEFAULT_SCOPE, &FailingRegistry, "a", &[])
                .await
                .is_err()
        );
    }

    // F19c: the get_config handler returns 500 when the store read fails.
    #[tokio::test]
    async fn get_config_handler_returns_500_on_store_error() {
        let (status, _body) =
            super::get_config(State(failing_scoped_plane()), Path("a".to_string()), None).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // F21b: the put_config handler returns 400 when the store write fails (body parses).
    #[tokio::test]
    async fn put_config_handler_returns_400_on_store_error() {
        let (status, _body) = super::put_config(
            State(failing_scoped_plane()),
            None,
            Path("a".to_string()),
            Json(json!({ "model": "gpt" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn resolve_agent_config_derives_the_effective_compaction_window_for_both_realizations() {
        struct WindowResolver;
        impl ModelResolver for WindowResolver {
            fn resolve_auto(&self) -> Result<ResolvedModel, String> {
                Ok(ResolvedModel {
                    primary: ModelBinding::new("p", "m-x", "b"),
                    candidates: vec![],
                })
            }
            fn context_window(&self, model_id: &str) -> Option<u32> {
                (model_id == "m-x").then_some(200_000)
            }
            fn max_output_tokens(&self, model_id: &str) -> Option<u32> {
                (model_id == "m-x").then_some(40_000)
            }
        }
        let service = ConfigService::new().with_model_resolver(Arc::new(WindowResolver));
        let pin = || ModelSelection::Pinned(ModelBinding::new("p", "m-x", "b"));
        // Usable budget = context_window − max_output_tokens = 200k − 40k = 160k;
        // default trigger = 3/4 × 160k = 120k.

        // NATIVE (compact section, no agent override): the effective window at ratio 1.0 —
        // the fold point IS the trigger (headroom + ratio already baked in).
        let mut cfg = agent_config("a1");
        cfg.model_binding = pin();
        cfg.plugin_config
            .insert("compact".into(), serde_json::json!({ "keep_last": 4 }));
        let out = resolve_config(&service, cfg);
        assert_eq!(out.plugin_config["compact"]["max_tokens"], 120_000);
        assert_eq!(out.plugin_config["compact"]["trigger_ratio"], 1.0);

        // ACP (acp section): the same effective window flows to the CLI's compact_window.
        let mut cfg_acp = agent_config("a-acp");
        cfg_acp.model_binding = pin();
        cfg_acp
            .plugin_config
            .insert("acp".into(), serde_json::json!({}));
        let out_acp = resolve_config(&service, cfg_acp);
        assert_eq!(out_acp.plugin_config["acp"]["compact_window"], 120_000);

        // Agent OVERRIDE (under budget) is honored verbatim, in BOTH realizations.
        let mut cfg2 = agent_config("a2");
        cfg2.model_binding = pin();
        cfg2.compaction = Some(awaken_config_store::CompactionStrategy {
            window: Some(90_000),
            keep_recent: None,
        });
        cfg2.plugin_config
            .insert("compact".into(), serde_json::json!({}));
        cfg2.plugin_config
            .insert("acp".into(), serde_json::json!({}));
        let out2 = resolve_config(&service, cfg2);
        assert_eq!(out2.plugin_config["compact"]["max_tokens"], 90_000);
        assert_eq!(out2.plugin_config["acp"]["compact_window"], 90_000);

        // An operator-pinned compact.max_tokens is never clobbered.
        let mut cfg3 = agent_config("a3");
        cfg3.model_binding = pin();
        cfg3.plugin_config
            .insert("compact".into(), serde_json::json!({ "max_tokens": 50 }));
        assert_eq!(
            resolve_config(&service, cfg3).plugin_config["compact"]["max_tokens"],
            50
        );

        // No compact/acp section → untouched (neither realization was opted into).
        let mut cfg4 = agent_config("a4");
        cfg4.model_binding = pin();
        let out4 = resolve_config(&service, cfg4);
        assert!(!out4.plugin_config.contains_key("compact"));
        assert!(!out4.plugin_config.contains_key("acp"));
    }

    // CEG F11d: a `compact` value that is present but NOT a JSON object (here a
    // bare string) is a no-op — `apply_compaction` only reaches into an object, so a
    // malformed section is left byte-identical and no window injected.
    #[test]
    fn resolve_agent_config_leaves_a_non_object_compact_untouched() {
        struct WindowResolver;
        impl ModelResolver for WindowResolver {
            fn resolve_auto(&self) -> Result<ResolvedModel, String> {
                Err("unused".into())
            }
            fn context_window(&self, _model_id: &str) -> Option<u32> {
                Some(200_000)
            }
        }
        let service = ConfigService::new().with_model_resolver(Arc::new(WindowResolver));
        let mut cfg = agent_config("a4");
        cfg.model_binding = ModelSelection::Pinned(ModelBinding::new("p", "m-x", "b"));
        cfg.plugin_config
            .insert("compact".into(), serde_json::json!("not-an-object"));
        let out = resolve_config(&service, cfg);
        assert_eq!(
            out.plugin_config["compact"],
            serde_json::json!("not-an-object")
        );
    }

    #[tokio::test]
    async fn publish_does_not_bake_session_resource_prompts_into_agent_instructions() {
        // Agent defaults remain authoring data until Session resolution. Publishing
        // the Agent must not bake a stale pre-merge resource prompt into its snapshot.
        let resources =
            Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new());
        resources
            .put_agent_inputs(
                DEFAULT_SCOPE,
                AgentInputConfig {
                    agent_id: "agent-1".into(),
                    inputs: vec![InputBinding {
                        binding_id: BindingId::from("memory"),
                        target: InputResourceId::MemoryStore(MemoryStoreId::from("memstore-7")),
                        mount_path: "/mnt/memory/prefs".into(),
                        access: ResourceAccess::ReadWrite,
                        instructions: Some("user preferences".into()),
                    }],
                    revision: 1,
                },
            )
            .unwrap();

        let scope = ScopeId::from(DEFAULT_SCOPE);
        let plane = plane_with(
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
            None,
            Some(resources),
        );
        plane.put(&scope, &agent_config("agent-1")).await.unwrap();
        plane.publish(&scope, "agent-1").await.unwrap();

        // Only the authored Agent instructions are compiled. The final resource
        // prompt is generated from Effective Session inputs at preparation time.
        let installed = plane.service().installed("agent-1").unwrap();
        let instructions = &installed.resolved_spec.instructions;
        assert!(instructions.starts_with("be helpful"));
        assert!(!instructions.contains("/mnt/memory/prefs"));
        assert!(!instructions.contains("user preferences"));
    }

    #[tokio::test]
    async fn publish_pins_agent_model_and_catalog_but_not_session_resources() {
        let resources =
            Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new());
        resources
            .put_agent_inputs(
                DEFAULT_SCOPE,
                AgentInputConfig {
                    agent_id: "pinned-inputs".into(),
                    inputs: vec![],
                    revision: 1,
                },
            )
            .unwrap();
        let tool = ToolDescriptor::pinned(
            "builtin",
            "search",
            "search",
            serde_json::json!({"type": "object"}),
        );
        let plane = plane_with(
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![tool.clone()])),
            None,
            Some(resources),
        );
        let scope = ScopeId::from(DEFAULT_SCOPE);
        let mut config = agent_config("pinned-inputs");
        config.tool_ids.push(tool.id.clone());
        plane.put(&scope, &config).await.unwrap();
        let publication = plane.publish(&scope, &config.id).await.unwrap();

        let metadata = &publication.snapshot.metadata;
        assert_eq!(metadata.source.agent_id.0, config.id);
        assert_eq!(metadata.source.revision, 1);
        assert_eq!(metadata.publication_version.0, publication.fingerprint);
        assert_eq!(metadata.fingerprint.0, publication.fingerprint);
        let kinds: Vec<_> = metadata
            .resolution
            .inputs
            .iter()
            .map(|input| input.kind.as_str())
            .collect();
        assert_eq!(kinds, ["agent_config", "model_binding", "tool"]);
        assert_eq!(
            metadata.resolution.inputs[2].version,
            awaken_runtime_contract::ResolvedInputVersion::ContentHash(tool.content_hash)
        );
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
        let spec = &installed.resolved_spec;
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
        assert_eq!(installed.resolved_spec.model_binding.model_ref, "m");
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
        assert_eq!(err.path, "tools", "an unknown tool is a `tools` issue");
        assert!(
            err.message.contains("unknown tool") && err.message.contains("admin_x"),
            "tenant scope must reject the admin tool: {}",
            err.message
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
        assert_eq!(installed.resolved_spec.instructions, "be helpful");
    }

    // ==== CEG section 04: extra publish / resolve / handler coverage ====

    /// A resolver whose catalog has no provider-backed model (P4): `resolve_auto`
    /// fails, so an `Auto` publish is `Unresolvable`.
    struct ErrResolver;
    impl ModelResolver for ErrResolver {
        fn resolve_auto(&self) -> Result<ResolvedModel, String> {
            Err("no provider-backed model in the catalog".into())
        }
    }

    // ---- publish + resolve_agent_config (F8/F10) ----

    #[tokio::test]
    async fn publish_missing_config_is_not_stored() {
        // P2: no config stored for the id → NotStored (before any resolve/compile).
        let plane = static_plane(None);
        let scope = ScopeId::from(DEFAULT_SCOPE);
        let err = plane.publish(&scope, "ghost").await.unwrap_err();
        assert!(matches!(err, PublishError::NotStored(_)), "{err:?}");
    }

    #[tokio::test]
    async fn publish_is_unresolvable_when_the_catalog_has_no_model() {
        // P4: Auto + a resolver, but the resolver reports no provider-backed model.
        let plane = static_plane(Some(Arc::new(ErrResolver)));
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &auto_config("mgmt")).await.unwrap();
        let err = plane.publish(&scope, "mgmt").await.unwrap_err();
        assert!(matches!(err, PublishError::Unresolvable(_)), "{err:?}");
    }

    #[tokio::test]
    async fn publish_compile_failure_when_config_names_an_unknown_tool() {
        // P5: resolve succeeds, compile fails because a named tool is not in the
        // (empty) catalog → Compile (not Unresolvable).
        let plane = static_plane(Some(Arc::new(FakeResolver)));
        let scope = ScopeId::from(DEFAULT_SCOPE);
        let mut cfg = agent_config("mgmt");
        cfg.tool_ids = vec!["ghost_tool".to_string()];
        plane.put(&scope, &cfg).await.unwrap();
        let err = plane.publish(&scope, "mgmt").await.unwrap_err();
        assert!(matches!(err, PublishError::Compile(_)), "{err:?}");
        assert!(err.to_string().contains("ghost_tool"), "{err}");
    }

    // ---- validate (F12) ----

    #[tokio::test]
    async fn validate_auto_without_resolver_is_a_model_issue() {
        // F12a: an Auto binding that cannot resolve is a `model`-field issue.
        let plane = static_plane(None);
        let issue = plane
            .validate(&ScopeId::from(DEFAULT_SCOPE), &auto_config("mgmt"))
            .unwrap_err();
        assert_eq!(issue.path, "model");
    }

    // ---- HTTP request_scope (F17) ----

    #[test]
    fn request_scope_uses_the_workspace_scope_when_present() {
        // F17a.
        let scope = super::request_scope(Some(Extension(WorkspaceScope("wrkspc_acme".into()))));
        assert_eq!(scope.as_str(), "wrkspc_acme");
    }

    #[test]
    fn request_scope_falls_back_to_the_default_scope() {
        // F17b.
        let scope = super::request_scope(None);
        assert_eq!(scope.as_str(), DEFAULT_SCOPE);
    }

    // ---- get_config handler (F19) ----

    #[tokio::test]
    async fn get_config_handler_returns_200_when_present() {
        // F19a.
        let plane = static_plane(None);
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &agent_config("mgmt")).await.unwrap();
        let (status, Json(body)) =
            super::get_config(State(plane), Path("mgmt".to_string()), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], "mgmt");
    }

    #[tokio::test]
    async fn get_config_handler_returns_404_when_absent() {
        // F19b: absent (or cross-tenant) → 404, never disclosed.
        let plane = static_plane(None);
        let (status, _body) =
            super::get_config(State(plane), Path("ghost".to_string()), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // ---- validate handler (F20) ----

    #[tokio::test]
    async fn validate_handler_returns_400_on_unparseable_body() {
        // F20a: a body the projection can't parse is a 400 (the one non-200 case).
        let plane = static_plane(None);
        let (status, Json(body)) = super::validate(
            State(plane),
            None,
            Path("mgmt".to_string()),
            Json(json!({ "context_policy": 123 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["valid"], json!(false));
    }

    #[tokio::test]
    async fn validate_handler_returns_200_valid_true() {
        // F20b: a parseable, valid config → 200 with valid:true.
        let plane = static_plane(None);
        let (status, Json(body)) = super::validate(
            State(plane),
            None,
            Path("mgmt".to_string()),
            Json(json!({ "model": { "id": "m" } })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["valid"], json!(true));
    }

    #[tokio::test]
    async fn validate_handler_returns_200_valid_false_on_compile_failure() {
        // F20c (the F20 decoupling): validation is a query — an *invalid* config
        // still succeeds as a request (200) with valid:false + a routed issue.
        let plane = static_plane(None);
        let (status, Json(body)) = super::validate(
            State(plane),
            None,
            Path("mgmt".to_string()),
            Json(json!({ "model": { "id": "m" }, "tools": ["ghost"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["valid"], json!(false));
        assert_eq!(body["issues"][0]["path"], "tools");
    }

    // ---- put_config handler (F21) ----

    #[tokio::test]
    async fn put_config_handler_returns_400_on_parse_failure() {
        // F21a.
        let plane = static_plane(None);
        let (status, _body) = super::put_config(
            State(plane),
            None,
            Path("mgmt".to_string()),
            Json(json!({ "context_policy": 123 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_config_handler_returns_200_on_success() {
        // F21c: a well-formed body is stored → 200, and is then readable.
        let plane = static_plane(None);
        let (status, Json(body)) = super::put_config(
            State(plane.clone()),
            None,
            Path("mgmt".to_string()),
            Json(json!({ "model": { "id": "m" }, "system": "hi" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], "mgmt");
        let stored = plane
            .get(&ScopeId::from(DEFAULT_SCOPE), "mgmt")
            .await
            .unwrap();
        assert_eq!(stored.unwrap().instructions, "hi");
    }

    // ---- agent_config_from_managed (F22) ----

    #[test]
    fn agent_config_from_managed_reads_the_model_ref() {
        // F22a/b/c: model as a String, an object {id}, or absent → "".
        let from_string =
            super::agent_config_from_managed("a".into(), &json!({ "model": "gpt-x" })).unwrap();
        assert_eq!(
            from_string.model_binding.resolved().unwrap().model_ref,
            "gpt-x"
        );

        let from_object =
            super::agent_config_from_managed("a".into(), &json!({ "model": { "id": "claude" } }))
                .unwrap();
        assert_eq!(
            from_object.model_binding.resolved().unwrap().model_ref,
            "claude"
        );

        let missing = super::agent_config_from_managed("a".into(), &json!({})).unwrap();
        assert_eq!(missing.model_binding.resolved().unwrap().model_ref, "");
    }

    #[test]
    fn agent_config_from_managed_rejects_unparseable_context_policy() {
        // F22d.
        let err = super::agent_config_from_managed("a".into(), &json!({ "context_policy": 123 }))
            .unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn agent_config_from_managed_rejects_unparseable_tool_overrides() {
        // F22e.
        let err = super::agent_config_from_managed("a".into(), &json!({ "tool_overrides": 123 }))
            .unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn managed_agent_view_round_trips_compaction_for_lossless_editing() {
        let config = super::agent_config_from_managed(
            "a".into(),
            &json!({
                "system": "compact carefully",
                "compaction": { "window": 32000, "keep_recent": 12 }
            }),
        )
        .unwrap();
        let projected = super::managed_from_agent_config(&config, false);
        assert_eq!(projected["compaction"]["window"], json!(32000));
        assert_eq!(projected["compaction"]["keep_recent"], json!(12));
    }

    // ---- publish handler (F23) ----

    #[tokio::test]
    async fn publish_handler_returns_200_on_success() {
        // F23a.
        let plane = static_plane(None);
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &agent_config("mgmt")).await.unwrap();
        let (status, Json(body)) =
            super::publish(State(plane), None, Path("mgmt".to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["installed"], json!(true));
    }

    #[tokio::test]
    async fn publish_handler_returns_409_on_unresolvable() {
        // F23b (the status partition): an unresolvable Auto binding → 409.
        let plane = static_plane(None);
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &auto_config("mgmt")).await.unwrap();
        let (status, _body) = super::publish(State(plane), None, Path("mgmt".to_string())).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn publish_handler_returns_400_on_other_publish_error() {
        // F23c: any other publish failure (here NotStored) stays a 400.
        let plane = static_plane(None);
        let (status, _body) = super::publish(State(plane), None, Path("ghost".to_string())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // ==== SEC: cross-scope isolation of the config registry ====
    //
    // The tool catalog fence is covered (`admin_tools_compile_only_in_the_reserved_scope`),
    // but the config *registry* fence — that scope A's authored/published config
    // is invisible and un-actionable from scope B — was unproven end-to-end
    // through the scope edge. A hole here is a cross-tenant config disclosure.

    /// A config plane (the scope edge) over a shared real store handle, returned
    /// alongside the store so a test can read the durable rows directly.
    fn plane_over(store: Arc<SqliteConfigStore>) -> ConfigPlane {
        ConfigPlane::new(
            Arc::new(ConfigService::new()),
            store,
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        )
    }

    #[tokio::test]
    async fn config_registry_is_fenced_across_scopes() {
        let plane = plane_over(Arc::new(SqliteConfigStore::open_in_memory().unwrap()));
        let scope_a = ScopeId::from("wrkspc_a");
        let scope_b = ScopeId::from("wrkspc_b");

        // Author (and it exists) in scope A under an id another scope might reuse.
        plane
            .put(&scope_a, &agent_config("shared-id"))
            .await
            .unwrap();

        // get from B → None (the handler renders this as a 404, never disclosing A).
        assert!(
            plane.get(&scope_b, "shared-id").await.unwrap().is_none(),
            "scope B must not read scope A's config by id"
        );
        // absent from B's list.
        assert!(
            plane.list(&scope_b).await.unwrap().is_empty(),
            "scope B's list must not include scope A's config"
        );
        // publish from B → NotStored: B has nothing by that id to compile.
        let err = plane.publish(&scope_b, "shared-id").await.unwrap_err();
        assert!(
            matches!(err, PublishError::NotStored(_)),
            "scope B must not publish scope A's config: {err:?}"
        );

        // The fence is directional: A still owns and sees its row.
        assert!(plane.get(&scope_a, "shared-id").await.unwrap().is_some());
        assert_eq!(plane.list(&scope_a).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn warm_install_rehydrates_the_latest_publication_per_agent() {
        // The `installed` catalog is populated only at publish time and held in
        // memory; a fresh process must warm-load it from the durable store or a
        // published agent resolves to the seed model. Latest-publication-wins
        // (rows arrive oldest-first; the last insert into the by-id map survives).
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let scope = ScopeId::from("wrkspc_warm");
        let author = plane_over(store.clone());

        // Publish v1, then re-author with new instructions and publish v2 (a
        // distinct content address → a second published row for the same agent).
        let mut v1 = agent_config("warm-agent");
        v1.instructions = "version one".into();
        author.put(&scope, &v1).await.unwrap();
        author.publish(&scope, "warm-agent").await.unwrap();

        let mut v2 = agent_config("warm-agent");
        v2.instructions = "version two".into();
        author.put(&scope, &v2).await.unwrap();
        author.publish(&scope, "warm-agent").await.unwrap();

        // A FRESH service (empty in-memory catalog) warm-loads from the store.
        let cold = ConfigService::new();
        let n = cold.warm_install(store.as_ref(), &scope).await;
        assert_eq!(n, 2, "both published rows are read");
        let installed = cold.installed("warm-agent").expect("agent hydrated");
        assert_eq!(
            installed.resolved_spec.instructions, "version two",
            "the latest publication wins on rehydrate"
        );

        // A store whose list fails → 0 installed (fail-closed rehydrate seam).
        let cold2 = ConfigService::new();
        assert_eq!(cold2.warm_install(&FailingScopedRegistry, &scope).await, 0);
    }

    #[tokio::test]
    async fn publish_is_idempotent_by_fingerprint() {
        // Re-publishing an unchanged config is content-addressed: the same
        // fingerprint both times, and exactly one durable published row
        // (`ON CONFLICT(fingerprint) DO NOTHING`).
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let scope = ScopeId::from("wrkspc_idem");
        let plane = plane_over(store.clone());
        plane
            .put(&scope, &agent_config("idem-agent"))
            .await
            .unwrap();

        let first = plane.publish(&scope, "idem-agent").await.unwrap();
        let second = plane.publish(&scope, "idem-agent").await.unwrap();
        assert_eq!(
            first.fingerprint, second.fingerprint,
            "an unchanged config publishes to the same content address"
        );

        let published = store.list_published_scoped(&scope).await.unwrap();
        assert_eq!(published.len(), 1, "idempotent by fingerprint: one row");
        assert_eq!(published[0].fingerprint, first.fingerprint);
    }
}
