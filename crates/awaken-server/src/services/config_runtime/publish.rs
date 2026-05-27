use super::{
    ConfigRuntimeError, ConfigRuntimeManager, ManagedConfigSnapshot, provider_capability_discovery,
    registry_compile,
};

impl ConfigRuntimeManager {
    pub(super) async fn publish(
        &self,
        managed: ManagedConfigSnapshot,
    ) -> Result<u64, ConfigRuntimeError> {
        let prepared_skills = self.prepare_skill_specs(&managed.skills)?;
        let prepared_mcp = self.prepare_mcp_registry(&managed.mcp_servers).await?;
        let provider_capabilities = provider_capability_discovery::discover_provider_capabilities(
            &managed.providers,
            &managed.models,
            &managed.pools,
        )
        .await;
        // Stage the capability cache update without committing: a failed
        // compile/validate/publish/runtime-swap below must not leave discovered
        // metadata in the trusted cache (where a later discovery failure could
        // re-serve it). Committed only after the runtime swap succeeds.
        let staged_capabilities =
            self.stage_provider_capability_cache(&managed.providers, provider_capabilities);
        let (candidate, next_provider_cache) =
            match self.compile_registry_set(registry_compile::RegistryCompileInput {
                providers: &managed.providers,
                models: &managed.models,
                pools: &managed.pools,
                agents: &managed.agents,
                tool_specs: &managed.tools,
                dynamic_tools: prepared_mcp.tool_registry.clone(),
                provider_capabilities: &staged_capabilities.resolved,
            }) {
                Ok(candidate) => candidate,
                Err(error) => {
                    prepared_mcp.cleanup().await;
                    return Err(error);
                }
            };

        if let Err(error) = self.validate_candidate(&candidate, &managed.agents, &managed.skills) {
            prepared_mcp.cleanup().await;
            return Err(error);
        }

        if let Err(error) = self.publish_versioned_registry(&managed).await {
            prepared_mcp.cleanup().await;
            return Err(error);
        }

        let runtime_set = self.published_or_candidate_registry_set(candidate).await;
        let version = match self.runtime.replace_registry_set(runtime_set) {
            Some(version) => version,
            None => {
                prepared_mcp.cleanup().await;
                return Err(ConfigRuntimeError::RuntimeNotConfigurable);
            }
        };

        if let Some(prepared_skills) = prepared_skills {
            prepared_skills.commit();
        }

        {
            // Commit the executor cache and the staged capability cache together,
            // only now that the publish transaction has fully succeeded.
            let mut provider_cache = self.provider_cache.lock();
            provider_cache.replace_executors(next_provider_cache);
            provider_cache.commit_capabilities(staged_capabilities);
        }

        let previous_mcp = if prepared_mcp.state_changed {
            let mut active = self.active_mcp_registry.lock();
            std::mem::replace(&mut *active, prepared_mcp.next_state)
        } else {
            None
        };

        *self.last_applied_fingerprint.write() = Some(managed.fingerprint);

        if let Some(previous) = previous_mcp
            && let Err(error) = previous.handle.close().await
        {
            tracing::warn!(
                error = %error,
                "failed to close replaced MCP registry"
            );
        }

        Ok(version)
    }

    fn stage_provider_capability_cache(
        &self,
        providers: &[awaken_contract::ProviderSpec],
        discovery: provider_capability_discovery::ProviderCapabilityDiscovery,
    ) -> super::provider_cache::StagedCapabilityCache {
        self.provider_cache.lock().stage_capability_snapshots(
            providers,
            discovery.discovered,
            &discovery.attempted,
            registry_compile::provider_definition_signature,
            std::time::SystemTime::now(),
        )
    }
}
