// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: The one Host dependency set selects every injected startup authority exactly once before
// @formal reason: construction; it opens no Store. This link is not a formal-proof claim.
//! [`HostDependencies`]: everything a [`SharedHost`] is built from, selected
//! once by the process composition before the Host exists.
//!
//! Acquiring a dependency may be asynchronous (opening a Store, probing a
//! sandbox tier); the composition does that while it fills this value. Turning
//! the value into a Host is the one synchronous constructor
//! (`HostDependencies::build`), which performs no I/O. Nothing installs a dependency
//! into a Host after it exists, so no Host is ever observed half-configured.
//!
//! The constructors take every dependency a role cannot run without; the
//! chainable `with_*` methods select the optional ones. Worker-only transports
//! travel inside its [`crate::UpstreamAuthority`].

use crate::host::*;

mod environment;
mod tools;

mod session_application;

pub(crate) use session_application::ApplicationPortBinding;
pub use session_application::{SessionApplicationBindingError, SessionApplicationBindings};

/// Immutable Session execution ports selected once at process startup.
/// Managed and claimed dispatch adapters both read this exact Host-owned value.
#[derive(Default)]
pub struct SessionExecutionDependencies {
    pub(crate) credential_refresh_factory: Option<Arc<dyn crate::CredentialRefreshFactory>>,
    pub(crate) platform_mcp_egress:
        Option<Arc<dyn awaken_credential_materializer::PlatformMcpEgressProvisioner>>,
    pub(crate) resource_validator:
        Option<Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>>,
    pub(crate) repository_binding_verifier: Option<
        Arc<
            dyn awaken_resource_contract::RepositoryBindingVerifier<
                    awaken_dispatch_contract::ResourceOperationFence,
                >,
        >,
    >,
    pub(crate) mcp_realizer:
        Option<Arc<dyn awaken_session_execution_contract::McpAttachmentRealizer>>,
}

