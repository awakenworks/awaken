//! Builder / configuration wiring for [`SharedHost`]: `new`, the chainable
//! `with_*` methods, per-thread registration, and the process dispatch pool.

use super::*;

/// Worker-side Resource content ports that must be installed as one unit.
/// Keeping the pair named prevents a partially remote Host composition from
/// silently opening a local content authority for the missing half.
struct WorkerContentAdapters {
    file_content_source: Arc<dyn crate::FileContentSource<awaken_run_ingress::RunClaim>>,
    memory_repository: Arc<dyn awaken_resource_contract::MemoryRepository>,
}
use awaken_runtime_contract::delegation::RunDelegationService;

impl SharedHost {
    /// Resolve or provision the stable local workspace coordinate owned by this
    /// installation. Composition roots call this once and pass the value to every
    /// resource adapter they assemble.
    pub fn provision_local_workspace() -> String {
        resolve_local_workspace(None)
    }

    /// Explicit-root variant for embedders/tests that do not configure through
    /// process environment.
    pub fn provision_local_workspace_at(root: &std::path::Path) -> String {
        resolve_local_workspace(Some(root))
    }

    /// Test-support selection of the same local extraction repository used by
    /// volatile/local host fixtures. Product composition injects its role-owned
    /// repository directly into the production constructor.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_memory_extraction_repository(
        storage_dir: Option<&std::path::Path>,
    ) -> Arc<dyn awaken_ext_memory::MemoryExtractionRepository> {
        local_memory_extraction_repository(storage_dir)
    }

    /// Volatile test/scenario host over `llm`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Self {
        Self::build(
            llm,
            model_ref.into(),
            None,
            None,
            None,
            crate::deployment_config::DeploymentConfig::ephemeral(),
        )
    }

    /// Test/scenario construction without an injected Resource component.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_deployment(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        deployment: crate::DeploymentConfig,
    ) -> Self {
        Self::build(llm, model_ref.into(), None, None, None, deployment)
    }

    /// Test-support replacement of a volatile fixture's deployment value.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_deployment_config(mut self, deployment: crate::DeploymentConfig) -> Self {
        self.session_blob_root = deployment.acp_session_blob_root.clone();
        self.deployment = deployment;
        self
    }

    /// The configured Session sandbox tier selected by the authoritative
    /// [`crate::DeploymentConfig`]. Composition adapters use this read-only view
    /// when their realization strategy must be compatible with the environment;
    /// it does not introduce another tier-selection source.
    #[must_use]
    pub fn sandbox_tier(&self) -> crate::SandboxTier {
        self.deployment.sandbox_tier
    }

    /// Install the exact durable dispatch transport owned by this host composition.
    ///
    /// An explicitly assembled queue is already the authority for ingress, so it
    /// also enables this host's dispatch pool. Embedders do not need to mutate the
    /// another process-global configuration source to activate a transport they
    /// supplied directly.
    #[must_use]
    pub fn with_dispatch_store(mut self, store: Arc<awaken_run_ingress::AnyDispatchStore>) -> Self {
        self.dispatch_store_override = Some(store);
        self.deployment.durable = true;
        self.deployment.disable_local_pool = false;
        self
    }

    /// Install the exact durable dispatch transport for a Coordinator-only Host.
    ///
    /// Unlike [`Self::with_dispatch_store`], this topology owns admission and
    /// completion observation but never drains the queue or realizes a Session
    /// Environment. A separately registered Worker is the sole physical effect
    /// owner. Keeping this as one constructor prevents call ordering from silently
    /// re-enabling the local pool after a composition selected remote placement.
    #[must_use]
    pub fn with_coordinator_dispatch_store(
        mut self,
        store: Arc<awaken_run_ingress::AnyDispatchStore>,
    ) -> Self {
        self.dispatch_store_override = Some(store);
        self.deployment.durable = true;
        self.deployment.disable_local_pool = true;
        self
    }

    /// Install the Coordinator-owned durable capability before the Host serves.
    #[must_use]
    pub fn with_runtime_authority(mut self, authority: Arc<dyn crate::RuntimeAuthority>) -> Self {
        self.authority = Some(authority);
        self
    }

    /// Install a dispatch port supplied by a database-less Worker transport.
    #[must_use]
    pub fn with_dispatch_port(
        self,
        dispatch: Arc<dyn awaken_run_ingress_contract::Dispatch>,
    ) -> Self {
        self.with_dispatch_store(Arc::new(
            awaken_run_ingress::AnyDispatchStore::from_dispatch(dispatch),
        ))
    }

    pub fn dispatch_store(&self) -> Result<Arc<awaken_run_ingress::AnyDispatchStore>, HostError> {
        self.dispatch_store_override.clone().map_or_else(
            || {
                self.authority
                    .as_ref()
                    .map(|authority| authority.dispatch_store())
                    .ok_or_else(|| {
                        HostError::internal(
                            "database-less Worker requires an injected dispatch transport",
                        )
                    })
            },
            Ok,
        )
    }

    /// Test/scenario construction with resources but no explicit deployment or
    /// durable extraction authority.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_resource_component(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        resources: awaken_resource_application::ResourceComponent,
    ) -> Self {
        Self::build(
            llm,
            model_ref.into(),
            Some(resources),
            None,
            None,
            crate::deployment_config::DeploymentConfig::ephemeral(),
        )
    }

    /// Construct directly from the composition root's resolved deployment.
    /// No resource or runtime backend is opened from process-global state first.
    pub fn new_with_resource_component_and_deployment(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        resources: awaken_resource_application::ResourceComponent,
        extraction_repository: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
        deployment: crate::DeploymentConfig,
    ) -> Self {
        Self::build(
            llm,
            model_ref.into(),
            Some(resources),
            None,
            Some(extraction_repository),
            deployment,
        )
    }

    /// Construct an execution Worker with its exact remote content adapters
    /// installed before any Resource data store is selected. Management-only
    /// File and durable extraction authorities fail closed; the Worker opens no
    /// File, Memory-content, or Memory-extraction authority database.
    pub fn new_worker_with_deployment(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        file_content_source: Arc<dyn crate::FileContentSource<awaken_run_ingress::RunClaim>>,
        memory_repository: Arc<dyn awaken_resource_contract::MemoryRepository>,
        deployment: crate::DeploymentConfig,
    ) -> Self {
        Self::build(
            llm,
            model_ref.into(),
            None,
            Some(WorkerContentAdapters {
                file_content_source,
                memory_repository,
            }),
            None,
            deployment,
        )
    }

    fn build(
        llm: Arc<dyn LlmExecutor>,
        model_ref: String,
        resources: Option<awaken_resource_application::ResourceComponent>,
        worker_content: Option<WorkerContentAdapters>,
        extraction_repository: Option<Arc<dyn awaken_ext_memory::MemoryExtractionRepository>>,
        deployment: crate::DeploymentConfig,
    ) -> Self {
        // Composition root: the deployment axes are parsed once from the environment
        // into one typed config. `DeploymentConfig::storage_dir` set → durable SQLite commit
        // and resource adapters (all survive a restart); unset → ephemeral adapters.
        let store_dir = deployment.storage_dir.clone();
        let local_workspace = resolve_local_workspace(store_dir.as_deref());
        let sandbox_root = crate::acp_backend::session_sandbox_base(&deployment);
        // Product Resource content is injected atomically through ResourceComponent;
        // a Worker receives its remote adapter. Only test-support construction may
        // still use the local opener below.
        let memory_stores = if let Some(content) = &worker_content {
            crate::memory_stores::MemoryStores::with_repository(content.memory_repository.clone())
        } else if let Some(plane) = &resources {
            crate::memory_stores::MemoryStores::with_repository(plane.memory_repository())
        } else {
            #[cfg(any(test, feature = "test-support"))]
            {
                crate::memory_stores::MemoryStores::open(store_dir.as_deref())
            }
            #[cfg(not(any(test, feature = "test-support")))]
            unreachable!("product Host construction requires an explicit Resource component")
        };
        let extraction_repository = match extraction_repository {
            Some(repository) => repository,
            None if worker_content.is_some() => {
                Arc::new(crate::unavailable_worker::UnavailableWorkerExtractions)
                    as Arc<dyn awaken_ext_memory::MemoryExtractionRepository>
            }
            None => {
                #[cfg(any(test, feature = "test-support"))]
                {
                    local_memory_extraction_repository(store_dir.as_deref())
                }
                #[cfg(not(any(test, feature = "test-support")))]
                unreachable!(
                    "product Host construction requires an explicit Memory extraction repository"
                )
            }
        };
        let memory = Arc::new(crate::memory::MemoryRuntime::new(
            llm.clone(),
            Arc::new(LocalProvider::new(sub_base("mem"))),
            Arc::new(BackgroundRuns::new()),
            extraction_repository,
        ));
        let mut skills = crate::skill_catalog::SkillCatalog::new();
        if let Some(plane) = &resources {
            skills.set_store(plane.skill_store());
        }
        let (file_store, file_catalog) = if worker_content.is_some() {
            let files = Arc::new(crate::unavailable_worker::UnavailableWorkerFiles);
            (
                files.clone() as Arc<dyn awaken_resource_contract::FileStore>,
                files as Arc<dyn awaken_resource_contract::FileCatalog>,
            )
        } else {
            resources.as_ref().map_or_else(
                || match store_dir.as_ref() {
                    Some(_) => unreachable!(
                        "product Host construction requires an injected Resource component"
                    ),
                    None => {
                        #[cfg(any(test, feature = "test-support"))]
                        {
                            let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
                            (
                                files.clone() as Arc<dyn awaken_resource_contract::FileStore>,
                                files as Arc<dyn awaken_resource_contract::FileCatalog>,
                            )
                        }
                        #[cfg(not(any(test, feature = "test-support")))]
                        unreachable!(
                            "product Host construction requires an explicit Resource component"
                        )
                    }
                },
                |plane| (plane.file_store(), plane.file_catalog()),
            )
        };
        let resource_reclamation = resources.as_ref().map(|plane| plane.reclamation());
        #[cfg(test)]
        let resource_reclamation =
            resource_reclamation.or_else(|| Some(super::tests::test_resource_reclamation()));
        #[cfg(not(test))]
        let file_application: Option<
            Arc<dyn awaken_resource_contract::FileApplicationService>,
        > = None;
        #[cfg(test)]
        let file_application = resource_reclamation.as_ref().map(|reclamation| {
            Arc::new(awaken_resource_application::FileApplication::new(
                file_store.clone(),
                file_catalog.clone(),
                reclamation.clone(),
            )) as Arc<dyn awaken_resource_contract::FileApplicationService>
        });
        let session_slots = crate::session_slot::SessionRuntimeSlots::default();
        let file_content_source: Arc<dyn crate::FileContentSource<awaken_run_ingress::RunClaim>> =
            worker_content
                .as_ref()
                .map(|content| content.file_content_source.clone())
                .unwrap_or_else(|| {
                    #[cfg(test)]
                    if let Some(application) = &file_application {
                        return Arc::new(
                            awaken_resource_application::ApplicationFileContentSource::new(
                                application.clone(),
                            ),
                        );
                    }
                    Arc::new(awaken_resource_contract::UnavailableFileContentSource)
                        as Arc<dyn crate::FileContentSource<awaken_run_ingress::RunClaim>>
                });
        let artifact_publisher: Arc<
            dyn awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::RunClaim>,
        > = {
            #[cfg(test)]
            if let Some(application) = &file_application {
                Arc::new(
                    awaken_resource_application::ApplicationArtifactPublisher::new(
                        application.clone(),
                    ),
                )
            } else {
                Arc::new(awaken_resource_contract::UnavailableArtifactPublisher)
            }
            #[cfg(not(test))]
            Arc::new(awaken_resource_contract::UnavailableArtifactPublisher)
        };
        let capture_decision = crate::redact::capture_decision(deployment.content_capture, false);
        Self {
            llm,
            model_ref,
            inference_routing: crate::inference_routing::InferenceRouting::new(
                session_slots.clone(),
            ),
            acp: None,
            acp_tool_exporter: None,
            remote_attempt_executor: None,
            remote_credential_realization: Default::default(),
            application_attempt_decorator: None,
            application_session_provisioner: None,
            application_session_control: None,
            provider: LocalProvider::new(sandbox_root.clone()),
            session_provider: crate::session_environment::SessionEnvironmentProvider::workdir(
                sandbox_root.clone(),
            ),
            cache_volume_prewarmer: crate::cache_volume::CacheVolumePrewarmer::default(),
            backend_owned_session_provider: None,
            session_provider_explicit: false,
            judge_snapshot: None,
            client_tools: HashSet::new(),
            local_workspace: local_workspace.clone(),
            session_slots: session_slots.clone(),
            skills,
            // Subagents share the parent's sandbox by default (`默认共用`).
            skill_fork_placement: crate::skills::SkillForkPlacement::SharedSession,
            plugin_ids: Vec::new(),
            plugin_config: std::collections::BTreeMap::new(),
            web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
            credential_materializer: None,
            hub: Arc::new(ThreadEventHub::new()),
            // `with_store_dir` still overrides this environment-derived default.
            store_dir: store_dir.clone(),
            // A shared root (e.g. a networked mount) enables cross-machine ACP
            // session recovery; unset means single-machine (stable config home).
            session_blob_root: deployment.acp_session_blob_root.clone(),
            session_blob_store: deployment
                .acp_session_blob_root
                .clone()
                .map(|root| Arc::new(awaken_run_executor_acp::FsSessionBlobStore::new(root))),
            upstream: None,
            memory_reference_encoder: None,
            worker_credential_resolver: None,
            deployment,
            memory,
            compaction: None,
            agent_publications: None,
            agent_resource_references: None,
            mcp_relay: tokio::sync::OnceCell::new(),
            dispatch_session_runtime: std::sync::RwLock::new(None),
            file_store,
            file_content_source,
            file_catalog,
            file_application,
            artifact_publisher,
            resource_reclamation,
            memory_stores,
            memory_mounter: std::sync::RwLock::new(None),
            gate_override: None,
            dispatch_pool: std::sync::OnceLock::new(),
            dispatch_maintenance: std::sync::OnceLock::new(),
            dispatch_store_override: None,
            authority: {
                #[cfg(any(test, feature = "test-support"))]
                {
                    Some(Arc::new(crate::EphemeralRuntimeAuthority::new()))
                }
                #[cfg(not(any(test, feature = "test-support")))]
                {
                    None
                }
            },
            completion: Arc::new(CompletionRegistry::default()),
            worker_stream_publisher: None,
            environment_binding_sink: std::sync::RwLock::new(None),
            capture_sink: std::sync::RwLock::new(None),
            capture_decision,
            data_subject_consent: Arc::new(awaken_runtime_contract::NullResolver),
            admin_tools: Vec::new(),
        }
    }

    /// Install the Worker process's one opaque local-credential adapter.
    #[must_use]
    pub fn with_worker_credential_resolver(
        mut self,
        resolver: Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>,
    ) -> Self {
        self.worker_credential_resolver = Some(resolver);
        self
    }

    #[must_use]
    pub fn with_acp_tool_exporter(mut self, exporter: Arc<dyn crate::AcpToolExporter>) -> Self {
        self.acp_tool_exporter = Some(exporter);
        self
    }

    /// Install the canonical exact credential materializer used by runtime
    /// extensions such as paid WebSearch. The value is cloned from the same
    /// composition-root instance used by Managed MCP/Resource realization.
    #[must_use]
    pub fn with_credential_materializer(
        mut self,
        materializer: crate::PinnedCredentialMaterializer,
    ) -> Self {
        self.credential_materializer = Some(materializer);
        self
    }

    /// Add one externally owned WebSearch provider to the authoritative
    /// registry. Duplicate ids fail at composition time instead of shadowing a
    /// built-in or changing dispatch order at runtime.
    pub fn with_web_search_provider(
        mut self,
        provider: Arc<dyn awaken_ext_builtin_tools::WebSearchProvider>,
    ) -> Result<Self, awaken_ext_builtin_tools::WebSearchRegistryError> {
        self.web_search_providers.register(provider)?;
        Ok(self)
    }

    /// Replace the built-in registry with one composition-owned registry. Use
    /// the same clone for capability projection and semantic publication
    /// validation to keep discovery, validation, and dispatch identical.
    #[must_use]
    pub fn with_web_search_provider_registry(
        mut self,
        providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    ) -> Self {
        self.web_search_providers = providers;
        self
    }

    /// Read-only provider discovery for an embedding capability endpoint. The
    /// returned registry is the same value Session dispatch uses.
    #[must_use]
    pub fn web_search_provider_registry(
        &self,
    ) -> &awaken_ext_builtin_tools::WebSearchProviderRegistry {
        &self.web_search_providers
    }

    /// Install the adapter that drives snapshot-selected remote attempts. The
    /// host depends only on the neutral attempt port; A2A construction remains a
    /// composition-root responsibility.
    pub fn with_remote_attempt_executor(
        mut self,
        installation: super::RemoteAttemptInstallation,
    ) -> Self {
        self.remote_attempt_executor = Some(installation.executor);
        self.remote_credential_realization = installation.credential_realization;
        self
    }

    /// Wrap the complete per-Session Native/ACP/A2A attempt boundary.
    ///
    /// The Host always constructs the authoritative backend router first. The
    /// application can adapt envelopes, capabilities, and business results
    /// around it, but cannot replace backend selection.
    #[must_use]
    pub fn with_application_attempt_decorator(
        mut self,
        decorator: super::AttemptExecutorDecorator,
    ) -> Self {
        self.application_attempt_decorator = Some(decorator);
        self
    }

    /// Install the only application hook that may add claim-bound material to
    /// the authoritative Session environment before it is created or adopted.
    #[must_use]
    pub fn with_application_session_provisioner(
        mut self,
        provisioner: Arc<dyn awaken_session_contract::ApplicationSessionProvisioner>,
    ) -> Self {
        self.application_session_provisioner = Some(provisioner);
        self
    }

    /// Install the sole outbound claim-fenced contribution client. It is paired
    /// with `ApplicationSessionProvisioner`; neither is useful as a local
    /// Session-authoring path.
    #[must_use]
    pub fn with_application_session_control(
        mut self,
        control: Arc<dyn awaken_run_ingress_contract::ClaimedSessionControl>,
    ) -> Self {
        self.application_session_control = Some(control);
        self
    }

    /// Platform-managed local workspace used when no authenticated/path scope
    /// exists. Composition roots may override it with a provisioned coordinate.
    #[must_use]
    pub fn with_local_workspace(mut self, workspace: impl Into<String>) -> Self {
        let workspace = workspace.into();
        assert!(
            !workspace.trim().is_empty(),
            "local workspace must not be empty"
        );
        self.local_workspace = workspace;
        self
    }

    /// The platform-managed local workspace coordinate.
    pub fn local_workspace(&self) -> &str {
        &self.local_workspace
    }

    /// The resolved durable storage root, when this host is persistent.
    ///
    /// Composition roots use this to place sibling repositories beside runtime
    /// truth without re-reading process configuration or teaching the runtime
    /// about those repositories.
    pub fn storage_dir(&self) -> Option<&std::path::Path> {
        self.store_dir.as_deref()
    }

    /// Test-support replacement of a volatile fixture's Resource lifecycle port.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_resource_reclamation(
        mut self,
        repository: Arc<dyn awaken_resource_contract::ResourceReclamationRepository>,
    ) -> Self {
        #[cfg(test)]
        {
            let application = Arc::new(awaken_resource_application::FileApplication::new(
                self.file_store.clone(),
                self.file_catalog.clone(),
                repository.clone(),
            ))
                as Arc<dyn awaken_resource_contract::FileApplicationService>;
            self.file_content_source = Arc::new(
                awaken_resource_application::ApplicationFileContentSource::new(application.clone()),
            );
            self.artifact_publisher = Arc::new(
                awaken_resource_application::ApplicationArtifactPublisher::new(application.clone()),
            );
            self.file_application = Some(application);
        }
        self.resource_reclamation = Some(repository);
        self
    }

    /// Install the one Resources-owned File command application. Runtime stores
    /// only this inward port and cannot construct a parallel implementation.
    #[must_use]
    pub fn with_file_application(
        mut self,
        application: Arc<dyn awaken_resource_contract::FileApplicationService>,
        content_source: Arc<dyn crate::FileContentSource<awaken_run_ingress::RunClaim>>,
        artifact_publisher: Arc<
            dyn awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::RunClaim>,
        >,
    ) -> Self {
        self.file_content_source = content_source;
        self.artifact_publisher = artifact_publisher;
        self.file_application = Some(application);
        self
    }

    /// Install the narrow artifact publisher used by a database-less Worker.
    /// This does not grant File management or catalog access.
    #[must_use]
    pub fn with_artifact_publisher(
        mut self,
        publisher: Arc<
            dyn awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::RunClaim>,
        >,
    ) -> Self {
        // This is the database-less Worker composition edge. Retaining the
        // test-only/local File application here would create a second command
        // path and incorrectly classify the remote publisher as locally fenced.
        self.file_application = None;
        self.artifact_publisher = publisher;
        self
    }

    pub fn resource_reclamation(
        &self,
    ) -> Option<Arc<dyn awaken_resource_contract::ResourceReclamationRepository>> {
        self.resource_reclamation.clone()
    }

    pub(crate) fn register_thread_workspace(&self, thread: &str, workspace: &str) {
        self.session_slots
            .update(thread, |slot| slot.workspace = Some(workspace.to_string()));
    }

    pub(crate) fn register_thread_agent_projection(&self, thread: &str, agent_id: &str) {
        self.session_slots
            .update(thread, |slot| slot.agent_id = Some(agent_id.to_string()));
    }

    /// Rebuildable Agent projection used only as input to Session admission.
    /// Runtime never treats this cache as an executable publication authority.
    pub fn thread_agent_projection(&self, thread: &str) -> Option<String> {
        self.session_slots
            .read(thread, |slot| slot.agent_id.clone())
            .flatten()
    }

    /// Workspace context supplied to the Session application admission boundary.
    /// Unknown flat-protocol threads use the process's persisted local workspace.
    pub fn thread_workspace(&self, thread: &str) -> String {
        self.session_slots
            .read(thread, |slot| slot.workspace.clone())
            .flatten()
            .unwrap_or_else(|| self.local_workspace.clone())
    }

    /// Workspace for a prepared thread; unlike `thread_workspace`, unknown ids
    /// do not inherit local ownership and therefore cannot enumerate artifacts.
    pub fn registered_thread_workspace(&self, thread: &str) -> Option<String> {
        self.session_slots
            .read(thread, |slot| slot.workspace.clone())
            .flatten()
    }

    /// Whether this process owns a co-located dispatch pool. Composition roots use
    /// the same parsed deployment value that admission uses, so the process cannot
    /// accidentally both advertise coordinator-only behavior and drain locally.
    #[must_use]
    pub fn runs_local_dispatch_pool(&self) -> bool {
        !self.deployment.disable_local_pool
    }

    /// Stable logical identity used by both local dispatch claims and local
    /// Session realization. The process incarnation is deliberately separate.
    #[must_use]
    pub fn dispatch_owner(&self) -> &str {
        &self.deployment.dispatch_owner
    }

    /// Clone the completion projection consumed by the authenticated Worker
    /// settle boundary.
    ///
    /// A coordinator-only embedding has no local dispatch pool. Its foreground
    /// Managed Session turn therefore registers with this sink before enqueueing
    /// and the remote Worker transport projects the committed terminal/awaiting
    /// state back into the same host. The sink is notification-only: committed
    /// Run truth remains authoritative and the bounded timeout retains its
    /// existing one-read recovery fallback.
    pub fn dispatch_completion_sink(&self) -> Arc<dyn awaken_run_ingress::CompletionSink> {
        self.completion.clone()
    }

    /// Register the management assistant's tool executables globally (ADR-0052). They
    /// run on every thread's runtime, but only the reserved-scope assistant's config
    /// names them, so only its runs can invoke them; their ids are auto-allowed on the
    /// gate (the tools are read-only).
    #[must_use]
    pub fn with_admin_tools(
        mut self,
        tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
    ) -> Self {
        self.admin_tools = tools;
        self
    }

    /// Wire the subject-tagged captured-content sink before sharing the Host.
    #[must_use]
    pub fn with_capture_sink(self, sink: Arc<dyn awaken_runtime_contract::CaptureSink>) -> Self {
        *self
            .capture_sink
            .write()
            .expect("capture sink lock poisoned") = Some(sink);
        self
    }

    /// Install the canonical Host-owned capture sink at a late composition edge.
    ///
    /// This is intentionally the same storage used by [`with_capture_sink`], not
    /// a process-global fallback. Existing sessions retain their immutable
    /// per-session dependencies; subsequently created sessions receive this sink.
    pub fn install_capture_sink(&self, sink: Arc<dyn awaken_runtime_contract::CaptureSink>) {
        *self
            .capture_sink
            .write()
            .expect("capture sink lock poisoned") = Some(sink);
    }

    /// Install the Control-owned consent read port used once per attributed Run.
    #[must_use]
    pub fn with_data_subject_consent_source(
        mut self,
        source: Arc<dyn awaken_runtime_contract::DataSubjectConsentSource>,
    ) -> Self {
        self.data_subject_consent = source;
        self
    }

    /// Deployment-resolved capture ceiling used by protocol decision projections.
    #[must_use]
    pub fn content_capture_ceiling(&self) -> awaken_runtime_contract::ContentCapture {
        self.capture_decision.level
    }

    /// Enable context compaction. Once a turn's conversation exceeds `threshold`
    /// messages, the `compact` plugin's `BeforeInference` hook summarizes everything
    /// but the last `keep_last` messages (through a `compactor` sub-agent) and injects
    /// the summary as request-only context and then activates a matching Run-scoped
    /// window so those covered raw turns drop from the model view. Non-destructive:
    /// committed truth is never rewritten (G13). The bounds are also exposed as the
    /// `compact` config section, so a per-run `plugin_config` can override them.
    pub fn with_compaction(self, threshold: usize, keep_last: usize) -> Self {
        self.enable_compaction(CompactConfig {
            threshold,
            keep_last,
            ..CompactConfig::default()
        })
    }

    /// Enable **token-aware** context compaction: fold once the estimated context
    /// reaches `trigger_ratio` of the model's `max_tokens` window (the "auto-compact
    /// at N% of the window" behavior), keeping the last `keep_last` messages. This
    /// is how compaction becomes aware of the model's max token instead of a bare
    /// message count. The request window activates only after a summary succeeds.
    pub fn with_compaction_tokens(
        self,
        max_tokens: u32,
        trigger_ratio: f64,
        keep_last: usize,
    ) -> Self {
        self.enable_compaction(CompactConfig {
            max_tokens: Some(max_tokens),
            trigger_ratio,
            keep_last,
            ..CompactConfig::default()
        })
    }

    /// Install a resolved `CompactConfig` and wire the `compactor` sub-agent.
    fn enable_compaction(mut self, config: CompactConfig) -> Self {
        self.compaction = Some(crate::compact::Compaction { config });
        self
    }

    /// Test-support shorthand for an unauthenticated Worker upstream.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_upstream(mut self, url: impl Into<String>) -> Self {
        self.upstream = Some(awaken_worker_transport_security::WorkerUpstream::new(url));
        self
    }

    /// Configure an authenticated worker upstream shared by dispatch and commit
    /// clients (for example a client carrying an mTLS identity).
    #[must_use]
    pub fn with_worker_upstream(
        mut self,
        upstream: awaken_worker_transport_security::WorkerUpstream,
    ) -> Self {
        self.upstream = Some(upstream);
        self
    }

    /// Install the Resource transport's opaque Memory capability encoder.
    /// The execution host does not own or inspect the wire representation.
    #[must_use]
    pub fn with_memory_reference_encoder(
        mut self,
        encoder: Arc<
            dyn awaken_resource_contract::MemoryMaterializationReferenceEncoder<
                    awaken_run_ingress::RunClaim,
                >,
        >,
    ) -> Self {
        self.memory_reference_encoder = Some(encoder);
        self
    }

    /// Install the registered Worker's claim-fenced live-progress publisher.
    #[must_use]
    pub fn with_worker_stream_publisher(
        mut self,
        publisher: Arc<dyn awaken_run_ingress::ClaimedStreamPublisher>,
    ) -> Self {
        self.worker_stream_publisher = Some(publisher);
        self
    }

    /// Await in-flight background auxiliary work (Memory extraction and Compact
    /// prefetch) up to `timeout` during shutdown. Returns `true` if all finished.
    pub async fn drain_memory(&self, timeout: std::time::Duration) -> bool {
        self.memory.drain(timeout).await
    }

    /// Replace the default authorization gate on every thread with `gate` (slice
    /// E): the scheduled-action server uses a gate that defers tool calls so the
    /// durable worker performs them (ADR-0020).
    pub fn with_gate_override(
        mut self,
        gate: Arc<dyn awaken_runtime_contract::permission::ToolGateHook>,
    ) -> Self {
        self.gate_override = Some(gate);
        self
    }

    /// Supply immutable executable publications without mounting the authoring
    /// plane. Intended for embedded composition roots and scenario fixtures.
    pub fn with_agent_publications(
        mut self,
        source: Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
    ) -> Self {
        self.agent_publications = Some(source);
        self
    }

    /// Supply the Coordinator projection of current Agent-to-Resource bindings.
    pub fn with_agent_resource_references(
        mut self,
        source: Arc<dyn awaken_resource_contract::AgentResourceReferenceSource>,
    ) -> Self {
        self.agent_resource_references = Some(source);
        self
    }

    /// Add client-executed tools: those ids are model-visible but unregistered, so
    /// a call awaits and the client supplies the result.
    pub fn with_client_tools(mut self, client_tools: HashSet<String>) -> Self {
        self.client_tools.extend(client_tools);
        self
    }

    /// Whether `context: fork` Skills reuse the Session environment. Ordinary
    /// delegated child Runs are unaffected and always share it.
    pub fn with_skill_fork_placement(
        mut self,
        placement: crate::skills::SkillForkPlacement,
    ) -> Self {
        self.skill_fork_placement = placement;
        self
    }

    /// Activate the tool state machine on every thread with `config` (its
    /// `{"machines":[…]}` section). The plugin gates and advances tool calls per
    /// the declared transitions (ADR tool-state-machine).
    pub fn with_state_machine(mut self, config: serde_json::Value) -> Self {
        self.plugin_ids
            .push(awaken_ext_state_machine::STATE_MACHINE_PLUGIN_ID.to_string());
        self.plugin_config.insert(
            awaken_ext_state_machine::STATE_MACHINE_PLUGIN_ID.to_string(),
            config,
        );
        self
    }

    /// Offer skills on every thread (ADR-0036): they are fronted by the single
    /// `Skill` tool, whose catalog lists them and whose invocation returns the
    /// activated skill's instructions. The host stays out of skill
    /// authoring/collection — it only carries the offered set.
    pub fn with_skills(mut self, skills: Vec<SkillSpec>) -> Self {
        self.skills.add_specs(skills);
        self
    }

    /// Test/scenario shorthand for a filesystem Skill store.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_skill_store(mut self, dir: impl Into<PathBuf>) -> Self {
        let store = Arc::new(
            awaken_skill_store::FsSkillStore::open(dir.into())
                .expect("open durable skill store root"),
        );
        self.skills.set_bundle_source(Arc::new(
            awaken_resource_application::StoreSkillBundleSource::new(store.clone()),
        ));
        self.skills.set_store(store);
        self
    }

    /// Test-support replacement of a volatile fixture's Skill store.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_skill_store_backend(
        mut self,
        store: Arc<dyn awaken_resource_contract::SkillStore>,
    ) -> Self {
        self.skills.set_bundle_source(Arc::new(
            awaken_resource_application::StoreSkillBundleSource::new(store.clone()),
        ));
        self.skills.set_store(store);
        self
    }

    /// Install only the exact immutable custom-Skill read port used by an
    /// execution Worker. This does not grant authoring or catalog access.
    pub fn with_skill_bundle_source(
        mut self,
        source: Arc<dyn awaken_session_contract::SkillBundleSource<awaken_run_ingress::RunClaim>>,
    ) -> Self {
        self.skills.set_bundle_source(source);
        self
    }

    /// Override the default tool-free Outcome Judge with the named Agent. The
    /// Judge runs through the same Run boundary in its own fresh context.
    pub fn with_judge(mut self, judge_agent_id: impl Into<String>) -> Self {
        let id = judge_agent_id.into();
        let snapshot = default_judge_agent(&self.model_ref, &id, DEFAULT_JUDGE_INSTRUCTIONS);
        self.judge_snapshot = Some(snapshot);
        self
    }

    /// Pin an arbitrary executable Agent snapshot as the Outcome Grader. Its
    /// backend may be Native or ACP; both execute through the same Run boundary.
    pub fn with_judge_snapshot(mut self, snapshot: ExecutableAgentSnapshot) -> Self {
        self.judge_snapshot = Some(snapshot);
        self
    }

    /// Test-support upgrade of a volatile fixture to local durable storage.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_store_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        // A durable host must place its Workdir sandboxes under a stable root too;
        // otherwise the dispatch row survives restart but its persisted handle
        // points into the previous process's random temp directory.
        let provider = LocalProvider::new(dir.join("sandboxes"));
        if let Some(mounter) = self
            .memory_mounter
            .read()
            .expect("memory mounter lock poisoned")
            .clone()
        {
            provider.install_memory_mounter(mounter);
        }
        self.provider = provider;
        std::fs::create_dir_all(&dir)
            .expect("create durable Memory extraction repository directory");
        self.memory.set_extraction_repository(Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(
                &dir.join("sessions.db").to_string_lossy(),
            )
            .expect("open durable Memory extraction repository"),
        ));
        self.session_provider = self.session_provider.at_root(dir.join("sandboxes"));
        self.backend_owned_session_provider = self
            .backend_owned_session_provider
            .as_ref()
            .map(|provider| provider.at_root(dir.join("trusted-local-sandboxes")));
        if let Some(mounter) = self.memory_mounter() {
            self.session_provider
                .install_memory_mounter(mounter.clone());
            if let Some(provider) = &self.backend_owned_session_provider {
                provider.install_memory_mounter(mounter);
            }
        }
        self.store_dir = Some(dir);
        self
    }

    /// Harvest/restore each ACP CLI's session under `dir` (keyed by thread+adapter)
    /// so it recovers across directories and — when `dir` is a shared location —
    /// across workers/machines. Consumed by [`Self::with_projected_acp`]. Leave unset
    /// on a single machine, where the per-thread config home is already stable.
    #[must_use]
    pub fn with_session_blob_root(mut self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        self.session_blob_store = Some(Arc::new(awaken_run_executor_acp::FsSessionBlobStore::new(
            dir.clone(),
        )));
        self.session_blob_root = Some(dir);
        self
    }

    /// The ACP session-content eraser, when portable session persistence is on.
    #[must_use]
    pub fn session_blob_eraser(&self) -> Option<Arc<dyn awaken_runtime_contract::ContentEraser>> {
        self.session_blob_store
            .as_ref()
            .map(|store| store.clone() as Arc<dyn awaken_runtime_contract::ContentEraser>)
    }

    /// Install runtime-only inference materialization. Remote workers use this
    /// without a configuration resolver because admission already pinned access.
    pub fn with_inference_materializer(
        mut self,
        materializer: Arc<dyn awaken_runtime_contract::inference::InferenceExecutorMaterializer>,
    ) -> Self {
        self.memory.set_inference_materializer(materializer.clone());
        self.inference_routing.set_materializer(materializer);
        self
    }

    /// Install the credential-file broker shared by every Session sandbox tier.
    /// Application plans retain opaque `MountSource::Secret` references; only
    /// the selected provider asks this port for bytes at realization time.
    #[must_use]
    pub fn with_session_secret_broker(
        self,
        broker: Arc<dyn awaken_provisioning_contract::SecretBroker>,
    ) -> Self {
        self.session_provider.install_secret_broker(broker.clone());
        if let Some(provider) = &self.backend_owned_session_provider {
            provider.install_secret_broker(broker);
        }
        self
    }

    /// Test-support replacement of a volatile fixture's File content port.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_file_content_source(
        mut self,
        source: Arc<dyn crate::FileContentSource<awaken_run_ingress::RunClaim>>,
    ) -> Self {
        self.file_content_source = source;
        self
    }

    /// Test-support replacement of a volatile fixture's Memory content port.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_memory_repository(
        mut self,
        repository: Arc<dyn awaken_resource_contract::MemoryRepository>,
    ) -> Self {
        self.memory_stores = crate::memory_stores::MemoryStores::with_repository(repository);
        self
    }

    /// Install the already-composed provider that owns every Session container
    /// environment on this Host.
    ///
    /// This is the public composition seam for downstream providers. The Host
    /// continues to own Session routing and lifecycle; the provider supplies only
    /// its enforceable Sandbox capabilities and create/adopt implementation. A
    /// caller that also installs a [`awaken_provisioning_contract::SecretBroker`]
    /// must do so after selecting the provider so there is one broker installation
    /// on the authoritative environment path.
    #[must_use]
    pub fn with_session_container_provider(
        self,
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        hand_factory: Arc<dyn crate::HandExecutorFactory>,
    ) -> Self {
        self.with_session_container_provider_and_capacity(provider, None, hand_factory)
    }

    /// Install one container provider and its optional never-used-capacity owner.
    /// Keeping both handles from the same composition prevents provider erasure
    /// from orphaning startup warmup and shutdown drain.
    #[must_use]
    pub fn with_session_container_provider_and_capacity(
        mut self,
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        capacity: Option<Arc<dyn awaken_sandbox_container::ContainerEnvironmentCapacity>>,
        hand_factory: Arc<dyn crate::HandExecutorFactory>,
    ) -> Self {
        let hand_bin = self.deployment.sandbox.container_hand_bin.clone();
        self.session_provider = crate::session_environment::SessionEnvironmentProvider::
            container_with_capacity_and_hand_idle(
                provider,
                capacity,
                Vec::new(),
                hand_factory,
                hand_bin,
                std::time::Duration::from_secs(self.deployment.sandbox.container_hand_idle_secs),
            );
        self.session_provider_explicit = true;
        if let Some(mounter) = self.memory_mounter() {
            self.session_provider.install_memory_mounter(mounter);
        }
        self
    }

    /// Replace the default directory-only CacheVolume preparation with one
    /// product-specific initializer. Explicit warmup and Session creation keep
    /// sharing the same single-flight owner.
    #[must_use]
    pub fn with_cache_volume_initializer(
        mut self,
        initializer: Arc<dyn crate::CacheVolumeInitializer>,
    ) -> Self {
        self.cache_volume_prewarmer = crate::cache_volume::CacheVolumePrewarmer::new(initializer);
        self
    }

    /// Eagerly prepare one caller-owned CacheVolume. The same `(key, host_path)`
    /// is a no-op when Session realization later requests it.
    pub async fn prewarm_cache_volume(
        &self,
        key: impl Into<String>,
        host_path: impl Into<std::path::PathBuf>,
    ) -> Result<(), String> {
        self.cache_volume_prewarmer
            .prewarm(crate::CacheVolumeWarmup::host_path(key, host_path))
            .await
    }

    /// Warm the deployment's canonical empty Session shape to the target selected
    /// by the Worker's one global capacity plan.
    pub async fn prewarm_default_environment_capacity(
        &self,
        target: usize,
    ) -> Result<usize, awaken_provisioning_contract::SandboxError> {
        if self.deployment.disable_local_pool
            || self.deployment.sandbox.warm_pool_size == 0
            || target == 0
        {
            return Ok(0);
        }
        self.session_provider
            .prewarm(
                &crate::provisioning::agent_run_sandbox_spec("environment-warmup"),
                target,
            )
            .await
    }

    #[must_use]
    pub fn default_environment_capacity_shape(
        &self,
    ) -> awaken_provisioning_contract::SandboxCapacityShapeId {
        awaken_provisioning_contract::SandboxCapacityShapeId::from_spec(
            &crate::provisioning::agent_run_sandbox_spec("environment-warmup"),
        )
        .expect("default Environment capacity spec is mount-less")
    }

    pub async fn discard_default_environment_capacity(&self) {
        self.session_provider
            .discard_capacity(&crate::provisioning::agent_run_sandbox_spec(
                "environment-warmup",
            ))
            .await;
    }

    /// Reconcile one current executable Environment into the exact mount-less
    /// shape ordinary Session creation requests. Docker, Podman and Kubernetes
    /// all enter through the installed provider/capacity pair.
    pub async fn prewarm_environment_snapshot(
        &self,
        environment: &awaken_session_contract::EnvironmentSnapshot,
        target: usize,
    ) -> Result<usize, awaken_provisioning_contract::SandboxError> {
        if self.deployment.disable_local_pool
            || self.deployment.sandbox.warm_pool_size == 0
            || target == 0
            || environment.self_hosted
        {
            return Ok(0);
        }
        let projection = crate::provisioning::environment_capacity_projection(
            environment,
            self.session_provider.capabilities().network_isolation,
        );
        self.session_provider
            .prewarm(&projection.spec, target)
            .await
    }

    /// Per-shape target and the global ready-capacity budget across both the
    /// default Session shape and current Environment shapes.
    #[must_use]
    pub fn environment_warmup_limits(&self) -> (usize, usize) {
        if self.deployment.disable_local_pool || self.deployment.sandbox.warm_pool_size == 0 {
            return (0, 0);
        }
        (
            self.deployment.sandbox.warm_pool_size,
            self.deployment.sandbox.warm_pool_total_size,
        )
    }

    /// Observe actual ready capacity instead of trusting the last reconciliation
    /// result; checkout and global eviction may change it between heartbeats.
    #[must_use]
    pub fn ready_environment_snapshot_capacity(
        &self,
        environment: &awaken_session_contract::EnvironmentSnapshot,
    ) -> usize {
        let projection = crate::provisioning::environment_capacity_projection(
            environment,
            self.session_provider.capabilities().network_isolation,
        );
        self.session_provider.ready_capacity(&projection.spec)
    }

    /// Exact identity published in Worker receipts for the same canonical spec
    /// consumed by the installed capacity provider.
    #[must_use]
    pub fn environment_snapshot_capacity_shape(
        &self,
        environment: &awaken_session_contract::EnvironmentSnapshot,
    ) -> awaken_provisioning_contract::SandboxCapacityShapeId {
        crate::provisioning::environment_capacity_projection(
            environment,
            self.session_provider.capabilities().network_isolation,
        )
        .shape_id
    }

    /// Drop unused capacity for an Environment shape removed from current
    /// desired state. Active Sessions are unaffected.
    pub async fn discard_environment_snapshot_capacity(
        &self,
        environment: &awaken_session_contract::EnvironmentSnapshot,
    ) {
        let projection = crate::provisioning::environment_capacity_projection(
            environment,
            self.session_provider.capabilities().network_isolation,
        );
        self.session_provider
            .discard_capacity(&projection.spec)
            .await;
    }

    /// Dispose never-used warm capacity after claim admission and in-flight work
    /// have drained. Active Session environments remain owned by their slots.
    pub async fn shutdown_environment_capacity(&self) {
        self.session_provider.shutdown_capacity().await;
    }

    /// Bind `model_ref` to `thread` (R2/R5), staged before its first turn.
    pub fn register_thread_model(&self, thread: &str, model_ref: impl Into<String>) {
        self.inference_routing.register(thread, model_ref);
    }

    /// Bind the exact published delegation targets to a prepared Session.
    pub fn register_thread_delegates(&self, thread: &str, delegates: Vec<String>) {
        self.session_slots
            .update(thread, |slot| slot.delegates = delegates);
    }

    pub(crate) fn thread_credential_realization(
        &self,
        thread: &str,
    ) -> Option<awaken_runtime_contract::CredentialRealizationProfile> {
        self.session_slots
            .read(thread, |slot| {
                slot.environment_projection
                    .as_ref()
                    .map(|environment| environment.credential_realization.clone())
            })
            .flatten()
    }

    pub(crate) fn thread_delegate_ids(&self, thread: &str) -> Option<Vec<String>> {
        self.session_slots
            .read(thread, |slot| slot.delegates.clone())
    }

    /// The model id echoed by adapters in their session/agent objects.
    pub fn model(&self) -> String {
        self.model_ref.clone()
    }

    /// The primary model currently selected for `thread`, including a Managed
    /// per-session/per-turn override. This is a read-only projection of the same
    /// inference-routing source that execution consumes.
    pub fn model_for_thread(&self, thread: &str) -> String {
        self.inference_routing.model_ref(thread, &self.model_ref)
    }

    /// The set of client-executed tool ids (model-visible, host-unregistered).
    pub fn client_tools(&self) -> &HashSet<String> {
        &self.client_tools
    }

    /// The shared per-thread live observation hub.
    pub fn hub(&self) -> &Arc<ThreadEventHub> {
        &self.hub
    }

    /// Route every run's tool calls through `hand` — a remote `ToolExecutor`
    /// Build the per-session delegation executor. `sandbox` is the calling thread's
    /// live environment: a native delegate shares it, so the parent
    /// and its native child collaborate in one Session-owned workspace.
    pub(crate) fn run_delegation(
        &self,
        thread: &str,
        sandbox: Arc<crate::session_environment::SessionEnvironment>,
        permission: Arc<dyn awaken_runtime_contract::permission::ToolPermissionPolicy>,
        commit: Arc<crate::store::HostCommit>,
        parent_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<Option<Arc<dyn RunDelegationService>>, HostError> {
        let Some(parent_snapshot) = parent_snapshot else {
            return Ok(None);
        };
        if parent_snapshot
            .resolved_spec
            .plugin_config
            .agent
            .delegates
            .is_empty()
        {
            return Ok(None);
        }
        let scheduler = if self.deployment.durable {
            let recovery_projection = commit.recovery_projection();
            let claimed_commit = self
                .upstream
                .as_ref()
                .map(crate::commit_ingest::remote_claimed_commit)
                .transpose()?;
            Some(crate::agent_runner::RunScheduler {
                store: self.dispatch_store()?,
                commit: commit.clone(),
                reader: commit,
                owner: self.deployment.dispatch_owner.clone(),
                claimed_commit,
                recovery_projection,
                session_resources: self.thread_resource_manifest(thread),
            })
        } else {
            None
        };
        let acp = self.acp.clone().map(|acp| {
            let sandbox = sandbox.clone();
            let permission = permission.clone();
            Arc::new(move |backend| {
                Ok(
                    acp.executor_for(sandbox.clone(), permission.clone(), backend, Vec::new())
                        as Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>,
                )
            }) as crate::agent_runner::ChildAcpExecutorFactory
        });
        let service = HostRunDelegationService::new(
            self.llm.clone(),
            sandbox,
            parent_snapshot,
            crate::agent_runner::ChildExecutionAdapters {
                acp,
                remote: self.remote_attempt_executor.clone(),
                remote_credentials: self.remote_credential_realization.clone(),
            },
            self.agent_publications.clone(),
            self.thread_workspace(thread),
        )
        .map_err(|error| HostError::bad_request(error.to_string()))?
        .with_scheduler(scheduler);
        Ok(Some(Arc::new(service)))
    }

    /// Spawn the one process-level [`DispatchPool`] (O2), once, when durable ingress
    /// is enabled. Called by `mount` — the single seam that owns an `Arc<SharedHost>`
    /// — because the pool's resolver needs a back-reference to open sessions. The
    /// pool is the sole claimer of the shared queue; it drives each claimed run by
    /// routing it to the worker that owns its thread (recovering crashed runs and
    /// draining background submissions without a foreground request). Idempotent.
    pub fn ensure_dispatch_pool(self: &Arc<Self>) {
        if !self.deployment.durable || self.dispatch_pool.get().is_some() {
            return;
        }
        let Ok(store) = self.dispatch_store() else {
            // Postgres backend not yet initialised — a later `mount` after
            // Coordinator composition supplies the shared dispatch authority and
            // this Host owns only its process-local claimer.
            return;
        };
        let resolver: Arc<dyn WorkerResolver<AnyDispatchStore>> = Arc::new(HostWorkerResolver {
            host: Arc::downgrade(self),
        });
        let config = DispatchServiceConfig {
            lease_renewal_interval: Some(awaken_run_ingress::DEFAULT_LEASE_RENEWAL),
            ..DispatchServiceConfig::default()
        };
        let concurrency = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        // On the Postgres backend with `AWAKEN_DISPATCH_WAKE=pg-notify` or `=nats`,
        // spawn the pool with the cross-node wake so a peer's enqueue nudges this pool
        // without busy-poll; otherwise the in-process `LocalWakeSignal` suffices.
        let completion = self.completion.clone() as Arc<dyn CompletionSink>;
        let shared_wake = self
            .authority
            .as_ref()
            .and_then(|authority| authority.dispatch_wake());
        let pool = match shared_wake {
            Some(wake) => DispatchPool::spawn_with_wake_and_completion(
                store,
                Arc::new(SystemClock),
                self.deployment.dispatch_owner.clone(),
                DEFAULT_LEASE_MS,
                config,
                resolver,
                concurrency,
                wake,
                completion,
            ),
            None => DispatchPool::spawn_with_completion(
                store,
                Arc::new(SystemClock),
                self.deployment.dispatch_owner.clone(),
                DEFAULT_LEASE_MS,
                config,
                resolver,
                concurrency,
                completion,
            ),
        };
        let _ = self.dispatch_pool.set(Arc::new(pool));
    }

    /// Begin a graceful drain of the dispatch pool (scale-in / SIGTERM): stop
    /// claiming new runs while the in-flight ones finish. A no-op when no pool is
    /// running (a direct/non-durable host, or a worker whose pool never started).
    /// Idempotent. The admin surface calls this on `POST /admin/drain`.
    pub async fn begin_pool_drain(&self) {
        if let Some(pool) = self.dispatch_pool.get() {
            pool.begin_drain().await;
        }
    }

    /// Whether the dispatch pool is up and still claiming work — the worker's
    /// readiness signal. `false` before the pool starts or once it is draining, so a
    /// readiness probe reports 503 in exactly the states where the worker should not
    /// receive (or keep being routed) new work.
    #[must_use]
    pub fn pool_accepting_work(&self) -> bool {
        self.dispatch_pool
            .get()
            .is_some_and(|pool| !pool.is_draining())
    }

    #[must_use]
    pub fn pool_in_flight(&self) -> u32 {
        self.dispatch_pool.get().map_or(0, |pool| pool.in_flight())
    }
}

