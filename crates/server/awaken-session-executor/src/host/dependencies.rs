// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: The one Host dependency set selects every injected startup authority exactly once before
// @formal reason: construction; it opens no Store. This link is not a formal-proof claim.
//! [`HostDependencies`]: everything a [`SharedHost`] is built from, selected
//! once by the process composition before the Host exists.
//!
//! Acquiring a dependency may be asynchronous (opening a Store, probing a
//! sandbox tier); the composition does that while it fills this value. Turning
//! the value into a Host is the one synchronous constructor
//! (`SharedHost::from`), which performs no I/O. Nothing installs a dependency
//! into a Host after it exists, so no Host is ever observed half-configured.
//!
//! The constructors take every dependency a role cannot run without; the
//! chainable `with_*` methods select the optional ones. Worker-only transports
//! travel inside its [`crate::UpstreamAuthority`].

use crate::host::*;

mod environment;
mod tools;

/// One construction-time edge from the Host back into an application that is
/// itself built over the Host, so the two cannot be constructed in one call.
///
/// The port exists from the moment the dependencies are selected; the
/// composition root binds it exactly once after it built the application, and
/// a read before that finds nothing and fails closed. This is the only kind of
/// late binding a Host has, and its type says so. Consumer:
/// [`SessionApplicationPorts`].
pub struct CyclicPort<T> {
    slot: Arc<std::sync::OnceLock<T>>,
}

/// A [`CyclicPort`] was bound a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("this composition edge is already bound")]
pub struct CyclicPortAlreadyBound;

impl<T> CyclicPort<T> {
    /// Bind the port to the application the composition just built.
    ///
    /// # Errors
    /// [`CyclicPortAlreadyBound`] when the port was already bound; the first
    /// binding stays.
    pub fn bind(&self, value: T) -> Result<(), CyclicPortAlreadyBound> {
        self.slot.set(value).map_err(|_| CyclicPortAlreadyBound)
    }

    /// The bound application, or `None` before the composition bound it.
    pub(crate) fn get(&self) -> Option<&T> {
        self.slot.get()
    }
}

impl<T> Clone for CyclicPort<T> {
    fn clone(&self) -> Self {
        Self {
            slot: self.slot.clone(),
        }
    }
}

impl<T> Default for CyclicPort<T> {
    fn default() -> Self {
        Self {
            slot: Arc::new(std::sync::OnceLock::new()),
        }
    }
}

/// The Host's edges back into the Session application built over it. Each is
/// a weak or notification-only port, so binding one creates no ownership
/// cycle. Consumer: the composition root, which binds each once after it built
/// the Session application (`SharedHost::session_application_ports`).
#[derive(Clone, Default)]
pub struct SessionApplicationPorts {
    /// The Session application's sole coordination admission owner. It
    /// carries no roster, Thread relationship, operation receipt, or lifecycle
    /// state.
    pub agent_coordination:
        CyclicPort<std::sync::Weak<dyn awaken_session_execution_contract::SessionAgentCoordination>>,
    /// The existing Session Run admission owner. BackgroundTask completion
    /// uses it only to publish a deterministic same-Thread attention Run; task
    /// lifecycle and result truth remain in committed Thread State.
    pub background_runs: CyclicPort<
        std::sync::Weak<dyn awaken_session_application_contract::SessionRunBackgroundApplication>,
    >,
    /// The process's sole wakeup for committed Run progress. It is
    /// notification-only; its consumer must reread durable truth.
    pub committed_progress: CyclicPort<Arc<dyn Fn() + Send + Sync>>,
}

