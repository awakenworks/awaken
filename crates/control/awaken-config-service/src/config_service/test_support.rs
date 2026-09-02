// Shared fixtures for Config Service owner and adapter tests.
//
// Structural cause/effect coverage rationale: moving an item changes neither
// runtime inputs nor domain decisions. The unchanged 105-test crate suite is
// the regression oracle for method ownership, helper visibility, and test
// identity; a runtime decision table is not applicable to this extraction.

use awaken_agent_config::{
    AgentConfig, AgentConfigRevision, AuditedConfigWrite, ConfigRegistry, ConfigStoreError,
    ConfigWrite, ModelSelection, ScopedConfig, StoredPublication,
};
use awaken_config_resolver::{
    AgentInputConfig, BindingId, InputBinding, InputResourceId, MemoryStoreId, ResourceAccess,
};
use awaken_config_store::SqliteConfigStore;
use awaken_executable_agent_catalog::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};
use awaken_executable_agent_contract::{
    ExecutableAgentRegistration, ExecutableAgentRegistrationError,
    ExecutableAgentRegistrationOutcome, ExecutableAgentWithdrawal,
    ExecutableAgentWithdrawalOutcome,
};
use awaken_runtime_contract::resolved::ContextPolicy;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;

use crate::binding_resolver::{PublicationResolutionError, ResolvedPublicationModels};
use crate::publication::{PublishError, prepare_agent_publication};
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
use awaken_tenancy::WorkspaceScope;

fn scope(id: &str) -> ScopeId {
    ScopeId::from(id)
}

pub(crate) fn agent_config(id: &str) -> AgentConfig {
    AgentConfig {
        id: id.to_string(),
        instructions: "be helpful".to_string(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b"),
        inference: Default::default(),
        tool_ids: vec![],
        model_fallbacks: Vec::new(),
        plugin_ids: vec![],
        plugin_config: Default::default(),
        context_policy: ContextPolicy::KeepAll,
        tool_patterns: Vec::new(),
        ..Default::default()
    }
}

fn auto_config(id: &str) -> AgentConfig {
    let mut cfg = agent_config(id);
    cfg.model_binding = ModelSelection::Auto;
    cfg
}

async fn resolve_config(service: &ConfigService, config: AgentConfig) -> AgentConfig {
    prepare_agent_publication(
        service.model_publication_resolver.as_ref(),
        &scope(DEFAULT_SCOPE),
        AgentConfigRevision {
            config,
            revision: 1,
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
        },
    )
    .await
    .unwrap()
    .config
}

struct FakeResolver;
#[async_trait::async_trait]
impl ModelPublicationResolver for FakeResolver {
    async fn resolve_models(
        &self,
        _workspace: &ScopeId,
        selection: &ModelSelection,
        candidates: &[ModelBinding],
    ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
        let (primary, candidates) = selection.resolved().map_or_else(
            || {
                (
                    ModelBinding::new("openai", "m-first", "genai"),
                    vec![ModelBinding::new("openai", "m-second", "genai")],
                )
            },
            |primary| (primary.clone(), candidates.to_vec()),
        );
        Ok(ResolvedPublicationModels::host(
            primary, candidates, None, None,
        ))
    }
}

struct ChangedPolicyResolver;

#[async_trait::async_trait]
impl ModelPublicationResolver for ChangedPolicyResolver {
    async fn resolve_models(
        &self,
        _workspace: &ScopeId,
        _selection: &ModelSelection,
        _candidates: &[ModelBinding],
    ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
        Ok(ResolvedPublicationModels::host(
            ModelBinding::new("openai", "m-policy-changed", "genai"),
            Vec::new(),
            None,
            None,
        ))
    }
}

/// A resolver whose catalog has no provider-backed model.
struct ErrResolver;

#[async_trait::async_trait]
impl ModelPublicationResolver for ErrResolver {
    async fn resolve_models(
        &self,
        _workspace: &ScopeId,
        _selection: &ModelSelection,
        _candidates: &[ModelBinding],
    ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
        Err("no provider-backed model in the catalog".into())
    }
}

fn test_registrar() -> Arc<dyn ExecutableAgentRegistrar> {
    Arc::new(LocalExecutableAgentRegistrar::new(Arc::new(
        ExecutableAgentCatalog::new(),
    )))
}

fn test_service_and_catalog() -> (ConfigService, Arc<ExecutableAgentCatalog>) {
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let registrar = Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone()));
    (
        ConfigService::new(Arc::new(FakeResolver), registrar),
        catalog,
    )
}

