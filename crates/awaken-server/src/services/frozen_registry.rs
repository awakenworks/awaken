use std::sync::Arc;

use crate::services::pinned_registry::{
    PinnedAgentSpecRegistry, PinnedModelRegistry, PinnedRegistryError, PinnedSpecMap,
};
use awaken_runtime::registry::RegistryHandle;
use awaken_runtime::resolution::{
    PersistenceRequirement, RegistryResolutionScope, ResolutionRequest, ResolveError,
    ResolvedRunPlan, Resolver,
};
use awaken_server_contract::contract::versioned_registry::VersionedRecord;
use awaken_server_contract::skill_spec::SkillSpec;
use awaken_server_contract::tool_spec::ToolSpec;
use awaken_server_contract::{
    AgentSpec, ModelPoolSpec, ModelSpec, PinnedRegistryEntry, PinnedRegistryManifest, ProviderSpec,
    REGISTRY_KIND_AGENT, REGISTRY_KIND_MODEL, REGISTRY_KIND_MODEL_POOL,
    REGISTRY_KIND_PLUGIN_CONFIG, REGISTRY_KIND_PROVIDER, REGISTRY_KIND_SKILL, REGISTRY_KIND_TOOL,
    RegistryGraphValidationError, RegistryGraphValidationRequest, RegistryGraphValidator,
    ScopeContext, ScopeError, ScopeId, StandardRegistryGraphValidator, VersionSelector,
    VersionedRegistryError, VersionedRegistryStore,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FrozenRegistryMaterializationError {
    #[error("registry graph validation failed: {0}")]
    Graph(#[from] RegistryGraphValidationError),
    #[error("versioned registry error: {0}")]
    Registry(#[from] VersionedRegistryError),
    #[error("pinned registry error: {0}")]
    Pinned(#[from] PinnedRegistryError),
    #[error("invalid frozen registry graph: {0}")]
    InvalidGraph(String),
}

#[non_exhaustive]
pub struct FrozenAgentRegistry {
    pub manifest: PinnedRegistryManifest,
    pub agents: Arc<PinnedAgentSpecRegistry>,
    /// Pinned model bindings reachable from the manifest (ADR-0035 D8).
    pub models: Arc<PinnedSpecMap<ModelSpec>>,
    /// Pinned model pools reachable from the manifest. A pool shares the model
    /// id namespace, so durable runs resolve it like a model (ADR-0042).
    pub pools: Arc<PinnedSpecMap<ModelPoolSpec>>,
    /// Pinned provider specs reachable from the manifest.
    pub providers: Arc<PinnedSpecMap<ProviderSpec>>,
    /// Pinned skill specs reachable from the manifest.
    pub skills: Arc<PinnedSpecMap<SkillSpec>>,
    /// Pinned tool specs reachable from the manifest.
    pub tools: Arc<PinnedSpecMap<ToolSpec>>,
    /// Pinned plugin-config payloads. Stored as raw `serde_json::Value`
    /// because plugin_config payload shapes are extension-specific.
    pub plugin_configs: Arc<PinnedSpecMap<Value>>,
}

impl FrozenAgentRegistry {
    /// Build a `RegistrySet` suitable for `RunActivation.pinned_registry_set`
    /// (ADR-0035 D9). `live` supplies the runtime objects we cannot
    /// rebuild from specs alone (tool/provider/plugin executors / backend
    /// factories); agents and models are sourced from the frozen pins so
    /// resolution honors the pinned graph.
    #[must_use]
    pub fn to_registry_set(
        &self,
        live: &awaken_runtime::registry::RegistrySet,
    ) -> awaken_runtime::registry::RegistrySet {
        let models = Arc::new(PinnedModelRegistry::new(
            self.models.clone(),
            self.pools.clone(),
        )) as Arc<dyn awaken_runtime::registry::ModelRegistry>;
        awaken_runtime::registry::RegistrySet {
            agents: self.agents.clone() as Arc<dyn awaken_runtime::registry::AgentSpecRegistry>,
            models,
            tools: live.tools.clone(),
            providers: live.providers.clone(),
            plugins: live.plugins.clone(),
            backends: live.backends.clone(),
        }
    }
}

pub struct FrozenAgentRegistryMaterializer {
    store: Arc<dyn VersionedRegistryStore>,
    validator: StandardRegistryGraphValidator,
}

pub struct ScopedServerResolver {
    scope_id: ScopeId,
    materializer: FrozenAgentRegistryMaterializer,
    registry_handle: RegistryHandle,
}

#[derive(Clone)]
pub struct ScopedServerResolverFactory {
    store: Arc<dyn VersionedRegistryStore>,
    registry_handle: RegistryHandle,
}

impl ScopedServerResolverFactory {
    #[must_use]
    pub fn new(store: Arc<dyn VersionedRegistryStore>, registry_handle: RegistryHandle) -> Self {
        Self {
            store,
            registry_handle,
        }
    }

    #[must_use]
    pub fn resolver_for_scope(&self, scope_id: ScopeId) -> Arc<dyn Resolver> {
        Arc::new(ScopedServerResolver::new(
            scope_id,
            self.store.clone(),
            self.registry_handle.clone(),
        ))
    }

    #[must_use]
    pub fn resolver_for_context(&self, scope: &ScopeContext) -> Arc<dyn Resolver> {
        self.resolver_for_scope(scope.scope_id.clone())
    }
}

impl ScopedServerResolver {
    #[must_use]
    pub fn new(
        scope_id: ScopeId,
        store: Arc<dyn VersionedRegistryStore>,
        registry_handle: RegistryHandle,
    ) -> Self {
        Self {
            scope_id,
            materializer: FrozenAgentRegistryMaterializer::new(store),
            registry_handle,
        }
    }

    #[must_use]
    pub fn from_scope_context(
        scope: ScopeContext,
        store: Arc<dyn VersionedRegistryStore>,
        registry_handle: RegistryHandle,
    ) -> Self {
        Self::new(scope.scope_id, store, registry_handle)
    }

    #[must_use]
    pub fn scope_id(&self) -> &ScopeId {
        &self.scope_id
    }
}

#[async_trait::async_trait]
impl Resolver for ScopedServerResolver {
    async fn resolve(
        &self,
        mut request: ResolutionRequest,
    ) -> Result<ResolvedRunPlan, ResolveError> {
        let live = self.registry_handle.snapshot().into_registries();
        if matches!(request.resolution_scope, RegistryResolutionScope::Live)
            && request.features.requested_persistence == PersistenceRequirement::NotRequired
        {
            return awaken_runtime::registry::resolve::RegistrySetResolver::new(live)
                .resolve(request)
                .await;
        }
        let selector = match request.resolution_scope.clone() {
            RegistryResolutionScope::Live => VersionSelector::LatestPublication {
                scope_id: self.scope_id.as_str().to_string(),
            },
            // The runtime carries an opaque resolution id; for the published
            // path it is the publication snapshot version.
            RegistryResolutionScope::Pinned(resolution_id) => match resolution_id.parse::<u64>() {
                Ok(snapshot_version) => VersionSelector::Publication {
                    scope_id: self.scope_id.as_str().to_string(),
                    snapshot_version,
                },
                Err(error) => {
                    return Err(ResolveError::Runtime(format!(
                        "invalid pinned registry resolution id '{resolution_id}': {error}"
                    )));
                }
            },
        };
        let frozen = self
            .materializer
            .materialize(selector)
            .await
            .map_err(|error| ResolveError::Runtime(error.to_string()))?;
        let snapshot_version = frozen.manifest.registry_snapshot_version.ok_or_else(|| {
            ResolveError::Runtime(
                "published registry manifest is missing registry_snapshot_version".to_string(),
            )
        })?;
        request.resolution_scope = RegistryResolutionScope::Pinned(snapshot_version.to_string());
        awaken_runtime::registry::resolve::RegistrySetResolver::new_replayable_snapshot(
            frozen.to_registry_set(&live),
        )
        .resolve(request)
        .await
    }
}

pub struct LatestPublicationResolver {
    inner: ScopedServerResolver,
}

impl LatestPublicationResolver {
    #[must_use]
    pub fn new(
        scope_id: impl Into<String>,
        store: Arc<dyn VersionedRegistryStore>,
        registry_handle: RegistryHandle,
    ) -> Self {
        Self::try_new(scope_id, store, registry_handle).expect("scope_id must be valid")
    }

    pub fn try_new(
        scope_id: impl Into<String>,
        store: Arc<dyn VersionedRegistryStore>,
        registry_handle: RegistryHandle,
    ) -> Result<Self, ScopeError> {
        Ok(Self {
            inner: ScopedServerResolver::new(
                ScopeId::new(scope_id.into())?,
                store,
                registry_handle,
            ),
        })
    }

    #[must_use]
    pub fn scope_id(&self) -> &ScopeId {
        self.inner.scope_id()
    }
}

#[async_trait::async_trait]
impl Resolver for LatestPublicationResolver {
    async fn resolve(&self, request: ResolutionRequest) -> Result<ResolvedRunPlan, ResolveError> {
        self.inner.resolve(request).await
    }
}

impl FrozenAgentRegistryMaterializer {
    #[must_use]
    pub fn new(store: Arc<dyn VersionedRegistryStore>) -> Self {
        Self {
            validator: StandardRegistryGraphValidator::new(Arc::clone(&store)),
            store,
        }
    }

    pub async fn materialize(
        &self,
        selector: VersionSelector,
    ) -> Result<FrozenAgentRegistry, FrozenRegistryMaterializationError> {
        let base_manifest = self.base_manifest(&selector).await?;
        let scope_id = selector_scope_id(&selector);
        let report = self
            .validator
            .validate(RegistryGraphValidationRequest {
                root: selector,
                reference_policy: Default::default(),
            })
            .await?;
        let manifest = PinnedRegistryManifest {
            publication_id: base_manifest
                .as_ref()
                .and_then(|manifest| manifest.publication_id.clone()),
            registry_snapshot_version: base_manifest
                .as_ref()
                .and_then(|manifest| manifest.registry_snapshot_version),
            entries: report.entries.clone(),
        };
        let agents = self.load_pinned_agents(&scope_id, &report.entries).await?;
        let models = self
            .load_pinned_kind::<ModelSpec>(
                &scope_id,
                &report.entries,
                REGISTRY_KIND_MODEL,
                |spec| spec.id.clone(),
            )
            .await?;
        let pools = self
            .load_pinned_kind::<ModelPoolSpec>(
                &scope_id,
                &report.entries,
                REGISTRY_KIND_MODEL_POOL,
                |spec| spec.id.clone(),
            )
            .await?;
        let providers = self
            .load_pinned_kind::<ProviderSpec>(
                &scope_id,
                &report.entries,
                REGISTRY_KIND_PROVIDER,
                |spec| spec.id.clone(),
            )
            .await?;
        let skills = self
            .load_pinned_kind::<SkillSpec>(
                &scope_id,
                &report.entries,
                REGISTRY_KIND_SKILL,
                |spec| spec.id.clone(),
            )
            .await?;
        let tools = self
            .load_pinned_kind::<ToolSpec>(&scope_id, &report.entries, REGISTRY_KIND_TOOL, |spec| {
                spec.id.clone()
            })
            .await?;
        // plugin_config payloads have no canonical Rust type, so they are
        // keyed by the pinned entry id rather than an inner field.
        let plugin_configs = self
            .load_pinned_kind::<Value>(
                &scope_id,
                &report.entries,
                REGISTRY_KIND_PLUGIN_CONFIG,
                |_| String::new(),
            )
            .await?;
        Ok(FrozenAgentRegistry {
            manifest,
            agents: Arc::new(agents),
            models: Arc::new(models),
            pools: Arc::new(pools),
            providers: Arc::new(providers),
            skills: Arc::new(skills),
            tools: Arc::new(tools),
            plugin_configs: Arc::new(plugin_configs),
        })
    }

    async fn load_pinned_kind<T: DeserializeOwned>(
        &self,
        scope_id: &str,
        entries: &[PinnedRegistryEntry],
        kind: &'static str,
        spec_id: impl Fn(&T) -> String,
    ) -> Result<PinnedSpecMap<T>, FrozenRegistryMaterializationError> {
        let mut map: PinnedSpecMap<T> = PinnedSpecMap::new(kind);
        for entry in entries.iter().filter(|entry| entry.kind == kind) {
            let record = self
                .store
                .get(scope_id, &entry.kind, &entry.id, entry.version)
                .await?
                .ok_or_else(|| RegistryGraphValidationError::MissingVersion {
                    kind: entry.kind.clone(),
                    id: entry.id.clone(),
                    version: entry.version,
                })?;
            self.verify_record_against_entry(&record, entry)?;
            let spec: T = serde_json::from_value(record.value).map_err(|error| {
                RegistryGraphValidationError::InvalidReference {
                    kind: entry.kind.clone(),
                    id: entry.id.clone(),
                    reason: format!("invalid {kind} spec: {error}"),
                }
            })?;
            let derived_id = spec_id(&spec);
            let key = if derived_id.is_empty() {
                entry.id.clone()
            } else {
                derived_id
            };
            map.insert(key, spec, entry.clone())?;
        }
        Ok(map)
    }

    fn verify_record_against_entry(
        &self,
        record: &VersionedRecord<Value>,
        entry: &PinnedRegistryEntry,
    ) -> Result<(), FrozenRegistryMaterializationError> {
        record
            .verify_content_hash()
            .map_err(|error| RegistryGraphValidationError::Backend(error.to_string()))?;
        if record.content_hash != entry.content_hash {
            return Err(RegistryGraphValidationError::ContentHashMismatch {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
                version: entry.version,
                expected: entry.content_hash.clone(),
                actual: record.content_hash.clone(),
            }
            .into());
        }
        Ok(())
    }

    async fn base_manifest(
        &self,
        selector: &VersionSelector,
    ) -> Result<Option<PinnedRegistryManifest>, FrozenRegistryMaterializationError> {
        match selector {
            VersionSelector::LatestPublication { scope_id } => Ok(self
                .store
                .latest_pinned_manifest(scope_id)
                .await?
                .ok_or_else(|| RegistryGraphValidationError::MissingResource {
                    kind: "publication".to_string(),
                    id: "latest".to_string(),
                })?
                .into()),
            VersionSelector::Publication {
                scope_id,
                snapshot_version,
            } => Ok(self
                .store
                .pinned_manifest_for_publication(scope_id, *snapshot_version)
                .await?
                .ok_or_else(|| RegistryGraphValidationError::MissingVersion {
                    kind: "publication".to_string(),
                    id: scope_id.clone(),
                    version: *snapshot_version,
                })?
                .into()),
            VersionSelector::Manifest { manifest, .. } => Ok(Some(manifest.clone())),
            VersionSelector::Exact { .. } => Ok(None),
        }
    }

    async fn load_pinned_agents(
        &self,
        scope_id: &str,
        entries: &[PinnedRegistryEntry],
    ) -> Result<PinnedAgentSpecRegistry, FrozenRegistryMaterializationError> {
        let mut pinned_agents = Vec::new();
        for entry in entries
            .iter()
            .filter(|entry| entry.kind == REGISTRY_KIND_AGENT)
        {
            let record = self
                .store
                .get(scope_id, &entry.kind, &entry.id, entry.version)
                .await?
                .ok_or_else(|| RegistryGraphValidationError::MissingVersion {
                    kind: entry.kind.clone(),
                    id: entry.id.clone(),
                    version: entry.version,
                })?;
            // ADR-0035 D9: resume must recompute the hash and reject any
            // record whose stored bytes no longer match its content_hash,
            // and also reject any drift between the pinned entry hash and
            // the stored hash. The graph validator already runs this check,
            // but loading happens separately and a concurrent column rewrite
            // would otherwise be loaded without notice.
            self.verify_record_against_entry(&record, entry)?;
            let spec = serde_json::from_value::<AgentSpec>(record.value).map_err(|error| {
                RegistryGraphValidationError::InvalidReference {
                    kind: entry.kind.clone(),
                    id: entry.id.clone(),
                    reason: format!("invalid AgentSpec: {error}"),
                }
            })?;
            pinned_agents.push((spec, entry.clone()));
        }
        if pinned_agents.is_empty() {
            return Err(FrozenRegistryMaterializationError::InvalidGraph(
                "frozen agent registry requires at least one agent".to_string(),
            ));
        }
        Ok(PinnedAgentSpecRegistry::from_pinned_agents(pinned_agents)?)
    }
}

fn selector_scope_id(selector: &VersionSelector) -> String {
    match selector {
        VersionSelector::LatestPublication { scope_id }
        | VersionSelector::Publication { scope_id, .. }
        | VersionSelector::Exact { scope_id, .. }
        | VersionSelector::Manifest { scope_id, .. } => scope_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime::registry::{
        AgentSpecRegistry, MapAgentSpecRegistry, MapModelRegistry, MapPluginSource,
        MapProviderRegistry, MapToolRegistry, RegistrySet,
    };
    use awaken_runtime::registry::{BackendRegistry, MapBackendRegistry};
    use awaken_runtime::resolution::{
        DelegatePersistence, ExecutionRole, HandoffTranscriptRef, ResolutionTarget, RunFeatureSet,
    };
    use awaken_server_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_server_contract::contract::identity::RunIdentity;
    use awaken_server_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_server_contract::contract::versioned_registry::PublishOutcome;
    use awaken_server_contract::{ModelSpec, ProviderSpec, VersionRef};
    use awaken_stores::InMemoryVersionedRegistryStore;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn materializes_latest_publication_into_pinned_agent_registry() {
        let store = InMemoryVersionedRegistryStore::new();
        let provider = publish_provider(&store, "provider-1").await;
        let model = publish_model(&store, "model-1", "provider-1").await;
        let delegate = publish_agent(&store, agent("delegate", "model-1", [])).await;
        let root = publish_agent(&store, agent("root", "model-1", ["delegate"])).await;
        store
            .create_publication(
                "default",
                "pub-1",
                refs([&provider, &model, &delegate, &root]),
                Vec::new(),
                None,
                json!({}),
            )
            .await
            .unwrap();

        let materializer = FrozenAgentRegistryMaterializer::new(Arc::new(store));
        let frozen = materializer
            .materialize(VersionSelector::LatestPublication {
                scope_id: "default".to_string(),
            })
            .await
            .unwrap();

        assert_eq!(frozen.manifest.publication_id.as_deref(), Some("pub-1"));
        assert_eq!(frozen.manifest.registry_snapshot_version, Some(1));
        assert_eq!(frozen.agents.get_agent("root").unwrap().id, "root");
        assert_eq!(
            frozen.agents.pin_for_agent("delegate").unwrap().version,
            delegate.version
        );
    }

    #[tokio::test]
    async fn scoped_server_resolver_resolves_delegate_from_current_scope() {
        let store = InMemoryVersionedRegistryStore::new();
        let provider = publish_provider(&store, "provider-1").await;
        let model = publish_model(&store, "model-1", "provider-1").await;
        let delegate = publish_agent(&store, agent("delegate", "model-1", [])).await;
        let root = publish_agent(&store, agent("root", "model-1", ["delegate"])).await;
        store
            .create_publication(
                "default",
                "pub-1",
                refs([&provider, &model, &delegate, &root]),
                Vec::new(),
                None,
                json!({}),
            )
            .await
            .unwrap();

        let resolver = ScopedServerResolver::from_scope_context(
            ScopeContext::default_scope(),
            Arc::new(store),
            live_registry_handle(),
        );
        let plan = resolver
            .resolve(nested_request(ResolutionTarget::Delegate {
                agent_id: "delegate".to_string(),
                parent_run: RunIdentity::for_thread("thread-1"),
                persistence: DelegatePersistence::Ephemeral,
            }))
            .await
            .unwrap();

        assert_eq!(resolver.scope_id().as_str(), "default");
        assert_eq!(plan.role(), ExecutionRole::Delegate);
        assert_eq!(plan.agent_spec().id, "delegate");
        // A published-registry resolution is replayable: it carries a
        // server-issued resolution id (the publication snapshot version).
        assert!(plan.resolution_id().is_some());
    }

    #[tokio::test]
    async fn scoped_server_resolver_resolves_handoff_from_current_scope() {
        let store = InMemoryVersionedRegistryStore::new();
        let provider = publish_provider(&store, "provider-1").await;
        let model = publish_model(&store, "model-1", "provider-1").await;
        let handoff = publish_agent(&store, agent("handoff", "model-1", [])).await;
        let root = publish_agent(&store, agent("root", "model-1", [])).await;
        store
            .create_publication(
                "default",
                "pub-1",
                refs([&provider, &model, &handoff, &root]),
                Vec::new(),
                None,
                json!({}),
            )
            .await
            .unwrap();

        let resolver = ScopedServerResolver::new(
            ScopeId::default_scope(),
            Arc::new(store),
            live_registry_handle(),
        );
        let plan = resolver
            .resolve(nested_request(ResolutionTarget::Handoff {
                agent_id: "handoff".to_string(),
                from_agent: "root".to_string(),
                transcript_ref: HandoffTranscriptRef {
                    run_id: "run-1".to_string(),
                },
            }))
            .await
            .unwrap();

        assert_eq!(plan.role(), ExecutionRole::Handoff);
        assert_eq!(plan.agent_spec().id, "handoff");
        assert!(matches!(plan, ResolvedRunPlan::Replayable(_)));
    }

    #[tokio::test]
    async fn scoped_server_resolver_round_trips_resolution_id_on_resume() {
        let store = InMemoryVersionedRegistryStore::new();
        let provider = publish_provider(&store, "provider-1").await;
        let model = publish_model(&store, "model-1", "provider-1").await;
        let root = publish_agent(&store, agent("root", "model-1", [])).await;
        store
            .create_publication(
                "default",
                "pub-1",
                refs([&provider, &model, &root]),
                Vec::new(),
                None,
                json!({}),
            )
            .await
            .unwrap();

        let resolver = ScopedServerResolver::new(
            ScopeId::default_scope(),
            Arc::new(store),
            live_registry_handle(),
        );

        // First (live) resolution pins the latest publication and surfaces an
        // opaque resolution id (the publication snapshot version).
        let plan = resolver
            .resolve(nested_request(ResolutionTarget::Root {
                agent_id: "root".into(),
                thread_id: "thread-1".into(),
            }))
            .await
            .unwrap();
        let resolution_id = plan
            .resolution_id()
            .expect("published resolution is pinned")
            .to_string();

        // Resuming with that id re-selects the same publication via the
        // `Pinned` path (VersionSelector::Publication), round-tripping the id.
        let mut resume = nested_request(ResolutionTarget::Root {
            agent_id: "root".into(),
            thread_id: "thread-1".into(),
        });
        resume.resolution_scope = RegistryResolutionScope::Pinned(resolution_id.clone());
        let resumed = resolver.resolve(resume).await.unwrap();
        assert_eq!(resumed.resolution_id(), Some(resolution_id.as_str()));
        assert_eq!(resumed.agent_spec().id, "root");
    }

    #[tokio::test]
    async fn scoped_server_resolver_rejects_invalid_pinned_resolution_id() {
        let store = InMemoryVersionedRegistryStore::new();
        let provider = publish_provider(&store, "provider-1").await;
        let model = publish_model(&store, "model-1", "provider-1").await;
        let root = publish_agent(&store, agent("root", "model-1", [])).await;
        store
            .create_publication(
                "default",
                "pub-1",
                refs([&provider, &model, &root]),
                Vec::new(),
                None,
                json!({}),
            )
            .await
            .unwrap();

        let resolver = ScopedServerResolver::new(
            ScopeId::default_scope(),
            Arc::new(store),
            live_registry_handle(),
        );
        let mut resume = nested_request(ResolutionTarget::Root {
            agent_id: "root".into(),
            thread_id: "thread-1".into(),
        });
        resume.resolution_scope = RegistryResolutionScope::Pinned("not-a-version".into());

        let error = match resolver.resolve(resume).await {
            Ok(_) => panic!("invalid pinned resolution id must fail"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("invalid pinned registry resolution id"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn scoped_server_resolver_factory_binds_scope_per_resolver() {
        let store = Arc::new(InMemoryVersionedRegistryStore::new());
        let scope_a_provider = publish_provider_in_scope(&store, "scope-a", "provider-1").await;
        let scope_a_model =
            publish_model_in_scope(&store, "scope-a", "model-1", "provider-1").await;
        let scope_a_root =
            publish_agent_in_scope(&store, "scope-a", agent_with_prompt("root", "model-1", "a"))
                .await;
        store
            .create_publication(
                "scope-a",
                "pub-a",
                refs([&scope_a_provider, &scope_a_model, &scope_a_root]),
                Vec::new(),
                None,
                json!({}),
            )
            .await
            .unwrap();

        let scope_b_provider = publish_provider_in_scope(&store, "scope-b", "provider-1").await;
        let scope_b_model =
            publish_model_in_scope(&store, "scope-b", "model-1", "provider-1").await;
        let scope_b_root =
            publish_agent_in_scope(&store, "scope-b", agent_with_prompt("root", "model-1", "b"))
                .await;
        store
            .create_publication(
                "scope-b",
                "pub-b",
                refs([&scope_b_provider, &scope_b_model, &scope_b_root]),
                Vec::new(),
                None,
                json!({}),
            )
            .await
            .unwrap();

        let factory = ScopedServerResolverFactory::new(store, live_registry_handle());
        let plan_a = factory
            .resolver_for_scope(ScopeId::new("scope-a").unwrap())
            .resolve(nested_request(ResolutionTarget::Root {
                agent_id: "root".into(),
                thread_id: "thread-a".into(),
            }))
            .await
            .unwrap();
        let plan_b = factory
            .resolver_for_scope(ScopeId::new("scope-b").unwrap())
            .resolve(nested_request(ResolutionTarget::Root {
                agent_id: "root".into(),
                thread_id: "thread-b".into(),
            }))
            .await
            .unwrap();

        assert_eq!(plan_a.agent_spec().system_prompt, "a");
        assert_eq!(plan_b.agent_spec().system_prompt, "b");
    }

    #[tokio::test]
    async fn latest_publication_resolver_rejects_invalid_scope_id() {
        let store = InMemoryVersionedRegistryStore::new();
        let result =
            LatestPublicationResolver::try_new(" ", Arc::new(store), live_registry_handle());

        assert!(matches!(result, Err(ScopeError::Empty)));
    }

    #[tokio::test]
    async fn materializes_pool_reachable_from_agent() {
        let store = InMemoryVersionedRegistryStore::new();
        let provider = publish_provider(&store, "provider-1").await;
        let m0 = publish_model(&store, "m0", "provider-1").await;
        let m1 = publish_model(&store, "m1", "provider-1").await;
        let pool = publish_model_pool(&store, "pool-1", ["m0", "m1"]).await;
        let root = publish_agent(&store, agent("root", "pool-1", [])).await;
        store
            .create_publication(
                "default",
                "pub-1",
                refs([&provider, &m0, &m1, &pool, &root]),
                Vec::new(),
                None,
                json!({}),
            )
            .await
            .unwrap();

        let materializer = FrozenAgentRegistryMaterializer::new(Arc::new(store));
        let frozen = materializer
            .materialize(VersionSelector::LatestPublication {
                scope_id: "default".to_string(),
            })
            .await
            .unwrap();

        // The pool and its member models are frozen for the run.
        assert_eq!(frozen.pools.get("pool-1").unwrap().members.len(), 2);
        assert!(frozen.models.get("m0").is_some());
        assert!(frozen.models.get("m1").is_some());
        assert!(frozen.manifest.entries.iter().any(|entry| {
            entry.kind == awaken_server_contract::REGISTRY_KIND_MODEL_POOL && entry.id == "pool-1"
        }));
    }

    #[tokio::test]
    async fn materializes_exact_agent_with_current_references() {
        let store = InMemoryVersionedRegistryStore::new();
        publish_provider(&store, "provider-1").await;
        publish_model(&store, "model-1", "provider-1").await;
        let root = publish_agent(&store, agent("root", "model-1", [])).await;

        let materializer = FrozenAgentRegistryMaterializer::new(Arc::new(store));
        let frozen = materializer
            .materialize(VersionSelector::Exact {
                scope_id: "default".to_string(),
                kind: "agent".to_string(),
                id: "root".to_string(),
                version: root.version,
            })
            .await
            .unwrap();

        assert!(frozen.manifest.publication_id.is_none());
        assert_eq!(frozen.agents.pin_for_agent("root").unwrap().version, 1);
        assert!(frozen.manifest.entries.iter().any(|entry| {
            entry.kind == awaken_server_contract::REGISTRY_KIND_MODEL && entry.id == "model-1"
        }));
    }

    #[tokio::test]
    async fn rejects_graphs_without_agents() {
        let store = InMemoryVersionedRegistryStore::new();
        let provider = publish_provider(&store, "provider-1").await;
        let manifest = PinnedRegistryManifest {
            publication_id: None,
            registry_snapshot_version: None,
            entries: vec![provider],
        };
        let materializer = FrozenAgentRegistryMaterializer::new(Arc::new(store));
        let error = materialization_error(
            materializer
                .materialize(VersionSelector::Manifest {
                    scope_id: "default".to_string(),
                    manifest,
                })
                .await,
        );

        assert!(matches!(
            error,
            FrozenRegistryMaterializationError::InvalidGraph(message)
                if message.contains("at least one agent")
        ));
    }

    /// ADR-0035 D9: the resume path must reject a manifest whose stored
    /// `content_hash` diverges from the published canonical bytes. The
    /// validator alone is unit-tested in `registry_graph_validator.rs`,
    /// but only the materializer guarantees that the resume entry point
    /// actually runs the check before handing a frozen registry to the
    /// runtime.
    #[tokio::test]
    async fn materialize_rejects_manifest_drift() {
        let store = InMemoryVersionedRegistryStore::new();
        let provider = publish_provider(&store, "provider-1").await;
        let model = publish_model(&store, "model-1", "provider-1").await;
        let root = publish_agent(&store, agent("root", "model-1", [])).await;

        let mut tampered_root = root.clone();
        tampered_root.content_hash = "sha256:deadbeef".to_string();
        let manifest = PinnedRegistryManifest {
            publication_id: None,
            registry_snapshot_version: None,
            entries: vec![tampered_root, model, provider],
        };

        let materializer = FrozenAgentRegistryMaterializer::new(Arc::new(store));
        let error = materialization_error(
            materializer
                .materialize(VersionSelector::Manifest {
                    scope_id: "default".to_string(),
                    manifest,
                })
                .await,
        );

        match error {
            FrozenRegistryMaterializationError::Graph(
                RegistryGraphValidationError::ContentHashMismatch {
                    kind, id, expected, ..
                },
            ) => {
                assert_eq!(kind, "agent");
                assert_eq!(id, "root");
                assert_eq!(expected, "sha256:deadbeef");
            }
            other => panic!("expected Graph(ContentHashMismatch), got {other:?}"),
        }
    }

    struct StubExecutor;

    #[async_trait::async_trait]
    impl LlmExecutor for StubExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: Vec::new(),
                tool_calls: Vec::new(),
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "stub"
        }
    }

    fn nested_request(target: ResolutionTarget) -> ResolutionRequest {
        ResolutionRequest {
            target,
            resolution_scope: RegistryResolutionScope::Live,
            overrides: None,
            frontend_tools: Vec::new(),
            features: RunFeatureSet {
                requested_persistence: PersistenceRequirement::CheckpointRequired,
                ..Default::default()
            },
        }
    }

    fn live_registry_handle() -> RegistryHandle {
        let mut providers = MapProviderRegistry::new();
        providers
            .register_provider("provider-1", Arc::new(StubExecutor))
            .unwrap();
        RegistryHandle::new(RegistrySet {
            agents: Arc::new(MapAgentSpecRegistry::new()),
            tools: Arc::new(MapToolRegistry::new()),
            models: Arc::new(MapModelRegistry::new()),
            providers: Arc::new(providers),
            plugins: Arc::new(MapPluginSource::new()),
            backends: Arc::new(MapBackendRegistry::with_default_remote_backends())
                as Arc<dyn BackendRegistry>,
        })
    }

    async fn publish_agent(
        store: &InMemoryVersionedRegistryStore,
        spec: AgentSpec,
    ) -> PinnedRegistryEntry {
        let id = spec.id.clone();
        publish(store, "agent", &id, serde_json::to_value(spec).unwrap()).await
    }

    async fn publish_agent_in_scope(
        store: &InMemoryVersionedRegistryStore,
        scope_id: &str,
        spec: AgentSpec,
    ) -> PinnedRegistryEntry {
        let id = spec.id.clone();
        publish_in_scope(
            store,
            scope_id,
            "agent",
            &id,
            serde_json::to_value(spec).unwrap(),
        )
        .await
    }

    async fn publish_model(
        store: &InMemoryVersionedRegistryStore,
        id: &str,
        provider_id: &str,
    ) -> PinnedRegistryEntry {
        let spec = ModelSpec::new(id, provider_id, "upstream");
        publish(store, "model", id, serde_json::to_value(spec).unwrap()).await
    }

    async fn publish_model_in_scope(
        store: &InMemoryVersionedRegistryStore,
        scope_id: &str,
        id: &str,
        provider_id: &str,
    ) -> PinnedRegistryEntry {
        let spec = ModelSpec::new(id, provider_id, "upstream");
        publish_in_scope(
            store,
            scope_id,
            "model",
            id,
            serde_json::to_value(spec).unwrap(),
        )
        .await
    }

    async fn publish_model_pool<'a>(
        store: &InMemoryVersionedRegistryStore,
        id: &str,
        members: impl IntoIterator<Item = &'a str>,
    ) -> PinnedRegistryEntry {
        let spec = ModelPoolSpec::new(id, members);
        publish(store, "model_pool", id, serde_json::to_value(spec).unwrap()).await
    }

    async fn publish_provider(
        store: &InMemoryVersionedRegistryStore,
        id: &str,
    ) -> PinnedRegistryEntry {
        let spec = ProviderSpec {
            id: id.to_string(),
            adapter: "openai".to_string(),
            ..Default::default()
        };
        publish(store, "provider", id, serde_json::to_value(spec).unwrap()).await
    }

    async fn publish_provider_in_scope(
        store: &InMemoryVersionedRegistryStore,
        scope_id: &str,
        id: &str,
    ) -> PinnedRegistryEntry {
        let spec = ProviderSpec {
            id: id.to_string(),
            adapter: "openai".to_string(),
            ..Default::default()
        };
        publish_in_scope(
            store,
            scope_id,
            "provider",
            id,
            serde_json::to_value(spec).unwrap(),
        )
        .await
    }

    async fn publish(
        store: &InMemoryVersionedRegistryStore,
        kind: &str,
        id: &str,
        value: Value,
    ) -> PinnedRegistryEntry {
        publish_in_scope(store, "default", kind, id, value).await
    }

    async fn publish_in_scope(
        store: &InMemoryVersionedRegistryStore,
        scope_id: &str,
        kind: &str,
        id: &str,
        value: Value,
    ) -> PinnedRegistryEntry {
        let outcome = store
            .publish_resource(scope_id, kind, id, value, 1, json!({}))
            .await
            .unwrap();
        let record = match outcome {
            PublishOutcome::Created(record) | PublishOutcome::Noop(record) => record,
        };
        PinnedRegistryEntry {
            kind: kind.to_string(),
            id: id.to_string(),
            version: record.version,
            content_hash: record.content_hash,
        }
    }

    fn agent<'a>(
        id: &str,
        model_id: &str,
        delegates: impl IntoIterator<Item = &'a str>,
    ) -> AgentSpec {
        AgentSpec {
            id: id.to_string(),
            model_id: model_id.to_string(),
            system_prompt: "system".to_string(),
            delegates: delegates.into_iter().map(str::to_string).collect(),
            ..Default::default()
        }
    }

    fn agent_with_prompt(id: &str, model_id: &str, system_prompt: &str) -> AgentSpec {
        AgentSpec {
            id: id.to_string(),
            model_id: model_id.to_string(),
            system_prompt: system_prompt.to_string(),
            ..Default::default()
        }
    }

    fn refs<'a>(entries: impl IntoIterator<Item = &'a PinnedRegistryEntry>) -> Vec<VersionRef> {
        entries
            .into_iter()
            .map(|entry| VersionRef {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
                version: entry.version,
            })
            .collect()
    }

    fn materialization_error(
        result: Result<FrozenAgentRegistry, FrozenRegistryMaterializationError>,
    ) -> FrozenRegistryMaterializationError {
        match result {
            Ok(_) => panic!("expected frozen registry materialization error"),
            Err(error) => error,
        }
    }
}