/// Every dependency of one [`SharedHost`]. Consumer: the process
/// compositions (`awaken-cli`, `awaken-worker`, `awaken-coordinator`, the
/// Scenario host and test fixtures) and `SharedHost::from`.
pub struct HostDependencies {
    pub(crate) local_workspace: String,
    pub(crate) deployment: crate::DeploymentConfig,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) store_dir: Option<PathBuf>,
    pub(crate) authority: Arc<dyn crate::SessionAuthority>,
    pub(crate) execution_authority:
        Option<Arc<dyn awaken_provisioning_contract::CurrentExecutionAuthority>>,
    pub(crate) environment_binding_sink:
        Option<Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>>,
    /// The Thread event hub the Host publishes through; ACP launch observers
    /// selected with these dependencies report into the same hub.
    pub(crate) hub: Arc<ThreadEventHub>,
    pub(crate) agent: capability::AgentSelection,
    pub(crate) exec: capability::AttemptBackends,
    pub(crate) worker: capability::WorkerTransports,
    pub(crate) env: capability::EnvironmentProviders,
    pub(crate) tools: capability::ToolOffering,
    pub(crate) resources: capability::ResourceSelection,
    pub(crate) mcp: awaken_session_mcp::McpSessions,
    pub(crate) dispatch: capability::DispatchSelection,
    pub(crate) capture: capability::ContentCaptureCapability,
    pub(crate) session_application: SessionApplicationPorts,
}

/// Select the role-neutral defaults around one role's required inputs.
fn select(
    llm: Arc<dyn LlmExecutor>,
    model_ref: String,
    resources: capability::ResourceInputs,
    process: HostProcess,
    authority: Arc<dyn crate::SessionAuthority>,
) -> HostDependencies {
    let HostProcess {
        local_workspace,
        deployment,
        clock,
    } = process;
    // Process startup parses the deployment axes once from the environment
    // into one typed config. `DeploymentConfig::storage_dir` set → durable SQLite commit
    // and resource adapters (all survive a restart); unset → ephemeral adapters.
    let store_dir = deployment.storage_dir.clone();
    let sandbox_root = crate::acp_backend::session_sandbox_base(&deployment);
    let skills = crate::skill_catalog::SkillCatalog::new();
    #[cfg(any(test, feature = "test-support"))]
    let mut skills = skills;
    #[cfg(any(test, feature = "test-support"))]
    if let Some(skill_ports) = resources
        .local
        .as_ref()
        .and_then(|plane| plane.skills.clone())
    {
        skills.set_store(
            skill_ports.store,
            skill_ports.bundle_source,
            skill_ports.application,
        );
    }
    let resources =
        capability::ResourceSelection::assemble(resources, store_dir.as_deref(), &clock);
    HostDependencies {
        agent: capability::AgentSelection::new(llm, model_ref),
        exec: capability::AttemptBackends::new(&deployment),
        worker: capability::WorkerTransports::default(),
        env: capability::EnvironmentProviders::new(
            sandbox_root,
            crate::auxiliary_sandbox::default_auxiliary_sandboxes(),
        ),
        tools: capability::ToolOffering::new(skills),
        resources,
        mcp: awaken_session_mcp::McpSessions::new(
            deployment.sandbox.container_mcp_relay.clone(),
            clock.clone(),
        ),
        dispatch: capability::DispatchSelection::default(),
        capture: capability::ContentCaptureCapability::new(&deployment),
        session_application: SessionApplicationPorts::default(),
        local_workspace,
        execution_authority: None,
        environment_binding_sink: None,
        hub: Arc::new(ThreadEventHub::new()),
        store_dir,
        deployment,
        authority,
        clock,
    }
}

impl HostDependencies {
    /// An execution Worker with its exact remote content adapters installed
    /// before any Resource data store is selected. Management-only File and
    /// durable extraction authorities fail closed; the Worker opens no File,
    /// Memory-content, or Memory-extraction authority database. `authority`
    /// carries the claim-fenced upstream transports through which this Worker
    /// reaches committed truth; no other dependency can grant it a local write
    /// authority.
    pub fn for_worker(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        file_content_source: Arc<
            dyn crate::FileContentSource<awaken_dispatch_contract::ResourceOperationFence>,
        >,
        memory_repository: Arc<dyn awaken_resource_contract::MemoryRepository>,
        process: HostProcess,
        authority: crate::UpstreamAuthority,
    ) -> Self {
        select(
            llm,
            model_ref.into(),
            capability::ResourceInputs {
                #[cfg(any(test, feature = "test-support"))]
                local: None,
                worker_content: Some(capability::WorkerContentAdapters {
                    file_content_source,
                    memory_repository,
                }),
                extraction_repository: None,
            },
            process,
            Arc::new(authority),
        )
    }

