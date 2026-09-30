// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: Host construction selects every injected startup authority exactly once and composes the
// @formal reason: capability groups; it opens no Store. This link is not a formal-proof claim.
//! Builder / configuration wiring for [`SharedHost`]: `new`, the chainable
//! `with_*` methods, per-thread registration, and the process dispatch pool.

use super::*;

#[path = "environment_capacity.rs"]
mod environment_capacity;

use awaken_runtime_contract::delegation::RunDelegationService;

/// The process a Host is constructed in: the installation's resolved platform
/// Workspace, the typed Deployment, and the one process clock. The process
/// composition resolves all three once; every Host constructor receives them
/// together so none can pair a Workspace or clock from a different process
/// selection. Consumer: the public `SharedHost` constructors and `build`.
pub struct HostProcess {
    /// The installation's resolved platform Workspace coordinate.
    pub local_workspace: String,
    /// The typed Deployment this Host serves.
    pub deployment: crate::DeploymentConfig,
    /// The process wall clock the Host drives its Workers on.
    pub clock: Arc<dyn awaken_lease_contract::WallClock>,
}

impl SharedHost {
    /// Mint a fresh process-unique platform Workspace coordinate. This Host
    /// owns the coordinate's format; it never persists one. A durable
    /// installation publishes the minted value once through its installation
    /// marker (`awaken-installation-workspace`) and passes the resolved
    /// coordinate to the constructor, so the Host never touches that file.
    ///
    /// Renamed from `provision_local_workspace` when the durable marker moved
    /// out: without a root this only ever minted a volatile value.
    #[must_use]
    pub fn mint_local_workspace() -> String {
        // Uniqueness, not a time fact: a random nonce keeps two mints in one
        // process distinct without reading a clock.
        let nonce = uuid::Uuid::new_v4().simple();
        format!("workspace_local_{:x}_{nonce}", std::process::id())
    }

    /// Volatile host over `llm` for this crate's own unit tests. Other
    /// compositions use `awaken-runtime-host-testkit`, which owns the
    /// concrete volatile Resource adapters this crate does not link.
    #[cfg(test)]
    pub fn new(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Self {
        Self::new_with_deployment(
            llm,
            model_ref,
            crate::deployment_config::DeploymentConfig::ephemeral(),
        )
    }

    /// Test/scenario construction without an injected Resource component.
    /// The platform Workspace is freshly minted even when `deployment` names a
    /// storage root; a fixture that needs a durable coordinate installs it with
    /// [`Self::with_local_workspace`].
    #[cfg(test)]
    pub fn new_with_deployment(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        deployment: crate::DeploymentConfig,
    ) -> Self {
        // A test or Scenario fixture is its own composition: it chooses the
        // host clock here.
        Self::build(
            llm,
            model_ref.into(),
            capability::ResourceInputs {
                #[cfg(any(test, feature = "test-support"))]
                local: None,
                worker_content: None,
                extraction_repository: None,
            },
            HostProcess {
                local_workspace: Self::mint_local_workspace(),
                deployment,
                clock: awaken_lease_contract::system_clock(),
            },
            HostAuthorityRole::StoreOwning,
        )
    }

    /// Test-support replacement of a volatile fixture's deployment value.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_deployment_config(mut self, deployment: crate::DeploymentConfig) -> Self {
        self.deployment = deployment;
        self
    }

    /// The configured Session sandbox tier selected by the authoritative
    /// [`crate::DeploymentConfig`]. Startup adapters use this read-only view
    /// when their realization strategy must be compatible with the environment;
    /// it does not introduce another tier-selection source.
    #[must_use]
    pub fn sandbox_tier(&self) -> crate::SandboxTier {
        self.deployment.sandbox_tier
    }

    /// The typed Deployment this Host was constructed from. Startup
    /// compositions read it (they never re-read process configuration) to
    /// select the environment tier they install.
    #[must_use]
    pub fn deployment_config(&self) -> &crate::DeploymentConfig {
        &self.deployment
    }

