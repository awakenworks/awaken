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
        let provider_capabilities =
            self.update_provider_capability_cache(&managed.providers, provider_capabilities);
        let (candidate, next_provider_cache) =
            match self.compile_registry_set(registry_compile::RegistryCompileInput {
                providers: &managed.providers,
                models: &managed.models,
                pools: &managed.pools,
                agents: &managed.agents,
                tool_specs: &managed.tools,
                dynamic_tools: prepared_mcp.tool_registry.clone(),
                provider_capabilities: &provider_capabilities,
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

        self.provider_cache
            .lock()
            .replace_executors(next_provider_cache);

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

    fn update_provider_capability_cache(
        &self,
        providers: &[awaken_contract::ProviderSpec],
        discovered: std::collections::HashMap<
            String,
            std::collections::HashMap<String, awaken_runtime::registry::ModelCapabilityPatch>,
        >,
    ) -> std::collections::HashMap<
        String,
        std::collections::HashMap<String, awaken_runtime::registry::ModelCapabilityPatch>,
    > {
        self.provider_cache.lock().update_capability_snapshots(
            providers,
            discovered,
            registry_compile::provider_definition_signature,
            std::time::SystemTime::now(),
        )
    }
}
