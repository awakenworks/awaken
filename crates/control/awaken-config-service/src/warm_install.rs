//! Startup recovery of the process-local executable Agent projection.
//!
//! Durable configs/publications remain the source of truth. This module owns the
//! one-way recovery projection, including archived lifecycle tombstones.

use awaken_config_store::ScopedConfigRegistry;
use awaken_tenancy::ScopeId;

use crate::ConfigService;

impl ConfigService {
    /// Rehydrate one Workspace's executable catalog after process restart.
    pub async fn warm_install(
        &self,
        registry: &dyn ScopedConfigRegistry,
        scope: &ScopeId,
    ) -> usize {
        self.warm_install_for_execution_workspace(registry, scope, scope.as_str())
            .await
    }

    /// Rehydrate publications authored in `configuration_scope` into an explicit
    /// execution Workspace. Reserved platform Agents are the only case where the
    /// two coordinates differ.
    pub async fn warm_install_for_execution_workspace(
        &self,
        registry: &dyn ScopedConfigRegistry,
        configuration_scope: &ScopeId,
        execution_workspace: &str,
    ) -> usize {
        if let Ok(configs) = registry.list_configs_scoped(configuration_scope).await {
            for config in configs {
                if config.archived_at.is_some() {
                    self.installed.uninstall(execution_workspace, &config.id);
                }
            }
        }
        let publications = match registry.list_published_scoped(configuration_scope).await {
            Ok(publications) => publications,
            Err(_) => return 0,
        };
        let mut installed = 0;
        for publication in publications {
            let active = registry
                .get_config_scoped(configuration_scope, &publication.agent_id)
                .await
                .ok()
                .flatten()
                .is_some_and(|config| config.archived_at.is_none());
            if !active {
                continue;
            }
            self.installed.install(
                execution_workspace,
                &publication.agent_id,
                publication.source_revision,
                publication.snapshot,
            );
            installed += 1;
        }
        installed
    }
}