struct UnavailableRegistrar;

#[async_trait::async_trait]
impl ExecutableAgentRegistrar for UnavailableRegistrar {
    async fn register(
        &self,
        _registration: ExecutableAgentRegistration,
    ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError> {
        Err(ExecutableAgentRegistrationError::Unavailable(
            "Coordinator is offline".into(),
        ))
    }

    async fn withdraw(
        &self,
        _withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
        Err(ExecutableAgentRegistrationError::Unavailable(
            "Coordinator is offline".into(),
        ))
    }
}

pub(crate) fn test_service() -> ConfigService {
    ConfigService::new(Arc::new(FakeResolver), test_registrar())
}

struct FakeProviderResolver;

#[async_trait::async_trait]
impl ModelPublicationResolver for FakeProviderResolver {
    async fn resolve_models(
        &self,
        workspace: &ScopeId,
        selection: &ModelSelection,
        candidates: &[ModelBinding],
    ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
        let primary = selection
            .resolved()
            .cloned()
            .ok_or_else(|| "test requires a pinned model".to_string())?;
        let candidate =
            |binding: ModelBinding| {
                ResolvedModelCandidate::try_provider(
                binding.clone(),
                "provider@2",
                "endpoint@4",
                workspace.clone(),
                Some(
                    awaken_runtime_contract::CredentialAccess::new(
                        awaken_runtime_contract::CredentialRef {
                            id: format!("credential-{workspace}"),
                            revision: 3,
                        },
                        awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                        awaken_runtime_contract::CredentialUsage::ProviderAdapter,
                        awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
                    )
                    .with_target(awaken_runtime_contract::CredentialTarget::new(
                        awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                        "provider",
                    )),
                ),
                awaken_runtime_contract::InferenceEndpoint {
                    adapter_kind: "openai".into(),
                    api_dialect: "open_ai_chat".into(),
                    base_url: "https://example.invalid/v1".into(),
                    upstream_model: binding.model_ref,
                    processing_placement: None,
                },
            )
            };
        Ok(ResolvedPublicationModels {
            primary: candidate(primary).map_err(|error| error.to_string())?,
            candidates: candidates
                .iter()
                .cloned()
                .map(candidate)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?,
            context_window: None,
            max_output_tokens: None,
        })
    }
}

/// A config plane (the scope edge) over a fresh in-memory store, an optional
/// resolver override, an optional resource store, and the given tool catalog.
fn plane_with(
    tools: Arc<dyn ToolCatalogSource>,
    resolver: Option<Arc<dyn ModelPublicationResolver>>,
    resources: Option<Arc<dyn AgentInputBindingRepository>>,
) -> ConfigPlane {
    let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
    let mut service = ConfigService::new(
        resolver.unwrap_or_else(|| Arc::new(FakeResolver)),
        test_registrar(),
    );
    if let Some(resources) = resources {
        service = service.with_resources(resources);
    }
    ConfigPlane::new(Arc::new(service), store, tools)
}

fn static_plane(resolver: Option<Arc<dyn ModelPublicationResolver>>) -> ConfigPlane {
    plane_with(
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        resolver,
        None,
    )
}

fn legacy_permission_config(id: &str) -> AgentConfig {
    let mut config = agent_config(id);
    config.plugin_config.insert(
        "permission".into(),
        serde_json::json!({
            "default_behavior": "ask",
            "rules": [{ "pattern": "write", "behavior": "ask" }]
        }),
    );
    config
}

struct ArchiveBetweenAdmissionAndCas {
    current: std::sync::Mutex<AgentConfigRevision>,
    interleaved: std::sync::atomic::AtomicBool,
}

impl ArchiveBetweenAdmissionAndCas {
    fn new(config: AgentConfig) -> Self {
        Self {
            current: std::sync::Mutex::new(AgentConfigRevision {
                config,
                revision: 1,
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
            }),
            interleaved: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl ConfigRegistry for ArchiveBetweenAdmissionAndCas {
    async fn put_config(&self, _config: &AgentConfig) -> Result<(), ConfigStoreError> {
        panic!("mutable application writes must use revision CAS")
    }

    async fn put_config_if_revision(
        &self,
        config: &AgentConfig,
        expected_revision: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        let mut current = self.current.lock().unwrap();
        if current.revision != expected_revision {
            return Ok(ConfigWrite::Conflict {
                current_revision: Some(current.revision),
            });
        }
        current.config = config.clone();
        current.revision += 1;
        Ok(ConfigWrite::Applied {
            revision: current.revision,
        })
    }

    async fn get_config(&self, _id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
        Ok(Some(self.current.lock().unwrap().config.clone()))
    }

    async fn get_config_revision(
        &self,
        _id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
        let mut current = self.current.lock().unwrap();
        let observed = current.clone();
        if !self
            .interleaved
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            current.config.archived_at = Some("2026-08-30T00:00:00Z".into());
            current.revision += 1;
        }
        Ok(Some(observed))
    }

    async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        Ok(vec![self.current.lock().unwrap().config.clone()])
    }

    async fn put_publication(
        &self,
        _publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError> {
        panic!("test does not publish")
    }

    async fn put_publication_if_config_revision(
        &self,
        _publication: &StoredPublication,
        _expected_revision: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        panic!("test does not publish")
    }

    async fn get_publication(
        &self,
        _fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        Ok(None)
    }
}

// --- store-error paths (CEG P1 / P6 / reconcile-a / F19c / F21b) ----------
// A real SqliteConfigStore never fails its ops deterministically, so the
// fail-closed error arms need test doubles that return `ConfigStoreError`.

/// Every operation fails — drives the read-failure arms.
struct FailingRegistry;
#[async_trait::async_trait]
impl ConfigRegistry for FailingRegistry {
    async fn put_config(&self, _c: &AgentConfig) -> Result<(), ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn get_config(&self, _id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn get_config_revision(
        &self,
        _id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn put_publication(&self, _p: &StoredPublication) -> Result<(), ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn put_publication_if_config_revision(
        &self,
        _publication: &StoredPublication,
        _expected_revision: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn get_publication(
        &self,
        _fp: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
}

/// Reads a stored pinned config fine, but fails when persisting the publication
/// — isolates the `put_publication` → `Store` branch (P6).
struct PublishFailRegistry;
#[async_trait::async_trait]
impl ConfigRegistry for PublishFailRegistry {
    async fn put_config(&self, _c: &AgentConfig) -> Result<(), ConfigStoreError> {
        Ok(())
    }
    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
        Ok(Some(agent_config(id)))
    }
    async fn get_config_revision(
        &self,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
        Ok(Some(AgentConfigRevision {
            config: agent_config(id),
            revision: 1,
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
        }))
    }
    async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        Ok(vec![])
    }
    async fn put_publication(&self, _p: &StoredPublication) -> Result<(), ConfigStoreError> {
        Err(ConfigStoreError("publication store down".into()))
    }
    async fn put_publication_if_config_revision(
        &self,
        _publication: &StoredPublication,
        _expected_revision: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        Err(ConfigStoreError("publication store down".into()))
    }
    async fn get_publication(
        &self,
        _fp: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        Ok(None)
    }
}

/// Simulates an authoring write that wins after publish reads generation 7
/// but before it tries to persist/install the compiled artifact.
struct StalePublishRegistry;

#[async_trait::async_trait]
impl ConfigRegistry for StalePublishRegistry {
    async fn put_config(&self, _c: &AgentConfig) -> Result<(), ConfigStoreError> {
        Ok(())
    }

    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
        Ok(Some(agent_config(id)))
    }

    async fn get_config_revision(
        &self,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
        Ok(Some(AgentConfigRevision {
            config: agent_config(id),
            revision: 7,
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
        }))
    }

    async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        Ok(Vec::new())
    }

    async fn put_publication(&self, _p: &StoredPublication) -> Result<(), ConfigStoreError> {
        panic!("generation-fenced publish must use the atomic method")
    }

    async fn put_publication_if_config_revision(
        &self,
        publication: &StoredPublication,
        expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        assert_eq!(publication.source_revision, 7);
        assert_eq!(expected_generation, 7);
        Ok(ConfigWrite::Conflict {
            current_revision: Some(8),
        })
    }

    async fn get_publication(
        &self,
        _fp: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        Ok(None)
    }
}

/// Simulates the real active/active race where a reconciler advances the
/// source exactly once between an unreviewed publish read and its CAS.
#[derive(Default)]
struct AdvancingPublishRegistry {
    reads: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ConfigRegistry for AdvancingPublishRegistry {
    async fn put_config(&self, _c: &AgentConfig) -> Result<(), ConfigStoreError> {
        Ok(())
    }

    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
        Ok(Some(agent_config(id)))
    }

    async fn get_config_revision(
        &self,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
        let revision = if self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            7
        } else {
            8
        };
        Ok(Some(AgentConfigRevision {
            config: agent_config(id),
            revision,
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
        }))
    }

    async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        Ok(Vec::new())
    }

