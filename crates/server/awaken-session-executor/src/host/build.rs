// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: Host construction selects every injected startup authority exactly once and composes the
// @formal reason: capability groups; it opens no Store. This link is not a formal-proof claim.
//! Construction of [`SharedHost`] from its [`HostDependencies`] (the one
//! constructor), per-thread registration, and the process dispatch pool.

use super::*;

use awaken_runtime_contract::delegation::RunDelegationService;

/// The process a Host is constructed in: the installation's resolved platform
/// Workspace, the process capabilities the composition projected from its
/// configuration, and the one process clock. The process composition resolves them once;
/// every [`HostDependencies`] constructor receives them together so none can
/// pair a Workspace or clock from a different process selection. Consumer: the
/// `HostDependencies` constructors.
pub struct HostProcess {
    /// The installation's resolved platform Workspace coordinate.
    pub local_workspace: String,
    /// This process's part in Run execution, selected by the composition.
    pub execution: crate::ExecutionTopology,
    /// The process content-capture ceiling and redactor. Per-request consent
    /// and the Workspace policy may only narrow it.
    pub content_capture: crate::ContentCaptureSettings,
    /// The durable storage root: set selects durable SQLite commit and
    /// Resource adapters that survive a restart; `None` selects ephemeral ones.
    pub storage_dir: Option<PathBuf>,
    /// The root the construction-time Session provider realizes sandboxes
    /// under until the composition installs its own provider.
    pub sandbox_root: PathBuf,
    /// Whether `trusted_host` Environments may be realized on this process.
    pub trusted_host: crate::TrustedHostPolicy,
    /// The warm Session capacity this process keeps.
    pub warm_pool: crate::WarmPool,
    /// How the Session MCP relay is exposed to container networks, if at all.
    pub mcp_relay: Option<awaken_session_mcp::ContainerMcpRelayExposure>,
    /// The exact ACP adapters this process advertises and serves.
    pub acp_profile: Option<crate::AcpWorkerProfile>,
    /// The process wall clock the Host drives its Workers on.
    pub clock: Arc<dyn awaken_time::WallClock>,
}

impl HostDependencies {
    /// The one Host constructor: validate the selected dependencies once and
    /// assemble the Host's runtime state around them. It performs no I/O; every
    /// dependency was acquired while the composition filled `dependencies`.
    pub fn build(self) -> Result<Arc<SharedHost>, HostError> {
        let dependencies = self;
        if dependencies.local_workspace.trim().is_empty() {
            return Err(HostError::bad_request("local workspace must not be empty"));
        }
        dependencies
            .execution
            .validate()
            .map_err(HostError::bad_request)?;
        // A registered capacity exists only inside an in-process pool, so the
        // "registered but coordinator-only" graph has no representation; the
        // remaining contradiction is a registration under a local authority.
        if dependencies
            .execution
            .local
            .pool_sizing()
            .and_then(crate::DispatchPoolSizing::registered)
            .is_some()
            && dependencies.authority.hosts_session_application()
        {
            return Err(HostError::bad_request(
                "registered Worker capacity requires upstream authority and a local execution pool",
            ));
        }
        Ok(Arc::new_cyclic(|host| {
            assemble_host(dependencies, host.clone())
        }))
    }
}

fn assemble_host(dependencies: HostDependencies, host: std::sync::Weak<SharedHost>) -> SharedHost {
    let HostDependencies {
        local_workspace,
        clock,
        store_dir,
        authority,
        execution_authority,
        environment_binding_sink,
        hub,
        agent,
        exec,
        worker,
        env,
        tools,
        resources,
        mcp,
        execution,
        capture,
        session_application,
        session_execution,
        skill_bundle_source,
    } = dependencies;
    let installed_sessions = Arc::new(crate::installed_session::InstalledSessions::default());
    let memory = Arc::new(crate::memory::MemoryRuntime::new(
        agent.llm.clone(),
        env.auxiliary_sandboxes.provider("mem"),
        Arc::new(BackgroundRuns::new()),
        resources.extraction_repository.clone(),
        clock.clone(),
        agent.inference_materializer.clone(),
    ));
    let background_supervisor =
        Arc::new(awaken_ext_background_task::BackgroundTaskSupervisor::new(
            awaken_runtime_contract::content_fingerprint(&(
                "background-worker-v1",
                std::process::id(),
                uuid::Uuid::new_v4().simple().to_string(),
            ))
            .expect("background worker identity serializes"),
            clock.clone(),
        ));
    SharedHost {
        agent: capability::AgentExecution::new(agent, installed_sessions.clone()),
        exec,
        worker,
        realizer: crate::RealizationExecutor {
            environments: crate::environment_holding::EnvironmentHolding::new(
                env,
                execution.local.runs_local_pool(),
                crate::resource_staging::ResourceStaging {
                    sessions: installed_sessions.realizations(),
                    bundle_source: skill_bundle_source,
                },
                execution_authority,
            ),
            cleanup: crate::terminal_cleanup::TerminalCleanup {
                sessions: installed_sessions.realizations(),
            },
        },
        tools,
        resources: capability::ResourcePorts::new(resources, memory),
        mcp,
        dispatch: capability::DispatchCapability::new(execution, host),
        capture,
        local_workspace,
        installed_sessions,
        hub: hub.clone(),
        store_dir,
        authority,
        clock,
        background_supervisor,
        completion: Arc::new(CompletionRegistry::new(
            hub,
            session_application.lifecycle_wakeup_binding(),
        )),
        session_application,
        session_execution,
        environment_binding_sink: std::sync::RwLock::new(environment_binding_sink),
    }
}

