// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: Host construction selects every injected startup authority exactly once and composes the
// @formal reason: capability groups; it opens no Store. This link is not a formal-proof claim.
//! Construction of [`ExecutorComponents`] from its [`SessionExecutorBuilder`] (the one
//! constructor), per-thread registration, and the process dispatch pool.

use super::*;
use awaken_session_realization::peers::EnvironmentPeer as _;

/// The process a Host is constructed in: the installation's resolved platform
/// Workspace, the process capabilities the composition projected from its
/// configuration, and the one process clock. The process composition resolves them once;
/// every [`SessionExecutorBuilder`] constructor receives them together so none can
/// pair a Workspace or clock from a different process selection. Consumer: the
/// `SessionExecutorBuilder` constructors.
pub struct SessionExecutionProcess {
    /// The installation's resolved platform Workspace coordinate.
    pub local_workspace: String,
    /// This process's part in Run execution, selected by the composition.
    pub execution: crate::ExecutionTopology,
    /// The process content-capture ceiling and redactor. Per-request consent
    /// and the Workspace policy may only narrow it.
    pub content_capture: awaken_deployment_config::ContentCaptureSettings,
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
    pub mcp_relay: Option<awaken_session_environment_contract::ContainerMcpRelayExposure>,
    /// The exact ACP adapters this process advertises and serves.
    pub acp_profile: Option<awaken_acp_contract::AcpWorkerProfile>,
    /// The process wall clock the Host drives its Workers on.
    pub clock: Arc<dyn awaken_time::WallClock>,
}

impl SessionExecutionProcess {
    /// The process a Host is constructed in, projected from the typed
    /// Deployment configuration by the composition root. Consumer: every
    /// `SessionExecutorBuilder` constructor.
    pub fn from_deployment(
        deployment: &awaken_deployment_config::DeploymentConfig,
        local_workspace: String,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, String> {
        Ok(Self {
            local_workspace,
            execution: deployment.execution_topology()?,
            content_capture: deployment.content_capture,
            storage_dir: deployment.storage_dir.clone(),
            sandbox_root: awaken_deployment_config::session_sandbox_base(deployment),
            trusted_host: if deployment.trusted_host_enabled() {
                crate::TrustedHostPolicy::Admitted
            } else {
                crate::TrustedHostPolicy::Refused
            },
            warm_pool: crate::WarmPool::new(
                deployment.sandbox.warm_pool_size,
                deployment.sandbox.warm_pool_total_size,
            ),
            mcp_relay: deployment.sandbox.container_mcp_relay.clone(),
            acp_profile: deployment.acp.clone(),
            clock,
        })
    }
}

impl SessionExecutorBuilder {
    /// The one Host constructor: validate the selected dependencies once and
    /// assemble the Host's runtime state around them. It performs no I/O; every
    /// dependency was acquired while the composition filled `dependencies`.
    pub fn build(self) -> Result<Arc<ExecutorComponents>, SessionExecutionError> {
        let dependencies = self;
        if dependencies.local_workspace.trim().is_empty() {
            return Err(SessionExecutionError::bad_request(
                "local workspace must not be empty",
            ));
        }
        dependencies
            .execution
            .validate()
            .map_err(SessionExecutionError::bad_request)?;
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
            return Err(SessionExecutionError::bad_request(
                "registered Worker capacity requires upstream authority and a local execution pool",
            ));
        }
        Ok(Arc::new(assemble_components(dependencies)))
    }
}