    async fn put_publication(&self, _p: &StoredPublication) -> Result<(), ConfigStoreError> {
        panic!("generation-fenced publish must use the atomic method")
    }

    async fn put_publication_if_config_revision(
        &self,
        publication: &StoredPublication,
        expected_generation: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        assert_eq!(publication.source_revision, expected_generation);
        if expected_generation == 7 {
            Ok(ConfigWrite::Conflict {
                current_revision: Some(8),
            })
        } else {
            assert_eq!(expected_generation, 8);
            Ok(ConfigWrite::Applied { revision: 8 })
        }
    }

    async fn get_publication(
        &self,
        _fp: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        Ok(None)
    }
}

/// A scope-bound registry whose reads and writes fail, so the HTTP handlers hit
/// their 500 / 400 error arms (F19c / F21b).
struct FailingScopedRegistry;
#[async_trait::async_trait]
impl ScopedConfigRegistry for FailingScopedRegistry {
    async fn list_config_scopes(&self) -> Result<Vec<ScopeId>, ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn put_config_scoped(
        &self,
        _s: &ScopeId,
        _c: &AgentConfig,
    ) -> Result<(), ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn get_config_scoped(
        &self,
        _s: &ScopeId,
        _id: &str,
    ) -> Result<Option<AgentConfig>, ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn get_config_revision_scoped(
        &self,
        _scope: &ScopeId,
        _id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn list_configs_scoped(
        &self,
        _s: &ScopeId,
    ) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn put_publication_scoped(
        &self,
        _s: &ScopeId,
        _p: &StoredPublication,
    ) -> Result<(), ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn put_publication_if_config_revision_scoped(
        &self,
        _scope: &ScopeId,
        _publication: &StoredPublication,
        _expected_revision: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn get_publication_scoped(
        &self,
        _s: &ScopeId,
        _fp: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
    async fn list_published_scoped(
        &self,
        _s: &ScopeId,
    ) -> Result<Vec<StoredPublication>, ConfigStoreError> {
        Err(ConfigStoreError("boom".into()))
    }
}

pub(crate) fn failing_scoped_plane() -> ConfigPlane {
    ConfigPlane::new(
        Arc::new(test_service()),
        Arc::new(FailingScopedRegistry),
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    )
}

/// A config plane (the scope edge) over a shared real store handle, returned
/// alongside the store so a test can read the durable rows directly.
fn plane_over(store: Arc<SqliteConfigStore>) -> ConfigPlane {
    ConfigPlane::new(
        Arc::new(test_service()),
        store,
        Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
    )
}
