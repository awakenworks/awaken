//! The scope-free Config Service: author and publish Agent configs.
//!
//! `ConfigService` is the config domain's authoring authority — it validates and
//! stores declarative [`AgentConfig`]s in a [`ConfigRegistry`], and on publish
//! compiles one into a content-addressed [`StoredPublication`] before registering
//! the exact immutable snapshot with Coordinator (ADR-0071).
//!
//! Runtime consumes compiled configuration and never edits authoring records.
use std::sync::Arc;

use awaken_agent_config::{
    AgentConfig, AgentConfigRevision, ConfigRegistry, ConfigWrite, StoredPublication,
};
use awaken_config_resolver::AgentInputBindingRepository;
use awaken_executable_agent_contract::{ExecutableAgentRegistrar, ExecutableAgentRegistration};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;

use crate::agent_projection::registered_session_profile;
use crate::binding_resolver::ModelPublicationResolver;
use crate::credential_reference::{CredentialReferenceValidator, validate_credential_references};
use crate::plugin_validation::{PluginPublicationResolver, resolve_plugin_configuration};
use crate::publication::{PublishError, prepare_agent_publication, snapshot_metadata};

#[cfg(test)]
use crate::ConfigPlane;
#[cfg(test)]
use crate::config_routes::{get_config, publish, put_config, request_scope, validate};
#[cfg(test)]
use crate::tool_catalog::{RESERVED_ADMIN_SCOPE, ToolCatalogSource};
#[cfg(test)]
use awaken_agent_config::DEFAULT_SCOPE;
#[cfg(test)]
use awaken_agent_config::ScopedConfigRegistry;
#[cfg(test)]
use awaken_executable_agent_contract::ExecutableAgentWithdrawal;
#[cfg(test)]
use axum::extract::{Path, State};
#[cfg(test)]
use axum::http::StatusCode;
#[cfg(test)]
use axum::{Extension, Json};
#[cfg(test)]
use serde_json::json;

/// The config domain service: validate, store, and publish Agent configuration.
///
/// **Authorization-free by design (ADR-0051/0052).** The already-scoped authoring
/// collaborators — a scope-bound [`ConfigRegistry`] (via
/// [`awaken_agent_config::ScopedConfig`]) and the
/// namespace's resolved tool catalog (`&[ToolDescriptor]`) — are passed in per call
/// by the edge ([`crate::ConfigPlane`] and router handlers). Publication also receives one
/// trusted execution Workspace coordinate for registration. It receives no principal,
/// role, policy, token, authorization decision, or Coordinator store.
pub struct ConfigService {
    /// The sole Control-to-Coordinator executable availability boundary.
    pub(crate) registrar: Arc<dyn ExecutableAgentRegistrar>,
    /// Per-agent resource bindings (ADR-0038). When wired, the agent's bound-resource
    /// prompt fragments are appended to its effective system prompt at compile (A3a).
    /// `None` → compilation is byte-identical to an unbound agent.
    pub(crate) resources: Option<Arc<dyn AgentInputBindingRepository>>,
    /// Resolves authored selection into complete ordered model candidates in one
    /// publication read. Required at construction so a config service can never
    /// publish through an implicit host/provider fallback.
    pub(crate) model_publication_resolver: Arc<dyn ModelPublicationResolver>,
    pub(crate) credential_reference_validator: Option<Arc<dyn CredentialReferenceValidator>>,
    pub(crate) plugin_publication_resolvers: Vec<Arc<dyn PluginPublicationResolver>>,
}

struct PreparedPublication {
    publication: StoredPublication,
    registration: ExecutableAgentRegistration,
}

impl ConfigService {
    /// Store a config draft (upsert by id) in the caller-supplied scope-bound
    /// `registry` (a [`awaken_agent_config::ScopedConfig`] the edge bound to the
    /// request scope).
    pub async fn put(
        &self,
        registry: &dyn ConfigRegistry,
        config: &AgentConfig,
    ) -> Result<(), String> {
        self.reject_archived_rewrite(registry, config).await?;
        registry.put_config(config).await.map_err(|e| e.to_string())
    }

    pub async fn put_if_revision(
        &self,
        registry: &dyn ConfigRegistry,
        config: &AgentConfig,
        expected_generation: u64,
    ) -> Result<ConfigWrite, String> {
        self.reject_archived_rewrite(registry, config).await?;
        registry
            .put_config_if_revision(config, expected_generation)
            .await
            .map_err(|e| e.to_string())
    }