    /// The process wall clock this Host drives its Workers on.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn awaken_lease_contract::WallClock> {
        self.clock.clone()
    }

    /// Install the Coordinator-owned durable capability before the Host serves.
    #[must_use]
    pub fn with_runtime_authority(mut self, authority: Arc<dyn crate::RuntimeAuthority>) -> Self {
        self.authority = Some(authority);
        self
    }

    /// Test/scenario construction with resources but no explicit deployment or
    /// durable extraction authority.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_resources(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        resources: capability::LocalResourcePorts,
        extraction_repository: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
    ) -> Self {
        let clock = resources.clock.clone();
        Self::build(
            llm,
            model_ref.into(),
            capability::ResourceInputs {
                local: Some(resources),
                worker_content: None,
                extraction_repository: Some(extraction_repository),
            },
            HostProcess {
                local_workspace: Self::mint_local_workspace(),
                deployment: crate::deployment_config::DeploymentConfig::ephemeral(),
                clock,
            },
            HostAuthorityRole::StoreOwning,
        )
    }

    /// Construct directly from the process's resolved deployment and its
    /// resolved platform Workspace coordinate.
    /// No resource or runtime backend is opened from process-global state first.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_resources_and_deployment(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        resources: capability::LocalResourcePorts,
        extraction_repository: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
        local_workspace: impl Into<String>,
        deployment: crate::DeploymentConfig,
    ) -> Self {
        let clock = resources.clock.clone();
        Self::build(
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
            HostAuthorityRole::StoreOwning,
        )
    }

    /// Construct an execution Worker with its exact remote content adapters
    /// installed before any Resource data store is selected. Management-only
    /// File and durable extraction authorities fail closed; the Worker opens no
    /// File, Memory-content, or Memory-extraction authority database.
    pub fn new_worker_with_deployment(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        file_content_source: Arc<
            dyn crate::FileContentSource<awaken_dispatch::ResourceOperationFence>,
        >,
        memory_repository: Arc<dyn awaken_resource_contract::MemoryRepository>,
        process: HostProcess,
    ) -> Self {
        Self::build(
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
            HostAuthorityRole::DatabaseLessWorker,
        )
    }

    /// Construct a Coordinator-local Runtime Host from only its execution data
    /// capability. Resources catalog, File commands, Skill authoring, and
    /// reclamation remain outside and are injected through narrow applications.
    /// `process` is the Coordinator composition's resolved process.
    pub fn new_with_runtime_resources_and_deployment(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        file_content_source: Arc<
            dyn crate::FileContentSource<awaken_dispatch::ResourceOperationFence>,
        >,
        memory_repository: Arc<dyn awaken_resource_contract::MemoryRepository>,
        extraction_repository: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
        process: HostProcess,
    ) -> Self {
        Self::build(
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
            HostAuthorityRole::StoreOwning,
        )
    }

    fn build(
        llm: Arc<dyn LlmExecutor>,
        model_ref: String,
        resources: capability::ResourceInputs,
        process: HostProcess,
        authority_role: HostAuthorityRole,
    ) -> Self {
        let HostProcess {
            local_workspace,
            deployment,
            clock,
        } = process;
        assert!(
            !local_workspace.trim().is_empty(),
            "local workspace must not be empty"
        );
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
        let auxiliary_sandboxes = crate::auxiliary_sandbox::default_auxiliary_sandboxes();
        let resource_ports = capability::ResourcePorts::assemble(
            &llm,
            resources,
            store_dir.as_deref(),
            &clock,
            auxiliary_sandboxes.provider("mem"),
        );
        let session_slots = crate::session_slot::SessionRuntimeSlots::default();
        let hub = Arc::new(ThreadEventHub::new());
        Self {
            agent: capability::AgentExecution::new(llm, model_ref, session_slots.clone()),
            exec: capability::AttemptBackends::new(&deployment),
            worker: capability::WorkerTransports::default(),
            env: capability::EnvironmentProviders::new(sandbox_root, auxiliary_sandboxes),
            tools: capability::ToolOffering::new(skills),
            resources: resource_ports,
            mcp: capability::McpRelayCapability::default(),
            dispatch: capability::DispatchCapability::new(),
            capture: capability::ContentCaptureCapability::new(&deployment),
            local_workspace,
            session_slots,
            execution_authority: None,
            hub: hub.clone(),
            // `with_store_dir` still overrides this environment-derived default.
            store_dir,
            authority_role,
            deployment,
            authority: {
                #[cfg(any(test, feature = "test-support"))]
                {
                    Some(Arc::new(crate::EphemeralRuntimeAuthority::new(
                        clock.clone(),
                    )))
                }
                #[cfg(not(any(test, feature = "test-support")))]
                {
                    None
                }
            },
            clock,
            completion: Arc::new(CompletionRegistry::new(hub)),
            environment_binding_sink: std::sync::RwLock::new(None),
        }
    }

    /// Install the same authenticated Control client as the pre-effect Session
    /// Environment root authority on a database-less Worker.
    #[must_use]
    pub fn with_environment_binding_sink(
        mut self,
        sink: Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) -> Self {
        self.environment_binding_sink = std::sync::RwLock::new(Some(sink));
        self
    }

    /// Platform-managed local workspace used when no authenticated/path scope
    /// exists. Process startup may override it with a provisioned coordinate.
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
    /// Process startup uses this to place sibling repositories beside runtime
    /// truth without re-reading process configuration or teaching the runtime
    /// about those repositories.
    pub fn storage_dir(&self) -> Option<&std::path::Path> {
        self.store_dir.as_deref()
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

    /// Clone the completion projection consumed by the authenticated Worker
    /// settle boundary.
    ///
    /// A coordinator-only embedding has no local dispatch pool. Its foreground
    /// Managed Session Run therefore registers with this sink before enqueueing
    /// and the remote Worker transport projects the committed terminal/awaiting
    /// state back into the same host. The sink is notification-only: committed
    /// Run truth remains authoritative and the bounded timeout retains its
    /// existing one-read recovery fallback.
    pub fn dispatch_completion_sink(&self) -> Arc<dyn awaken_dispatch::CompletionSink> {
        self.completion.clone()
    }

    /// Install the process composition's sole wakeup for committed Run progress.
    /// The callback is notification-only; its consumer must reread durable truth.
    pub fn install_committed_progress_wakeup(
        &self,
        wakeup: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), &'static str> {
        self.completion.install_committed_progress_wakeup(wakeup)
    }

    /// The role selected at construction, independent of whether any one
    /// claim-fenced transport has been installed yet.
    pub(crate) fn is_database_less_worker(&self) -> bool {
        self.authority_role == HostAuthorityRole::DatabaseLessWorker
    }

    /// Vary only the role in unit tests that exercise missing-port rejection.
    /// Product Worker composition always uses `new_worker_with_deployment`.
    #[cfg(test)]
    pub(crate) fn with_database_less_worker_role_for_test(mut self) -> Self {
        self.authority_role = HostAuthorityRole::DatabaseLessWorker;
        self
    }

    /// Test-support upgrade of a volatile fixture to local durable storage
    /// under `dir`, with the durable Memory extraction repository the caller
    /// opened there and the relocation of its Session environment providers
    /// under that root (`awaken-runtime-host-testkit` supplies both for the
    /// sandbox-backed providers it links; this crate never does).
    #[cfg(any(test, feature = "test-support"))]
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
        self.resources
            .memory
            .set_extraction_repository(extraction_repository);
        // A durable host must place its Workdir sandboxes under a stable root too;
        // otherwise the dispatch row survives restart but its persisted handle
        // points into the previous process's random temp directory.
        self.env.session_provider = relocate(&self.env.session_provider, dir.join("sandboxes"));
        self.env.backend_owned_session_provider = self
            .env
            .backend_owned_session_provider
            .as_ref()
            .map(|provider| relocate(provider, dir.join("trusted-local-sandboxes")));
        if let Some(mounter) = self.memory_mounter() {
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
    pub fn with_store_dir(self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .expect("create durable Memory extraction repository directory");
        let repository = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(
                &dir.join("sessions.db").to_string_lossy(),
            )
            .expect("open durable Memory extraction repository"),
        );
        self.with_durable_store_dir(dir, repository, |provider, base| {
            crate::session_environment::relocated_provider(provider, base)
        })
    }

    /// Bind the exact published delegation targets to a prepared Session.
    pub fn register_thread_delegates(&self, thread: &str, delegates: Vec<String>) {
        self.session_slots
            .update(thread, |slot| slot.delegates = delegates);
    }

    pub(crate) fn thread_delegate_ids(&self, thread: &str) -> Option<Vec<String>> {
        self.session_slots
            .read(thread, |slot| slot.delegates.clone())
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
        commit: Arc<crate::store::HostCommit>,
        parent_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        publication_source: Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
        tool_configuration: crate::delegate::CallerToolConfiguration,
    ) -> Result<Option<Arc<dyn RunDelegationService>>, HostError> {
        let Some(parent_snapshot) = parent_snapshot else {
            return Ok(None);
        };
        let agent_bindings = &parent_snapshot.resolved_spec.plugin_config.agent;
        if agent_bindings.delegates.is_empty() && agent_bindings.advisor.is_none() {
            return Ok(None);
        }
        let workspace = self.thread_workspace(thread);
        // Use the same topology decision as pool startup. Ephemeral and durable
        // Runtime authorities both own dispatch; gating children on persistence
        // durability leaves a claim-bound parent executing its child with the
        // parent's checkpoint/commit projection instead of a child claim.
        let scheduler = if self.runs_local_dispatch_pool() {
            let recovery_projection = commit.recovery_projection();
            let claimed_commit = self.worker_claimed_commit()?;
            Some(crate::agent_runner::RunScheduler {
                clock: self.clock.clone(),
                store: self.dispatch_store()?,
                commit: commit.clone(),
                reader: commit,
                owner: self.deployment.dispatch_owner.clone(),
                claimed_commit,
                recovery_projection,
                session_resources: self.thread_resource_manifest(thread),
                publication_source: Some(publication_source.clone()),
                claimed_model_request_gate: self.coordination_endpoint().map(|endpoint| {
                    crate::coordination::claimed_model_request_gate_factory(endpoint, thread)
                }),
            })
        } else {
            None
        };
        let adapters = self.child_execution_adapters(thread, sandbox.clone());
        let service = HostRunDelegationService::new(
            self.agent.llm.clone(),
            sandbox,
            parent_snapshot,
            adapters,
            Some(publication_source),
            workspace,
        )
        .map_err(|error| HostError::bad_request(error.to_string()))?
        .with_scheduler(scheduler)
        .with_tool_configuration(tool_configuration);
        Ok(Some(Arc::new(service)))
    }
}

