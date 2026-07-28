//! The config data plane (slice A): author, publish, and install agent configs.
//!
//! `ConfigService` is the config domain's authoring authority — it validates and
//! stores declarative [`AgentConfig`]s in a [`ConfigRegistry`], and on publish
//! compiles one into a content-addressed [`StoredPublication`] and hot-swaps it
//! into the installed catalog; the host resolves a session's agent to that snapshot,
//! tools, and plugins (ADR-0031; the config/runtime seam is the compiled snapshot).
//!
//! Runtime consumes compiled configuration and never edits authoring records.
use std::sync::Arc;

use awaken_config_resolver::AgentInputBindingRepository;
use awaken_config_store::{
    AgentConfig, AgentConfigRevision, AuditedConfigWrite, ConfigRegistry, ConfigWrite,
    ManagementAuditEntry, ManagementAuditRecord, ManagementEffect, ScopedConfig,
    ScopedConfigRegistry, StoredPublication,
};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;

use crate::binding_resolver::ModelPublicationResolver;
use crate::credential_reference::{CredentialReferenceValidator, validate_credential_references};
use crate::installed_catalog::InstalledAgentCatalog;
use crate::publication::{
    PublishError, ValidationIssue, prepare_agent_publication, snapshot_metadata,
};
use crate::tool_catalog::RESERVED_ADMIN_SCOPE;
use crate::tool_catalog::ToolCatalogSource;

#[cfg(test)]
use crate::config_routes::{get_config, publish, put_config, request_scope, validate};
#[cfg(test)]
use awaken_config_store::DEFAULT_SCOPE;
#[cfg(test)]
use axum::extract::{Path, State};
#[cfg(test)]
use axum::http::StatusCode;
#[cfg(test)]
use axum::{Extension, Json};
#[cfg(test)]
use serde_json::json;

/// The config domain service: validate, store, publish, and expose the installed
/// published executable snapshot per agent.
///
/// **Authorization-free by design (ADR-0051/0052).** The already-scoped authoring
/// collaborators — a scope-bound [`ConfigRegistry`] (via `ScopedConfig`) and the
/// namespace's resolved tool catalog (`&[ToolDescriptor]`) — are passed in per call
/// by the edge ([`ConfigPlane`] and router handlers). Publication also receives one
/// trusted execution Workspace coordinate so the installed catalog cannot leak a
/// same-id Agent across Workspaces. It receives no principal, role, policy, token, or
/// authorization decision.
pub struct ConfigService {
    /// Workspace-keyed hot catalog; a runtime lookup must never observe another
    /// Workspace's same-id Agent publication.
    pub(crate) installed: InstalledAgentCatalog,
    /// Per-agent resource bindings (ADR-0038). When wired, the agent's bound-resource
    /// prompt fragments are appended to its effective system prompt at compile (A3a).
    /// `None` → compilation is byte-identical to an unbound agent.
    pub(crate) resources: Option<Arc<dyn AgentInputBindingRepository>>,
    /// Resolves authored selection into complete ordered model candidates in one
    /// publication read. Required at construction so a config service can never
    /// publish through an implicit host/provider fallback.
    pub(crate) model_publication_resolver: Arc<dyn ModelPublicationResolver>,
    pub(crate) credential_reference_validator: Option<Arc<dyn CredentialReferenceValidator>>,
}