    async fn reject_archived_rewrite(
        &self,
        registry: &dyn ConfigRegistry,
        config: &AgentConfig,
    ) -> Result<(), String> {
        let current = registry
            .get_config(&config.id)
            .await
            .map_err(|error| error.to_string())?;
        let invalid = current.as_ref().is_some_and(|stored| {
            use awaken_agent_config::AgentLifecycle::{Archived, Disabled, Published};
            match (stored.lifecycle(), config.lifecycle()) {
                (Published, _) | (Disabled, Archived) => false,
                (Disabled | Archived, _) => stored != config,
            }
        });
        if invalid {
            return Err(format!("agent `{}` is not published", config.id));
        }
        Ok(())
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

    async fn prepare_publication(
        &self,
        workspace: &ScopeId,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
        expected_source_revision: Option<u64>,
        expected_resource_revision: Option<i64>,
    ) -> Result<PreparedPublication, PublishError> {
        let versioned = registry
            .get_config_revision(id)
            .await
            .map_err(|e| PublishError::Store(e.to_string()))?
            .ok_or_else(|| PublishError::NotStored(id.to_string()))?;
        if expected_source_revision.is_some_and(|expected| expected != versioned.revision) {
            return Err(PublishError::StaleRevision(Some(versioned.revision)));
        }
        if versioned.config.lifecycle() != awaken_agent_config::AgentLifecycle::Published {
            return Err(PublishError::Unavailable(id.to_string()));
        }
        let source_revision = versioned.revision;
        let mut resolved = prepare_agent_publication(
            self.model_publication_resolver.as_ref(),
            workspace,
            versioned,
        )
        .await?;
        resolve_plugin_configuration(
            &self.plugin_publication_resolvers,
            workspace,
            &mut resolved.config,
        )
        .await
        .map_err(|error| {
            PublishError::Unresolvable(format!("{}: {}", error.path, error.message))
        })?;
        validate_credential_references(
            self.credential_reference_validator.as_ref(),
            workspace,
            &resolved.config,
        )
        .await
        .map_err(|error| {
            PublishError::Unresolvable(format!("{}: {}", error.path, error.message))
        })?;
        let mut metadata = snapshot_metadata(&resolved);
        let defaults = match self.resources.as_ref() {
            Some(store) => store
                .get_agent_inputs(workspace.as_str(), id)
                .map_err(|error| PublishError::Store(error.to_string()))?,
            None => None,
        };
        let current_resource_revision = defaults.as_ref().map_or(0, |inputs| inputs.revision);
        if expected_resource_revision.is_some_and(|expected| expected != current_resource_revision)
        {
            return Err(PublishError::StaleResourceRevision(
                current_resource_revision,
            ));
        }
        if let Some(defaults) = &defaults {
            let mut inputs = std::mem::take(&mut metadata.resolution.inputs);
            inputs.push(awaken_runtime_contract::ResolvedInputRef {
                kind: "agent_session_defaults".into(),
                id: id.to_string(),
                version: awaken_runtime_contract::ResolvedInputVersion::Revision(
                    defaults.revision as u64,
                ),
            });
            metadata.resolution = awaken_runtime_contract::ResolutionManifest::new(inputs)
                .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
        }
        let snapshot = awaken_agent_config::compile_published(
            &resolved.config,
            catalog,
            metadata,
            resolved.models.primary,
            resolved.models.candidates,
            resolved.advisor,
        )
        .map_err(|e| PublishError::Compile(e.to_string()))?;
        let stored_inputs = defaults
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| PublishError::Store(error.to_string()))?;
        let publication =
            StoredPublication::published_at_revision(snapshot.clone(), id, source_revision)
                .with_agent_inputs(stored_inputs);
        let session_profile =
            registered_session_profile(&snapshot, &resolved.authored_model_selection, defaults)
                .ok_or_else(|| {
                    PublishError::Registration(
                        awaken_executable_agent_contract::ExecutableAgentRegistrationError::Invalid(
                            "Agent Session defaults changed while the publication was compiled"
                                .into(),
                        ),
                    )
                })?;
        Ok(PreparedPublication {
            publication,
            registration: ExecutableAgentRegistration {
                workspace_id: workspace.as_str().to_owned(),
                agent_id: id.to_owned(),
                source_revision,
                snapshot,
                session_profile,
            },
        })
    }

    /// Compile the exact publication that a subsequent [`Self::publish`] would
    /// persist, without changing Control or Coordinator state.
    ///
    /// Composition adapters use this to distinguish a byte-identical retry from
    /// a dependency re-resolution (for example, an exact credential rotation)
    /// that requires a new authoring revision before registration.
    pub async fn preview_publication(
        &self,
        workspace: &ScopeId,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
    ) -> Result<StoredPublication, PublishError> {
        Ok(self
            .prepare_publication(workspace, registry, id, catalog, None, None)
            .await?
            .publication)
    }

    /// Publish: resolve an `Auto` model to a concrete binding (D5), compile the stored
    /// config against the caller-supplied `catalog`, persist the publication
    /// (idempotent by fingerprint) into the scope-bound `registry`, and register it
    /// for future Coordinator Session resolution. The stored source config is left untouched —
    /// its `Auto` selection persists so the reconciler can re-resolve it later.
    pub async fn publish(
        &self,
        workspace: &ScopeId,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
    ) -> Result<StoredPublication, PublishError> {
        self.publish_at_revisions(workspace, registry, id, catalog, None, None)
            .await
    }

    /// Publish one reviewed Agent aggregate only when both mutable sources still
    /// have the revisions observed by the caller, then freeze the exact Resource
    /// defaults into the durable publication and Coordinator registration.
    pub async fn publish_at_revisions(
        &self,
        workspace: &ScopeId,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
        expected_source_revision: Option<u64>,
        expected_resource_revision: Option<i64>,
    ) -> Result<StoredPublication, PublishError> {
        let prepared = self
            .prepare_publication(
                workspace,
                registry,
                id,
                catalog,
                expected_source_revision,
                expected_resource_revision,
            )
            .await?;
        let write = registry
            .put_publication_if_config_revision(
                &prepared.publication,
                prepared.publication.source_revision,
            )
            .await
            .map_err(|e| PublishError::Store(e.to_string()))?;
        if let ConfigWrite::Conflict { current_revision } = write {
            return Err(PublishError::StaleRevision(current_revision));
        }
        self.registrar
            .register(prepared.registration)
            .await
            .map_err(PublishError::Registration)?;
        Ok(prepared.publication)
    }

