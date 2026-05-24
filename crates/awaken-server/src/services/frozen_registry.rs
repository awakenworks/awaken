use std::sync::Arc;

use awaken_contract::contract::run::RunResolutionScope;
use awaken_contract::contract::versioned_registry::VersionedRecord;
use awaken_contract::skill_spec::SkillSpec;
use awaken_contract::tool_spec::ToolSpec;
use awaken_contract::{
    AgentSpec, ModelSpec, PinnedRegistryEntry, PinnedRegistryManifest, ProviderSpec,
    REGISTRY_KIND_AGENT, REGISTRY_KIND_MODEL, REGISTRY_KIND_PLUGIN_CONFIG, REGISTRY_KIND_PROVIDER,
    REGISTRY_KIND_SKILL, REGISTRY_KIND_TOOL, RegistryGraphValidationError,
    RegistryGraphValidationRequest, RegistryGraphValidator, StandardRegistryGraphValidator,
    VersionSelector, VersionedRegistryError, VersionedRegistryStore,
};
use awaken_runtime::registry::{
    PinnedAgentSpecRegistry, PinnedRegistryError, PinnedSpecMap, RegistryHandle,
};
use awaken_runtime::resolution::{
    PersistenceRequirement, ResolutionRequest, ResolveError, ResolvedRunPlan, Resolver,
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
        awaken_runtime::registry::RegistrySet {
            agents: self.agents.clone() as Arc<dyn awaken_runtime::registry::AgentSpecRegistry>,
            models: self.models.clone() as Arc<dyn awaken_runtime::registry::ModelRegistry>,
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

pub struct LatestPublicationResolver {
    scope_id: String,
    materializer: FrozenAgentRegistryMaterializer,
    registry_handle: RegistryHandle,
}

impl LatestPublicationResolver {
    #[must_use]
    pub fn new(
        scope_id: impl Into<String>,
        store: Arc<dyn VersionedRegistryStore>,
        registry_handle: RegistryHandle,
    ) -> Self {
        Self {
            scope_id: scope_id.into(),
            materializer: FrozenAgentRegistryMaterializer::new(store),
            registry_handle,
        }
    }
}

#[async_trait::async_trait]
impl Resolver for LatestPublicationResolver {
    async fn resolve(
        &self,
        mut request: ResolutionRequest,
    ) -> Result<ResolvedRunPlan, ResolveError> {
        let live = self.registry_handle.snapshot().into_registries();
        if matches!(request.resolution_scope, RunResolutionScope::Live)
            && request.features.requested_persistence == PersistenceRequirement::NotRequired
        {
            return awaken_runtime::registry::resolve::RegistrySetResolver::new(live)
                .resolve(request)
                .await;
        }
        let selector = match request.resolution_scope.clone() {
            RunResolutionScope::Live => VersionSelector::LatestPublication {
                scope_id: self.scope_id.clone(),
            },
            RunResolutionScope::Pinned(manifest) => VersionSelector::Manifest {
                scope_id: self.scope_id.clone(),
                manifest,
            },
        };
        let frozen = self
            .materializer
            .materialize(selector)
            .await
            .map_err(|error| ResolveError::Runtime(error.to_string()))?;
        request.resolution_scope = RunResolutionScope::Pinned(frozen.manifest.clone());
        awaken_runtime::registry::resolve::RegistrySetResolver::new(frozen.to_registry_set(&live))
            .resolve(request)
            .await
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
    use awaken_contract::contract::versioned_registry::PublishOutcome;
    use awaken_contract::{ModelSpec, ProviderSpec, VersionRef};
    use awaken_runtime::registry::AgentSpecRegistry;
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
            entry.kind == awaken_contract::REGISTRY_KIND_MODEL && entry.id == "model-1"
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

    async fn publish_agent(
        store: &InMemoryVersionedRegistryStore,
        spec: AgentSpec,
    ) -> PinnedRegistryEntry {
        let id = spec.id.clone();
        publish(store, "agent", &id, serde_json::to_value(spec).unwrap()).await
    }

    async fn publish_model(
        store: &InMemoryVersionedRegistryStore,
        id: &str,
        provider_id: &str,
    ) -> PinnedRegistryEntry {
        let spec = ModelSpec::new(id, provider_id, "upstream");
        publish(store, "model", id, serde_json::to_value(spec).unwrap()).await
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

    async fn publish(
        store: &InMemoryVersionedRegistryStore,
        kind: &str,
        id: &str,
        value: Value,
    ) -> PinnedRegistryEntry {
        let outcome = store
            .publish_resource("default", kind, id, value, 1, json!({}))
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
