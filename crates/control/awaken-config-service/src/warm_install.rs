//! Startup recovery of the process-local executable Agent projection.
//!
//! Durable configs/publications remain the source of truth. This module owns the
//! one-way recovery projection, including archived lifecycle tombstones.

use awaken_config_store::ScopedConfigRegistry;
use awaken_tenancy::ScopeId;
use std::collections::BTreeSet;

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
        let current_configs = match registry.list_configs_scoped(configuration_scope).await {
            Ok(configs) => configs,
            Err(_) => return 0,
        };
        let mut executable_agents = BTreeSet::new();
        for config in current_configs {
            if config.lifecycle() == awaken_config_store::AgentLifecycle::Published {
                executable_agents.insert(config.id);
            } else {
                self.installed.uninstall(execution_workspace, &config.id);
            }
        }
        let publications = match registry.list_published_scoped(configuration_scope).await {
            Ok(publications) => publications,
            Err(_) => return 0,
        };
        let mut installed = 0;
        for publication in publications {
            if !executable_agents.contains(&publication.agent_id) {
                continue;
            }
            let config = registry
                .list_config_revisions_scoped(configuration_scope, &publication.agent_id)
                .await
                .ok()
                .and_then(|revisions| {
                    revisions
                        .into_iter()
                        .find(|revision| revision.revision == publication.source_revision)
                })
                .map(|revision| revision.config);
            let Some(config) = config.filter(|config| {
                config.lifecycle() == awaken_config_store::AgentLifecycle::Published
            }) else {
                continue;
            };
            self.installed.install(
                execution_workspace,
                &publication.agent_id,
                publication.source_revision,
                publication.snapshot,
                config.model_binding,
                config.hand,
            );
            installed += 1;
        }
        installed
    }
}