impl SharedHost {
    /// The realization executor this Host was built with (warm Environment capacity).
    #[must_use]
    pub fn realization_executor(&self) -> &crate::RealizationExecutor {
        &self.realizer
    }

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

    /// The process wall clock this Host drives its Workers on.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn awaken_time::WallClock> {
        self.clock.clone()
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
        self.installed_sessions
            .replace_frozen_with(thread, |next| next.workspace = Some(workspace.to_string()));
    }

    pub(crate) fn register_thread_agent_projection(&self, thread: &str, agent_id: &str) {
        self.installed_sessions
            .replace_frozen_with(thread, |next| next.agent_id = Some(agent_id.to_string()));
    }

    /// Rebuildable Agent projection used only as input to Session admission.
    /// Runtime never treats this cache as an executable publication authority.
    pub fn thread_agent_projection(&self, thread: &str) -> Option<String> {
        let realizations = self.installed_sessions.realizations();
        realizations.frozen(thread).agent_id.clone()
    }

    /// Workspace context supplied to the Session application admission boundary.
    /// Unknown flat-protocol threads use the process's persisted local workspace.
    pub fn thread_workspace(&self, thread: &str) -> String {
        let realizations = self.installed_sessions.realizations();
        realizations
            .frozen(thread)
            .workspace
            .clone()
            .unwrap_or_else(|| self.local_workspace.clone())
    }

    /// Workspace for a prepared thread; unlike `thread_workspace`, unknown ids
    /// do not inherit local ownership and therefore cannot enumerate artifacts.
    pub fn registered_thread_workspace(&self, thread: &str) -> Option<String> {
        let realizations = self.installed_sessions.realizations();
        realizations.frozen(thread).workspace.clone()
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
    pub fn dispatch_completion_sink(&self) -> Arc<dyn awaken_dispatch_execution::CompletionSink> {
        self.completion.clone()
    }

    /// The edges back into the Session application built over this Host. The
    /// composition root binds each once, after it built that application.
    #[must_use]
    pub fn session_application_bindings(&self) -> &SessionApplicationBindings {
        &self.session_application
    }

    /// Bind the exact published delegation targets to a prepared Session.
    pub fn register_thread_delegates(&self, thread: &str, delegates: Vec<String>) {
        self.installed_sessions
            .replace_frozen_with(thread, |next| next.delegates = delegates);
    }

    pub(crate) fn thread_delegate_ids(&self, thread: &str) -> Option<Vec<String>> {
        self.installed_sessions
            .read(thread, |slot| slot.inputs.delegates.clone())
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
            Some(crate::agent_runner::RunScheduler {
                clock: self.clock.clone(),
                binding: self.authority.dispatch_binding().ok_or_else(|| {
                    HostError::internal("child scheduling requires a dispatch binding")
                })?,
                commit: commit.clone(),
                reader: commit,
                owner: self.dispatch_owner().to_owned(),
                session_resources: self.realizer.staging.thread_resource_manifest(thread),
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

    #[tokio::test]
    async fn explicit_extraction_repository_is_the_initial_authority() {
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
                .await
                .expect("test extraction repository"),
        );
        let host = (HostDependencies::for_coordinator(
            Arc::new(crate::NoModelConfiguredExecutor),
            crate::UNCONFIGURED_MODEL_REF,
            Arc::new(awaken_resource_contract::UnavailableFileContentSource),
            Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
                awaken_time::system_clock(),
            )),
            expected.clone(),
            crate::DeploymentConfig::ephemeral()
                .host_process(
                    SharedHost::mint_local_workspace(),
                    awaken_time::system_clock(),
                )
                .expect("ephemeral process"),
            Arc::new(crate::EphemeralRuntimeAuthority::new(
                awaken_time::system_clock(),
            )),
        ))
        .build()
        .expect("validated Host dependencies");

        assert!(
            Arc::ptr_eq(&expected, &host.resources.memory.extraction_repository()),
            "T1/E1"
        );
    }

    #[tokio::test]
    async fn constructor_selects_worker_role_without_a_transport_marker() {
        // C1 constructor={database-less Worker, Coordinator-local}; C2 both
        // receive the same File/Memory content ports; C3 all required Worker
        // connections and registered capacity are supplied. R0 missing C3
        // refuses before construction; R1 Worker => remote boundary and selected commit;
        // R2 Coordinator => selected local commit and no remote request. R3 a
        // local replacement requested on Worker dependencies => typed refusal
        // before construction, never a role change. A content port
        // cannot choose C1, and a Worker's transports exist only inside its
        // upstream authority; commit_ingest W1-W3 covers the port combinations.
        let file: Arc<dyn crate::FileContentSource<crate::ResourceOperationFence>> =
            Arc::new(awaken_resource_contract::UnavailableFileContentSource);
        let memory: Arc<dyn awaken_resource_contract::MemoryRepository> = Arc::new(
            awaken_memory_store::VolatileMemoryRepository::new(awaken_time::system_clock()),
        );
        let extraction: Arc<dyn awaken_ext_memory::MemoryExtractionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .expect("test extraction repository"),
        );
        let process = || {
            crate::DeploymentConfig::ephemeral()
                .host_process(
                    SharedHost::mint_local_workspace(),
                    awaken_time::system_clock(),
                )
                .expect("ephemeral process")
        };
        assert!(
            HostDependencies::for_worker(
                Arc::new(crate::NoModelConfiguredExecutor),
                "stub",
                file.clone(),
                memory.clone(),
                process(),
                crate::UpstreamAuthority::default(),
                awaken_dispatch_contract::RegisteredCapacity::of(
                    &awaken_dispatch_contract::WorkerManifest::default()
                )
                .unwrap(),
            )
            .is_err(),
            "R0 missing required upstream connections"
        );
        let worker = (HostDependencies::for_worker(
            Arc::new(crate::NoModelConfiguredExecutor),
            "stub",
            file.clone(),
            memory.clone(),
            process(),
            crate::host::test_upstream_authority(),
            awaken_dispatch_contract::RegisteredCapacity::of(
                &awaken_dispatch_contract::WorkerManifest::default(),
            )
            .unwrap(),
        )
        .expect("complete Worker connections"))
        .build()
        .expect("validated Host dependencies");
        let coordinator = (HostDependencies::for_coordinator(
            Arc::new(crate::NoModelConfiguredExecutor),
            "stub",
            file,
            memory,
            extraction,
            process(),
            Arc::new(crate::EphemeralRuntimeAuthority::new(
                awaken_time::system_clock(),
            )),
        ))
        .build()
        .expect("validated Host dependencies");
        assert!(
            matches!(
                worker.authority.open_commit("role").await,
                Ok(crate::store::HostCommit::Remote(_))
            ),
            "R1 remote commit boundary"
        );
        assert!(
            worker.authority.claimed_commit().unwrap().is_some(),
            "R1 selected required port"
        );
        assert!(
            coordinator.authority.open_commit("role").await.is_ok(),
            "R2 selected local commit boundary"
        );
        assert!(
            coordinator.authority.claimed_commit().unwrap().is_none(),
            "R2"
        );
        let worker_dependencies = HostDependencies::for_worker(
            Arc::new(crate::NoModelConfiguredExecutor),
            "stub",
            Arc::new(awaken_resource_contract::UnavailableFileContentSource),
            Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
                awaken_time::system_clock(),
            )),
            process(),
            crate::host::test_upstream_authority(),
            awaken_dispatch_contract::RegisteredCapacity::of(
                &awaken_dispatch_contract::WorkerManifest::default(),
            )
            .unwrap(),
        )
        .expect("complete Worker connections");
        assert!(
            worker_dependencies
                .with_runtime_authority(Arc::new(crate::EphemeralRuntimeAuthority::new(
                    awaken_time::system_clock()
                ),))
                .is_err(),
            "R3 Worker authority cannot become local",
        );
    }
}