impl ConfigService {
    /// A scope-free config service with one mandatory model-publication policy.
    /// Validate a config by compiling it against the caller-supplied tool `catalog`
    /// (a dry run of publish); no store write. Mirrors publish: an `Auto` model is
    /// resolved first (D5) so a draft with the default binding validates, and a config
    /// naming a tool absent from that catalog fails closed with `UnknownTool` (D3 —
    /// the edge resolves the catalog for the request scope).
    pub async fn validate(
        &self,
        workspace: &ScopeId,
        config: &AgentConfig,
        catalog: &[ToolDescriptor],
    ) -> Result<(), ValidationIssue> {
        // The config domain owns validation truth; it also owns *which field* failed
        // (`CompileError::field_path`), so the UI projects the issue to the right section
        // instead of parsing a free-text string. An auto-model that can't resolve is a
        // `model` issue; a compile failure carries its own field.
        let resolved = prepare_agent_publication(
            self.model_publication_resolver.as_ref(),
            workspace,
            AgentConfigRevision {
                config: config.clone(),
                revision: 0,
            },
        )
        .await
        .map_err(|e| ValidationIssue {
            path: "model".to_string(),
            message: e.to_string(),
        })?;
        validate_credential_references(
            self.credential_reference_validator.as_ref(),
            workspace,
            &resolved.config,
        )
        .await
        .map_err(|error| ValidationIssue {
            path: "mcp_servers".to_string(),
            message: error,
        })?;
        awaken_config_store::compile_published(
            &resolved.config,
            catalog,
            snapshot_metadata(&resolved),
            resolved.models.primary,
            resolved.models.candidates,
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
        self.reject_archived_rewrite(registry, config).await?;
        registry.put_config(config).await.map_err(|e| e.to_string())
    }

    pub async fn put_if_revision(
        &self,
        registry: &dyn ConfigRegistry,
        config: &AgentConfig,
        expected_generation: u64,
    ) -> Result<ConfigWrite, String> {
        self.reject_archived_rewrite(registry, config).await?;
        registry
            .put_config_if_revision(config, expected_generation)
            .await
            .map_err(|e| e.to_string())
    }

    async fn reject_archived_rewrite(
        &self,
        registry: &dyn ConfigRegistry,
        config: &AgentConfig,
    ) -> Result<(), String> {
        let current = registry
            .get_config(&config.id)
            .await
            .map_err(|error| error.to_string())?;
        if current
            .as_ref()
            .is_some_and(|stored| stored.archived_at.is_some() && stored != config)
        {
            return Err(format!("agent `{}` is archived", config.id));
        }
        Ok(())
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

    pub async fn list_revisions(
        &self,
        registry: &dyn ConfigRegistry,
        id: &str,
    ) -> Result<Vec<AgentConfigRevision>, String> {
        registry
            .list_config_revisions(id)
            .await
            .map_err(|error| error.to_string())
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
        workspace: &ScopeId,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
    ) -> Result<StoredPublication, PublishError> {
        let versioned = registry
            .get_config_revision(id)
            .await
            .map_err(|e| PublishError::Store(e.to_string()))?
            .ok_or_else(|| PublishError::NotStored(id.to_string()))?;
        if versioned.config.archived_at.is_some() {
            return Err(PublishError::Archived(id.to_string()));
        }
        let source_revision = versioned.revision;
        let resolved = prepare_agent_publication(
            self.model_publication_resolver.as_ref(),
            workspace,
            versioned,
        )
        .await?;
        validate_credential_references(
            self.credential_reference_validator.as_ref(),
            workspace,
            &resolved.config,
        )
        .await
        .map_err(PublishError::Unresolvable)?;
        let mut metadata = snapshot_metadata(&resolved);
        if let Some(defaults) = self.resources.as_ref().and_then(|store| {
            store
                .get_agent_inputs(workspace.as_str(), id)
                .ok()
                .flatten()
        }) {
            let mut inputs = std::mem::take(&mut metadata.resolution.inputs);
            inputs.push(awaken_runtime_contract::ResolvedInputRef {
                kind: "agent_session_defaults".into(),
                id: id.to_string(),
                version: awaken_runtime_contract::ResolvedInputVersion::Revision(
                    defaults.revision as u64,
                ),
            });
            metadata.resolution = awaken_runtime_contract::ResolutionManifest::new(inputs)
                .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
        }
        let snapshot = awaken_config_store::compile_published(
            &resolved.config,
            catalog,
            metadata,
            resolved.models.primary,
            resolved.models.candidates,
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
        self.installed
            .install(workspace.as_str(), id, source_revision, snapshot);
        Ok(publication)
    }

    /// Re-resolve and re-publish an `Auto`-bound agent (ADR-0052 D5), reading and
    /// writing through the caller-supplied scope-bound `registry`. Returns `true` if it
    /// re-published (a `Pinned` agent is skipped; a missing one is skipped). Idempotent
    /// by content address, so a retry after a catalog change is safe. This is what
    /// [`ConfigServiceReconciler`](crate::binding_resolver::ConfigServiceReconciler)
    /// drives from the catalog write path.
    pub async fn reconcile(
        &self,
        workspace: &ScopeId,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
    ) -> Result<bool, String> {
        let stored = registry.get_config(id).await.map_err(|e| e.to_string())?;
        match stored {
            // Only auto bindings are re-resolved; an operator pin is authoritative.
            Some(config) if config.model_binding.requires_reconciliation() => {
                self.publish(workspace, registry, id, catalog)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(true)
            }
            _ => Ok(false),
        }
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
    pub async fn validate(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
    ) -> Result<(), ValidationIssue> {
        self.validate_for_execution_workspace(scope, scope.as_str(), config)
            .await
    }

    /// Validate authoring in `scope` using the real Workspace whose catalog and
    /// credential facts publication would freeze. Reserved configuration scopes
    /// must never be reused as resource/credential owners.
    pub async fn validate_for_execution_workspace(
        &self,
        scope: &ScopeId,
        execution_workspace: &str,
        config: &AgentConfig,
    ) -> Result<(), ValidationIssue> {
        self.service
            .validate(
                &ScopeId::from(execution_workspace),
                config,
                &self.catalog_for(scope),
            )
            .await
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

    pub async fn list_revisions(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Vec<AgentConfigRevision>, String> {
        self.service
            .list_revisions(&self.registry_for(scope), id)
            .await
    }

    /// Read one exact immutable publication owned by `scope`.
    pub async fn publication(
        &self,
        scope: &ScopeId,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, String> {
        self.registry_for(scope)
            .get_publication(fingerprint)
            .await
            .map_err(|error| error.to_string())
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
        if scope.as_str() == RESERVED_ADMIN_SCOPE {
            return Err(PublishError::ExecutionWorkspaceRequired);
        }
        self.publish_for_execution_workspace(scope, scope.as_str(), id)
            .await
    }

    /// Publish from an authoring namespace into an explicit execution Workspace.
    /// The split is required for reserved platform Agents; it is not an
    /// authorization decision and does not change the scoped config repository.
    pub async fn publish_for_execution_workspace(
        &self,
        configuration_scope: &ScopeId,
        execution_workspace: &str,
        id: &str,
    ) -> Result<StoredPublication, PublishError> {
        self.service
            .publish(
                &ScopeId::from(execution_workspace),
                &self.registry_for(configuration_scope),
                id,
                &self.catalog_for(configuration_scope),
            )
            .await
    }

    /// Remove an archived Agent from the process-local execution projection.
    /// Durable authoring data and revision history remain in the scoped store.
    pub fn uninstall(&self, execution_workspace: &str, id: &str) {
        self.service.installed.uninstall(execution_workspace, id);
    }

    /// Re-resolve and re-publish an `Auto`-bound agent in `scope` (ADR-0052 D5).
    pub async fn reconcile(&self, scope: &ScopeId, id: &str) -> Result<bool, String> {
        if scope.as_str() == RESERVED_ADMIN_SCOPE {
            return Err(PublishError::ExecutionWorkspaceRequired.to_string());
        }
        self.reconcile_for_execution_workspace(scope, scope.as_str(), id)
            .await
    }

    pub async fn reconcile_for_execution_workspace(
        &self,
        configuration_scope: &ScopeId,
        execution_workspace: &str,
        id: &str,
    ) -> Result<bool, String> {
        self.service
            .reconcile(
                &ScopeId::from(execution_workspace),
                &self.registry_for(configuration_scope),
                id,
                &self.catalog_for(configuration_scope),
            )
            .await
    }

    /// The scope-free service (for the installed-projection reads).
    #[must_use]
    pub fn service(&self) -> &Arc<ConfigService> {
        &self.service
    }
}

#[cfg(test)]
pub(crate) mod resource_prompt_tests {
    use super::*;
    use awaken_config_resolver::{
        AgentInputConfig, BindingId, InputBinding, InputResourceId, MemoryStoreId, ResourceAccess,
    };
    use awaken_config_store::{ConfigStoreError, ModelSelection, SqliteConfigStore};
    use awaken_runtime_contract::resolved::ContextPolicy;

    use crate::binding_resolver::{
        ModelPublicationResolver, PublicationResolutionError, ResolvedPublicationModels,
    };
    use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
    use awaken_tenancy::WorkspaceScope;

    fn scope(id: &str) -> ScopeId {
        ScopeId::from(id)
    }

    pub(crate) fn agent_config(id: &str) -> AgentConfig {
        AgentConfig {
            id: id.to_string(),
            instructions: "be helpful".to_string(),
            max_steps: 8,
            delegation_limits: Default::default(),
            model_binding: awaken_config_store::ModelSelection::pinned("p", "m", "b"),
            inference: Default::default(),
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

    async fn resolve_config(service: &ConfigService, config: AgentConfig) -> AgentConfig {
        prepare_agent_publication(
            service.model_publication_resolver.as_ref(),
            &scope(DEFAULT_SCOPE),
            AgentConfigRevision {
                config,
                revision: 1,
            },
        )
        .await
        .unwrap()
        .config
    }

    struct FakeResolver;
    #[async_trait::async_trait]
    impl ModelPublicationResolver for FakeResolver {
        async fn resolve_models(
            &self,
            _workspace: &ScopeId,
            selection: &ModelSelection,
            candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
            let (primary, candidates) = selection.resolved().map_or_else(
                || {
                    (
                        ModelBinding::new("openai", "m-first", "genai"),
                        vec![ModelBinding::new("openai", "m-second", "genai")],
                    )
                },
                |primary| (primary.clone(), candidates.to_vec()),
            );
            Ok(ResolvedPublicationModels::host(
                primary, candidates, None, None,
            ))
        }
    }

    pub(crate) fn test_service() -> ConfigService {
        ConfigService::new(Arc::new(FakeResolver))
    }

    struct FakeProviderResolver;

    #[async_trait::async_trait]
    impl ModelPublicationResolver for FakeProviderResolver {
        async fn resolve_models(
            &self,
            workspace: &ScopeId,
            selection: &ModelSelection,
            candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
            let primary = selection
                .resolved()
                .cloned()
                .ok_or_else(|| "test requires a pinned model".to_string())?;
            let candidate = |binding: ModelBinding| {
                ResolvedModelCandidate::provider(
                    binding.clone(),
                    "provider@2",
                    "endpoint@4",
                    workspace.clone(),
                    Some(awaken_runtime_contract::CredentialAccess::new(
                        awaken_runtime_contract::CredentialRef {
                            id: format!("credential-{workspace}"),
                            revision: 3,
                        },
                        awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                        awaken_runtime_contract::CredentialUsage::ProviderAdapter,
                        awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
                    )),
                    awaken_runtime_contract::InferenceEndpoint {
                        adapter_kind: "openai".into(),
                        api_dialect: "open_ai_chat".into(),
                        base_url: "https://example.invalid/v1".into(),
                        upstream_model: binding.model_ref,
                    },
                )
            };
            Ok(ResolvedPublicationModels {
                primary: candidate(primary),
                candidates: candidates.iter().cloned().map(candidate).collect(),
                context_window: None,
                max_output_tokens: None,
            })
        }
    }

    /// A config plane (the scope edge) over a fresh in-memory store, an optional
    /// resolver override, an optional resource store, and the given tool catalog.
    fn plane_with(
        tools: Arc<dyn ToolCatalogSource>,
        resolver: Option<Arc<dyn ModelPublicationResolver>>,
        resources: Option<Arc<dyn AgentInputBindingRepository>>,
    ) -> ConfigPlane {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let mut service = ConfigService::new(resolver.unwrap_or_else(|| Arc::new(FakeResolver)));
        if let Some(resources) = resources {
            service = service.with_resources(resources);
        }
        ConfigPlane::new(Arc::new(service), store, tools)
    }

    #[tokio::test]
    async fn publish_resolves_scope_access_once_into_the_persisted_snapshot() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let service = Arc::new(ConfigService::new(Arc::new(FakeProviderResolver)));
        let plane = ConfigPlane::new(
            service,
            store,
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let scope = ScopeId::from("workspace-a");
        plane.put(&scope, &agent_config("agent-a")).await.unwrap();
        let publication = plane.publish(&scope, "agent-a").await.unwrap();
        let candidate = &publication.snapshot.resolved_spec.model_binding;
        let awaken_runtime_contract::resolved::ModelProvisioning::Provider {
            scope_id,
            credential: Some(credential),
            ..
        } = &candidate.provisioning
        else {
            panic!("publication carries a complete provider candidate")
        };
        assert_eq!(scope_id.as_str(), "workspace-a");
        assert_eq!(credential.credential.id, "credential-workspace-a");
        assert_eq!(publication.fingerprint, publication.snapshot.fingerprint.0);
    }

    #[tokio::test]
    async fn local_publication_pins_host_access_instead_of_deferring_to_runtime() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let plane = ConfigPlane::new(
            Arc::new(test_service()),
            store,
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let scope = ScopeId::from("workspace-a");
        plane.put(&scope, &agent_config("agent-a")).await.unwrap();

        let publication = plane.publish(&scope, "agent-a").await.unwrap();
        assert!(matches!(
            publication
                .snapshot
                .resolved_spec
                .model_binding
                .provisioning,
            awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor
        ));
    }

    fn static_plane(resolver: Option<Arc<dyn ModelPublicationResolver>>) -> ConfigPlane {
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

    pub(crate) fn failing_scoped_plane() -> ConfigPlane {
        ConfigPlane::new(
            Arc::new(test_service()),
            Arc::new(FailingScopedRegistry),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        )
    }

    // P1: a registry read failure on publish surfaces as `PublishError::Store`.
    #[tokio::test]
    async fn publish_maps_a_registry_read_failure_to_store() {
        let err = test_service()
            .publish(&scope(DEFAULT_SCOPE), &FailingRegistry, "a", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, PublishError::Store(_)), "got {err:?}");
    }

    // P6: a publication-persist failure (after a clean read + compile) is `Store`.
    #[tokio::test]
    async fn publish_maps_a_publication_persist_failure_to_store() {
        let err = test_service()
            .publish(&scope(DEFAULT_SCOPE), &PublishFailRegistry, "a", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, PublishError::Store(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn publish_never_installs_an_artifact_from_a_stale_source_revision() {
        let service = test_service();
        let err = service
            .publish(&scope(DEFAULT_SCOPE), &StalePublishRegistry, "a", &[])
            .await
            .unwrap_err();

        assert!(matches!(err, PublishError::StaleRevision(Some(8))));
        assert!(
            service.installed_in(DEFAULT_SCOPE, "a").is_none(),
            "a stale publication must not enter the live catalog"
        );
    }

    // reconcile-a: a registry read failure on reconcile is a returned `Err`.
    #[tokio::test]
    async fn reconcile_propagates_a_registry_read_failure() {
        assert!(
            test_service()
                .reconcile(&scope(DEFAULT_SCOPE), &FailingRegistry, "a", &[])
                .await
                .is_err()
        );
    }

    // F19c: the get_config handler returns 500 when the store read fails.
    #[tokio::test]
    async fn get_config_handler_returns_500_on_store_error() {
        let (status, _body) = super::get_config(
            State(failing_scoped_plane()),
            Path("a".to_string()),
            None,
            None,
        )
        .await;
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

    #[tokio::test]
    async fn resolve_agent_config_derives_the_effective_compaction_window_for_both_realizations() {
        struct WindowResolver;
        #[async_trait::async_trait]
        impl ModelPublicationResolver for WindowResolver {
            async fn resolve_models(
                &self,
                _workspace: &ScopeId,
                selection: &ModelSelection,
                candidates: &[ModelBinding],
            ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
                Ok(ResolvedPublicationModels::host(
                    selection
                        .resolved()
                        .cloned()
                        .unwrap_or_else(|| ModelBinding::new("p", "m-x", "b")),
                    candidates.to_vec(),
                    Some(200_000),
                    Some(40_000),
                ))
            }
        }
        let service = ConfigService::new(Arc::new(WindowResolver));
        let pin = || ModelSelection::Pinned(ModelBinding::new("p", "m-x", "b"));
        // Usable budget = context_window − max_output_tokens = 200k − 40k = 160k;
        // default trigger = 3/4 × 160k = 120k.

        // NATIVE (compact section, no agent override): the effective window at ratio 1.0 —
        // the fold point IS the trigger (headroom + ratio already baked in).
        let mut cfg = agent_config("a1");
        cfg.model_binding = pin();
        cfg.plugin_config
            .insert("compact".into(), serde_json::json!({ "keep_last": 4 }));
        let out = resolve_config(&service, cfg).await;
        assert_eq!(out.plugin_config["compact"]["max_tokens"], 120_000);
        assert_eq!(out.plugin_config["compact"]["trigger_ratio"], 1.0);

        // ACP (acp section): the same effective window flows to the CLI's compact_window.
        let mut cfg_acp = agent_config("a-acp");
        cfg_acp.model_binding = pin();
        cfg_acp
            .plugin_config
            .insert("acp".into(), serde_json::json!({}));
        let out_acp = resolve_config(&service, cfg_acp).await;
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
        let out2 = resolve_config(&service, cfg2).await;
        assert_eq!(out2.plugin_config["compact"]["max_tokens"], 90_000);
        assert_eq!(out2.plugin_config["acp"]["compact_window"], 90_000);

        // An operator-pinned compact.max_tokens is never clobbered.
        let mut cfg3 = agent_config("a3");
        cfg3.model_binding = pin();
        cfg3.plugin_config
            .insert("compact".into(), serde_json::json!({ "max_tokens": 50 }));
        assert_eq!(
            resolve_config(&service, cfg3).await.plugin_config["compact"]["max_tokens"],
            50
        );

        // No compact/acp section → untouched (neither realization was opted into).
        let mut cfg4 = agent_config("a4");
        cfg4.model_binding = pin();
        let out4 = resolve_config(&service, cfg4).await;
        assert!(!out4.plugin_config.contains_key("compact"));
        assert!(!out4.plugin_config.contains_key("acp"));
    }

    // CEG F11d: a `compact` value that is present but NOT a JSON object (here a
    // bare string) is a no-op — `apply_compaction` only reaches into an object, so a
    // malformed section is left byte-identical and no window injected.
    #[tokio::test]
    async fn resolve_agent_config_leaves_a_non_object_compact_untouched() {
        struct WindowResolver;
        #[async_trait::async_trait]
        impl ModelPublicationResolver for WindowResolver {
            async fn resolve_models(
                &self,
                _workspace: &ScopeId,
                selection: &ModelSelection,
                candidates: &[ModelBinding],
            ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
                Ok(ResolvedPublicationModels::host(
                    selection.resolved().cloned().ok_or_else(|| {
                        PublicationResolutionError::Invalid(
                            "test requires a pinned model".to_string(),
                        )
                    })?,
                    candidates.to_vec(),
                    Some(200_000),
                    None,
                ))
            }
        }
        let service = ConfigService::new(Arc::new(WindowResolver));
        let mut cfg = agent_config("a4");
        cfg.model_binding = ModelSelection::Pinned(ModelBinding::new("p", "m-x", "b"));
        cfg.plugin_config
            .insert("compact".into(), serde_json::json!("not-an-object"));
        let out = resolve_config(&service, cfg).await;
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
                    environment: None,
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
        let installed = plane
            .service()
            .installed_in(DEFAULT_SCOPE, "agent-1")
            .unwrap();
        let instructions = &installed.resolved_spec.instructions;
        assert!(instructions.starts_with("be helpful"));
        assert!(!instructions.contains("/mnt/memory/prefs"));
        assert!(!instructions.contains("user preferences"));
    }

    #[tokio::test]
    async fn publish_pins_agent_model_catalog_and_session_defaults_revision() {
        let resources =
            Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new());
        resources
            .put_agent_inputs(
                DEFAULT_SCOPE,
                AgentInputConfig {
                    agent_id: "pinned-inputs".into(),
                    environment: Some(awaken_config_resolver::AgentEnvironmentBinding {
                        environment_id: "env-production".into(),
                        revision: 9,
                    }),
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
            Some(resources.clone()),
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
        assert_eq!(
            kinds,
            [
                "agent_config",
                "agent_session_defaults",
                "model_binding",
                "tool"
            ]
        );
        assert_eq!(
            metadata.resolution.inputs[1].version,
            awaken_runtime_contract::ResolvedInputVersion::Revision(1)
        );
        assert_eq!(
            metadata.resolution.inputs[3].version,
            awaken_runtime_contract::ResolvedInputVersion::ContentHash(tool.content_hash)
        );

        use awaken_session_contract::AgentConfigSource as _;
        let source = crate::ConfigServiceAgentSource(plane.service().clone());
        let view = source
            .agent_view_in(DEFAULT_SCOPE, "pinned-inputs")
            .expect("matching published defaults");
        assert_eq!(view.environment.unwrap().revision, 9);

        resources
            .put_agent_inputs(
                DEFAULT_SCOPE,
                AgentInputConfig {
                    agent_id: "pinned-inputs".into(),
                    environment: Some(awaken_config_resolver::AgentEnvironmentBinding {
                        environment_id: "env-production".into(),
                        revision: 9,
                    }),
                    inputs: vec![],
                    revision: 2,
                },
            )
            .unwrap();
        assert!(
            source
                .agent_view_in(DEFAULT_SCOPE, "pinned-inputs")
                .is_none(),
            "current defaults cannot replace the publication-pinned revision"
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
        let installed = plane.service().installed_in(DEFAULT_SCOPE, "mgmt").unwrap();
        let spec = &installed.resolved_spec;
        assert_eq!(spec.model_binding.model_ref, "m-first");
        assert_eq!(spec.model_candidates.len(), 1);
        assert_eq!(spec.model_candidates[0].model_ref, "m-second");

        // The stored *source* config is still Auto — so a later catalog change can
        // re-resolve it (reconcile returns true only for an Auto source).
        assert!(plane.reconcile(&scope, "mgmt").await.unwrap());
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
            DEFAULT_SCOPE,
            vec!["assistant".to_string(), "pinned".to_string()],
        );
        assert_eq!(reconciler.reconcile().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn pinned_publication_uses_the_explicit_host_resolver() {
        let plane = static_plane(None);
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &agent_config("pinned")).await.unwrap();
        plane.publish(&scope, "pinned").await.unwrap();
        let installed = plane
            .service()
            .installed_in(DEFAULT_SCOPE, "pinned")
            .unwrap();
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
                .await
                .is_ok(),
            "admin tool must resolve in the reserved scope"
        );

        // In any tenant scope it fails closed — the admin tool is not even disclosed,
        // so the same config hits UnknownTool at compile.
        let err = plane
            .validate(&ScopeId::from("wrkspc_acme"), &cfg)
            .await
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
        let installed = plane
            .service()
            .installed_in(DEFAULT_SCOPE, "agent-2")
            .unwrap();
        assert_eq!(installed.resolved_spec.instructions, "be helpful");
    }

    // ==== CEG section 04: extra publish / resolve / handler coverage ====

    /// A resolver whose catalog has no provider-backed model (P4): `resolve_auto`
    /// fails, so an `Auto` publish is `Unresolvable`.
    struct ErrResolver;
    #[async_trait::async_trait]
    impl ModelPublicationResolver for ErrResolver {
        async fn resolve_models(
            &self,
            _workspace: &ScopeId,
            _selection: &ModelSelection,
            _candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
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
    async fn validate_reports_a_resolver_failure_as_a_model_issue() {
        // F12a: an Auto binding that cannot resolve is a `model`-field issue.
        let plane = static_plane(Some(Arc::new(ErrResolver)));
        let issue = plane
            .validate(&ScopeId::from(DEFAULT_SCOPE), &auto_config("mgmt"))
            .await
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
            super::get_config(State(plane), Path("mgmt".to_string()), None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], "mgmt");
    }

    #[tokio::test]
    async fn get_config_handler_returns_404_when_absent() {
        // F19b: absent (or cross-tenant) → 404, never disclosed.
        let plane = static_plane(None);
        let (status, _body) =
            super::get_config(State(plane), Path("ghost".to_string()), None, None).await;
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

    // ---- publish handler (F23) ----

    #[tokio::test]
    async fn publish_handler_returns_200_on_success() {
        // F23a.
        let plane = static_plane(None);
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &agent_config("mgmt")).await.unwrap();
        let (status, Json(body)) =
            super::publish(State(plane), None, None, Path("mgmt".to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["installed"], json!(true));
    }

    #[tokio::test]
    async fn publish_handler_returns_409_on_unresolvable() {
        // F23b (the status partition): an unresolvable Auto binding → 409.
        let plane = static_plane(Some(Arc::new(ErrResolver)));
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &auto_config("mgmt")).await.unwrap();
        let (status, _body) =
            super::publish(State(plane), None, None, Path("mgmt".to_string())).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn publish_handler_returns_400_on_other_publish_error() {
        // F23c: any other publish failure (here NotStored) stays a 400.
        let plane = static_plane(None);
        let (status, _body) =
            super::publish(State(plane), None, None, Path("ghost".to_string())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn reserved_publication_requires_an_explicit_execution_workspace() {
        let plane = static_plane(None);
        let scope = ScopeId::from(RESERVED_ADMIN_SCOPE);
        plane.put(&scope, &agent_config("mgmt")).await.unwrap();

        let error = plane.publish(&scope, "mgmt").await.unwrap_err();
        assert!(matches!(error, PublishError::ExecutionWorkspaceRequired));

        let publication = plane
            .publish_for_execution_workspace(&scope, "workspace-real", "mgmt")
            .await
            .unwrap();
        assert_eq!(publication.agent_id, "mgmt");
        assert!(plane.service().installed_in("__admin", "mgmt").is_none());
        assert!(
            plane
                .service()
                .installed_in("workspace-real", "mgmt")
                .is_some()
        );
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
            Arc::new(test_service()),
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
    async fn installed_catalog_is_keyed_by_workspace_and_agent_id() {
        // Separate scope-bound registries may legitimately reuse a local Agent id
        // (for example when a router shards configuration storage). The live index
        // must preserve that external Workspace coordinate rather than collapse it.
        let registry_a = SqliteConfigStore::open_in_memory().unwrap();
        let registry_b = SqliteConfigStore::open_in_memory().unwrap();
        let service = test_service();

        let mut a = agent_config("shared-id");
        a.instructions = "workspace A".into();
        ConfigRegistry::put_config(&registry_a, &a).await.unwrap();
        service
            .publish(&scope("wrkspc_a"), &registry_a, &a.id, &[])
            .await
            .unwrap();

        let mut b = agent_config("shared-id");
        b.instructions = "workspace B".into();
        ConfigRegistry::put_config(&registry_b, &b).await.unwrap();
        service
            .publish(&scope("wrkspc_b"), &registry_b, &b.id, &[])
            .await
            .unwrap();

        assert_eq!(
            service
                .installed_in("wrkspc_a", "shared-id")
                .unwrap()
                .resolved_spec
                .instructions,
            "workspace A"
        );
        assert_eq!(
            service
                .installed_in("wrkspc_b", "shared-id")
                .unwrap()
                .resolved_spec
                .instructions,
            "workspace B"
        );
        assert!(
            service.installed_in("wrkspc_c", "shared-id").is_none(),
            "an uninstalled Workspace must fail closed even when another Workspace uses the id"
        );
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
        let cold = test_service();
        let n = cold.warm_install(store.as_ref(), &scope).await;
        assert_eq!(n, 2, "both published rows are read");
        let installed = cold
            .installed_in(scope.as_str(), "warm-agent")
            .expect("agent hydrated");
        assert_eq!(
            installed.resolved_spec.instructions, "version two",
            "the latest publication wins on rehydrate"
        );

        // A store whose list fails → 0 installed (fail-closed rehydrate seam).
        let cold2 = test_service();
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