    /// Re-resolve and re-publish a policy-bound agent (ADR-0052 D5), reading and
    /// writing through the caller-supplied scope-bound `registry`. Returns `true` if it
    /// re-published (a `Pinned` agent is skipped; a missing one is skipped). Idempotent
    /// by content address, so a retry after a catalog change is safe. This is what
    /// [`ConfigServiceReconciler`](crate::binding_resolver::ConfigServiceReconciler)
    /// drives from the catalog write path.
    pub async fn reconcile(
        &self,
        workspace: &ScopeId,
        registry: &dyn ConfigRegistry,
        id: &str,
        catalog: &[ToolDescriptor],
    ) -> Result<bool, String> {
        let stored = registry.get_config(id).await.map_err(|e| e.to_string())?;
        match stored {
            // Policy selections refresh their authority-owned pins; an operator's
            // concrete pinned binding remains authoritative.
            Some(config) if config.model_binding.requires_reconciliation() => {
                self.publish(workspace, registry, id, catalog)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
pub(crate) mod resource_prompt_tests {
    use super::*;
    use awaken_agent_config::{ConfigStoreError, ModelSelection, ScopedConfig};
    use awaken_config_resolver::{
        AgentInputConfig, BindingId, InputBinding, InputResourceId, MemoryStoreId, ResourceAccess,
    };
    use awaken_config_store::SqliteConfigStore;
    use awaken_executable_agent_catalog::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};
    use awaken_executable_agent_contract::{
        ExecutableAgentRegistrationError, ExecutableAgentRegistrationOutcome,
        ExecutableAgentWithdrawalOutcome,
    };
    use awaken_runtime_contract::resolved::ContextPolicy;

    use crate::binding_resolver::{
        ModelPublicationResolver, PublicationResolutionError, ResolvedPublicationModels,
    };
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
            let candidate = |binding: ModelBinding| {
                ResolvedModelCandidate::provider(
                    binding.clone(),
                    "provider@2",
                    "endpoint@4",
                    workspace.clone(),
                    Some(awaken_runtime_contract::CredentialAccess::new(
                        awaken_runtime_contract::CredentialRef {
                            id: format!("credential-{workspace}"),
                            revision: 3,
                        },
                        awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                        awaken_runtime_contract::CredentialUsage::ProviderAdapter,
                        awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
                    )),
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
                primary: candidate(primary),
                candidates: candidates.iter().cloned().map(candidate).collect(),
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

    #[tokio::test]
    async fn publish_resolves_scope_access_once_into_the_persisted_snapshot() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let service = Arc::new(ConfigService::new(
            Arc::new(FakeProviderResolver),
            test_registrar(),
        ));
        let plane = ConfigPlane::new(
            service,
            store,
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let scope = ScopeId::from("workspace-a");
        plane.put(&scope, &agent_config("agent-a")).await.unwrap();
        let publication = plane.publish(&scope, "agent-a").await.unwrap();
        let candidate = &publication.snapshot.resolved_spec.model_binding;
        let awaken_runtime_contract::resolved::ModelProvisioning::Provider {
            scope_id,
            credential: Some(credential),
            ..
        } = &candidate.provisioning
        else {
            panic!("publication carries a complete provider candidate")
        };
        assert_eq!(scope_id.as_str(), "workspace-a");
        assert_eq!(credential.credential.id, "credential-workspace-a");
        assert_eq!(publication.fingerprint, publication.snapshot.fingerprint.0);
    }

    #[tokio::test]
    async fn preview_registers_inline_inputs_without_authoring_persistence() {
        // Preview cause/effect decision table:
        // P1 exact preview/config/input id + valid draft -> one Coordinator
        // registration carrying the exact inline resources; P2 no ConfigRegistry
        // is supplied -> no authoring draft or StoredPublication can be written;
        // P3 the monotonic successor withdrawal -> current resolution disappears.
        let (service, catalog) = test_service_and_catalog();
        let workspace = scope("workspace-preview");
        let preview_id = "preview-causal-1";
        let inputs = AgentInputConfig {
            agent_id: preview_id.into(),
            environment: None,
            inputs: vec![InputBinding {
                binding_id: BindingId::from("memory"),
                target: InputResourceId::MemoryStore(MemoryStoreId::from("memstore-7")),
                mount_path: "/mnt/memory".into(),
                access: ResourceAccess::ReadWrite,
                instructions: Some("prefer current project decisions".into()),
            }],
            revision: 1,
        };

        service
            .preview(
                &workspace,
                preview_id,
                &agent_config(preview_id),
                inputs.clone(),
                &[],
            )
            .await
            .unwrap();
        let registration = catalog
            .current(workspace.as_str(), preview_id)
            .expect("P1 preview registration");
        assert_eq!(registration.session_profile.resources, inputs.inputs, "P1");

        service
            .registrar
            .withdraw(ExecutableAgentWithdrawal {
                workspace_id: workspace.as_str().into(),
                agent_id: preview_id.into(),
                lifecycle_revision: 2,
            })
            .await
            .unwrap();
        assert!(
            catalog.current(workspace.as_str(), preview_id).is_none(),
            "P3"
        );
    }

    #[tokio::test]
    async fn local_publication_pins_host_access_instead_of_deferring_to_runtime() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let plane = ConfigPlane::new(
            Arc::new(test_service()),
            store,
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let scope = ScopeId::from("workspace-a");
        plane.put(&scope, &agent_config("agent-a")).await.unwrap();

        let publication = plane.publish(&scope, "agent-a").await.unwrap();
        assert!(matches!(
            publication
                .snapshot
                .resolved_spec
                .model_binding
                .provisioning,
            awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor
        ));
    }

    fn static_plane(resolver: Option<Arc<dyn ModelPublicationResolver>>) -> ConfigPlane {
        plane_with(
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
            resolver,
            None,
        )
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
        async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
            Err(ConfigStoreError("boom".into()))
        }
        async fn put_publication(&self, _p: &StoredPublication) -> Result<(), ConfigStoreError> {
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
        async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
            Ok(vec![])
        }
        async fn put_publication(&self, _p: &StoredPublication) -> Result<(), ConfigStoreError> {
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

    // P1: a registry read failure on publish surfaces as `PublishError::Store`.
    #[tokio::test]
    async fn publish_maps_a_registry_read_failure_to_store() {
        let err = test_service()
            .publish(&scope(DEFAULT_SCOPE), &FailingRegistry, "a", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, PublishError::Store(_)), "got {err:?}");
    }

    // P6: a publication-persist failure (after a clean read + compile) is `Store`.
    #[tokio::test]
    async fn publish_maps_a_publication_persist_failure_to_store() {
        let err = test_service()
            .publish(&scope(DEFAULT_SCOPE), &PublishFailRegistry, "a", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, PublishError::Store(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn publish_never_installs_an_artifact_from_a_stale_source_revision() {
        let (service, catalog) = test_service_and_catalog();
        let err = service
            .publish(&scope(DEFAULT_SCOPE), &StalePublishRegistry, "a", &[])
            .await
            .unwrap_err();

        assert!(matches!(err, PublishError::StaleRevision(Some(8))));
        assert!(
            catalog.current(DEFAULT_SCOPE, "a").is_none(),
            "a stale publication must not enter the live catalog"
        );
    }

    // reconcile-a: a registry read failure on reconcile is a returned `Err`.
    #[tokio::test]
    async fn reconcile_propagates_a_registry_read_failure() {
        assert!(
            test_service()
                .reconcile(&scope(DEFAULT_SCOPE), &FailingRegistry, "a", &[])
                .await
                .is_err()
        );
    }

    // F19c: the get_config handler returns 500 when the store read fails.
    #[tokio::test]
    async fn get_config_handler_returns_500_on_store_error() {
        let (status, _body) = super::get_config(
            State(failing_scoped_plane()),
            Path("a".to_string()),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // F21b: the put_config handler returns 400 when the store write fails (body parses).
    #[tokio::test]
    async fn put_config_handler_returns_400_on_store_error() {
        let (status, _body) = super::put_config(
            State(failing_scoped_plane()),
            None,
            Path("a".to_string()),
            Json(json!({ "model": "gpt" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn resolve_agent_config_derives_the_effective_compaction_window_for_both_realizations() {
        struct WindowResolver;
        #[async_trait::async_trait]
        impl ModelPublicationResolver for WindowResolver {
            async fn resolve_models(
                &self,
                _workspace: &ScopeId,
                selection: &ModelSelection,
                candidates: &[ModelBinding],
            ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
                Ok(ResolvedPublicationModels::host(
                    selection
                        .resolved()
                        .cloned()
                        .unwrap_or_else(|| ModelBinding::new("p", "m-x", "b")),
                    candidates.to_vec(),
                    Some(200_000),
                    Some(40_000),
                ))
            }
        }
        let service = ConfigService::new(Arc::new(WindowResolver), test_registrar());
        let pin = || ModelSelection::Pinned(ModelBinding::new("p", "m-x", "b"));
        // Usable budget = context_window − max_output_tokens = 200k − 40k = 160k;
        // default trigger = 3/4 × 160k = 120k.

        // NATIVE (compact section, no agent override): the effective window at ratio 1.0 —
        // the fold point IS the trigger (headroom + ratio already baked in).
        let mut cfg = agent_config("a1");
        cfg.model_binding = pin();
        cfg.plugin_config
            .insert("compact".into(), serde_json::json!({ "keep_last": 4 }));
        let out = resolve_config(&service, cfg).await;
        assert_eq!(out.plugin_config["compact"]["max_tokens"], 120_000);
        assert_eq!(out.plugin_config["compact"]["trigger_ratio"], 1.0);

        // ACP (acp section): the same effective window flows to the CLI's compact_window.
        let mut cfg_acp = agent_config("a-acp");
        cfg_acp.model_binding = pin();
        cfg_acp
            .plugin_config
            .insert("acp".into(), serde_json::json!({}));
        let out_acp = resolve_config(&service, cfg_acp).await;
        assert_eq!(out_acp.plugin_config["acp"]["compact_window"], 120_000);

        // Agent OVERRIDE (under budget) is honored verbatim, in BOTH realizations.
        let mut cfg2 = agent_config("a2");
        cfg2.model_binding = pin();
        cfg2.compaction = Some(awaken_agent_config::CompactionStrategy {
            window: Some(90_000),
            keep_recent: None,
        });
        cfg2.plugin_config
            .insert("compact".into(), serde_json::json!({}));
        cfg2.plugin_config
            .insert("acp".into(), serde_json::json!({}));
        let out2 = resolve_config(&service, cfg2).await;
        assert_eq!(out2.plugin_config["compact"]["max_tokens"], 90_000);
        assert_eq!(out2.plugin_config["acp"]["compact_window"], 90_000);

        // An operator-pinned compact.max_tokens is never clobbered.
        let mut cfg3 = agent_config("a3");
        cfg3.model_binding = pin();
        cfg3.plugin_config
            .insert("compact".into(), serde_json::json!({ "max_tokens": 50 }));
        assert_eq!(
            resolve_config(&service, cfg3).await.plugin_config["compact"]["max_tokens"],
            50
        );

        // No compact/acp section → untouched (neither realization was opted into).
        let mut cfg4 = agent_config("a4");
        cfg4.model_binding = pin();
        let out4 = resolve_config(&service, cfg4).await;
        assert!(!out4.plugin_config.contains_key("compact"));
        assert!(!out4.plugin_config.contains_key("acp"));
    }

    // CEG F11d: a `compact` value that is present but NOT a JSON object (here a
    // bare string) is a no-op — `apply_compaction` only reaches into an object, so a
    // malformed section is left byte-identical and no window injected.
    #[tokio::test]
    async fn resolve_agent_config_leaves_a_non_object_compact_untouched() {
        struct WindowResolver;
        #[async_trait::async_trait]
        impl ModelPublicationResolver for WindowResolver {
            async fn resolve_models(
                &self,
                _workspace: &ScopeId,
                selection: &ModelSelection,
                candidates: &[ModelBinding],
            ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
                Ok(ResolvedPublicationModels::host(
                    selection.resolved().cloned().ok_or_else(|| {
                        PublicationResolutionError::Invalid(
                            "test requires a pinned model".to_string(),
                        )
                    })?,
                    candidates.to_vec(),
                    Some(200_000),
                    None,
                ))
            }
        }
        let service = ConfigService::new(Arc::new(WindowResolver), test_registrar());
        let mut cfg = agent_config("a4");
        cfg.model_binding = ModelSelection::Pinned(ModelBinding::new("p", "m-x", "b"));
        cfg.plugin_config
            .insert("compact".into(), serde_json::json!("not-an-object"));
        let out = resolve_config(&service, cfg).await;
        assert_eq!(
            out.plugin_config["compact"],
            serde_json::json!("not-an-object")
        );
    }

    #[tokio::test]
    async fn publish_does_not_bake_session_resource_prompts_into_agent_instructions() {
        // Agent defaults remain authoring data until Session resolution. Publishing
        // the Agent must not bake a stale pre-merge resource prompt into its snapshot.
        let resources =
            Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new());
        resources
            .put_agent_inputs(
                DEFAULT_SCOPE,
                AgentInputConfig {
                    agent_id: "agent-1".into(),
                    environment: None,
                    inputs: vec![InputBinding {
                        binding_id: BindingId::from("memory"),
                        target: InputResourceId::MemoryStore(MemoryStoreId::from("memstore-7")),
                        mount_path: "/mnt/memory/prefs".into(),
                        access: ResourceAccess::ReadWrite,
                        instructions: Some("user preferences".into()),
                    }],
                    revision: 1,
                },
            )
            .unwrap();

        let scope = ScopeId::from(DEFAULT_SCOPE);
        let plane = plane_with(
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
            None,
            Some(resources),
        );
        plane.put(&scope, &agent_config("agent-1")).await.unwrap();
        let publication = plane.publish(&scope, "agent-1").await.unwrap();

        // Only the authored Agent instructions are compiled. The final resource
        // prompt is generated from Effective Session inputs at preparation time.
        let instructions = &publication.snapshot.resolved_spec.instructions;
        assert!(instructions.starts_with("be helpful"));
        assert!(!instructions.contains("/mnt/memory/prefs"));
        assert!(!instructions.contains("user preferences"));
    }

    #[tokio::test]
    async fn publish_pins_agent_model_catalog_and_session_defaults_revision() {
        let resources =
            Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new());
        resources
            .put_agent_inputs(
                DEFAULT_SCOPE,
                AgentInputConfig {
                    agent_id: "pinned-inputs".into(),
                    environment: Some(awaken_config_resolver::AgentEnvironmentBinding {
                        environment_id: "env-production".into(),
                        revision: 9,
                    }),
                    inputs: vec![],
                    revision: 1,
                },
            )
            .unwrap();
        let tool = ToolDescriptor::pinned(
            "builtin",
            "search",
            "search",
            serde_json::json!({"type": "object"}),
        );
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let service = ConfigService::new(
            Arc::new(FakeResolver),
            Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
        )
        .with_resources(resources.clone());
        let plane = ConfigPlane::new(
            Arc::new(service),
            store,
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![tool.clone()])),
        );
        let scope = ScopeId::from(DEFAULT_SCOPE);
        let mut config = agent_config("pinned-inputs");
        config.tool_ids.push(tool.id.clone());
        plane.put(&scope, &config).await.unwrap();
        let publication = plane.publish(&scope, &config.id).await.unwrap();

        let metadata = &publication.snapshot.metadata;
        assert_eq!(metadata.source.agent_id.0, config.id);
        assert_eq!(metadata.source.revision, 1);
        assert_eq!(metadata.publication_version.0, publication.fingerprint);
        assert_eq!(metadata.fingerprint.0, publication.fingerprint);
        let kinds: Vec<_> = metadata
            .resolution
            .inputs
            .iter()
            .map(|input| input.kind.as_str())
            .collect();
        assert_eq!(
            kinds,
            [
                "agent_config",
                "agent_session_defaults",
                "model_binding",
                "tool"
            ]
        );
        assert_eq!(
            metadata.resolution.inputs[1].version,
            awaken_runtime_contract::ResolvedInputVersion::Revision(1)
        );
        assert_eq!(
            metadata.resolution.inputs[3].version,
            awaken_runtime_contract::ResolvedInputVersion::ContentHash(tool.content_hash)
        );

        use awaken_executable_agent_contract::ExecutableAgentProfileSource as _;
        let view = catalog
            .session_profile_in(DEFAULT_SCOPE, "pinned-inputs")
            .expect("matching published defaults");
        assert_eq!(view.environment.unwrap().revision, 9);

        resources
            .put_agent_inputs(
                DEFAULT_SCOPE,
                AgentInputConfig {
                    agent_id: "pinned-inputs".into(),
                    environment: Some(awaken_config_resolver::AgentEnvironmentBinding {
                        environment_id: "env-production".into(),
                        revision: 9,
                    }),
                    inputs: vec![],
                    revision: 2,
                },
            )
            .unwrap();
        assert!(
            catalog
                .session_profile_in(DEFAULT_SCOPE, "pinned-inputs")
                .is_some(),
            "registered Coordinator view remains frozen when Control defaults later change"
        );
        assert_eq!(
            publication.agent_inputs.as_ref().unwrap()["revision"],
            1,
            "the publication owns the exact Resource defaults used at compile time"
        );
    }

    #[tokio::test]
    async fn reviewed_publish_fences_config_and_resource_revisions() {
        // Reviewed-publish cause/effect table: R1 stale config + exact resources
        // -> StaleRevision; R2 exact config + stale resources ->
        // StaleResourceRevision; R3 both exact -> one self-contained publication.
        let resources =
            Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new());
        resources
            .put_agent_inputs(
                DEFAULT_SCOPE,
                AgentInputConfig {
                    agent_id: "reviewed".into(),
                    environment: None,
                    inputs: vec![],
                    revision: 1,
                },
            )
            .unwrap();
        let plane = plane_with(
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
            None,
            Some(resources),
        );
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &agent_config("reviewed")).await.unwrap();

        assert!(matches!(
            plane.publish_at_revisions(&scope, "reviewed", 99, 1).await,
            Err(PublishError::StaleRevision(Some(1)))
        ));
        assert!(matches!(
            plane.publish_at_revisions(&scope, "reviewed", 1, 99).await,
            Err(PublishError::StaleResourceRevision(1))
        ));
        let publication = plane
            .publish_at_revisions(&scope, "reviewed", 1, 1)
            .await
            .expect("R3 matching reviewed aggregate publishes");
        assert_eq!(publication.source_revision, 1);
        assert_eq!(publication.agent_inputs.unwrap()["revision"], 1);
    }

    #[tokio::test]
    async fn publish_resolves_auto_to_the_first_offering_and_keeps_the_source_auto() {
        let plane = static_plane(Some(Arc::new(FakeResolver)));
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &auto_config("mgmt")).await.unwrap();
        let publication = plane.publish(&scope, "mgmt").await.unwrap();

        // The compiled publication carries the resolved concrete binding +
        // the remaining offerings as pool candidates (ADR-0052 D5).
        let spec = &publication.snapshot.resolved_spec;
        assert_eq!(spec.model_binding.model_ref, "m-first");
        assert_eq!(spec.model_candidates.len(), 1);
        assert_eq!(spec.model_candidates[0].model_ref, "m-second");

        // The stored *source* config is still Auto — so a later catalog change can
        // re-resolve it (reconcile returns true for policy-owned sources).
        assert!(plane.reconcile(&scope, "mgmt").await.unwrap());
    }

    #[tokio::test]
    async fn managed_projection_preserves_the_published_authoring_model_id() {
        // Causes: C1 a Target carries a user-facing endpoint name while the
        // executable binding carries only the resolved provider identity; C2
        // Auto/Profile has no public authored id. Effects: E1 Target projection
        // round-trips the authored id exactly; E2 policy selection falls back to
        // the immutable resolved binding. This case exercises decision rule C1.
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let plane = ConfigPlane::new(
            Arc::new(ConfigService::new(
                Arc::new(FakeResolver),
                Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
            )),
            store,
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let scope = ScopeId::from(DEFAULT_SCOPE);
        let mut config = agent_config("managed-target");
        config.model_binding = ModelSelection::Target {
            target: awaken_agent_config::ModelTarget {
                model_id: "m-first".into(),
                provider_id: Some("openai".into()),
                protocol_endpoint_id: None,
                endpoint_name: Some("edge".into()),
            },
            backend_ref: "genai".into(),
            configuration: Default::default(),
        };
        plane.put(&scope, &config).await.unwrap();
        plane.publish(&scope, &config.id).await.unwrap();

        use awaken_executable_agent_contract::ExecutableAgentProfileSource as _;
        let view = catalog
            .session_profile_in(DEFAULT_SCOPE, &config.id)
            .unwrap();
        // Cause/effect rule: a provider-qualified Managed target (C1) projects
        // its full display id for the API (E1) and the already-published exact
        // candidate model_ref for execution (E2); neither string substitutes for
        // the other at Session preparation.
        assert_eq!(view.model.as_deref(), Some("openai@edge/m-first"), "E1");
        assert_eq!(view.execution_model_ref.as_deref(), Some("m-first"), "E2");
    }

    #[tokio::test]
    async fn reconcile_re_publishes_auto_but_skips_pinned() {
        let plane = static_plane(Some(Arc::new(FakeResolver)));
        let scope = ScopeId::from(DEFAULT_SCOPE);

        // A pinned agent: reconcile is a no-op (operator pin is authoritative).
        plane.put(&scope, &agent_config("pinned")).await.unwrap();
        plane.publish(&scope, "pinned").await.unwrap();
        assert!(!plane.reconcile(&scope, "pinned").await.unwrap());

        // An auto agent: reconcile re-publishes (idempotent by content address).
        plane.put(&scope, &auto_config("auto")).await.unwrap();
        plane.publish(&scope, "auto").await.unwrap();
        assert!(plane.reconcile(&scope, "auto").await.unwrap());

        // A missing agent: skipped, not an error.
        assert!(!plane.reconcile(&scope, "ghost").await.unwrap());
    }

    #[tokio::test]
    async fn reconciler_adapter_republishes_the_named_auto_agents() {
        use crate::binding_resolver::{ConfigServiceReconciler, PublicationBindingReconciler};

        let plane = static_plane(Some(Arc::new(FakeResolver)));
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &auto_config("assistant")).await.unwrap();
        plane.publish(&scope, "assistant").await.unwrap();
        plane.put(&scope, &agent_config("pinned")).await.unwrap();
        plane.publish(&scope, "pinned").await.unwrap();

        // The catalog-write path drives one seam over a fixed id set; only the auto
        // one is re-published.
        let reconciler = ConfigServiceReconciler::new(
            plane.clone(),
            DEFAULT_SCOPE,
            DEFAULT_SCOPE,
            vec!["assistant".to_string(), "pinned".to_string()],
        );
        assert_eq!(reconciler.reconcile().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn observation_reconcile_republishes_policy_agents_in_every_owned_scope() {
        use crate::binding_resolver::{ConfigServiceReconciler, PublicationBindingReconciler};

        // Cause/effect graph:
        // C1 configs span ordinary and reserved scopes; C2 Auto is policy-bound;
        // C3 Pinned is operator-owned; C4 one observation event invokes reconcile_all.
        // E1 every Auto is republished in its own scope/execution Workspace; E2
        // Pinned is unchanged; E3 no scope is flattened into another.
        //
        // Decision rule O1: C1+C2+C3+C4 => three E1 publications + E2 + E3.
        // Store-failure propagation is covered by FailingScopedRegistry; single-id
        // policy/pinned/missing rules are covered by the test above.
        let plane = static_plane(Some(Arc::new(FakeResolver)));
        for (scope, id) in [("workspace-a", "auto-a"), ("workspace-b", "auto-b")] {
            let scope = ScopeId::from(scope);
            plane.put(&scope, &auto_config(id)).await.unwrap();
            plane.publish(&scope, id).await.unwrap();
        }
        let reserved = ScopeId::from(RESERVED_ADMIN_SCOPE);
        plane
            .put(&reserved, &auto_config("assistant"))
            .await
            .unwrap();
        plane
            .publish_for_execution_workspace(&reserved, "platform-workspace", "assistant")
            .await
            .unwrap();
        let pinned_scope = ScopeId::from("workspace-a");
        plane
            .put(&pinned_scope, &agent_config("pinned"))
            .await
            .unwrap();
        plane.publish(&pinned_scope, "pinned").await.unwrap();

        let reconciler = ConfigServiceReconciler::new(
            plane,
            RESERVED_ADMIN_SCOPE,
            "platform-workspace",
            Vec::new(),
        );
        assert_eq!(reconciler.reconcile_all().await.unwrap(), 3, "O1");
    }

    #[tokio::test]
    async fn pinned_publication_uses_the_explicit_host_resolver() {
        let plane = static_plane(None);
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &agent_config("pinned")).await.unwrap();
        let publication = plane.publish(&scope, "pinned").await.unwrap();
        assert_eq!(
            publication.snapshot.resolved_spec.model_binding.model_ref,
            "m"
        );
    }

    #[tokio::test]
    async fn admin_tools_compile_only_in_the_reserved_scope() {
        use crate::tool_catalog::{RESERVED_ADMIN_SCOPE, ScopedToolCatalog};
        use awaken_runtime_contract::resolved::ToolDescriptor;

        // A catalog where `admin_x` is reserved-scope only (ADR-0052 D3).
        let admin = ToolDescriptor::pinned(
            "admin",
            "admin_x",
            "a management tool",
            serde_json::json!({"type": "object"}),
        );
        let catalog = ScopedToolCatalog::new(Vec::new(), RESERVED_ADMIN_SCOPE, vec![admin]);
        let plane = plane_with(Arc::new(catalog), None, None);

        // A config that names the admin tool.
        let mut cfg = agent_config("mgmt");
        cfg.tool_ids = vec!["admin_x".to_string()];

        // In the reserved scope it compiles: the descriptor is visible there.
        assert!(
            plane
                .validate(&ScopeId::from(RESERVED_ADMIN_SCOPE), &cfg)
                .await
                .is_ok(),
            "admin tool must resolve in the reserved scope"
        );

        // In any tenant scope it fails closed — the admin tool is not even disclosed,
        // so the same config hits UnknownTool at compile.
        let err = plane
            .validate(&ScopeId::from("wrkspc_acme"), &cfg)
            .await
            .unwrap_err();
        assert_eq!(err.path, "tools", "an unknown tool is a `tools` issue");
        assert!(
            err.message.contains("unknown tool") && err.message.contains("admin_x"),
            "tenant scope must reject the admin tool: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn no_resource_store_compiles_byte_identically() {
        // Without a wired resource store, instructions are the base verbatim.
        let scope = ScopeId::from(DEFAULT_SCOPE);
        let plane = static_plane(None);
        plane.put(&scope, &agent_config("agent-2")).await.unwrap();
        let publication = plane.publish(&scope, "agent-2").await.unwrap();
        assert_eq!(
            publication.snapshot.resolved_spec.instructions,
            "be helpful"
        );
    }

    // ==== CEG section 04: extra publish / resolve / handler coverage ====

    /// A resolver whose catalog has no provider-backed model (P4): `resolve_auto`
    /// fails, so an `Auto` publish is `Unresolvable`.
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

    // ---- publish + resolve_agent_config (F8/F10) ----

    #[tokio::test]
    async fn publish_missing_config_is_not_stored() {
        // P2: no config stored for the id → NotStored (before any resolve/compile).
        let plane = static_plane(None);
        let scope = ScopeId::from(DEFAULT_SCOPE);
        let err = plane.publish(&scope, "ghost").await.unwrap_err();
        assert!(matches!(err, PublishError::NotStored(_)), "{err:?}");
    }

    #[tokio::test]
    async fn publish_is_unresolvable_when_the_catalog_has_no_model() {
        // P4: Auto + a resolver, but the resolver reports no provider-backed model.
        let plane = static_plane(Some(Arc::new(ErrResolver)));
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &auto_config("mgmt")).await.unwrap();
        let err = plane.publish(&scope, "mgmt").await.unwrap_err();
        assert!(matches!(err, PublishError::Unresolvable(_)), "{err:?}");
    }

    #[tokio::test]
    async fn publish_compile_failure_when_config_names_an_unknown_tool() {
        // P5: resolve succeeds, compile fails because a named tool is not in the
        // (empty) catalog → Compile (not Unresolvable).
        let plane = static_plane(Some(Arc::new(FakeResolver)));
        let scope = ScopeId::from(DEFAULT_SCOPE);
        let mut cfg = agent_config("mgmt");
        cfg.tool_ids = vec!["ghost_tool".to_string()];
        plane.put(&scope, &cfg).await.unwrap();
        let err = plane.publish(&scope, "mgmt").await.unwrap_err();
        assert!(matches!(err, PublishError::Compile(_)), "{err:?}");
        assert!(err.to_string().contains("ghost_tool"), "{err}");
    }

    // ---- validate (F12) ----

    #[tokio::test]
    async fn validate_reports_a_resolver_failure_as_a_model_issue() {
        // F12a: an Auto binding that cannot resolve is a `model`-field issue.
        let plane = static_plane(Some(Arc::new(ErrResolver)));
        let issue = plane
            .validate(&ScopeId::from(DEFAULT_SCOPE), &auto_config("mgmt"))
            .await
            .unwrap_err();
        assert_eq!(issue.path, "model");
    }

    // ---- HTTP request_scope (F17) ----

    #[test]
    fn request_scope_uses_the_workspace_scope_when_present() {
        // F17a.
        let scope = super::request_scope(Some(Extension(WorkspaceScope("wrkspc_acme".into()))));
        assert_eq!(scope.as_str(), "wrkspc_acme");
    }

    #[test]
    fn request_scope_falls_back_to_the_default_scope() {
        // F17b.
        let scope = super::request_scope(None);
        assert_eq!(scope.as_str(), DEFAULT_SCOPE);
    }

    // ---- get_config handler (F19) ----

    #[tokio::test]
    async fn get_config_handler_returns_200_when_present() {
        // F19a.
        let plane = static_plane(None);
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &agent_config("mgmt")).await.unwrap();
        let (status, Json(body)) =
            super::get_config(State(plane), Path("mgmt".to_string()), None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], "mgmt");
    }

    #[tokio::test]
    async fn get_config_handler_returns_404_when_absent() {
        // F19b: absent (or cross-tenant) → 404, never disclosed.
        let plane = static_plane(None);
        let (status, _body) =
            super::get_config(State(plane), Path("ghost".to_string()), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // ---- validate handler (F20) ----

    #[tokio::test]
    async fn validate_handler_returns_400_on_unparseable_body() {
        // F20a: a body the projection can't parse is a 400 (the one non-200 case).
        let plane = static_plane(None);
        let (status, Json(body)) = super::validate(
            State(plane),
            None,
            None,
            Path("mgmt".to_string()),
            Json(json!({ "context_policy": 123 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["valid"], json!(false));
    }

    #[tokio::test]
    async fn validate_handler_returns_200_valid_true() {
        // F20b: a parseable, valid config → 200 with valid:true.
        let plane = static_plane(None);
        let (status, Json(body)) = super::validate(
            State(plane),
            None,
            None,
            Path("mgmt".to_string()),
            Json(json!({ "model": { "id": "m" } })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["valid"], json!(true), "{body}");
    }

    #[tokio::test]
    async fn validate_handler_returns_200_valid_false_on_compile_failure() {
        // F20c (the F20 decoupling): validation is a query — an *invalid* config
        // still succeeds as a request (200) with valid:false + a routed issue.
        let plane = static_plane(None);
        let (status, Json(body)) = super::validate(
            State(plane),
            None,
            None,
            Path("mgmt".to_string()),
            Json(json!({ "model": { "id": "m" }, "tools": ["ghost"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["valid"], json!(false));
        assert_eq!(body["issues"][0]["path"], "tools");
    }

    // ---- put_config handler (F21) ----

    #[tokio::test]
    async fn put_config_handler_returns_400_on_parse_failure() {
        // F21a.
        let plane = static_plane(None);
        let (status, _body) = super::put_config(
            State(plane),
            None,
            Path("mgmt".to_string()),
            Json(json!({ "context_policy": 123 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_config_handler_returns_200_on_success() {
        // F21c: a well-formed body is stored → 200, and is then readable.
        let plane = static_plane(None);
        let (status, Json(body)) = super::put_config(
            State(plane.clone()),
            None,
            Path("mgmt".to_string()),
            Json(json!({ "model": { "id": "m" }, "system": "hi" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], "mgmt");
        let stored = plane
            .get(&ScopeId::from(DEFAULT_SCOPE), "mgmt")
            .await
            .unwrap();
        assert_eq!(stored.unwrap().instructions, "hi");
    }

    // ---- publish handler (F23) ----

    #[tokio::test]
    async fn publish_handler_returns_200_on_success() {
        // F23a.
        let plane = static_plane(None);
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &agent_config("mgmt")).await.unwrap();
        let (status, Json(body)) =
            super::publish(State(plane), None, None, Path("mgmt".to_string()), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["installed"], json!(true));
    }

    #[tokio::test]
    async fn publish_handler_returns_409_on_unresolvable() {
        // F23b (the status partition): an unresolvable Auto binding → 409.
        let plane = static_plane(Some(Arc::new(ErrResolver)));
        let scope = ScopeId::from(DEFAULT_SCOPE);
        plane.put(&scope, &auto_config("mgmt")).await.unwrap();
        let (status, _body) =
            super::publish(State(plane), None, None, Path("mgmt".to_string()), None).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn publish_handler_returns_400_on_other_publish_error() {
        // F23c: any other publish failure (here NotStored) stays a 400.
        let plane = static_plane(None);
        let (status, _body) =
            super::publish(State(plane), None, None, Path("ghost".to_string()), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn reserved_publication_requires_an_explicit_execution_workspace() {
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let plane = ConfigPlane::new(
            Arc::new(ConfigService::new(
                Arc::new(FakeResolver),
                Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
            )),
            Arc::new(SqliteConfigStore::open_in_memory().unwrap()),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let scope = ScopeId::from(RESERVED_ADMIN_SCOPE);
        plane.put(&scope, &agent_config("mgmt")).await.unwrap();

        let error = plane.publish(&scope, "mgmt").await.unwrap_err();
        assert!(matches!(error, PublishError::ExecutionWorkspaceRequired));

        let publication = plane
            .publish_for_execution_workspace(&scope, "workspace-real", "mgmt")
            .await
            .unwrap();
        assert_eq!(publication.agent_id, "mgmt");
        assert!(catalog.current("__admin", "mgmt").is_none());
        assert!(catalog.current("workspace-real", "mgmt").is_some());
    }

    // ==== SEC: cross-scope isolation of the config registry ====
    //
    // The tool catalog fence is covered (`admin_tools_compile_only_in_the_reserved_scope`),
    // but the config *registry* fence — that scope A's authored/published config
    // is invisible and un-actionable from scope B — was unproven end-to-end
    // through the scope edge. A hole here is a cross-tenant config disclosure.

    /// A config plane (the scope edge) over a shared real store handle, returned
    /// alongside the store so a test can read the durable rows directly.
    fn plane_over(store: Arc<SqliteConfigStore>) -> ConfigPlane {
        ConfigPlane::new(
            Arc::new(test_service()),
            store,
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        )
    }

    #[tokio::test]
    async fn config_registry_is_fenced_across_scopes() {
        let plane = plane_over(Arc::new(SqliteConfigStore::open_in_memory().unwrap()));
        let scope_a = ScopeId::from("wrkspc_a");
        let scope_b = ScopeId::from("wrkspc_b");

        // Author (and it exists) in scope A under an id another scope might reuse.
        plane
            .put(&scope_a, &agent_config("shared-id"))
            .await
            .unwrap();

        // get from B → None (the handler renders this as a 404, never disclosing A).
        assert!(
            plane.get(&scope_b, "shared-id").await.unwrap().is_none(),
            "scope B must not read scope A's config by id"
        );
        // absent from B's list.
        assert!(
            plane.list(&scope_b).await.unwrap().is_empty(),
            "scope B's list must not include scope A's config"
        );
        // publish from B → NotStored: B has nothing by that id to compile.
        let err = plane.publish(&scope_b, "shared-id").await.unwrap_err();
        assert!(
            matches!(err, PublishError::NotStored(_)),
            "scope B must not publish scope A's config: {err:?}"
        );

        // The fence is directional: A still owns and sees its row.
        assert!(plane.get(&scope_a, "shared-id").await.unwrap().is_some());
        assert_eq!(plane.list(&scope_a).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn executable_catalog_is_keyed_by_workspace_and_agent_id() {
        // Separate scope-bound registries may legitimately reuse a local Agent id
        // (for example when a router shards configuration storage). The live index
        // must preserve that external Workspace coordinate rather than collapse it.
        let registry_a = SqliteConfigStore::open_in_memory().unwrap();
        let registry_b = SqliteConfigStore::open_in_memory().unwrap();
        let (service, catalog) = test_service_and_catalog();

        let mut a = agent_config("shared-id");
        a.instructions = "workspace A".into();
        ConfigRegistry::put_config(&registry_a, &a).await.unwrap();
        service
            .publish(&scope("wrkspc_a"), &registry_a, &a.id, &[])
            .await
            .unwrap();

        let mut b = agent_config("shared-id");
        b.instructions = "workspace B".into();
        ConfigRegistry::put_config(&registry_b, &b).await.unwrap();
        service
            .publish(&scope("wrkspc_b"), &registry_b, &b.id, &[])
            .await
            .unwrap();

        assert_eq!(
            catalog
                .current("wrkspc_a", "shared-id")
                .unwrap()
                .snapshot
                .resolved_spec
                .instructions,
            "workspace A"
        );
        assert_eq!(
            catalog
                .current("wrkspc_b", "shared-id")
                .unwrap()
                .snapshot
                .resolved_spec
                .instructions,
            "workspace B"
        );
        assert!(
            catalog.current("wrkspc_c", "shared-id").is_none(),
            "an uninstalled Workspace must fail closed even when another Workspace uses the id"
        );
    }

    include!("config_service/publication_recovery_tests.rs");
    #[tokio::test]
    async fn durable_publication_survives_unavailable_registration_and_retry() {
        // Cause/effect graph:
        // C1 Control storage succeeds; C2 Coordinator registration is unavailable;
        // C3 the same publication is retried through a healthy registrar.
        // Effects: E1 the immutable publication remains durable after C2; E2 the
        // HTTP edge reports retryable 503; E3 C3 installs that same fingerprint
        // without a second publication row.
        //
        // Decision table:
        // | Rule | C1 | Registrar | Effect |
        // | R1 | T | unavailable | durable row + 503 |
        // | R2 | existing | healthy | same row + current registration |
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let scope = ScopeId::from("wrkspc_registration_retry");
        let unavailable = ConfigPlane::new(
            Arc::new(ConfigService::new(
                Arc::new(FakeResolver),
                Arc::new(UnavailableRegistrar),
            )),
            store.clone(),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        unavailable
            .put(&scope, &agent_config("retry-agent"))
            .await
            .unwrap();

        let (status, _) = publish(
            State(unavailable),
            Some(Extension(WorkspaceScope(scope.as_str().into()))),
            None,
            Path("retry-agent".into()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "R1/E2");
        let durable = store.list_published_scoped(&scope).await.unwrap();
        assert_eq!(durable.len(), 1, "R1/E1");

        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let retry = ConfigPlane::new(
            Arc::new(ConfigService::new(
                Arc::new(FakeResolver),
                Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
            )),
            store.clone(),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let registered = retry.publish(&scope, "retry-agent").await.unwrap();
        assert_eq!(registered.fingerprint, durable[0].fingerprint, "R2/E3");
        assert_eq!(store.list_published_scoped(&scope).await.unwrap().len(), 1);
        assert_eq!(
            catalog
                .current(scope.as_str(), "retry-agent")
                .unwrap()
                .snapshot
                .fingerprint
                .0,
            durable[0].fingerprint,
            "R2/E3"
        );
    }
}
