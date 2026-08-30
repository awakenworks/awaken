//! Mutable Config Service authoring and lifecycle CAS operations.
//!
//! This module owns the ordinary authoring port but delegates the single domain
//! canonicalization decision to AgentConfig and exact system transitions to the
//! existing lifecycle authority. It introduces no additional writer or state.

use awaken_agent_config::{AgentConfig, AgentConfigRevision, ConfigRegistry, ConfigWrite};

use super::{ConfigService, lifecycle};

impl ConfigService {
    /// Ordinary-write admission seam. It delegates the sole domain decision to
    /// `AgentConfig::canonicalize_mutable_authoring_against`; audited stores call
    /// that same decision inside their generation-fenced transaction.
    pub(crate) async fn admit_mutable_write(
        &self,
        registry: &dyn ConfigRegistry,
        config: &AgentConfig,
    ) -> Result<(AgentConfig, u64), String> {
        let current = registry
            .get_config_revision(&config.id)
            .await
            .map_err(|error| error.to_string())?;
        let config = config.canonicalize_mutable_authoring_against(
            current.as_ref().map(|revision| &revision.config),
        )?;
        let expected_revision = current.map_or(0, |revision| revision.revision);
        Ok((config, expected_revision))
    }

    /// Store a config draft (upsert by id) in the caller-supplied scope-bound
    /// `registry` (a [`awaken_agent_config::ScopedConfig`] the edge bound to the
    /// request scope).
    pub async fn put(
        &self,
        registry: &dyn ConfigRegistry,
        config: &AgentConfig,
    ) -> Result<(), String> {
        let (config, expected_revision) = self.admit_mutable_write(registry, config).await?;
        match registry
            .put_config_if_revision(&config, expected_revision)
            .await
            .map_err(|e| e.to_string())?
        {
            ConfigWrite::Applied { .. } => Ok(()),
            ConfigWrite::Conflict { current_revision } => Err(format!(
                "agent `{}` changed concurrently (current revision: {current_revision:?})",
                config.id
            )),
        }
    }

    pub async fn put_if_revision(
        &self,
        registry: &dyn ConfigRegistry,
        config: &AgentConfig,
        expected_generation: u64,
    ) -> Result<ConfigWrite, String> {
        let (config, observed_revision) = self.admit_mutable_write(registry, config).await?;
        if observed_revision != expected_generation {
            return Ok(ConfigWrite::Conflict {
                current_revision: (observed_revision != 0).then_some(observed_revision),
            });
        }
        registry
            .put_config_if_revision(&config, observed_revision)
            .await
            .map_err(|e| e.to_string())
    }

    /// Archive through the lifecycle authority while preserving every authored
    /// byte, including historical legacy policy, exactly as stored.
    pub async fn archive_if_revision(
        &self,
        registry: &dyn ConfigRegistry,
        config: &AgentConfig,
        expected_generation: u64,
    ) -> Result<ConfigWrite, String> {
        lifecycle::put_exact_system_transition_if_revision(
            registry,
            config,
            expected_generation,
            lifecycle::ExactSystemTransition::Archive,
        )
        .await
    }

    pub(crate) async fn advance_reconciliation_revision(
        &self,
        registry: &dyn ConfigRegistry,
        config: &AgentConfig,
        expected_generation: u64,
    ) -> Result<ConfigWrite, String> {
        lifecycle::put_exact_system_transition_if_revision(
            registry,
            config,
            expected_generation,
            lifecycle::ExactSystemTransition::ReconciliationRevision,
        )
        .await
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
}

#[cfg(test)]
mod tests {
    use super::super::*;

    include!("authoring_tests.rs");
}
