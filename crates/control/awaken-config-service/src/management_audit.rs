//! Durable management-command audit edge.
//!
//! The audit store is deliberately usable without constructing ConfigService.
//! Coordinator can therefore protect its own management routes without taking
//! ownership of Control authoring or publication behavior.

use std::sync::Arc;

use awaken_config_store::{
    AuditedConfigWrite, ManagementAuditEntry, ManagementAuditRecord, ScopedConfigRegistry,
};
use awaken_tenancy::ScopeId;

#[derive(Clone)]
pub struct ManagementAuditPlane {
    store: Arc<dyn ScopedConfigRegistry>,
}

impl ManagementAuditPlane {
    #[must_use]
    pub fn new(store: Arc<dyn ScopedConfigRegistry>) -> Self {
        Self { store }
    }

    pub async fn record(
        &self,
        scope: &ScopeId,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, String> {
        self.store
            .record_management_audit_scoped(scope, audit)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn get(
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

    pub async fn mark_committed(
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
