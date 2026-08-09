//! Scope-aware edge over the scope-free Config Service.

use std::sync::Arc;

use awaken_config_resolver::AgentInputConfig;
use awaken_config_store::{
    AgentConfig, AgentConfigRevision, AuditedConfigWrite, ConfigRegistry, ConfigWrite,
    ManagementAuditEntry, ManagementAuditRecord, ManagementEffect, ScopedConfig,
    ScopedConfigRegistry, StoredPublication,
};
use awaken_executable_agent_contract::ExecutableAgentWithdrawal;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;

use crate::config_service::ConfigService;
use crate::management_audit::ManagementAuditPlane;
use crate::publication::{PublishError, ValidationIssue};
use crate::tool_catalog::{RESERVED_ADMIN_SCOPE, ToolCatalogSource};

mod reconciliation;

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

    /// Project the one durable management-audit edge without exposing the
    /// Config Service or reconstructing an audit store adapter.
    #[must_use]
    pub fn management_audit_plane(&self) -> ManagementAuditPlane {
        ManagementAuditPlane::new(self.store.clone())
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

    pub async fn preview_for_execution_workspace(
        &self,
        configuration_scope: &ScopeId,
        execution_workspace: &str,
        preview_id: &str,
        config: &AgentConfig,
        inputs: AgentInputConfig,
    ) -> Result<awaken_runtime_contract::ExecutableAgentSnapshot, PublishError> {
        self.service
            .preview(
                &ScopeId::from(execution_workspace),
                preview_id,
                config,
                inputs,
                &self.catalog_for(configuration_scope),
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
        self.management_audit_plane().record(scope, audit).await
    }

    pub async fn get_management_audit(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, String> {
        self.management_audit_plane()
            .get(scope, tool, call_id)
            .await
    }

    pub async fn mark_management_audit_committed(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<(), String> {
        self.management_audit_plane()
            .mark_committed(scope, tool, call_id)
            .await
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

    /// Whether one exact authoring revision completed publication durably.
    pub async fn has_published_revision(
        &self,
        scope: &ScopeId,
        agent_id: &str,
        revision: u64,
    ) -> Result<bool, String> {
        self.store
            .list_published_scoped(scope)
            .await
            .map(|publications| {
                publications.into_iter().any(|publication| {
                    publication.agent_id == agent_id
                        && publication.source_revision == revision
                        && publication.state == awaken_config_store::PublicationState::Published
                })
            })
            .map_err(|error| error.to_string())
    }

    /// Latest durable publication for one Agent in the authoring scope. This is
    /// a Control read; it never consults Coordinator's rebuildable catalog.
    pub async fn latest_publication(
        &self,
        scope: &ScopeId,
        agent_id: &str,
    ) -> Result<Option<StoredPublication>, String> {
        self.store
            .list_published_scoped(scope)
            .await
            .map(|publications| {
                publications
                    .into_iter()
                    .filter(|publication| publication.agent_id == agent_id)
                    .max_by_key(|publication| publication.source_revision)
            })
            .map_err(|error| error.to_string())
    }

    /// Exact durable publication produced from one authoring revision.
    pub async fn publication_at_revision(
        &self,
        scope: &ScopeId,
        agent_id: &str,
        source_revision: u64,
    ) -> Result<Option<StoredPublication>, String> {
        self.store
            .list_published_scoped(scope)
            .await
            .map(|publications| {
                publications.into_iter().find(|publication| {
                    publication.agent_id == agent_id
                        && publication.source_revision == source_revision
                })
            })
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

    /// Publish only if the reviewed config and Resource defaults still have the
    /// exact revisions supplied by the caller.
    pub async fn publish_at_revisions(
        &self,
        scope: &ScopeId,
        id: &str,
        expected_source_revision: u64,
        expected_resource_revision: i64,
    ) -> Result<StoredPublication, PublishError> {
        if scope.as_str() == RESERVED_ADMIN_SCOPE {
            return Err(PublishError::ExecutionWorkspaceRequired);
        }
        self.publish_for_execution_workspace_at_revisions(
            scope,
            scope.as_str(),
            id,
            Some(expected_source_revision),
            Some(expected_resource_revision),
        )
        .await
    }

    /// Publish from an authoring namespace into an explicit execution Workspace.
    pub async fn publish_for_execution_workspace(
        &self,
        configuration_scope: &ScopeId,
        execution_workspace: &str,
        id: &str,
    ) -> Result<StoredPublication, PublishError> {
        self.publish_for_execution_workspace_at_revisions(
            configuration_scope,
            execution_workspace,
            id,
            None,
            None,
        )
        .await
    }

    pub async fn publish_for_execution_workspace_at_revisions(
        &self,
        configuration_scope: &ScopeId,
        execution_workspace: &str,
        id: &str,
        expected_source_revision: Option<u64>,
        expected_resource_revision: Option<i64>,
    ) -> Result<StoredPublication, PublishError> {
        self.service
            .publish_at_revisions(
                &ScopeId::from(execution_workspace),
                &self.registry_for(configuration_scope),
                id,
                &self.catalog_for(configuration_scope),
                expected_source_revision,
                expected_resource_revision,
            )
            .await
    }

    /// Withdraw an archived Agent from future Coordinator Session resolution.
    /// Exact registered revisions remain addressable for existing Sessions.
    pub async fn withdraw(
        &self,
        execution_workspace: &str,
        id: &str,
        lifecycle_revision: u64,
    ) -> Result<(), String> {
        self.service
            .registrar
            .withdraw(ExecutableAgentWithdrawal {
                workspace_id: execution_workspace.to_owned(),
                agent_id: id.to_owned(),
                lifecycle_revision,
            })
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Remove one immutable draft preview from current Session resolution.
    /// Revision two is the monotonic successor to the preview registration's
    /// fixed source revision one.
    pub async fn remove_preview(&self, execution_workspace: &str, id: &str) -> Result<(), String> {
        self.withdraw(execution_workspace, id, 2).await
    }

    /// The scope-free authoring/publication service.
    #[must_use]
    pub fn service(&self) -> &Arc<ConfigService> {
        &self.service
    }
}