fn resolve_local_workspace(store_dir: Option<&std::path::Path>) -> String {
    let path = store_dir.map(|dir| dir.join("platform-workspace-id"));
    if let Some(path) = &path
        && let Ok(existing) = std::fs::read_to_string(path)
        && !existing.trim().is_empty()
    {
        return existing.trim().to_string();
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let generated = format!("workspace_local_{:x}_{nonce:x}", std::process::id());
    if let Some(path) = path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create platform workspace directory");
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        std::io::Write::write_all(
            &mut options.open(path).expect("create platform workspace id"),
            generated.as_bytes(),
        )
        .expect("persist platform workspace id");
    }
    generated
}

#[cfg(any(test, feature = "test-support"))]
fn local_memory_extraction_repository(
    storage_dir: Option<&std::path::Path>,
) -> Arc<dyn awaken_ext_memory::MemoryExtractionRepository> {
    match storage_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir)
                .expect("create durable Memory extraction repository directory");
            Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(
                    &dir.join("sessions.db").to_string_lossy(),
                )
                .expect("open durable Memory extraction repository"),
            )
        }
        None => Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("open ephemeral Memory extraction repository"),
        ),
    }
}

#[cfg(test)]
mod composition_tests {
    use super::*;

    #[test]
    fn explicit_extraction_repository_is_the_initial_authority() {
        // Cause/effect graph: C1 an extraction repository is explicit; C2 local
        // storage is configured. Effects: E1 exact injected identity is installed;
        // E2 a local SQLite repository is derived; E3 volatile fixture is derived.
        // Constraint: the production resource/deployment constructor requires C1.
        //
        // | Rule | explicit repo | storage dir | initial extraction authority |
        // | T1   | yes           | any         | exact injected repository    |
        // | T2   | no            | yes         | derived durable SQLite       |
        // | T3   | no            | no          | test-only ephemeral SQLite   |
        //
        // T2/T3 remain covered by existing local restart/fixture suites. T1
        // prevents the former construct-then-replace duplicate authority.
        let expected: Arc<dyn awaken_ext_memory::MemoryExtractionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("test extraction repository"),
        );
        let host = SharedHost::build(
            Arc::new(crate::NoModelConfiguredExecutor),
            crate::UNCONFIGURED_MODEL_REF.into(),
            None,
            None,
            Some(expected.clone()),
            crate::DeploymentConfig::ephemeral(),
        );

        assert!(
            Arc::ptr_eq(&expected, &host.memory.extraction_repository()),
            "T1/E1"
        );
    }
}
