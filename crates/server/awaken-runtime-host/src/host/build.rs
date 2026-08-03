//! Builder / configuration wiring for [`SharedHost`]: `new`, the chainable
//! `with_*` methods, per-thread registration, and the process dispatch pool.

use super::*;
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

    /// A host over `llm`. Configure it with the chainable `with_*` builders
    /// (client tools, delegates, a judge grader, a durable store).
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

    /// Construct without resource overrides from an explicitly resolved
    /// deployment snapshot.
    pub fn new_with_deployment(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        deployment: crate::DeploymentConfig,
    ) -> Self {
        Self::build(llm, model_ref.into(), None, None, None, deployment)
    }

    /// Replace the environment-derived deployment value with the exact typed
    /// value owned by an embedding composition root.
    #[must_use]
    pub fn with_deployment_config(mut self, deployment: crate::DeploymentConfig) -> Self {
        self.session_blob_root = deployment.acp_session_blob_root.clone();
        self.deployment = deployment;
        self
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

    pub(crate) fn dispatch_store(
        &self,
    ) -> Result<Arc<awaken_run_ingress::AnyDispatchStore>, HostError> {
        self.dispatch_store_override.clone().map_or_else(
            || {
                crate::dispatch_backend::shared_durable_store_for(
                    &self.deployment,
                    self.store_dir.as_deref(),
                )
            },
            Ok,
        )
    }

    /// Construct with an already selected resource persistence family. Unlike
    /// post-construction overrides, this never opens node-local resource stores
    /// before installing shared adapters, so there is no unused second truth.
    pub fn new_with_resource_component(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        resources: awaken_resource_contract::ResourceComponent,
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
        resources: awaken_resource_contract::ResourceComponent,
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
        file_content_source: Arc<dyn crate::FileContentSource>,
        memory_repository: Arc<dyn awaken_memory_store::MemoryRepository>,
        deployment: crate::DeploymentConfig,
    ) -> Self {
        Self::build(
            llm,
            model_ref.into(),
            None,
            Some((file_content_source, memory_repository)),
            None,
            deployment,
        )
    }

    fn build(
        llm: Arc<dyn LlmExecutor>,
        model_ref: String,
        resources: Option<awaken_resource_contract::ResourceComponent>,
        worker_content: Option<(
            Arc<dyn crate::FileContentSource>,
            Arc<dyn awaken_memory_store::MemoryRepository>,
        )>,
        extraction_repository: Option<Arc<dyn awaken_ext_memory::MemoryExtractionRepository>>,
        deployment: crate::DeploymentConfig,
    ) -> Self {
        // Composition root: the deployment axes are parsed once from the environment
        // into one typed config. `DeploymentConfig::storage_dir` set → durable SQLite commit
        // and resource adapters (all survive a restart); unset → ephemeral adapters.
        let store_dir = deployment.storage_dir.clone();
        let local_workspace = resolve_local_workspace(store_dir.as_deref());
        let sandbox_root = store_dir
            .as_ref()
            .map(|dir| dir.join("sandboxes"))
            .unwrap_or_else(|| sub_base(""));
        // The ADR-0038/0053 memory content stores follow one storage-dir durability
        // rule, owned by `MemoryStores::open` (durable under the dir; ephemeral
        // per-process otherwise). Resource identity/configuration is injected into
        // the server composition root through `ResourceCatalog`.
        let memory_stores = if let Some((_, repository)) = &worker_content {
            crate::memory_stores::MemoryStores::with_repository(repository.clone())
        } else {
            resources.as_ref().map_or_else(
                || crate::memory_stores::MemoryStores::open(store_dir.as_deref()),
                |plane| {
                    crate::memory_stores::MemoryStores::with_repository(plane.memory_repository())
                },
            )
        };
        let extraction_repository = extraction_repository.unwrap_or_else(|| {
            if worker_content.is_some() {
                Arc::new(crate::unavailable_worker::UnavailableWorkerExtractions)
                    as Arc<dyn awaken_ext_memory::MemoryExtractionRepository>
            } else {
                local_memory_extraction_repository(store_dir.as_deref())
            }
        });
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
                files.clone() as Arc<dyn awaken_file_store::FileStore>,
                files as Arc<dyn awaken_resource_contract::FileCatalog>,
            )
        } else {
            resources.as_ref().map_or_else(
                || match store_dir.as_ref() {
                    Some(dir) => {
                        let files = Arc::new(
                            awaken_file_store::sqlite::SqliteFileStore::open(
                                &dir.join("files.db").to_string_lossy(),
                            )
                            .expect("open durable file store"),
                        );
                        (
                            files.clone() as Arc<dyn awaken_file_store::FileStore>,
                            files as Arc<dyn awaken_resource_contract::FileCatalog>,
                        )
                    }
                    None => {
                        let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
                        (
                            files.clone() as Arc<dyn awaken_file_store::FileStore>,
                            files as Arc<dyn awaken_resource_contract::FileCatalog>,
                        )
                    }
                },
                |plane| (plane.file_store(), plane.file_catalog()),
            )
        };
        let resource_lifecycle = resources.as_ref().map(|plane| plane.lifecycle());
        #[cfg(test)]
        let resource_lifecycle =
            resource_lifecycle.or_else(|| Some(super::tests::test_resource_lifecycle()));
        #[cfg(not(test))]
        let file_application = None;
        #[cfg(test)]
        let file_application = resource_lifecycle.as_ref().map(|lifecycle| {
            Arc::new(awaken_file_application::FileApplication::new(
                file_store.clone(),
                file_catalog.clone(),
                lifecycle.clone(),
            )) as Arc<dyn awaken_resource_contract::FileApplicationService>
        });
        let session_slots = crate::session_slot::SessionRuntimeSlots::default();
        let file_content_source: Arc<dyn crate::FileContentSource> = worker_content
            .as_ref()
            .map(|(source, _)| source.clone())
            .unwrap_or_else(|| {
                Arc::new(crate::StoreFileContentSource::new(
                    file_catalog.clone(),
                    file_store.clone(),
                ))
            });
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
            resource_lifecycle,
            memory_stores,
            memory_mounter: std::sync::RwLock::new(None),
            gate_override: None,
            dispatch_pool: std::sync::OnceLock::new(),
            dispatch_store_override: None,
            completion: Arc::new(CompletionRegistry::default()),
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
        provisioner: Arc<dyn crate::ApplicationSessionProvisioner>,
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
        control: Arc<dyn crate::ApplicationSessionControlClient>,
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

    /// Install the resource-lifecycle adapter selected by the composition root.
    /// The runtime sees only the resource contract and remains independent of IAM
    /// and storage technology; without this port, managed resource operations fail
    /// closed instead of creating a second Host-local resource truth.
    #[must_use]
    pub fn with_resource_lifecycle(
        mut self,
        repository: Arc<dyn awaken_resource_contract::ResourceLifecycleRepository>,
    ) -> Self {
        #[cfg(test)]
        {
            self.file_application = Some(Arc::new(awaken_file_application::FileApplication::new(
                self.file_store.clone(),
                self.file_catalog.clone(),
                repository.clone(),
            )));
        }
        self.resource_lifecycle = Some(repository);
        self
    }

    /// Install the one Resources-owned File command application. Runtime stores
    /// only this inward port and cannot construct a parallel implementation.
    #[must_use]
    pub fn with_file_application(
        mut self,
        application: Arc<dyn awaken_resource_contract::FileApplicationService>,
    ) -> Self {
        self.file_application = Some(application);
        self
    }

    pub fn resource_lifecycle(
        &self,
    ) -> Option<Arc<dyn awaken_resource_contract::ResourceLifecycleRepository>> {
        self.resource_lifecycle.clone()
    }

    pub(crate) fn register_thread_workspace(&self, thread: &str, workspace: &str) {
        self.session_slots
            .update(thread, |slot| slot.workspace = Some(workspace.to_string()));
    }

    pub(crate) fn register_thread_agent_projection(&self, thread: &str, agent_id: &str) {
        self.session_slots
            .update(thread, |slot| slot.agent_id = Some(agent_id.to_string()));
    }

    pub(crate) fn thread_agent_projection(&self, thread: &str) -> Option<String> {
        self.session_slots
            .read(thread, |slot| slot.agent_id.clone())
            .flatten()
    }

    pub(crate) fn thread_workspace(&self, thread: &str) -> String {
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

    /// Make this host a database-less **Worker** of the Coordinator at `url`.
    /// Attempts use the registered HTTP dispatch transport and a claim-fenced
    /// operation coordinator; the Worker holds no authoritative store.
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

    /// Back the delivered skill catalog with a durable [`awaken_skill_store::SkillStore`]
    /// rooted at `dir`. Its `SKILL.md`s are offered on every thread alongside any
    /// static [`with_skills`](Self::with_skills) set and survive a restart, so a skill
    /// added through `/v1/skills` is still offered by a later process over the same
    /// dir. The extension never learns of the store — the host scans it into the
    /// `SkillSource` port as plain file data.
    pub fn with_skill_store(mut self, dir: impl Into<PathBuf>) -> Self {
        let store = awaken_skill_store::FsSkillStore::open(dir.into())
            .expect("open durable skill store root");
        self.skills.set_store(Arc::new(store));
        self
    }

    /// Back the delivered skill catalog with an arbitrary [`SkillStore`] backend
    /// (e.g. `PgSkillStore` for a multi-node deployment). Sibling of
    /// [`with_skill_store`](Self::with_skill_store), which wires the filesystem one.
    pub fn with_skill_store_backend(
        mut self,
        store: Arc<dyn awaken_skill_store::SkillStore>,
    ) -> Self {
        self.skills.set_store(store);
        self
    }

    /// Install only the exact immutable custom-Skill read port used by an
    /// execution Worker. This does not grant authoring or catalog access.
    pub fn with_skill_bundle_source(mut self, source: Arc<dyn crate::SkillBundleSource>) -> Self {
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

    /// Persist every thread's committed truth to a durable SQLite database under
    /// `dir` (one file per thread). A run awaiting on a thread survives a restart:
    /// a host rebuilt over the same directory recovers the awaiting position and can
    /// resume it. Without this, sessions are in-memory and lost on restart.
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
        materializer: Arc<dyn crate::inference_routing::InferenceExecutorMaterializer>,
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

    /// Replace the embedded File adapter with the exact per-kind boundary selected
    /// by the composition root. This does not change Files API authoring ownership.
    #[must_use]
    pub fn with_file_content_source(mut self, source: Arc<dyn crate::FileContentSource>) -> Self {
        self.file_content_source = source;
        self
    }

    /// Replace only the Memory content data-plane port. Distributed Workers use
    /// the claim-fenced HTTP repository through
    /// [`new_worker_with_deployment`](Self::new_worker_with_deployment), which
    /// installs it before store selection and therefore opens no Resource DB.
    #[must_use]
    pub fn with_memory_repository(
        mut self,
        repository: Arc<dyn awaken_memory_store::MemoryRepository>,
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
        mut self,
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        hand_factory: Arc<dyn crate::HandExecutorFactory>,
    ) -> Self {
        let hand_bin = self.deployment.sandbox.container_hand_bin.clone();
        self.session_provider = crate::session_environment::SessionEnvironmentProvider::container(
            provider,
            Vec::new(),
            hand_factory,
            hand_bin,
        );
        self.session_provider_explicit = true;
        if let Some(mounter) = self.memory_mounter() {
            self.session_provider.install_memory_mounter(mounter);
        }
        self
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
            // `init_shared_postgres_dispatch` will spawn the pool.
            return;
        };
        let resolver: Arc<dyn WorkerResolver<AnyDispatchStore>> = Arc::new(HostWorkerResolver {
            host: Arc::downgrade(self),
        });
        let config = DispatchServiceConfig {
            lease_renewal_interval: Some(crate::dispatch_backend::LEASE_RENEWAL),
            ..DispatchServiceConfig::default()
        };
        let concurrency = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        // On the Postgres backend with `AWAKEN_DISPATCH_WAKE=pg-notify` or `=nats`,
        // spawn the pool with the cross-node wake so a peer's enqueue nudges this pool
        // without busy-poll; otherwise the in-process `LocalWakeSignal` suffices.
        let completion = self.completion.clone() as Arc<dyn CompletionSink>;
        let pool = match crate::dispatch_backend::shared_dispatch_wake() {
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
