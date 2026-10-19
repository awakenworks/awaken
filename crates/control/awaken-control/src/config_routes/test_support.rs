// @formal surface exclude=test_only
// @formal owner crates/control/awaken-control/src/config_routes.rs
// @formal reason: Port-based config HTTP fixtures and failure injectors moved with the original adapter tests.
use awaken_agent_config::{
    AgentConfig, AgentConfigRevision, ConfigStoreError, ModelSelection, PublicationWrite,
    ScopedConfigRegistry, StoredPublication,
};
use awaken_config_resolver::AgentInputBindingRepository;
use awaken_config_service::{
    ConfigPlane, ConfigService, ModelPublicationResolver, PublicationResolutionError,
    ResolvedPublicationModels, ToolCatalogSource,
};
use awaken_config_store::SqliteConfigStore;
use awaken_executable_agent_catalog::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};
use awaken_executable_agent_contract::{
    ExecutableAgentRegistrar, ExecutableAgentRegistration, ExecutableAgentRegistrationError,
    ExecutableAgentRegistrationOutcome, ExecutableAgentWithdrawal,
    ExecutableAgentWithdrawalOutcome,
};
use awaken_runtime_contract::resolved::{ContextPolicy, ModelBinding};
use awaken_tenancy::ScopeId;
use std::sync::Arc;
pub(crate) fn agent_config(id: &str) -> AgentConfig {
    AgentConfig {
        id: id.to_string(),
        instructions: "be helpful".to_string(),
        max_steps: Some(8),
        delegation_limits: Default::default(),
        model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b")
            .expect("literal coordinates"),
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

pub(super) struct FakeResolver;
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
                    ModelBinding::parse("openai", "m-first", "genai").expect("fixture coordinates"),
                    vec![
                        ModelBinding::parse("openai", "m-second", "genai")
                            .expect("fixture coordinates"),
                    ],
                )
            },
            |primary| (primary.clone(), candidates.to_vec()),
        );
        Ok(ResolvedPublicationModels::host(
            primary, candidates, None, None,
        ))
    }
}

pub(super) struct ErrResolver;

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

pub(super) fn test_registrar() -> Arc<dyn ExecutableAgentRegistrar> {
    Arc::new(LocalExecutableAgentRegistrar::new(Arc::new(
        ExecutableAgentCatalog::new(),
    )))
}

pub(super) fn test_service_and_catalog() -> (ConfigService, Arc<ExecutableAgentCatalog>) {
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let registrar = Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone()));
    (
        ConfigService::new(
            Arc::new(FakeResolver),
            registrar,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            Vec::new(),
        ),
        catalog,
    )
}

pub(crate) fn test_service() -> ConfigService {
    ConfigService::new(
        Arc::new(FakeResolver),
        test_registrar(),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        Vec::new(),
    )
}

pub(super) async fn plane_with(
    tools: Arc<dyn ToolCatalogSource>,
    resolver: Option<Arc<dyn ModelPublicationResolver>>,
    resources: Option<Arc<dyn AgentInputBindingRepository>>,
) -> ConfigPlane {
    let store = Arc::new(SqliteConfigStore::open_in_memory().await.unwrap());
    let mut service = ConfigService::new(
        resolver.unwrap_or_else(|| Arc::new(FakeResolver)),
        test_registrar(),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        Vec::new(),
    );
    if let Some(resources) = resources {
        service = service.with_resources(resources);
    }
    ConfigPlane::new(Arc::new(service), store, tools)
}

pub(super) async fn static_plane(
    resolver: Option<Arc<dyn ModelPublicationResolver>>,
) -> ConfigPlane {
    plane_with(
        Arc::new(awaken_config_service::StaticToolCatalog(vec![])),
        resolver,
        None,
    )
    .await
}

pub(super) struct FailingScopedRegistry;
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
    async fn put_publication_if_source_current_scoped(
        &self,
        _scope: &ScopeId,
        _publication: &StoredPublication,
    ) -> Result<PublicationWrite, ConfigStoreError> {
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
        Arc::new(awaken_config_service::StaticToolCatalog(vec![])),
    )
}

/// A config plane (the scope edge) over a shared real store handle, returned
/// alongside the store so a test can read the durable rows directly.
pub(super) fn plane_over(store: Arc<SqliteConfigStore>) -> ConfigPlane {
    ConfigPlane::new(
        Arc::new(test_service()),
        store,
        Arc::new(awaken_config_service::StaticToolCatalog(vec![])),
    )
}

pub(super) fn scope(id: &str) -> ScopeId {
    ScopeId::from(id)
}
pub(super) fn auto_config(id: &str) -> AgentConfig {
    let mut config = agent_config(id);
    config.model_binding = ModelSelection::Auto;
    config
}

pub(super) struct UnavailableRegistrar;

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