fn assemble_components(dependencies: SessionExecutorBuilder) -> ExecutorComponents {
    let SessionExecutorBuilder {
        #[cfg(test)]
        realization_source,
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
    let realizations = realization_view(
        &installed_sessions,
        #[cfg(test)]
        realization_source,
    );
    let memory = memory_runtime(&agent, &env, &resources, &clock);
    let realization_executor = Arc::new(crate::realization_executor::RealizationExecutor::new(
        clock.clone(),
        realizations.clone(),
        environment_holding(
            env,
            execution.local.runs_local_pool(),
            &realizations,
            environment_binding_sink,
            authority.as_ref(),
            &local_workspace,
            agent.agent_publications.clone(),
        ),
        crate::resource_staging::ResourceStaging::new(
            realizations.clone(),
            resource_sources(
                skill_bundle_source,
                &resources,
                &session_execution,
                authority.as_ref(),
                exec.credential_materializer.clone(),
            ),
        ),
        terminal_effects(&realizations, &local_workspace, &resources),
        crate::execution_authority::ExecutionAuthority::new(
            realizations.clone(),
            execution_authority,
        ),
    ));
    let (commit_source, thread_reads) =
        committed_reads(&authority, &installed_sessions, &realization_executor);
    let resources = Arc::new(capability::ResourcePorts::new(
        resources,
        memory,
        Arc::downgrade(&installed_sessions),
        realization_executor.staging().memory_binding_compiler(),
        local_workspace.clone(),
        tools
            .plugin_config
            .get(awaken_ext_memory::MEMORY_PLUGIN_ID)
            .cloned(),
    ));
    let capture = Arc::new(capture);
    let completion = Arc::new(CompletionRegistry::new(hub.clone()));
    let dispatch_hint = dispatch_hint(authority.as_ref());
    let durable_run_cancellation = crate::host::DurableRunCancellation::new(
        installed_sessions.clone(),
        execution.local.runs_local_pool(),
        capability::local_dispatch_owner(authority.as_ref()),
        dispatch_hint.clone(),
    );
    let (mcp_root_authority, mcp_application_inputs) =
        mcp_application_inputs(&installed_sessions, &mcp, &durable_run_cancellation);
    let capabilities = Arc::new(crate::RunCapabilities {
        local_workspace: local_workspace.clone(),
        installed_sessions: installed_sessions.clone(),
        clock: clock.clone(),
        background_supervisor: background_supervisor(&clock),
        agent: Arc::new(capability::AgentExecution::new(
            agent,
            installed_sessions.clone(),
        )),
        exec: Arc::new(exec),
        tools: Arc::new(tools),
        worker: Arc::new(worker),
        session_application: session_application.clone(),
    });
    let run_assembly = Arc::new(crate::RunAssembly {
        child_runs: Arc::new(crate::ChildRunLauncher::new(
            capabilities.clone(),
            authority.clone(),
            clock.clone(),
            execution.clone(),
            realization_executor.clone(),
        )),
        capabilities: capabilities.clone(),
        local_workspace: local_workspace.clone(),
        installed_sessions: installed_sessions.clone(),
        hub: hub.clone(),
        authority: authority.clone(),
        clock: clock.clone(),
        completion: completion.clone(),
        session_execution: Arc::new(session_execution),
        execution: execution.clone(),
        realization_executor: realization_executor.clone(),
        thread_reads: thread_reads.clone(),
        commit_source,
        resources: resources.clone(),
        mcp: mcp.clone(),
        capture: capture.clone(),
        mcp_application_inputs,
        durable_run_cancellation,
        mcp_root_authority,
    });
    let (dispatch, session_runs) = session_run_delivery(&run_assembly, &completion, dispatch_hint);
    ExecutorComponents {
        realization_executor,
        thread_reads,
        live_inboxes: Arc::new(crate::LiveInboxes::new(installed_sessions.clone())),
        capabilities,
        run_assembly,
        coordination: Arc::new(crate::ChildRunCoordination::new(session_runs.clone())),
        outcomes: Arc::new(crate::Outcomes::new(session_runs.clone())),
        budget: Arc::new(session_budgets(&session_runs)),
        session_runs,
        resources,
        dispatch,
        capture,
        local_workspace,
        hub,
        store_dir,
        clock,
        completion,
        session_application,
    }
}

/// The realization executor's view of the installed Sessions; a unit test may
/// select a replacement source over the same allocation.
fn realization_view(
    installed_sessions: &Arc<crate::InstalledSessions>,
    #[cfg(test)] realization_source: Option<super::dependencies::RealizationSourceFactory>,
) -> crate::session_realizations::SessionRealizations {
    #[cfg(test)]
    if let Some(select) = realization_source {
        return crate::session_realizations::SessionRealizations::new(select(
            installed_sessions.clone(),
        ));
    }
    installed_sessions.realizations()
}

/// The Memory auxiliary-agent capability: extraction runs on its own sandbox
/// provider and the selected extraction repository.
fn memory_runtime(
    agent: &capability::AgentSelection,
    env: &capability::EnvironmentProviders,
    resources: &capability::ResourceSelection,
    clock: &Arc<dyn awaken_time::WallClock>,
) -> Arc<crate::memory::MemoryRuntime> {
    Arc::new(crate::memory::MemoryRuntime::new(
        agent.llm.clone(),
        env.auxiliary_sandboxes.provider("mem"),
        Arc::new(BackgroundRuns::new()),
        resources.extraction_repository.clone(),
        clock.clone(),
        agent.inference_materializer.clone(),
    ))
}

/// The Environment holding over the installed Sessions, with the dispatch
/// instance narrowed to execution binding and the frozen publication selection.
fn environment_holding(
    env: capability::EnvironmentProviders,
    runs_local_pool: bool,
    realizations: &crate::session_realizations::SessionRealizations,
    sink: Option<Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>>,
    authority: &dyn crate::SessionAuthority,
    local_workspace: &str,
    publications: Option<Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>>,
) -> crate::environment_holding::EnvironmentHolding {
    crate::environment_holding::EnvironmentHolding::new(
        env,
        runs_local_pool,
        realizations.clone(),
        sink,
        dispatch_execution(authority),
        crate::session_publication::SessionPublicationSelection::new(
            realizations.clone(),
            local_workspace.to_owned(),
            publications,
        ),
    )
}

/// The Resource sources the realization executor stages from.
fn resource_sources(
    bundle_source: Option<
        Arc<
            dyn awaken_session_configuration_contract::SkillBundleSource<
                    crate::ResourceOperationFence,
                >,
        >,
    >,
    resources: &capability::ResourceSelection,
    session_execution: &crate::SessionExecutionDependencies,
    authority: &dyn crate::SessionAuthority,
    credential_materializer: Option<crate::PinnedCredentialMaterializer>,
) -> crate::resource_staging::ResourceSources {
    crate::resource_staging::ResourceSources {
        bundle_source,
        file_content_source: resources.file_content_source.clone(),
        memory: resources.memory_stores.clone(),
        resource_validator: session_execution.resource_validator.clone(),
        repository_binding_verifier: session_execution.repository_binding_verifier.clone(),
        fetches_resources_upstream: authority.fetches_resources_upstream(),
        credential_materializer,
        memory_reference_encoder: authority.memory_reference_encoder(),
    }
}

/// Terminal cleanup effects with the Artifact harvester they flush through.
fn terminal_effects(
    realizations: &crate::session_realizations::SessionRealizations,
    local_workspace: &str,
    resources: &capability::ResourceSelection,
) -> crate::terminal_effects::TerminalEffects {
    crate::terminal_effects::TerminalEffects::new(
        realizations.clone(),
        Arc::new(crate::artifact_harvest::ArtifactHarvester::new(
            realizations.clone(),
            local_workspace.to_owned(),
            resources.artifact_publisher.clone(),
        )),
    )
}

/// The same dispatch instance this process uses, narrowed to execution binding.
fn dispatch_execution(
    authority: &dyn crate::SessionAuthority,
) -> Option<Arc<dyn awaken_dispatch_contract::DispatchExecution>> {
    authority
        .dispatch_binding()
        .map(|binding| binding.queue() as Arc<dyn awaken_dispatch_contract::DispatchExecution>)
}

/// Committed Thread reads over this executor's resident-context-aware commit
/// source, labelling advisor links from the publications it resolves.
fn committed_reads(
    authority: &Arc<dyn crate::SessionAuthority>,
    installed_sessions: &Arc<crate::InstalledSessions>,
    realization_executor: &Arc<crate::realization_executor::RealizationExecutor>,
) -> (
    Arc<crate::thread_reads::InstalledCommitSource>,
    Arc<crate::CommittedReads>,
) {
    let commit_source = Arc::new(crate::thread_reads::InstalledCommitSource::new(
        authority.clone(),
        installed_sessions,
    ));
    let reads = Arc::new(crate::CommittedReads::new(
        commit_source.clone(),
        Arc::new(crate::thread_reads::ResolvedAdvisorModels::new(
            installed_sessions,
            realization_executor,
        )),
    ));
    (commit_source, reads)
}

/// The process dispatch and the Session Run component that submits, cancels
/// and publishes through it.
fn session_run_delivery(
    run_assembly: &Arc<crate::RunAssembly>,
    completion: &Arc<CompletionRegistry>,
    hint: awaken_dispatch_execution::DispatchHint,
) -> (
    Arc<awaken_dispatch_execution::ProcessDispatch>,
    Arc<crate::SessionRuns>,
) {
    let dispatch = Arc::new(awaken_dispatch_execution::ProcessDispatch::new(
        run_assembly.execution.clone(),
        run_assembly.clock.clone(),
        hint,
    ));
    let runs = Arc::new(crate::SessionRuns::new(
        run_assembly.clone(),
        completion.clone(),
        dispatch.clone(),
    ));
    (dispatch, runs)
}

/// The process-local BackgroundTask slot registry, under a process-unique
/// Worker identity.
fn background_supervisor(
    clock: &Arc<dyn awaken_time::WallClock>,
) -> Arc<awaken_ext_background_task::BackgroundTaskSupervisor> {
    Arc::new(awaken_ext_background_task::BackgroundTaskSupervisor::new(
        awaken_agent_contract::content_fingerprint(&(
            "background-worker-v1",
            std::process::id(),
            uuid::Uuid::new_v4().simple().to_string(),
        ))
        .expect("background worker identity serializes"),
        clock.clone(),
    ))
}

/// The local claimer waits on this exact wake; publishers hint through it
/// without holding the claimer.
fn dispatch_hint(
    authority: &dyn crate::SessionAuthority,
) -> awaken_dispatch_execution::DispatchHint {
    awaken_dispatch_execution::DispatchHint::new(
        authority
            .dispatch_wake()
            .unwrap_or_else(|| Arc::new(awaken_dispatch_execution::LocalWakeSignal::new())),
    )
}

/// The MCP root authority and the application-input target its handlers
/// reach, both over the installed Sessions.
fn mcp_application_inputs(
    installed_sessions: &Arc<crate::InstalledSessions>,
    mcp: &awaken_session_mcp::McpSessions,
    cancellation: &crate::host::DurableRunCancellation,
) -> (
    Arc<crate::mcp_host::McpRootAuthority>,
    Arc<crate::mcp_host::McpApplicationInputs>,
) {
    let authority = Arc::new(crate::mcp_host::McpRootAuthority::new(
        installed_sessions.clone(),
    ));
    let inputs = Arc::new(crate::mcp_host::McpApplicationInputs::new(
        installed_sessions.clone(),
        mcp.clone(),
        authority.clone(),
        cancellation.clone(),
    ));
    (authority, inputs)
}

impl ExecutorComponents {
    /// Warm Environment capacity supplied by this Host's existing providers.
    #[must_use]
    pub fn environment_capacity(&self) -> &crate::environment_providers::EnvironmentProviders {
        self.realization_executor.environments().providers()
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

    /// The edges back into the Session application built over this Host. The
    /// composition root binds each once, after it built that application.
    #[must_use]
    pub fn session_application_bindings(&self) -> &SessionApplicationBindings {
        &self.session_application
    }

    /// Capability assembly for this process's Runs.
    #[must_use]
    pub fn capabilities(&self) -> &Arc<crate::RunCapabilities> {
        &self.capabilities
    }

    /// The Session context assembly of this process.
    #[must_use]
    pub fn run_assembly(&self) -> &Arc<crate::RunAssembly> {
        &self.run_assembly
    }

    /// Session Run delivery of this process.
    #[must_use]
    pub fn session_runs(&self) -> &Arc<crate::SessionRuns> {
        &self.session_runs
    }

    /// Session root and child Run coordination of this process.
    #[must_use]
    pub fn coordination(&self) -> &Arc<crate::ChildRunCoordination> {
        &self.coordination
    }

    /// Session Outcome orchestration of this process.
    #[must_use]
    pub fn outcomes(&self) -> &Arc<crate::Outcomes> {
        &self.outcomes
    }

    /// The Session application's executor roles: each role is served by the
    /// component that owns it.
    #[must_use]
    pub fn session_execution_roles(
        &self,
    ) -> awaken_session_execution_contract::SessionExecutionRoles {
        awaken_session_execution_contract::SessionExecutionRoles {
            default_model: self.capabilities.model(),
            placement: awaken_session_execution_contract::SessionPlacementRoles::Colocated(
                awaken_session_execution_contract::ColocatedRealizationRoles {
                    mcp: self.run_assembly.clone(),
                    realization: self.run_assembly.clone(),
                    realization_effects: self.realization_executor.clone(),
                    environment: self.run_assembly.clone(),
                    terminal_cleanup: self.run_assembly.clone(),
                },
            ),
            coordination: self.coordination.clone(),
            coordination_reads: self.session_runs.threads.clone(),
            budget: self.budget.clone(),
            streaming_runs: self.session_runs.clone(),
            thread_reads: self.thread_reads.clone(),
            outcomes: self.outcomes.clone(),
            live_inbox: self.live_inboxes.clone(),
            capability: self.capabilities.clone(),
        }
    }

    /// Session usage and budget accounting of this process.
    #[must_use]
    pub fn budget(&self) -> &Arc<awaken_session_run_delivery::SessionBudgets> {
        &self.budget
    }

    /// Committed Thread readback over this process's selected authority.
    #[must_use]
    pub fn thread_reads(&self) -> &Arc<crate::CommittedReads> {
        &self.thread_reads
    }

    /// The live inboxes of the attempts this process currently owns.
    #[must_use]
    pub fn live_inboxes(&self) -> &Arc<crate::LiveInboxes> {
        &self.live_inboxes
    }

    /// The shared per-thread live observation hub.
    pub fn hub(&self) -> &Arc<ThreadEventHub> {
        &self.hub
    }
}

/// The budget accounting of the Sessions `runs` delivers.
fn session_budgets(runs: &crate::SessionRuns) -> awaken_session_run_delivery::SessionBudgets {
    awaken_session_run_delivery::SessionBudgets::new(
        runs.threads.clone(),
        runs.thread_reads.clone(),
    )
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[tokio::test]
    async fn explicit_extraction_repository_is_the_initial_authority() {
        // Item-order maintenance changes no constructor or selected handle:
        // cfg(test) + a production item after this module triggers Clippy;
        // keeping the unchanged helper above it admits all-target validation.
        // The T1-T3 and R0-R3 cases retain runtime construction coverage.
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
        let host = (SessionExecutorBuilder::for_coordinator(
            Arc::new(crate::NoModelConfiguredExecutor),
            awaken_dispatch_execution::session_dispatch::UNCONFIGURED_MODEL_REF,
            {
                let memory = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
                    awaken_time::system_clock(),
                ));
                crate::WorkerContentAdapters {
                    file_content_source: Arc::new(
                        awaken_resource_contract::UnavailableFileContentSource,
                    ),
                    memory: awaken_resource_contract::MemoryExecutionContent::new(memory),
                }
            },
            expected.clone(),
            crate::SessionExecutionProcess::from_deployment(
                &awaken_deployment_config::DeploymentConfig::ephemeral(),
                awaken_deployment_config::mint_local_workspace(),
                awaken_time::system_clock(),
            )
            .expect("ephemeral process"),
            Arc::new(
                awaken_session_executor_testkit::volatile_reference_authority(
                    awaken_time::system_clock(),
                ),
            ),
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
        let memory = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
            awaken_time::system_clock(),
        ));
        let extraction: Arc<dyn awaken_ext_memory::MemoryExtractionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .expect("test extraction repository"),
        );
        let process = || {
            crate::SessionExecutionProcess::from_deployment(
                &awaken_deployment_config::DeploymentConfig::ephemeral(),
                awaken_deployment_config::mint_local_workspace(),
                awaken_time::system_clock(),
            )
            .expect("ephemeral process")
        };
        assert!(
            SessionExecutorBuilder::for_worker(
                Arc::new(crate::NoModelConfiguredExecutor),
                "stub",
                {
                    let memory = memory.clone();
                    crate::WorkerContentAdapters {
                        file_content_source: file.clone(),
                        memory: awaken_resource_contract::MemoryExecutionContent::new(memory),
                    }
                },
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
        let worker = (SessionExecutorBuilder::for_worker(
            Arc::new(crate::NoModelConfiguredExecutor),
            "stub",
            {
                let memory = memory.clone();
                crate::WorkerContentAdapters {
                    file_content_source: file.clone(),
                    memory: awaken_resource_contract::MemoryExecutionContent::new(memory),
                }
            },
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
        let coordinator = (SessionExecutorBuilder::for_coordinator(
            Arc::new(crate::NoModelConfiguredExecutor),
            "stub",
            {
                let memory = memory;
                crate::WorkerContentAdapters {
                    file_content_source: file,
                    memory: awaken_resource_contract::MemoryExecutionContent::new(memory),
                }
            },
            extraction,
            process(),
            Arc::new(
                awaken_session_executor_testkit::volatile_reference_authority(
                    awaken_time::system_clock(),
                ),
            ),
        ))
        .build()
        .expect("validated Host dependencies");
        assert!(
            matches!(
                worker
                    .run_assembly
                    .authority
                    .open_for_read("role")
                    .await
                    .as_deref(),
                Ok(awaken_session_run_delivery::SessionCommit::Remote(_))
            ),
            "R1 remote commit boundary"
        );
        assert!(
            worker
                .run_assembly
                .authority
                .claimed_commit()
                .unwrap()
                .is_some(),
            "R1 selected required port"
        );
        assert!(
            coordinator
                .run_assembly
                .authority
                .open_for_read("role")
                .await
                .is_ok(),
            "R2 selected local commit boundary"
        );
        assert!(
            coordinator
                .run_assembly
                .authority
                .claimed_commit()
                .unwrap()
                .is_none(),
            "R2"
        );
        let worker_dependencies = SessionExecutorBuilder::for_worker(
            Arc::new(crate::NoModelConfiguredExecutor),
            "stub",
            {
                let memory = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
                    awaken_time::system_clock(),
                ));
                crate::WorkerContentAdapters {
                    file_content_source: Arc::new(
                        awaken_resource_contract::UnavailableFileContentSource,
                    ),
                    memory: awaken_resource_contract::MemoryExecutionContent::new(memory),
                }
            },
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
                .with_runtime_authority(Arc::new(
                    awaken_session_executor_testkit::volatile_reference_authority(
                        awaken_time::system_clock()
                    ),
                ))
                .is_err(),
            "R3 Worker authority cannot become local",
        );
    }
}
