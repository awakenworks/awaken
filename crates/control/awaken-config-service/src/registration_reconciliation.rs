//! Startup reconciliation through the same executable Agent registrar used by
//! live publication. There is no direct catalog warm-install path.

use std::collections::BTreeSet;

use awaken_agent_config::{AgentLifecycle, ScopedConfigRegistry};
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

        let publications = registry
            .list_published_scoped(configuration_scope)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|publication| publication.targets_execution_workspace(execution_workspace));
        // The store contract is oldest-first. Older releases allowed an exact
        // dependency rotation to persist a second fingerprint at the same
        // authored revision before executable registration rejected it. Keep
        // that immutable history durable, but converge the rebuildable catalog
        // on the last persisted snapshot for each source revision. New writes
        // are fenced before they can create this legacy shape.
        let mut latest_per_source_revision = std::collections::BTreeMap::new();
        for publication in publications {
            latest_per_source_revision.insert(
                (publication.agent_id.clone(), publication.source_revision),
                publication,
            );
        }
        let mut publications = latest_per_source_revision.into_values().collect::<Vec<_>>();
        // Newest first is required: historical rows may lack an old defaults
        // value, but they must never transiently become current during recovery.
        publications.sort_by(|left, right| {
            left.agent_id
                .cmp(&right.agent_id)
                .then_with(|| right.source_revision.cmp(&left.source_revision))
        });

        let mut registered = 0;
        let mut first_for_agent = BTreeSet::new();
        let mut degraded = Vec::new();
        for publication in publications {
            if !executable_agents.contains(&publication.agent_id) {
                continue;
            }
            let source = registry
                .list_config_revisions_scoped(configuration_scope, &publication.agent_id)
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|revision| revision.revision == publication.source_revision);
            let Some(source) = source else {
                degraded.push(format!(
                    "publication `{}` has no source Agent revision {}",
                    publication.fingerprint, publication.source_revision
                ));
                continue;
            };
            let is_current = !first_for_agent.contains(&publication.agent_id);
            let frozen_defaults = publication.agent_inputs.clone();
            let session_profile = if let Some(defaults) = frozen_defaults {
                registered_session_profile(
                    &publication.snapshot,
                    &source.config,
                    &source.config.model_binding,
                    Some(defaults),
                )
                .ok_or_else(|| {
                    format!(
                        "publication `{}` frozen Agent inputs do not match its snapshot",
                        publication.fingerprint
                    )
                })
            } else if is_current {
                let defaults = self.resources.as_ref().and_then(|store| {
                    store
                        .get_agent_inputs(execution_workspace, &publication.agent_id)
                        .ok()
                        .flatten()
                });
                registered_session_profile(
                    &publication.snapshot,
                    &source.config,
                    &source.config.model_binding,
                    defaults,
                )
                .ok_or_else(|| {
                    format!(
                        "current publication `{}` no longer matches Agent Session defaults",
                        publication.fingerprint
                    )
                })
            } else {
                Ok(historical_session_profile(
                    &publication.snapshot,
                    &source.config,
                    &source.config.model_binding,
                ))
            };
            let session_profile = match session_profile {
                Ok(profile) => profile,
                Err(error) => {
                    degraded.push(error);
                    continue;
                }
            };
            match self
                .registrar
                .register(ExecutableAgentRegistration {
                    workspace_id: execution_workspace.to_owned(),
                    agent_id: publication.agent_id.clone(),
                    source_revision: publication.source_revision,
                    snapshot: publication.snapshot,
                    session_profile,
                })
                .await
            {
                Ok(_) => {
                    first_for_agent.insert(publication.agent_id);
                    registered += 1;
                }
                Err(error) => degraded.push(error.to_string()),
            }
        }
        if !degraded.is_empty() {
            return Err(format!(
                "{registered} executable Agent registrations recovered; {} quarantined: {}",
                degraded.len(),
                degraded.join("; ")
            ));
        }
        Ok(registered)
    }
}