    /// A Coordinator-local Runtime Host over only its execution data
    /// capability. Resources catalog, File commands, Skill authoring, and
    /// reclamation remain outside and are injected through narrow applications.
    /// A store-owning composition selects its durable Runtime authority with
    /// [`Self::with_runtime_authority`].
    pub fn for_coordinator(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        file_content_source: Arc<
            dyn crate::FileContentSource<awaken_dispatch_contract::ResourceOperationFence>,
        >,
        memory_repository: Arc<dyn awaken_resource_contract::MemoryRepository>,
        extraction_repository: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
        process: HostProcess,
    ) -> Self {
        select(
            llm,
            model_ref.into(),
            capability::ResourceInputs {
                #[cfg(any(test, feature = "test-support"))]
                local: None,
                worker_content: Some(capability::WorkerContentAdapters {
                    file_content_source,
                    memory_repository,
                }),
                extraction_repository: Some(extraction_repository),
            },
            process,
            Arc::new(crate::LocalAuthority::default()),
        )
    }

    /// A test or Scenario composition over local Resource authorities, its
    /// resolved platform Workspace, and its Deployment. The process clock is
    /// the Resources clock, so Resource records and Host Workers share one
    /// timeline.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_local_resources(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        resources: capability::LocalResourcePorts,
        extraction_repository: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
        local_workspace: impl Into<String>,
        deployment: crate::DeploymentConfig,
    ) -> Self {
        let clock = resources.clock.clone();
        select(
            llm,
            model_ref.into(),
            capability::ResourceInputs {
                local: Some(resources),
                worker_content: None,
                extraction_repository: Some(extraction_repository),
            },
            HostProcess {
                local_workspace: local_workspace.into(),
                deployment,
                clock,
            },
            Arc::new(crate::LocalAuthority::default()),
        )
    }

    /// Volatile dependencies over `llm` for this crate's own unit tests: an
    /// ephemeral Deployment, a fresh platform Workspace, the system clock and
    /// the volatile Runtime authority. Other compositions use
    /// `awaken-runtime-host-testkit`, which owns the concrete volatile
    /// Resource adapters this crate does not link.
    #[cfg(test)]
    pub async fn ephemeral(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Self {
        Self::ephemeral_with_deployment(
            llm,
            model_ref,
            crate::deployment_config::DeploymentConfig::ephemeral(),
        )
        .await
    }

    /// [`Self::ephemeral`] over `deployment`. The platform Workspace is freshly
    /// minted even when `deployment` names a storage root; a fixture that needs
    /// a durable coordinate selects it with [`Self::with_local_workspace`].
    #[cfg(test)]
    pub async fn ephemeral_with_deployment(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        deployment: crate::DeploymentConfig,
    ) -> Self {
        // A test fixture is its own composition: it chooses the host clock and
        // selects the volatile Runtime authority here.
        let clock = awaken_time::system_clock();
        let extraction_repository =
            capability::local_memory_extraction_repository(deployment.storage_dir.as_deref()).await;
        select(
            llm,
            model_ref.into(),
            capability::ResourceInputs {
                #[cfg(any(test, feature = "test-support"))]
                local: None,
                worker_content: None,
                extraction_repository: Some(extraction_repository),
            },
            HostProcess {
                local_workspace: SharedHost::mint_local_workspace(),
                deployment,
                clock: clock.clone(),
            },
            Arc::new(crate::LocalAuthority::default()),
        )
        .with_runtime_authority(Arc::new(crate::EphemeralRuntimeAuthority::new(clock)))
    }

    /// The typed Deployment these dependencies serve. Compositions read it
    /// (they never re-read process configuration) to select the environment
    /// tier they install.
    #[must_use]
    pub fn deployment_config(&self) -> &crate::DeploymentConfig {
        &self.deployment
    }

    /// The process wall clock the Host will drive its Workers on.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn awaken_time::WallClock> {
        self.clock.clone()
    }

    /// The platform-managed local workspace coordinate.
    #[must_use]
    pub fn local_workspace(&self) -> &str {
        &self.local_workspace
    }

    /// The Memory content data-plane port, for a composition that builds a
    /// worker-side mounter over it. It carries no principal or policy state.
    #[must_use]
    pub fn memory_repository(&self) -> Arc<dyn awaken_resource_contract::MemoryRepository> {
        self.resources.memory_stores.fs_handle()
    }

    /// Test-support replacement of a volatile fixture's deployment value.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_deployment_config(mut self, deployment: crate::DeploymentConfig) -> Self {
        self.mcp
            .set_relay_exposure(deployment.sandbox.container_mcp_relay.clone());
        self.deployment = deployment;
        self
    }

    /// Select the Coordinator-owned durable capability: the Host then owns
    /// committed truth locally. Only store-owning compositions select it; a
    /// Worker is constructed with its upstream authority instead.
    #[must_use]
    pub fn with_runtime_authority(mut self, authority: Arc<dyn crate::RuntimeAuthority>) -> Self {
        self.authority = Arc::new(crate::LocalAuthority::new(authority));
        self
    }

    /// Give a unit-test fixture the authority of the role under test. Product
    /// Worker composition always uses [`Self::for_worker`].
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_authority(
        mut self,
        authority: impl crate::SessionAuthority + 'static,
    ) -> Self {
        self.authority = Arc::new(authority);
        self
    }

    /// Select the current registration's proof before dispatch. An
    /// unregistered direct Host keeps its existing Session authority owner;
    /// absence is not evidence for enabling physical process-tree supervision.
    #[must_use]
    pub fn with_execution_authority(
        mut self,
        authority: Arc<dyn awaken_provisioning_contract::CurrentExecutionAuthority>,
    ) -> Self {
        self.execution_authority = Some(authority);
        self
    }

    /// Select the same authenticated Control client as the pre-effect Session
    /// Environment root authority on a database-less Worker.
    #[must_use]
    pub fn with_environment_binding_sink(
        mut self,
        sink: Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) -> Self {
        self.environment_binding_sink = Some(sink);
        self
    }

    /// Platform-managed local workspace used when no authenticated/path scope
    /// exists. Process startup may override it with a provisioned coordinate.
    #[must_use]
    pub fn with_local_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.local_workspace = workspace.into();
        self
    }

    /// Test-support upgrade of volatile dependencies to local durable storage
    /// under `dir`, with the durable Memory extraction repository the caller
    /// opened there and the relocation of the Session environment providers
    /// selected so far under that root (`awaken-runtime-host-testkit` supplies
    /// both for the sandbox-backed providers it links; this crate never does).
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_durable_store_dir(
        mut self,
        dir: impl Into<PathBuf>,
        extraction_repository: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
        relocate: impl Fn(
            &awaken_session_environment_contract::SessionEnvironmentProvider,
            PathBuf,
        ) -> awaken_session_environment_contract::SessionEnvironmentProvider,
    ) -> Self {
        let dir = dir.into();
        self.resources.extraction_repository = extraction_repository;
        // A durable host must place its Workdir sandboxes under a stable root too;
        // otherwise the dispatch row survives restart but its persisted handle
        // points into the previous process's random temp directory.
        self.env.session_provider = relocate(&self.env.session_provider, dir.join("sandboxes"));
        self.env.backend_owned_session_provider = self
            .env
            .backend_owned_session_provider
            .as_ref()
            .map(|provider| relocate(provider, dir.join("trusted-local-sandboxes")));
        if let Some(mounter) = self.env.memory_mounter.clone() {
            self.env
                .session_provider
                .installation()
                .install_memory_mounter(mounter.clone());
            if let Some(provider) = &self.env.backend_owned_session_provider {
                provider.installation().install_memory_mounter(mounter);
            }
        }
        self.store_dir = Some(dir);
        self
    }

    /// [`Self::with_durable_store_dir`] with the local SQLite extraction
    /// repository at `dir/sessions.db` and the sandbox-backed relocation, for
    /// this crate's own unit tests.
    #[cfg(test)]
    pub async fn with_store_dir(self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let repository = capability::local_memory_extraction_repository(Some(&dir)).await;
        self.with_durable_store_dir(dir, repository, |provider, base| {
            crate::session_environment::relocated_provider(provider, base)
        })
    }

    /// Replace the Memory extraction authority of a unit-test fixture.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_memory_extraction_repository(
        mut self,
        repository: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
    ) -> Self {
        self.resources.extraction_repository = repository;
        self
    }

    /// Select the adapter that drives snapshot-selected remote attempts. The
    /// host depends only on the neutral attempt port; A2A construction remains a
    /// process-startup responsibility.
    #[must_use]
    pub fn with_remote_attempt_executor(mut self, installation: RemoteAttemptInstallation) -> Self {
        self.agent.remote_attempt_executor = Some(installation.executor);
        self.agent.remote_credential_realization = installation.credential_realization;
        self
    }

    /// Supply immutable executable publications without mounting the authoring
    /// plane. Intended for embedded process startups and scenario fixtures.
    #[must_use]
    pub fn with_agent_publications(
        mut self,
        source: Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
    ) -> Self {
        self.agent.agent_publications = Some(source);
        self
    }

    /// Override the default tool-free Outcome Judge with the named Agent. The
    /// Judge runs through the same Run boundary in its own fresh context.
    /// Refused when the configured `model_ref` is not an executable binding,
    /// since the Judge snapshot pins it.
    pub fn with_judge(
        mut self,
        judge_agent_id: impl Into<String>,
    ) -> Result<Self, awaken_runtime_contract::resolved::InvalidModelBinding> {
        let id = judge_agent_id.into();
        let snapshot = default_judge_agent(&self.agent.model_ref, &id, DEFAULT_JUDGE_INSTRUCTIONS)?;
        self.agent.judge_snapshot = Some(snapshot);
        Ok(self)
    }

    /// Pin an arbitrary executable Agent snapshot as the Outcome Grader. Its
    /// backend may be Native or ACP; both execute through the same Run boundary.
    #[must_use]
    pub fn with_judge_snapshot(mut self, snapshot: ExecutableAgentSnapshot) -> Self {
        self.agent.judge_snapshot = Some(snapshot);
        self
    }

    /// Select runtime-only inference materialization for per-thread routing
    /// and Memory extraction. Remote workers use this without a configuration
    /// resolver because admission already pinned access.
    #[must_use]
    pub fn with_inference_materializer(
        mut self,
        materializer: Arc<dyn awaken_credential_materializer::InferenceExecutorMaterializer>,
    ) -> Self {
        self.agent.inference_materializer = Some(materializer);
        self
    }

    /// Wire the subject-tagged captured-content sink (ADR-0050).
    #[must_use]
    pub fn with_capture_sink(
        mut self,
        sink: Arc<dyn awaken_runtime_contract::CaptureSink>,
    ) -> Self {
        self.capture.capture_sink = Some(sink);
        self
    }

    /// Select the Control-owned consent read port used once per attributed Run.
    #[must_use]
    pub fn with_data_subject_consent_source(
        mut self,
        source: Arc<dyn awaken_data_governance_contract::DataSubjectConsentSource>,
    ) -> Self {
        self.capture.data_subject_consent = source;
        self
    }

    /// Select the exact durable dispatch transport owned by this host startup.
    ///
    /// An explicitly assembled queue is already the authority for ingress, so it
    /// also enables this host's dispatch pool. Embedders do not need to mutate
    /// another process-global configuration source to activate a transport they
    /// supplied directly.
    #[must_use]
    pub fn with_dispatch_store(
        mut self,
        store: Arc<awaken_dispatch_erasure::AnyDispatchStore>,
    ) -> Self {
        self.dispatch.dispatch_store_override = Some(store);
        self.deployment.durable = true;
        self.deployment.disable_local_pool = false;
        self
    }

    /// Select the exact durable dispatch transport for a Coordinator-only Host.
    ///
    /// Unlike [`Self::with_dispatch_store`], this topology owns admission and
    /// completion observation but never drains the queue or realizes a Session
    /// Environment. A separately registered Worker is the sole physical effect
    /// owner. Keeping this as one selection prevents call ordering from silently
    /// re-enabling the local pool after a startup selected remote placement.
    #[must_use]
    pub fn with_coordinator_dispatch_store(
        mut self,
        store: Arc<awaken_dispatch_erasure::AnyDispatchStore>,
    ) -> Self {
        self.dispatch.dispatch_store_override = Some(store);
        self.deployment.durable = true;
        self.deployment.disable_local_pool = true;
        self
    }

    /// Select the dispatch service together with the capacity its Worker
    /// registered. CPU quota governs CPU time, not the number of concurrent
    /// I/O-bound Session preparations, so the registered manifest — never a
    /// local guess — is the Worker capacity authority.
    ///
    /// The capacity is taken by value: a Worker that reads
    /// [`RegisteredCapacity::of`] and does not reach this selection gets an
    /// `unused_must_use` diagnostic instead of a pool that quietly disagrees
    /// with the registry.
    ///
    /// [`RegisteredCapacity::of`]: awaken_dispatch_contract::RegisteredCapacity::of
    #[must_use]
    pub fn with_worker_dispatch(
        mut self,
        dispatch: Arc<dyn awaken_dispatch_contract::Dispatch>,
        capacity: awaken_dispatch_contract::RegisteredCapacity,
    ) -> Self {
        self.dispatch.dispatch_pool_sizing =
            crate::DispatchPoolSizing::Registered(capacity.into_slots());
        self.with_dispatch_store(Arc::new(
            awaken_dispatch_erasure::AnyDispatchStore::from_dispatch(dispatch),
        ))
    }

    /// Select only the read-only ACP catalog port on a Coordinator that
    /// delegates physical execution to a registered Worker.
    #[must_use]
    pub fn with_acp_projection_policy(
        mut self,
        policy: &'static dyn awaken_acp_contract::AcpAdapterPolicy<
            awaken_runtime_contract::resolved::ProviderServerTool,
        >,
    ) -> Self {
        assert!(
            self.exec.acp.is_none(),
            "ACP projection policy cannot shadow an executor"
        );
        self.exec.acp_projection_policy = Some(policy);
        self
    }

    /// Select the one process-composed ACP execution factory. The frozen
    /// Session projection, not this selection, selects an ACP backend.
    #[must_use]
    pub fn with_acp(
        mut self,
        factory: Arc<dyn awaken_session_environment_contract::AcpSessionExecutorFactory>,
    ) -> Self {
        self.exec.acp_projection_policy = None;
        self.exec.acp = Some(factory);
        self
    }

    /// The launch observer an ACP factory selected with these dependencies
    /// reports through: it publishes into the Host's Thread event hub.
    #[must_use]
    pub fn acp_launch_observer(&self) -> Arc<dyn awaken_acp_contract::LaunchObserver> {
        Arc::new(crate::acp_backend::HubLaunchObserver::new(self.hub.clone()))
    }

    #[must_use]
    pub fn with_acp_tool_exporter(
        mut self,
        exporter: Arc<dyn awaken_session_environment_contract::tool_export::AcpToolExporter>,
    ) -> Self {
        self.exec.acp_tool_exporter = Some(exporter);
        self
    }

    /// Select the canonical exact credential materializer used by runtime
    /// extensions such as paid WebSearch. The value is cloned from the same
    /// process-startup instance used by Managed MCP/Resource realization.
    #[must_use]
    pub fn with_credential_materializer(
        mut self,
        materializer: crate::PinnedCredentialMaterializer,
    ) -> Self {
        self.exec.credential_materializer = Some(materializer);
        self
    }

    /// Wrap the complete per-Session Native/ACP/A2A attempt boundary.
    ///
    /// The Host always constructs the authoritative backend router first. The
    /// application can adapt envelopes, capabilities, and business results
    /// around it, but cannot replace backend selection.
    #[must_use]
    pub fn with_attempt_decorator(mut self, decorator: AttemptExecutorDecorator) -> Self {
        self.exec.attempt_decorator = Some(decorator);
        self
    }

    /// Select the sole outbound claim-fenced client for frozen Session
    /// realization on a Worker.
    #[must_use]
    pub fn with_session_realization_control(
        mut self,
        control: Arc<dyn awaken_dispatch_contract::ClaimedSessionRealizationControl>,
    ) -> Self {
        self.worker.session_control = Some(control);
        self
    }

    /// Select both independent claim-fenced Session roles from the canonical
    /// Worker Control client.
    #[must_use]
    pub fn with_session_control<T>(mut self, control: Arc<T>) -> Self
    where
        T: awaken_dispatch_contract::ClaimedSessionRealizationControl
            + awaken_dispatch_contract::ClaimedSessionCoordinationControl
            + 'static,
    {
        self.worker.session_control = Some(control.clone());
        self.worker.session_coordination_control = Some(control);
        self
    }

    /// Select the Worker process's one opaque local-credential adapter.
    #[must_use]
    pub fn with_worker_credential_resolver(
        mut self,
        resolver: Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>,
    ) -> Self {
        self.worker.worker_credential_resolver = Some(resolver);
        self
    }

    /// Select the relay's outbound proxy for Session MCP bearers.
    #[must_use]
    pub fn with_mcp_relay_outbound_proxy(
        mut self,
        issuer: Arc<dyn awaken_session_mcp::McpRelayOutboundProxy>,
    ) -> Self {
        self.mcp.set_outbound_proxy(issuer);
        self
    }

    /// Select the process reaper that stops Session MCP server processes.
    #[must_use]
    pub fn with_mcp_process_reaper(
        mut self,
        reaper: Arc<dyn awaken_session_environment_contract::ProcessReaper>,
    ) -> Self {
        self.mcp.set_process_reaper(reaper);
        self
    }

    /// Select the one Resources-owned File command application. Runtime stores
    /// only this inward port and cannot construct a parallel implementation.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_file_application(
        mut self,
        application: Arc<dyn awaken_resource_contract::FileApplicationService>,
        content_source: Arc<
            dyn crate::FileContentSource<awaken_dispatch_contract::ResourceOperationFence>,
        >,
        artifact_publisher: Arc<
            dyn awaken_resource_contract::ArtifactPublisher<
                    awaken_dispatch_contract::ArtifactPublicationFence,
                >,
        >,
    ) -> Self {
        self.resources.file_content_source = content_source;
        self.resources.artifact_publisher = artifact_publisher;
        self.resources.file_application = Some(application);
        self
    }

    /// Select the narrow artifact publisher used by a database-less Worker.
    /// This does not grant File management or catalog access.
    #[must_use]
    pub fn with_artifact_publisher(
        mut self,
        publisher: Arc<
            dyn awaken_resource_contract::ArtifactPublisher<
                    awaken_dispatch_contract::ArtifactPublicationFence,
                >,
        >,
    ) -> Self {
        // This is the database-less Worker startup edge. Retaining the
        // test-only/local File application here would create a second command
        // path and incorrectly classify the remote publisher as locally fenced.
        #[cfg(any(test, feature = "test-support"))]
        {
            self.resources.file_application = None;
        }
        self.resources.artifact_publisher = publisher;
        self
    }

    /// Test-support replacement of a volatile fixture's File content port.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_file_content_source(
        mut self,
        source: Arc<
            dyn crate::FileContentSource<awaken_dispatch_contract::ResourceOperationFence>,
        >,
    ) -> Self {
        self.resources.file_content_source = source;
        self
    }
}
