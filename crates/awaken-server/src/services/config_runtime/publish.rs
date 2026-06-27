use super::{
    ConfigRuntimeError, ConfigRuntimeManager, ManagedConfigSnapshot, catalog_installer,
    provider_capability_discovery, registry_compile,
};
use catalog_installer::RuntimeCatalogInstaller;

impl ConfigRuntimeManager {
    pub(super) async fn publish(
        &self,
        managed: ManagedConfigSnapshot,
    ) -> Result<u64, ConfigRuntimeError> {
        let prepared_skills = self.prepare_skill_specs(&managed.skills)?;
        let discovered_agents = self.discover_a2a_agents(&managed.a2a_servers).await;
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
                discovered_agents,
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

        RuntimeCatalogInstaller::new(
            managed.fingerprint,
            candidate,
            next_provider_cache,
            staged_capabilities,
            prepared_skills,
            prepared_mcp,
        )
        .commit_to(self)
        .await
    }

    fn stage_provider_capability_cache(
        &self,
        providers: &[awaken_server_contract::ProviderSpec],
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
