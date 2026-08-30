//! Immutable publication persistence, registration, preview, and reconciliation.
//!
//! Publication construction remains delegated to publication_build and policy
//! refresh remains delegated to the existing reconciliation authority. This
//! module only owns their ConfigService orchestration boundary.

use awaken_agent_config::{ConfigRegistry, ConfigWrite, StoredPublication};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;

use super::{ConfigService, publication_build};
use crate::publication::{PreparedPublication, PublishError};

impl ConfigService {
    async fn prepare_publication(
        &self,
        workspace: &ScopeId,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
        expected_source_revision: Option<u64>,
        expected_resource_revision: Option<i64>,
    ) -> Result<PreparedPublication, PublishError> {
        publication_build::prepare_publication(
            self,
            workspace,
            registry,
            id,
            catalog,
            expected_source_revision,
            expected_resource_revision,
        )
        .await
    }

    /// Compile the exact publication that a subsequent [`Self::publish`] would
    /// persist, without changing Control or Coordinator state.
    ///
    /// Composition adapters use this to distinguish a byte-identical retry from
    /// a dependency re-resolution (for example, an exact credential rotation)
    /// that requires a new authoring revision before registration.
    pub async fn preview_publication(
        &self,
        workspace: &ScopeId,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
    ) -> Result<StoredPublication, PublishError> {
        Ok(self
            .prepare_publication(workspace, registry, id, catalog, None, None)
            .await?
            .publication)
    }

    /// Publish: resolve an `Auto` model to a concrete binding (D5), compile the stored
    /// config against the caller-supplied `catalog`, persist the publication
    /// (idempotent by fingerprint) into the scope-bound `registry`, and register it
    /// for future Coordinator Session resolution. The stored source config is left untouched —
    /// its `Auto` selection persists so the reconciler can re-resolve it later.
    pub async fn publish(
        &self,
        workspace: &ScopeId,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
    ) -> Result<StoredPublication, PublishError> {
        self.publish_at_revisions(workspace, registry, id, catalog, None, None)
            .await
    }

    /// Publish one reviewed Agent aggregate only when both mutable sources still
    /// have the revisions observed by the caller, then freeze the exact Resource
    /// defaults into the durable publication and Coordinator registration.
    pub async fn publish_at_revisions(
        &self,
        workspace: &ScopeId,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
        expected_source_revision: Option<u64>,
        expected_resource_revision: Option<i64>,
    ) -> Result<StoredPublication, PublishError> {
        let prepared = self
            .prepare_publication(
                workspace,
                registry,
                id,
                catalog,
                expected_source_revision,
                expected_resource_revision,
            )
            .await?;
        let write = registry
            .put_publication_if_config_revision(
                &prepared.publication,
                prepared.publication.source_revision,
            )
            .await
            .map_err(|e| PublishError::Store(e.to_string()))?;
        if let ConfigWrite::Conflict { current_revision } = write {
            return Err(PublishError::StaleRevision(current_revision));
        }
        self.registrar
            .register(prepared.registration)
            .await
            .map_err(PublishError::Registration)?;
        Ok(prepared.publication)
    }

    /// Re-resolve and re-publish a policy-bound agent (ADR-0052 D5), reading and
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
        crate::reconciliation::reconcile(self, workspace, registry, id, catalog).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;

    include!("publication_tests.rs");
    include!("publication_recovery_tests.rs");
}
