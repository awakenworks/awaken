use std::sync::Arc;

use awaken_contract::{
    ProviderSpec, RegistryResourcePublish, VersionSelector, VersionedRegistryStore,
};
use awaken_runtime::registry::RegistrySet;
use serde::Serialize;
use serde_json::json;

use super::{ConfigRuntimeError, ConfigRuntimeManager};
use crate::services::config_runtime::managed_config::ManagedConfigSnapshot;
use crate::services::frozen_registry::{
    FrozenAgentRegistryMaterializer, LatestPublicationResolver,
};

pub(super) struct VersionedRegistryPublicationTarget {
    pub(super) scope_id: String,
    pub(super) store: Arc<dyn VersionedRegistryStore>,
}

impl ConfigRuntimeManager {
    #[must_use]
    pub fn with_versioned_registry_store(
        mut self,
        scope_id: impl Into<String>,
        store: Arc<dyn VersionedRegistryStore>,
    ) -> Self {
        let scope_id = scope_id.into();
        if let Some(handle) = self.runtime.registry_handle() {
            self.runtime
                .set_run_resolver(Arc::new(LatestPublicationResolver::new(
                    scope_id.clone(),
                    store.clone(),
                    handle,
                )));
        }
        self.versioned_registry = Some(VersionedRegistryPublicationTarget { scope_id, store });
        self
    }

