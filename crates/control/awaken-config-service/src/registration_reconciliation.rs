//! Startup reconciliation through the same executable Agent registrar used by
//! live publication. There is no direct catalog warm-install path.

use std::collections::BTreeSet;

use awaken_config_store::{AgentLifecycle, ScopedConfigRegistry};
use awaken_executable_agent_contract::{ExecutableAgentRegistration, ExecutableAgentWithdrawal};
use awaken_tenancy::ScopeId;

use crate::ConfigService;
use crate::agent_projection::{historical_session_profile, registered_session_profile};

impl ConfigService {
    /// Reconcile one Workspace's durable publications and lifecycle tombstones
    /// through the normal registration boundary.
    pub async fn reconcile_registrations(
        &self,
        registry: &dyn ScopedConfigRegistry,
        scope: &ScopeId,
    ) -> Result<usize, String> {
        self.reconcile_registrations_for_execution_workspace(registry, scope, scope.as_str())
            .await
    }

    /// Reconcile publications authored in `configuration_scope` into an explicit
    /// execution Workspace. Reserved platform Agents are the only case where the
    /// two coordinates differ.
    pub async fn reconcile_registrations_for_execution_workspace(
        &self,
        registry: &dyn ScopedConfigRegistry,
        configuration_scope: &ScopeId,
        execution_workspace: &str,
    ) -> Result<usize, String> {
        let current_configs = registry
            .list_configs_scoped(configuration_scope)
            .await
            .map_err(|error| error.to_string())?;
        let mut executable_agents = BTreeSet::new();
        for config in current_configs {
            let versioned = registry
                .get_config_revision_scoped(configuration_scope, &config.id)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("listed Agent `{}` disappeared", config.id))?;
            if config.lifecycle() == AgentLifecycle::Published {
                executable_agents.insert(config.id.clone());
            } else {
                self.registrar
                    .withdraw(ExecutableAgentWithdrawal {
                        workspace_id: execution_workspace.to_owned(),
                        agent_id: config.id.clone(),
                        lifecycle_revision: versioned.revision,
                    })
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }

        let mut publications = registry
            .list_published_scoped(configuration_scope)
            .await
            .map_err(|error| error.to_string())?;
        // Newest first is required: historical rows may lack an old defaults
        // value, but they must never transiently become current during recovery.
        publications.sort_by(|left, right| {
            left.agent_id
                .cmp(&right.agent_id)
                .then_with(|| right.source_revision.cmp(&left.source_revision))
        });

        let mut registered = 0;
        let mut first_for_agent = BTreeSet::new();
        for publication in publications {
            if !executable_agents.contains(&publication.agent_id) {
                continue;
            }
            let source = registry
                .list_config_revisions_scoped(configuration_scope, &publication.agent_id)
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|revision| revision.revision == publication.source_revision)
                .ok_or_else(|| {
                    format!(
                        "publication `{}` has no source Agent revision {}",
                        publication.fingerprint, publication.source_revision
                    )
                })?;
            let is_current = first_for_agent.insert(publication.agent_id.clone());
            let session_profile = if is_current {
                let defaults = self.resources.as_ref().and_then(|store| {
                    store
                        .get_agent_inputs(execution_workspace, &publication.agent_id)
                        .ok()
                        .flatten()
                });
                registered_session_profile(
                    &publication.snapshot,
                    &source.config.model_binding,
                    defaults,
                )
                .ok_or_else(|| {
                    format!(
                        "current publication `{}` no longer matches Agent Session defaults",
                        publication.fingerprint
                    )
                })?
            } else {
                historical_session_profile(&publication.snapshot, &source.config.model_binding)
            };
            self.registrar
                .register(ExecutableAgentRegistration {
                    workspace_id: execution_workspace.to_owned(),
                    agent_id: publication.agent_id,
                    source_revision: publication.source_revision,
                    snapshot: publication.snapshot,
                    session_profile,
                    declared_hand: source.config.hand,
                })
                .await
                .map_err(|error| error.to_string())?;
            registered += 1;
        }
        Ok(registered)
    }
}