#[cfg(test)]
mod startup_tests {
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
            capability::ResourceInputs {
                #[cfg(any(test, feature = "test-support"))]
                local: None,
                worker_content: None,
                extraction_repository: Some(expected.clone()),
            },
            HostProcess {
                local_workspace: SharedHost::mint_local_workspace(),
                deployment: crate::DeploymentConfig::ephemeral(),
                clock: awaken_lease_contract::system_clock(),
            },
            HostAuthorityRole::StoreOwning,
        );

        assert!(
            Arc::ptr_eq(&expected, &host.resources.memory.extraction_repository()),
            "T1/E1"
        );
    }

    #[test]
    fn constructor_selects_worker_role_without_a_transport_marker() {
        // C1 constructor={database-less Worker, Coordinator-local}; C2 both
        // receive the same File/Memory content ports; C3 claimed-commit port
        // is absent. R1 Worker => remote role and fail-closed missing commit;
        // R2 Coordinator => store-owning role and no remote commit request.
        // A content port or an injected commit port alone cannot choose C1;
        // commit_ingest W1-W3 covers the independent port combinations.
        let file: Arc<dyn crate::FileContentSource<awaken_dispatch::ResourceOperationFence>> =
            Arc::new(awaken_resource_contract::UnavailableFileContentSource);
        let memory: Arc<dyn awaken_resource_contract::MemoryRepository> =
            Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
                awaken_lease_contract::system_clock(),
            ));
        let extraction: Arc<dyn awaken_ext_memory::MemoryExtractionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("test extraction repository"),
        );
        let process = || HostProcess {
            local_workspace: SharedHost::mint_local_workspace(),
            deployment: crate::DeploymentConfig::ephemeral(),
            clock: awaken_lease_contract::system_clock(),
        };
        let worker = SharedHost::new_worker_with_deployment(
            Arc::new(crate::NoModelConfiguredExecutor),
            "stub",
            file.clone(),
            memory.clone(),
            process(),
        );
        let coordinator = SharedHost::new_with_runtime_resources_and_deployment(
            Arc::new(crate::NoModelConfiguredExecutor),
            "stub",
            file,
            memory,
            extraction,
            process(),
        );
        assert!(worker.is_database_less_worker(), "R1 role");
        assert!(worker.worker_claimed_commit().is_err(), "R1 missing port");
        assert!(!coordinator.is_database_less_worker(), "R2 role");
        assert!(coordinator.worker_claimed_commit().unwrap().is_none(), "R2");
    }
}
