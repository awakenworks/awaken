//! Builder / configuration wiring for [`SharedHost`]: `new`, the chainable
//! `with_*` methods, per-thread registration, and the process dispatch pool.

use super::*;
use awaken_runtime_contract::delegation::{RemoteAgent, RunDelegationService};

/// Backend-neutral resource ports selected atomically by an outer composition
/// root. This is a wiring value, not a resource aggregate or authorization
/// context; it contains no principal, credential, role, policy, or PDP result.
#[derive(Clone)]
pub struct ResourcePlanePorts {
    file_store: Arc<dyn awaken_file_store::FileStore>,
    memory_repository: Arc<dyn awaken_memory_store::MemoryRepository>,
    skill_store: Arc<dyn awaken_skill_store::SkillStore>,
    lifecycle: Arc<dyn awaken_protocol_managed::resource_plane::ResourceLifecycleRepository>,
}

/// The four backend-neutral ports exposed when a composition root needs to
/// mount the resource APIs beside the runtime host.
pub type ResourcePlanePortSet = (
    Arc<dyn awaken_file_store::FileStore>,
    Arc<dyn awaken_memory_store::MemoryRepository>,
    Arc<dyn awaken_skill_store::SkillStore>,
    Arc<dyn awaken_protocol_managed::resource_plane::ResourceLifecycleRepository>,
);

impl ResourcePlanePorts {
    pub fn new(
        file_store: Arc<dyn awaken_file_store::FileStore>,
        memory_repository: Arc<dyn awaken_memory_store::MemoryRepository>,
        skill_store: Arc<dyn awaken_skill_store::SkillStore>,
        lifecycle: Arc<dyn awaken_protocol_managed::resource_plane::ResourceLifecycleRepository>,
    ) -> Self {
        Self {
            file_store,
            memory_repository,
            skill_store,
            lifecycle,
        }
    }

    /// Decompose the wiring value at an outer composition root that also mounts
    /// the resource APIs. All returned ports still refer to the same opened family.
    pub fn into_parts(self) -> ResourcePlanePortSet {
        (
            self.file_store,
            self.memory_repository,
            self.skill_store,
            self.lifecycle,
        )
    }
}

impl SharedHost {
    /// Resolve or provision the stable local workspace coordinate owned by this
    /// installation. Composition roots call this once and pass the value to every
    /// resource adapter they assemble.
    pub fn provision_local_workspace() -> String {
        let deployment = crate::deployment_config::DeploymentConfig::from_env();
        let fallback = std::env::var("AWAKEN_MGMT_DIR")
            .ok()
            .filter(|dir| !dir.trim().is_empty())
            .map(PathBuf::from);
        resolve_local_workspace(deployment.storage_dir.as_deref().or(fallback.as_deref()))
    }

    /// Explicit-root variant for embedders/tests that do not configure through
    /// process environment.
    pub fn provision_local_workspace_at(root: &std::path::Path) -> String {
        resolve_local_workspace(Some(root))
    }

    /// A host over `llm`. Configure it with the chainable `with_*` builders
    /// (client tools, delegates, a judge grader, a durable store).
    pub fn new(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Self {
        Self::build(llm, model_ref.into(), None)
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
    /// process-global `AWAKEN_INGRESS` environment variable to activate a transport
    /// they supplied directly.
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
            || crate::dispatch_backend::shared_durable_store(self.store_dir.as_deref()),
            Ok,
        )
    }

    /// Construct with an already selected resource persistence family. Unlike
    /// post-construction overrides, this never opens node-local resource stores
    /// before installing shared adapters, so there is no unused second truth.
    pub fn new_with_resource_plane(
        llm: Arc<dyn LlmExecutor>,
        model_ref: impl Into<String>,
        resources: ResourcePlanePorts,
    ) -> Self {
        Self::build(llm, model_ref.into(), Some(resources))
    }

