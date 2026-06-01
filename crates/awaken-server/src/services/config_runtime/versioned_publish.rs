use std::sync::Arc;

use awaken_runtime::registry::RegistrySet;
use awaken_server_contract::{
    ProviderSpec, RegistryResourcePublish, ScopeError, ScopeId, VersionSelector,
    VersionedRegistryStore,
};
use serde::Serialize;
use serde_json::json;

use super::{ConfigRuntimeError, ConfigRuntimeManager};
use crate::services::config_runtime::managed_config::ManagedConfigSnapshot;
use crate::services::frozen_registry::{
    FrozenAgentRegistryMaterializer, ScopedServerResolver, ScopedServerResolverFactory,
};

pub(super) struct VersionedRegistryPublicationTarget {
    pub(super) scope_id: ScopeId,
    pub(super) store: Arc<dyn VersionedRegistryStore>,
    pub(super) resolver_factory: Option<Arc<ScopedServerResolverFactory>>,
}

impl ConfigRuntimeManager {
    #[must_use]
    pub fn with_versioned_registry_store(
        self,
        scope_id: impl Into<String>,
        store: Arc<dyn VersionedRegistryStore>,
    ) -> Self {
        self.try_with_versioned_registry_store(scope_id, store)
            .expect("scope_id must be valid")
    }

    pub fn try_with_versioned_registry_store(
        mut self,
        scope_id: impl Into<String>,
        store: Arc<dyn VersionedRegistryStore>,
    ) -> Result<Self, ScopeError> {
        let scope_id = ScopeId::new(scope_id.into())?;
        let resolver_factory = self.runtime.registry_handle().map(|handle| {
            self.runtime
                .set_run_resolver(Arc::new(ScopedServerResolver::new(
                    scope_id.clone(),
                    store.clone(),
                    handle.clone(),
                )));
            Arc::new(ScopedServerResolverFactory::new(store.clone(), handle))
        });
        self.versioned_registry = Some(VersionedRegistryPublicationTarget {
            scope_id,
            store,
            resolver_factory,
        });
        Ok(self)
    }

    #[must_use]
    pub fn scoped_resolver_factory(&self) -> Option<Arc<ScopedServerResolverFactory>> {
        self.versioned_registry
            .as_ref()
            .and_then(|target| target.resolver_factory.clone())
    }

    #[must_use]
    pub fn has_versioned_registry_store(&self) -> bool {
        self.versioned_registry.is_some()
    }

    /// Pick the `RegistrySet` to install via runtime hot-swap. When a
    /// versioned store is wired, the published registry must materialize; the
    /// editing candidate is only valid for unversioned runtimes.
    pub(super) async fn published_or_candidate_registry_set(
        &self,
        candidate: RegistrySet,
    ) -> Result<RegistrySet, ConfigRuntimeError> {
        let Some(target) = self.versioned_registry.as_ref() else {
            return Ok(candidate);
        };
        let materializer = FrozenAgentRegistryMaterializer::new(target.store.clone());
        let frozen = materializer
            .materialize(VersionSelector::LatestPublication {
                scope_id: target.scope_id.as_str().to_string(),
            })
            .await
            .map_err(|error| ConfigRuntimeError::VersionedRegistry(error.to_string()))?;
        Ok(frozen.to_registry_set(&candidate))
    }