    /// Pick the `RegistrySet` to install via runtime hot-swap: prefer
    /// the materialized `RegistryPublication`, fall back to the editing
    /// candidate when no versioned store is wired or materialization
    /// fails (ADR-0035 D8/D11). Takes ownership of `candidate` so the
    /// caller never accidentally hot-swaps with stale editing data after
    /// a successful publication.
    pub(super) async fn published_or_candidate_registry_set(
        &self,
        candidate: RegistrySet,
    ) -> RegistrySet {
        let Some(target) = self.versioned_registry.as_ref() else {
            return candidate;
        };
        let materializer = FrozenAgentRegistryMaterializer::new(target.store.clone());
        match materializer
            .materialize(VersionSelector::LatestPublication {
                scope_id: target.scope_id.clone(),
            })
            .await
        {
            Ok(frozen) => frozen.to_registry_set(&candidate),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "failed to materialize published registry set; falling back to candidate"
                );
                candidate
            }
        }
    }

    pub(super) async fn publish_versioned_registry(
        &self,
        managed: &ManagedConfigSnapshot,
    ) -> Result<(), ConfigRuntimeError> {
        let Some(target) = &self.versioned_registry else {
            return Ok(());
        };

        let mut resources = Vec::new();
        append_provider_specs(&managed.providers, &mut resources)?;
        append_specs(
            awaken_contract::REGISTRY_KIND_MODEL,
            &managed.models,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;
        append_specs(
            awaken_contract::REGISTRY_KIND_AGENT,
            &managed.agents,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;
        append_specs(
            awaken_contract::REGISTRY_KIND_TOOL,
            &managed.tools,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;
        append_specs(
            awaken_contract::REGISTRY_KIND_SKILL,
            &managed.skills,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;

        if resources.is_empty() {
            return Ok(());
        }

        target
            .store
            .publish_resources_and_create_publication(
                &target.scope_id,
                &uuid::Uuid::now_v7().to_string(),
                resources,
                managed.source_config_revisions.clone(),
                None,
                json!({ "config_fingerprint": managed.fingerprint }),
            )
            .await
            .map_err(to_config_error)?;
        Ok(())
    }
}

fn append_provider_specs(
    specs: &[ProviderSpec],
    resources: &mut Vec<RegistryResourcePublish>,
) -> Result<(), ConfigRuntimeError> {
    for spec in specs {
        if spec.api_key.as_ref().is_some_and(|value| !value.is_empty()) {
            return Err(ConfigRuntimeError::VersionedRegistry(format!(
                "provider '{}' cannot be published with plaintext api_key; use a runtime credential reference",
                spec.id
            )));
        }
    }
    append_specs(
        awaken_contract::REGISTRY_KIND_PROVIDER,
        specs,
        |spec| spec.id.as_str(),
        resources,
    )
}

fn append_specs<T>(
    kind: &str,
    specs: &[T],
    id: fn(&T) -> &str,
    resources: &mut Vec<RegistryResourcePublish>,
) -> Result<(), ConfigRuntimeError>
where
    T: Serialize,
{
    for spec in specs {
        let id = id(spec);
        let value = serde_json::to_value(spec).map_err(|error| {
            ConfigRuntimeError::VersionedRegistry(format!(
                "failed to serialize {kind}/{id}: {error}"
            ))
        })?;
        resources.push(RegistryResourcePublish {
            kind: kind.to_string(),
            id: id.to_string(),
            value,
            value_schema_version: 1,
            metadata: json!({}),
        });
    }
    Ok(())
}

fn to_config_error(error: awaken_contract::VersionedRegistryError) -> ConfigRuntimeError {
    ConfigRuntimeError::VersionedRegistry(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_contract::contract::config_store::ConfigStore;
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::{
        AgentSpec, BuiltinSeedSet, BuiltinSpec, ConfigRecord, ModelSpec, ProviderSpec,
        RecordMeta, RegistryPublication,
    };
    use awaken_stores::{InMemoryStore, InMemoryVersionedRegistryStore};

    use super::*;
    use crate::services::config_runtime::ProviderExecutorFactory;

    struct StubExecutor;

    #[async_trait::async_trait]
    impl LlmExecutor for StubExecutor {
        async fn execute(
            &self,
            _: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: vec![],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "stub"
        }
    }

    struct StubFactory;

    impl ProviderExecutorFactory for StubFactory {
        fn build(&self, _spec: &ProviderSpec) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
            Ok(Arc::new(StubExecutor))
        }
    }

    async fn make_manager_with_versioned_store() -> (
        ConfigRuntimeManager,
        Arc<dyn ConfigStore>,
        Arc<InMemoryVersionedRegistryStore>,
    ) {
        let config_store = Arc::new(InMemoryStore::new()) as Arc<dyn ConfigStore>;
        let thread_store = Arc::new(InMemoryStore::new());
        let runtime = Arc::new(
            awaken_runtime::builder::AgentRuntimeBuilder::new()
                .with_provider("boot", Arc::new(StubExecutor))
                .with_model(awaken_contract::ModelSpec::new("boot", "boot", "boot-model"))
                .with_agent_spec(AgentSpec {
                    id: "boot".into(),
                    model_id: "boot".into(),
                    system_prompt: "boot".into(),
                    max_rounds: 1,
                    ..Default::default()
                })
                .with_in_memory_thread_run_store(thread_store)
                .build()
                .expect("build runtime"),
        );
        let versioned = Arc::new(InMemoryVersionedRegistryStore::new());
        let manager = ConfigRuntimeManager::new(runtime, Arc::clone(&config_store))
            .expect("manager")
            .with_provider_factory(Arc::new(StubFactory))
            .with_versioned_registry_store("default", versioned.clone());

        (manager, config_store, versioned)
    }

    fn base_seed(system_prompt: &str) -> BuiltinSeedSet {
        base_seed_with_provider_api_key(system_prompt, None)
    }

    fn base_seed_with_provider_api_key(
        system_prompt: &str,
        api_key: Option<&str>,
    ) -> BuiltinSeedSet {
        BuiltinSeedSet {
            binary_version: "test".to_string(),
            specs: vec![
                BuiltinSpec::Provider(ProviderSpec {
                    id: "provider-1".to_string(),
                    adapter: "openai".to_string(),
                    api_key: api_key.map(Into::into),
                    ..Default::default()
                }),
                BuiltinSpec::Model(ModelSpec::new("model-1", "provider-1", "upstream")),
                BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "agent-1".to_string(),
                    model_id: "model-1".to_string(),
                    system_prompt: system_prompt.to_string(),
                    ..Default::default()
                })),
            ],
        }
    }

    fn entry_version(publication: &RegistryPublication, kind: &str, id: &str) -> u64 {
        publication
            .entries
            .iter()
            .find(|entry| entry.kind == kind && entry.id == id)
            .unwrap_or_else(|| panic!("publication must include {kind}/{id}"))
            .version
    }

    #[tokio::test]
    async fn apply_publishes_managed_config_to_versioned_registry() {
        let (manager, _, versioned) = make_manager_with_versioned_store().await;

        manager
            .apply_seed(&base_seed("system"))
            .await
            .expect("seed config");

        manager.apply().await.expect("apply config");

        let publication = versioned
            .latest_publication("default")
            .await
            .expect("read latest publication")
            .expect("publication");
        assert!(publication.entries.iter().any(|entry| {
            entry.kind == awaken_contract::REGISTRY_KIND_AGENT && entry.id == "agent-1"
        }));
        assert!(publication.entries.iter().any(|entry| {
            entry.kind == awaken_contract::REGISTRY_KIND_MODEL && entry.id == "model-1"
        }));
        assert!(publication.entries.iter().any(|entry| {
            entry.kind == awaken_contract::REGISTRY_KIND_PROVIDER && entry.id == "provider-1"
        }));
        assert!(publication.source_config_revisions.iter().any(|revision| {
            revision.namespace == "agents" && revision.id == "agent-1" && revision.revision > 0
        }));
    }

    #[tokio::test]
    async fn apply_rejects_provider_api_key_before_versioned_publish() {
        let (manager, _, versioned) = make_manager_with_versioned_store().await;

        manager
            .apply_seed(&base_seed_with_provider_api_key(
                "system",
                Some("sk-test-secret"),
            ))
            .await
            .expect("seed config");

        let error = manager
            .apply()
            .await
            .expect_err("provider api_key must not be published as plaintext");
        assert!(
            matches!(
                error,
                ConfigRuntimeError::VersionedRegistry(ref message)
                    if message.contains("provider-1") && message.contains("api_key")
            ),
            "expected versioned registry secret error, got: {error:?}"
        );
        assert!(
            versioned
                .latest_publication("default")
                .await
                .expect("read latest publication")
                .is_none(),
            "failed publish must not create a publication"
        );
        assert!(
            versioned
                .current(
                    "default",
                    awaken_contract::REGISTRY_KIND_PROVIDER,
                    "provider-1"
                )
                .await
                .expect("read provider current")
                .is_none(),
            "failed publish must not create a provider resource version"
        );
    }

    #[tokio::test]
    async fn reapply_keeps_versions_and_changed_config_bumps_changed_resource() {
        let (manager, config_store, versioned) = make_manager_with_versioned_store().await;

        manager
            .apply_seed(&base_seed("system"))
            .await
            .expect("seed config");
        manager.apply().await.expect("first apply");
        let first = versioned
            .latest_publication("default")
            .await
            .expect("read first publication")
            .expect("first publication");
        let first_agent_version =
            entry_version(&first, awaken_contract::REGISTRY_KIND_AGENT, "agent-1");
        let first_model_version =
            entry_version(&first, awaken_contract::REGISTRY_KIND_MODEL, "model-1");

        manager.apply().await.expect("unchanged apply");
        let unchanged = versioned
            .latest_publication("default")
            .await
            .expect("read unchanged publication")
            .expect("unchanged publication");
        assert_eq!(
            entry_version(&unchanged, awaken_contract::REGISTRY_KIND_AGENT, "agent-1"),
            first_agent_version,
            "unchanged effective config must reuse the existing agent resource version"
        );
        assert_eq!(
            entry_version(&unchanged, awaken_contract::REGISTRY_KIND_MODEL, "model-1"),
            first_model_version,
            "unchanged effective config must reuse the existing model resource version"
        );

        let mut meta = RecordMeta::new_user();
        meta.revision = 7;
        let changed = ConfigRecord {
            spec: AgentSpec {
                id: "agent-1".to_string(),
                model_id: "model-1".to_string(),
                system_prompt: "changed".to_string(),
                ..Default::default()
            },
            meta,
        };
        config_store
            .put(
                "agents",
                "agent-1",
                &changed.to_value().expect("serialize changed agent"),
            )
            .await
            .expect("write changed config");

        manager.apply().await.expect("changed apply");
        let changed_publication = versioned
            .latest_publication("default")
            .await
            .expect("read changed publication")
            .expect("changed publication");
        assert!(
            entry_version(
                &changed_publication,
                awaken_contract::REGISTRY_KIND_AGENT,
                "agent-1"
            ) > first_agent_version,
            "changed effective agent config must publish a new agent resource version"
        );
        assert_eq!(
            entry_version(
                &changed_publication,
                awaken_contract::REGISTRY_KIND_MODEL,
                "model-1"
            ),
            first_model_version,
            "unchanged model config must keep its existing resource version"
        );
        assert!(
            changed_publication
                .source_config_revisions
                .iter()
                .any(|revision| revision.namespace == "agents"
                    && revision.id == "agent-1"
                    && revision.revision == 7),
            "publication must retain the source config revision that produced the registry version"
        );
    }
}