/// Every dependency of one [`SharedHost`]. Consumer: the process
/// compositions (`awaken-cli`, `awaken-worker`, `awaken-coordinator`, the
/// Scenario host and test fixtures) and `HostDependencies::build`.
pub struct HostDependencies {
    pub(crate) local_workspace: String,
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
    /// This process's part in Run execution; see [`crate::ExecutionTopology`].
    pub(crate) execution: crate::ExecutionTopology,
    pub(crate) capture: capability::ContentCaptureCapability,
    pub(crate) session_application: SessionApplicationBindings,
    pub(crate) session_execution: SessionExecutionDependencies,
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
        execution,
        content_capture,
        storage_dir: store_dir,
        sandbox_root,
        trusted_host,
        warm_pool,
        mcp_relay,
        acp_profile,
        clock,
    } = process;
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
        exec: capability::AttemptBackends::new(acp_profile),
        worker: capability::WorkerTransports::default(),
        env: capability::EnvironmentProviders::new(
            sandbox_root,
            crate::auxiliary_sandbox::default_auxiliary_sandboxes(),
            trusted_host,
            warm_pool,
        ),
        tools: capability::ToolOffering::new(skills),
        resources,
        mcp: awaken_session_mcp::McpSessions::new(mcp_relay, clock.clone()),
        execution,
        capture: capability::ContentCaptureCapability::new(content_capture),
        session_application: SessionApplicationBindings::default(),
        session_execution: SessionExecutionDependencies::default(),
        local_workspace,
        execution_authority: None,
        environment_binding_sink: None,
        hub: Arc::new(ThreadEventHub::new()),
        store_dir,
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
        capacity: awaken_dispatch_contract::RegisteredCapacity,
    ) -> Result<Self, HostError> {
        authority.validate_worker_connections()?;
        if !process.execution.local.runs_local_pool() {
            return Err(HostError::bad_request(
                "registered Worker capacity requires upstream authority and a local execution pool",
            ));
        }
        let mut selected = select(
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
        );
        // A Worker exists to drain the durable queue with exactly the capacity
        // it registered; its composition cannot select anything else.
        selected.execution.ingress = crate::RunIngress::Durable;
        selected.execution.local = crate::LocalExecution::InProcessPool(
            crate::DispatchPoolSizing::Registered(capacity.into_slots()),
        );
        Ok(selected)
    }

    /// A Coordinator-local Runtime Host over only its execution data
    /// capability. Resources catalog, File commands, Skill authoring, and
    /// reclamation remain outside and are injected through narrow applications.
    /// The same required authority owns commit, Dispatch and checkpoints.
    pub fn for_coordinator(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        file_content_source: Arc<
            dyn crate::FileContentSource<awaken_dispatch_contract::ResourceOperationFence>,
        >,
        memory_repository: Arc<dyn awaken_resource_contract::MemoryRepository>,
        extraction_repository: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
        process: HostProcess,
        runtime_authority: Arc<dyn crate::RuntimeAuthority>,
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
            Arc::new(crate::LocalAuthority::new(runtime_authority)),
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
        runtime_authority: Arc<dyn crate::RuntimeAuthority>,
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
            deployment
                .host_process(local_workspace.into(), clock)
                .expect("a local fixture selects a valid process"),
            Arc::new(crate::LocalAuthority::new(runtime_authority)),
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
            deployment
                .host_process(SharedHost::mint_local_workspace(), clock.clone())
                .expect("a unit-test fixture selects a valid process"),
            Arc::new(crate::LocalAuthority::new(Arc::new(
                crate::EphemeralRuntimeAuthority::new(clock),
            ))),
        )
    }

    /// The durable storage root these dependencies were selected with, when
    /// the process is persistent. Compositions place sibling repositories
    /// beside runtime truth through it.
    #[must_use]
    pub fn storage_dir(&self) -> Option<&std::path::Path> {
        self.store_dir.as_deref()
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

    /// Select the Coordinator-owned durable capability: the Host then owns
    /// committed truth locally. Only store-owning compositions select it; a
    /// Worker is constructed with its upstream authority instead.
    pub fn with_runtime_authority(
        mut self,
        authority: Arc<dyn crate::RuntimeAuthority>,
    ) -> Result<Self, HostError> {
        if !self.authority.hosts_session_application() {
            return Err(HostError::bad_request(
                "a Worker cannot replace its upstream authority with a local Runtime authority",
            ));
        }
        self.authority = Arc::new(crate::LocalAuthority::new(authority));
        Ok(self)
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

    /// Select the Control-owned consent and Workspace-narrowing ports read once per Run.
    #[must_use]
    pub fn with_capture_policy_sources(
        mut self,
        sources: awaken_data_governance_contract::CapturePolicySources,
    ) -> Self {
        self.capture.policy_sources = sources;
        self
    }

    /// Select the canonical volatile Runtime authority over an explicit queue
    /// for a local test or Scenario fixture. Product selects its authority in
    /// `for_coordinator`; the Host never owns a second queue override.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_ephemeral_dispatch(
        mut self,
        store: Arc<awaken_dispatch_erasure::AnyDispatchStore>,
    ) -> Self {
        assert!(
            self.authority.hosts_session_application(),
            "volatile dispatch selection requires a local fixture"
        );
        self.authority = Arc::new(crate::LocalAuthority::new(Arc::new(
            crate::EphemeralRuntimeAuthority::with_dispatch(self.clock.clone(), store),
        )));
        self.execution.ingress = crate::RunIngress::Durable;
        self.execution.local = crate::LocalExecution::in_process_pool();
        self
    }

    /// The same local fixture authority, without a co-located execution pool.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_ephemeral_coordinator_dispatch(
        self,
        store: Arc<awaken_dispatch_erasure::AnyDispatchStore>,
    ) -> Self {
        let mut selected = self.with_ephemeral_dispatch(store);
        selected.execution.local = crate::LocalExecution::RemoteWorkersOnly;
        selected
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
        resolver: Arc<dyn awaken_credential_contract::WorkerLocalCredentialResolver>,
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

impl SessionExecutionDependencies {
    /// One live Memory binding verification used at both staging and activation.
    pub(crate) async fn verify_memory_binding(
        &self,
        workspace: &str,
        id: &str,
        version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_session_types::RunError> {
        self.resource_validator
            .as_ref()
            .ok_or_else(|| {
                awaken_session_types::RunError::bad_request(
                    "memory resources require a configured resource binding validator",
                )
            })?
            .verify_memory_binding(workspace, id, version)
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))
    }

    /// Wire the live resource-invariant port used at activation and Memory use.
    /// Configuration was already selected by the Session control plane; this port
    /// only validates trusted Workspace ownership, lifecycle state, and the frozen
    /// config version. It does not make an authorization decision.
    #[must_use]
    pub fn with_resource_validator(
        mut self,
        validator: Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>,
    ) -> Self {
        self.resource_validator = Some(validator);
        self
    }

    /// Install the Repository-specific live binding guard used by a distributed
    /// Worker without granting it Resource Registry database access.
    #[must_use]
    pub fn with_repository_binding_verifier(
        mut self,
        verifier: Arc<
            dyn awaken_resource_contract::RepositoryBindingVerifier<
                    awaken_dispatch_contract::ResourceOperationFence,
                >,
        >,
    ) -> Self {
        self.repository_binding_verifier = Some(verifier);
        self
    }

    /// Install the credential adapter's exact OAuth refresh port. The Host
    /// retains only this factory and never receives Credential/Secret Store
    /// handles.
    #[must_use]
    pub fn with_credential_refresh_factory(
        mut self,
        factory: Arc<dyn crate::CredentialRefreshFactory>,
    ) -> Self {
        self.credential_refresh_factory = Some(factory);
        self
    }

    /// Install the hosted deployment's platform MCP egress mediator.
    ///
    /// Without it, a Platform-held MCP credential is refused: the Host never
    /// falls back to resolving upstream material it is not authorized to hold.
    #[must_use]
    pub fn with_platform_mcp_egress_provisioner(
        mut self,
        provisioner: Arc<dyn awaken_credential_materializer::PlatformMcpEgressProvisioner>,
    ) -> Self {
        self.platform_mcp_egress = Some(provisioner);
        self
    }

    /// Replace the local Host MCP realization adapter with one downstream
    /// implementation of the same exact-generation Session port. This is the
    /// sole injection seam used by durable Worker commands; desired state and
    /// credential selection remain outside the implementation.
    #[must_use]
    pub fn with_mcp_attachment_realizer(
        mut self,
        realizer: Arc<dyn awaken_session_execution_contract::McpAttachmentRealizer>,
    ) -> Self {
        self.mcp_realizer = Some(realizer);
        self
    }
}

impl HostDependencies {
    /// Select a volatile Vault materializer for this crate's unit fixtures.
    #[cfg(test)]
    #[must_use]
    pub fn with_credentials(
        self,
        credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    ) -> Self {
        self.with_credential_materializer(crate::PinnedCredentialMaterializer::new(
            Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
                credentials,
                secrets,
                awaken_time::system_clock(),
            )),
            awaken_time::system_clock(),
        ))
    }

    /// Select the complete immutable Session execution port group once.
    #[must_use]
    pub fn with_session_execution(mut self, execution: SessionExecutionDependencies) -> Self {
        self.session_execution = execution;
        self
    }
}