    /// Publish a ONE-OFF ephemeral publication = the current managed config
    /// resources plus `draft` (overriding any saved agent with the same id),
    /// returning its `snapshot_version`. The admin sandbox uses this to run an
    /// unsaved draft agent durably: the run resolves to the latest publication
    /// (this one) and the mailbox re-materializes it at execution by
    /// resolution_id. `source_config_revisions` is empty — this is NOT a config
    /// edit, so it never touches the audit/config-edit stream; only the
    /// per-scope publication version counter advances.
    pub async fn publish_ephemeral_with_extra_agent(
        &self,
        draft: &awaken_server_contract::AgentSpec,
    ) -> Result<u64, ConfigRuntimeError> {
        let Some(target) = &self.versioned_registry else {
            return Err(ConfigRuntimeError::VersionedRegistry(
                "ephemeral draft publication requires a versioned registry store".into(),
            ));
        };
        let managed = self.load_managed_config().await?;

        let mut resources = Vec::new();
        append_provider_specs(&managed.providers, &mut resources)?;
        append_specs(
            awaken_server_contract::REGISTRY_KIND_MODEL,
            &managed.models,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;
        append_specs(
            awaken_server_contract::REGISTRY_KIND_MODEL_POOL,
            &managed.pools,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;
        let agents: Vec<awaken_server_contract::AgentSpec> = managed
            .agents
            .iter()
            .filter(|agent| agent.id != draft.id)
            .cloned()
            .chain(std::iter::once(draft.clone()))
            .collect();
        append_specs(
            awaken_server_contract::REGISTRY_KIND_AGENT,
            &agents,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;
        append_specs(
            awaken_server_contract::REGISTRY_KIND_TOOL,
            &managed.tools,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;
        append_specs(
            awaken_server_contract::REGISTRY_KIND_SKILL,
            &managed.skills,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;

        let publication = target
            .store
            .publish_resources_and_create_publication(
                target.scope_id.as_str(),
                &uuid::Uuid::now_v7().to_string(),
                resources,
                Vec::new(),
                None,
                json!({ "ephemeral_draft_agent": draft.id }),
            )
            .await
            .map_err(to_config_error)?;
        Ok(publication.snapshot_version)
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
            awaken_server_contract::REGISTRY_KIND_MODEL,
            &managed.models,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;
        append_specs(
            awaken_server_contract::REGISTRY_KIND_MODEL_POOL,
            &managed.pools,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;
        append_specs(
            awaken_server_contract::REGISTRY_KIND_AGENT,
            &managed.agents,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;
        append_specs(
            awaken_server_contract::REGISTRY_KIND_TOOL,
            &managed.tools,
            |spec| spec.id.as_str(),
            &mut resources,
        )?;
        append_specs(
            awaken_server_contract::REGISTRY_KIND_SKILL,
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
                target.scope_id.as_str(),
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
        awaken_server_contract::REGISTRY_KIND_PROVIDER,
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

fn to_config_error(error: awaken_server_contract::VersionedRegistryError) -> ConfigRuntimeError {
    ConfigRuntimeError::VersionedRegistry(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_server_contract::contract::config_store::ConfigStore;
    use awaken_server_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_server_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_server_contract::{
        AgentSpec, BuiltinSeedSet, BuiltinSpec, ConfigRecord, ModelSpec, ProviderSpec, RecordMeta,
        RegistryPublication,
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
                .with_model(awaken_server_contract::ModelSpec::new(
                    "boot",
                    "boot",
                    "boot-model",
                ))
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
            entry.kind == awaken_server_contract::REGISTRY_KIND_AGENT && entry.id == "agent-1"
        }));
        assert!(publication.entries.iter().any(|entry| {
            entry.kind == awaken_server_contract::REGISTRY_KIND_MODEL && entry.id == "model-1"
        }));
        assert!(publication.entries.iter().any(|entry| {
            entry.kind == awaken_server_contract::REGISTRY_KIND_PROVIDER && entry.id == "provider-1"
        }));
        assert!(publication.source_config_revisions.iter().any(|revision| {
            revision.namespace == "agents" && revision.id == "agent-1" && revision.revision > 0
        }));
    }

    #[tokio::test]
    async fn published_registry_materialization_failure_does_not_use_candidate() {
        let (manager, _, _) = make_manager_with_versioned_store().await;
        let candidate = manager
            .runtime
            .registry_handle()
            .expect("runtime registry handle")
            .snapshot()
            .into_registries();

        let error = match manager.published_or_candidate_registry_set(candidate).await {
            Ok(_) => panic!("missing latest publication must fail"),
            Err(error) => error,
        };
        assert!(
            matches!(error, ConfigRuntimeError::VersionedRegistry(ref message)
                if message.contains("publication") && message.contains("latest")),
            "unexpected error: {error:?}"
        );
    }

    #[tokio::test]
    async fn publish_ephemeral_with_extra_agent_includes_draft_without_config_revisions() {
        let (manager, _, versioned) = make_manager_with_versioned_store().await;
        manager
            .apply_seed(&base_seed("system"))
            .await
            .expect("seed config");
        manager.apply().await.expect("apply config");

        let draft = AgentSpec {
            id: "draft-sandbox".to_string(),
            model_id: "model-1".to_string(),
            system_prompt: "draft preview".to_string(),
            ..Default::default()
        };
        let version = manager
            .publish_ephemeral_with_extra_agent(&draft)
            .await
            .expect("ephemeral publish");

        let publication = versioned
            .get_publication("default", version)
            .await
            .expect("read publication")
            .expect("publication exists");
        // The draft agent is present so a durable run pinned to this version
        // resolves it at execution/resume.
        assert!(
            publication.entries.iter().any(|entry| entry.kind
                == awaken_server_contract::REGISTRY_KIND_AGENT
                && entry.id == "draft-sandbox"),
            "ephemeral publication must include the draft agent"
        );
        // Saved resources still ride along so the draft's deps resolve.
        assert!(publication.entries.iter().any(|entry| entry.kind
            == awaken_server_contract::REGISTRY_KIND_MODEL
            && entry.id == "model-1"));
        // Not a config edit: no source config revisions, so the audit/config
        // version stream is untouched.
        assert!(
            publication.source_config_revisions.is_empty(),
            "ephemeral publication must not carry config revisions"
        );
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
                    awaken_server_contract::REGISTRY_KIND_PROVIDER,
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
        let first_agent_version = entry_version(
            &first,
            awaken_server_contract::REGISTRY_KIND_AGENT,
            "agent-1",
        );
        let first_model_version = entry_version(
            &first,
            awaken_server_contract::REGISTRY_KIND_MODEL,
            "model-1",
        );

        manager.apply().await.expect("unchanged apply");
        let unchanged = versioned
            .latest_publication("default")
            .await
            .expect("read unchanged publication")
            .expect("unchanged publication");
        assert_eq!(
            entry_version(
                &unchanged,
                awaken_server_contract::REGISTRY_KIND_AGENT,
                "agent-1"
            ),
            first_agent_version,
            "unchanged effective config must reuse the existing agent resource version"
        );
        assert_eq!(
            entry_version(
                &unchanged,
                awaken_server_contract::REGISTRY_KIND_MODEL,
                "model-1"
            ),
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
                awaken_server_contract::REGISTRY_KIND_AGENT,
                "agent-1"
            ) > first_agent_version,
            "changed effective agent config must publish a new agent resource version"
        );
        assert_eq!(
            entry_version(
                &changed_publication,
                awaken_server_contract::REGISTRY_KIND_MODEL,
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
