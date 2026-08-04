//! Durable management-command audit edge.
//!
//! The audit store is deliberately usable without constructing ConfigService.
//! Coordinator can therefore protect its own management routes without taking
//! ownership of Control authoring or publication behavior.

use std::sync::Arc;

use awaken_agent_config::{
    AuditedConfigWrite, ManagementAuditEntry, ManagementAuditRecord, ScopedConfigRegistry,
};
use awaken_tenancy::ScopeId;

#[async_trait::async_trait]
pub trait ManagementAuditRepository: Send + Sync {
    async fn record(
        &self,
        scope: &ScopeId,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, String>;

    async fn get(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, String>;

    async fn mark_committed(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<(), String>;
}

struct ScopedConfigManagementAuditRepository {
    store: Arc<dyn ScopedConfigRegistry>,
}

#[async_trait::async_trait]
impl ManagementAuditRepository for ScopedConfigManagementAuditRepository {
    async fn record(
        &self,
        scope: &ScopeId,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, String> {
        self.store
            .record_management_audit_scoped(scope, audit)
            .await
            .map_err(|error| error.to_string())
    }

    async fn get(
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

    async fn mark_committed(
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
}

#[derive(Clone)]
pub struct ManagementAuditPlane {
    repository: Arc<dyn ManagementAuditRepository>,
}

impl ManagementAuditPlane {
    #[must_use]
    pub fn new(store: Arc<dyn ScopedConfigRegistry>) -> Self {
        Self::from_repository(Arc::new(ScopedConfigManagementAuditRepository { store }))
    }

    #[must_use]
    pub fn from_repository(repository: Arc<dyn ManagementAuditRepository>) -> Self {
        Self { repository }
    }

    pub async fn record(
        &self,
        scope: &ScopeId,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, String> {
        self.repository.record(scope, audit).await
    }

    pub async fn get(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, String> {
        self.repository.get(scope, tool, call_id).await
    }

    pub async fn mark_committed(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<(), String> {
        self.repository.mark_committed(scope, tool, call_id).await
    }
}

#[async_trait::async_trait]
impl ManagementAuditRepository for ManagementAuditPlane {
    async fn record(
        &self,
        scope: &ScopeId,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, String> {
        ManagementAuditPlane::record(self, scope, audit).await
    }

    async fn get(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, String> {
        ManagementAuditPlane::get(self, scope, tool, call_id).await
    }

    async fn mark_committed(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<(), String> {
        ManagementAuditPlane::mark_committed(self, scope, tool, call_id).await
    }
}
