//! Publication freshness operations over the scope-aware configuration edge.

use super::*;

impl ConfigPlane {
    /// Re-resolve and re-publish a policy-bound Agent in `scope`.
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

    /// Re-publish every policy-bound Agent after an authoritative model or
    /// Worker-observation change. Scope enumeration is system-owned, but every
    /// aggregate is re-bound through its own scoped registry before access.
    pub async fn reconcile_all_policy_bound(
        &self,
        reserved_execution_workspace: &str,
    ) -> Result<usize, String> {
        let scopes = self
            .store
            .list_config_scopes()
            .await
            .map_err(|error| error.to_string())?;
        let mut republished = 0;
        for scope in scopes {
            let configs = self
                .store
                .list_configs_scoped(&scope)
                .await
                .map_err(|error| error.to_string())?;
            let execution_workspace = if scope.as_str() == RESERVED_ADMIN_SCOPE {
                reserved_execution_workspace
            } else {
                scope.as_str()
            };
            for config in configs {
                if config.model_binding.requires_reconciliation()
                    && self
                        .reconcile_for_execution_workspace(&scope, execution_workspace, &config.id)
                        .await?
                {
                    republished += 1;
                }
            }
        }
        Ok(republished)
    }
}