    fn build(
        llm: Arc<dyn LlmExecutor>,
        model_ref: String,
        resources: Option<ResourcePlanePorts>,
    ) -> Self {
        // Composition root: the deployment axes are parsed once from the environment
        // into one typed config. `AWAKEN_STORAGE_DIR` set → durable SQLite commit
        // and resource adapters (all survive a restart); unset → ephemeral adapters.
        let deployment = crate::deployment_config::DeploymentConfig::from_env();
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
        let memory_stores = resources.as_ref().map_or_else(
            || crate::memory_stores::MemoryStores::open(store_dir.as_deref()),
            |ports| {
                crate::memory_stores::MemoryStores::with_repository(ports.memory_repository.clone())
            },
        );
        let memory_catalog = Arc::new(AgentCatalog::new().with_agent(default_memory_agent(
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                awaken_runtime_contract::resolved::ModelBinding::new(
                    "default", &model_ref, "default",
                ),
            ),
            DEFAULT_MEMORY_INSTRUCTIONS,
        )));
        let extraction_repository: Arc<dyn awaken_protocol_managed::MemoryExtractionRepository> =
            match store_dir.as_ref() {
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
            };
        let memory = Arc::new(crate::memory::MemoryRuntime::new(
            llm.clone(),
            Arc::new(LocalProvider::new(sub_base("mem"))),
            memory_catalog,
            Arc::new(BackgroundRuns::new()),
            extraction_repository,
        ));
        let memory_selector = Some(Arc::new(crate::memory::AgentSelector::new(
            llm.clone(),
            &model_ref,
        )) as Arc<dyn awaken_ext_memory::RecallSelector>);
        let mut skills = crate::skill_catalog::SkillCatalog::new();
        if let Some(ports) = &resources {
            skills.set_store(ports.skill_store.clone());
        }
        let file_store = resources.as_ref().map_or_else(
            || match store_dir.as_ref() {
                Some(dir) => Arc::new(
                    awaken_file_store::sqlite::SqliteFileStore::open(
                        &dir.join("files.db").to_string_lossy(),
                    )
                    .expect("open durable file store"),
                ) as Arc<dyn awaken_file_store::FileStore>,
                None => Arc::new(awaken_file_store::InMemoryFileStore::new()),
            },
            |ports| ports.file_store.clone(),
        );
        let resource_lifecycle = resources.as_ref().map(|ports| ports.lifecycle.clone());
        #[cfg(test)]
        let resource_lifecycle =
            resource_lifecycle.or_else(|| Some(super::tests::test_resource_lifecycle()));
        let session_slots = crate::session_slot::SessionRuntimeSlots::default();
        Self {
            llm,
            model_ref,
            inference_routing: crate::inference_routing::InferenceRouting::new(
                session_slots.clone(),
            ),
            acp: None,
            remote_attempt_executor: None,
            application_attempt_decorator: None,
            application_session_provisioner: None,
            application_session_control: None,
            provider: LocalProvider::new(sandbox_root.clone()),
            session_provider: crate::session_environment::SessionEnvironmentProvider::workdir(
                sandbox_root.clone(),
            ),
            session_provider_explicit: false,
            judge_snapshot: None,
            client_tools: HashSet::new(),
            local_workspace: local_workspace.clone(),
            session_slots: session_slots.clone(),
            skills,
            remote_agents: crate::delegate::RemoteAgentDirectory::new(),
            // Subagents share the parent's sandbox by default (`默认共用`).
            skill_fork_placement: crate::skills::SkillForkPlacement::SharedSession,
            plugin_ids: Vec::new(),
            plugin_config: std::collections::BTreeMap::new(),
            hub: Arc::new(ThreadEventHub::new()),
            // `with_store_dir` still overrides this environment-derived default.
            store_dir: store_dir.clone(),
            // A shared root (e.g. a networked mount) enables cross-machine ACP
            // session recovery; unset means single-machine (stable config home).
            session_blob_root: deployment.acp_session_blob_root.clone(),
            upstream: None,
            deployment,
            memory,
            memory_selector,
            compaction: None,
            config_service: None,
            agent_publications: None,
            mcp_relay: tokio::sync::OnceCell::new(),
            dispatch_session_runtime: std::sync::RwLock::new(None),
            file_store,
            resource_lifecycle,
            memory_stores,
            memory_mounter: std::sync::RwLock::new(None),
            gate_override: None,
            dispatch_pool: std::sync::OnceLock::new(),
            dispatch_store_override: None,
            completion: Arc::new(CompletionRegistry::default()),
            hand_placement: crate::hand_placement::HandPlacement::new(),
            capture_sink: None,
            admin_tools: Vec::new(),
        }
    }

    /// Install the adapter that drives snapshot-selected remote attempts. The
    /// host depends only on the neutral attempt port; A2A construction remains a
    /// composition-root responsibility.
    pub fn with_remote_attempt_executor(
        mut self,
        executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>,
    ) -> Self {
        self.remote_attempt_executor = Some(executor);
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
        repository: Arc<dyn awaken_protocol_managed::resource_plane::ResourceLifecycleRepository>,
    ) -> Self {
        self.resource_lifecycle = Some(repository);
        self
    }

    pub fn resource_lifecycle(
        &self,
    ) -> Option<Arc<dyn awaken_protocol_managed::resource_plane::ResourceLifecycleRepository>> {
        self.resource_lifecycle.clone()
    }

    pub(crate) fn register_thread_workspace(&self, thread: &str, workspace: &str) {
        self.session_slots
            .update(thread, |slot| slot.workspace = Some(workspace.to_string()));
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

    /// Wire the subject-tagged captured-content sink (ADR-0050). A run whose
    /// resolved capture level permits content writes it here, attributed to the
    /// `AWAKEN_CONTENT_SUBJECT` on the open surface.
    #[must_use]
    pub fn with_capture_sink(
        mut self,
        sink: Arc<dyn awaken_runtime_contract::CaptureSink>,
    ) -> Self {
        self.capture_sink = Some(sink);
        self
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

    /// Make this host a database-less **Worker** of the Control Node at `url`.
    /// Attempts use the registered HTTP dispatch transport and a claim-fenced
    /// operation coordinator; the Worker holds no authoritative store.
    #[must_use]
    pub fn with_upstream(mut self, url: impl Into<String>) -> Self {
        self.upstream = Some(crate::worker_security::WorkerUpstream::new(url));
        self
    }

    /// Configure an authenticated worker upstream shared by dispatch and commit
    /// clients (for example a client carrying an mTLS identity).
    #[must_use]
    pub fn with_worker_upstream(
        mut self,
        upstream: crate::worker_security::WorkerUpstream,
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

    /// Wire the config data plane, so a session's agent resolves to its installed
    /// (published) config (slice A).
    pub fn with_config_service(mut self, service: Arc<crate::config_plane::ConfigService>) -> Self {
        self.agent_publications = Some(service.clone());
        self.config_service = Some(service);
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
        if let Some(mounter) = self.memory_mounter() {
            self.session_provider.install_memory_mounter(mounter);
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
        self.session_blob_root = Some(dir.into());
        self
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
        self.session_provider.install_secret_broker(broker);
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
        self.session_provider = crate::session_environment::SessionEnvironmentProvider::container(
            provider,
            Vec::new(),
            hand_factory,
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

    /// The set of client-executed tool ids (model-visible, host-unregistered).
    pub fn client_tools(&self) -> &HashSet<String> {
        &self.client_tools
    }

    /// The shared per-thread live observation hub.
    pub fn hub(&self) -> &Arc<ThreadEventHub> {
        &self.hub
    }

    /// Route every run's tool calls through `hand` — a remote `ToolExecutor`
    /// (ADR-0044) — instead of the in-process registry. The brain still commits
    /// the hand's returned output. `None` (the default) keeps in-process execution.
    pub fn with_remote_hand(mut self, hand: Arc<dyn ToolExecutor>) -> Self {
        self.hand_placement.set_remote_hand(hand);
        self
    }

    /// Install a hand-placement provider (ADR-0046): per run it selects the
    /// `ToolExecutor` (the in-process default or a placed hand). Takes precedence
    /// over [`with_remote_hand`] for any run it places; runs it declines (`None`)
    /// fall back to `remote_hand`/in-process. This is the seam a config-driven
    /// self-hosted brain–hand split — or a host's own richer policy — plugs into.
    /// Provider failure aborts run setup; only an explicit `Ok(None)` permits the
    /// configured remote-hand/in-process fallback.
    pub fn with_tool_executor_provider(mut self, provider: Arc<dyn ToolExecutorProvider>) -> Self {
        self.hand_placement.set_provider(provider);
        self
    }

    /// Register a remote Agent behind the neutral [`RemoteAgent`] interface. The
    /// composition root builds the protocol adapter (e.g. an A2A delegate over an
    /// `HttpTransport`) and injects it here, so the host names no wire type.
    pub fn with_remote_agent(
        mut self,
        agent_id: impl Into<String>,
        delegate: Arc<dyn RemoteAgent>,
    ) -> Self {
        let agent_id = agent_id.into();
        self.remote_agents.add_remote(agent_id, delegate);
        self
    }

    /// Build the per-session delegation executor. `sandbox` is the calling thread's
    /// live environment: a native delegate shares it, so the parent
    /// and its native child collaborate in one Session-owned workspace.
    pub(crate) fn run_delegation(
        &self,
        thread: &str,
        sandbox: Arc<crate::session_environment::SessionEnvironment>,
        commit: Arc<crate::store::HostCommit>,
        allowed_targets: HashSet<awaken_runtime_contract::snapshot::AgentId>,
    ) -> Result<Option<Arc<dyn RunDelegationService>>, HostError> {
        if allowed_targets.is_empty() {
            return Ok(None);
        }
        let remote_agents = self.remote_agents.clone();
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
                owner: crate::dispatch_backend::dispatch_owner(),
                claimed_commit,
                recovery_projection,
                session_resources: self.thread_resource_manifest(thread),
            })
        } else {
            None
        };
        let service = HostRunDelegationService::new(
            self.llm.clone(),
            sandbox,
            allowed_targets,
            remote_agents,
        )
        .with_publications(
            self.agent_publications.clone(),
            self.thread_workspace(thread),
        )
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
                crate::dispatch_backend::dispatch_owner(),
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
                crate::dispatch_backend::dispatch_owner(),
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
    if let Ok(configured) = std::env::var("AWAKEN_LOCAL_WORKSPACE_ID")
        && !configured.trim().is_empty()
    {
        return configured;
    }
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
