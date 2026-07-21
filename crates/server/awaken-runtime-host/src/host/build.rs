//! Builder / configuration wiring for [`SharedHost`]: `new`, the chainable
//! `with_*` methods, per-thread registration, and the process dispatch pool.

use super::*;
use awaken_runtime_contract::delegation::{RemoteAgent, RunDelegationService};

/// Backend-neutral resource ports selected atomically by an outer composition
/// root. This is a wiring value, not a resource aggregate or authorization
/// context; it contains no principal, credential, role, policy, or PDP result.
pub struct ResourcePlanePorts {
    file_store: Arc<dyn awaken_file_store::FileStore>,
    memory_repository: Arc<dyn awaken_memory_store::MemoryRepository>,
    skill_store: Arc<dyn awaken_skill_store::SkillStore>,
    lifecycle: Arc<dyn awaken_protocol_managed::resource_plane::ResourceLifecycleRepository>,
}

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
            &model_ref,
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
                None => Arc::new(awaken_session_store::InMemorySessionRepository::default()),
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
        let mut skills = crate::skill_catalog::SkillCatalog::new(local_workspace.clone());
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
        let resource_lifecycle = resources.as_ref().map_or_else(
            || {
                Arc::new(crate::resource_lifecycle::EphemeralResourceLifecycle::default())
                    as Arc<dyn awaken_protocol_managed::resource_plane::ResourceLifecycleRepository>
            },
            |ports| ports.lifecycle.clone(),
        );
        Self {
            llm,
            model_ref,
            inference_routing: crate::inference_routing::InferenceRouting::new(),
            acp: None,
            provider: LocalProvider::new(sandbox_root),
            grader: Arc::new(KeywordGrader),
            client_tools: HashSet::new(),
            local_workspace: local_workspace.clone(),
            thread_workspaces: std::sync::Mutex::new(HashMap::new()),
            skills,
            delegates: crate::delegate::Delegates::new(),
            // Subagents share the parent's sandbox by default (`默认共用`).
            agent_run_reuse_sandbox: true,
            plugin_ids: Vec::new(),
            plugin_config: std::collections::BTreeMap::new(),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            hub: Arc::new(ThreadEventHub::new()),
            // `with_store_dir` still overrides this environment-derived default.
            store_dir: store_dir.clone(),
            // A shared root (e.g. a networked mount) enables cross-machine ACP
            // session recovery; unset means single-machine (stable config home).
            session_blob_root: std::env::var("AWAKEN_ACP_SESSION_BLOBS")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            upstream: None,
            deployment,
            memory,
            thread_memory: std::sync::Mutex::new(HashMap::new()),
            memory_selector,
            compaction: None,
            config_service: None,
            thread_mcp: std::sync::Mutex::new(HashMap::new()),
            thread_skills: std::sync::Mutex::new(HashMap::new()),
            mcp_relay: tokio::sync::OnceCell::new(),
            thread_resources: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            thread_egress: crate::sandbox_source::ThreadEgress::new(),
            thread_sandbox: crate::sandbox_source::ThreadSandbox::new(),
            file_store,
            resource_lifecycle,
            memory_stores,
            memory_mounter: std::sync::RwLock::new(None),
            gate_override: None,
            dispatch_pool: std::sync::OnceLock::new(),
            completion: Arc::new(CompletionRegistry::default()),
            hand_placement: crate::hand_placement::HandPlacement::new(),
            capture_sink: None,
            admin_tools: Vec::new(),
            // Default α: never hand a raw MCP bearer to the CLI — a trusted-local
            // deployment opts into β with `with_trusted_acp_mcp`.
            mcp_trusted_inline: false,
        }
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
        self.local_workspace = workspace.clone();
        self.skills.set_local_workspace(workspace);
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

    /// Replace the ephemeral resource-lifecycle adapter. Composition roots use
    /// this for SQLite/Postgres/cloud implementations; the runtime sees only the
    /// resource contract and remains independent of IAM and storage technology.
    #[must_use]
    pub fn with_resource_lifecycle(
        mut self,
        repository: Arc<dyn awaken_protocol_managed::resource_plane::ResourceLifecycleRepository>,
    ) -> Self {
        self.resource_lifecycle = repository;
        self
    }

    pub fn resource_lifecycle(
        &self,
    ) -> Arc<dyn awaken_protocol_managed::resource_plane::ResourceLifecycleRepository> {
        self.resource_lifecycle.clone()
    }

    pub(crate) fn register_thread_workspace(&self, thread: &str, workspace: &str) {
        self.thread_workspaces
            .lock()
            .expect("thread workspaces")
            .insert(thread.to_string(), workspace.to_string());
    }

    pub(crate) fn thread_workspace(&self, thread: &str) -> String {
        self.thread_workspaces
            .lock()
            .expect("thread workspaces")
            .get(thread)
            .cloned()
            .unwrap_or_else(|| self.local_workspace.clone())
    }

    /// Workspace for a prepared thread; unlike `thread_workspace`, unknown ids
    /// do not inherit local ownership and therefore cannot enumerate artifacts.
    pub fn registered_thread_workspace(&self, thread: &str) -> Option<String> {
        self.thread_workspaces
            .lock()
            .expect("thread workspaces")
            .get(thread)
            .cloned()
    }

    /// Whether this process owns a co-located dispatch pool. Composition roots use
    /// the same parsed deployment value that admission uses, so the process cannot
    /// accidentally both advertise coordinator-only behavior and drain locally.
    #[must_use]
    pub fn runs_local_dispatch_pool(&self) -> bool {
        !self.deployment.disable_local_pool
    }

    /// Opt this host into **β** (trusted-inline) MCP credential delivery for its ACP
    /// runs: a staged server's raw bearer is handed to the CLI inline instead of as a
    /// secretless α reference. Only sound when the CLI is a trusted-local (non-sandboxed)
    /// process — the host owns the isolation decision. Default (unset) is α.
    #[must_use]
    pub fn with_trusted_acp_mcp(mut self, trusted: bool) -> Self {
        self.mcp_trusted_inline = trusted;
        self
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
    /// the summary as request-only context; the main agent runs a matching `KeepLast`
    /// window so those older raw turns drop from the model view. Non-destructive:
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
    /// message count. The main agent still runs a matching `KeepLast(keep_last)`.
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
        let agent_tool = build_compact_runner(self.llm.clone(), &self.model_ref);
        self.compaction = Some(crate::compact::Compaction { config, agent_tool });
        self
    }

    /// Make this host a database-less **worker** of the cell server at `url`: every
    /// thread's commit posts facts to the server's commit ingest instead of a local
    /// store (paired with an `HttpDispatchQueue` for claim/settle). The worker holds
    /// no store; the server stays the single writer.
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

    /// Await in-flight background memory extractions up to `timeout` (shutdown
    /// flush). Returns `true` if all finished.
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
        self.config_service = Some(service);
        self
    }

    /// Add client-executed tools: those ids are model-visible but unregistered, so
    /// a call awaits and the client supplies the result.
    pub fn with_client_tools(mut self, client_tools: HashSet<String>) -> Self {
        self.client_tools.extend(client_tools);
        self
    }

    /// Add local delegate agents callable via `agent_run`.
    pub fn with_delegates(mut self, delegates: HashSet<String>) -> Self {
        self.delegates.add_local(delegates);
        self
    }

    /// Configure the delegation roster owned by a locally runnable Agent.
    /// A child Run uses this roster exactly as the same Agent would when started
    /// directly; delegation never copies the initiating Agent's capabilities.
    pub fn with_agent_delegates(
        mut self,
        agent_id: impl Into<String>,
        delegates: HashSet<String>,
    ) -> Self {
        self.delegates.set_agent_roster(agent_id.into(), delegates);
        self
    }

    /// Whether native subagents (delegation / skill fork) reuse the parent agent's
    /// sandbox (`true`, the default) or run in a fresh, isolated one. Housekeeping
    /// sub-runs (judge / memory / compaction) stay isolated regardless.
    pub fn with_agent_run_reuse_sandbox(mut self, reuse: bool) -> Self {
        self.agent_run_reuse_sandbox = reuse;
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

    /// Grade outcomes with a real judge sub-agent (`judge_agent_id`) run through the
    /// kernel, instead of the deterministic keyword grader. The judge grades in its
    /// own fresh context.
    pub fn with_judge(mut self, judge_agent_id: impl Into<String>) -> Self {
        let id = judge_agent_id.into();
        let catalog = Arc::new(AgentCatalog::new().with_agent(default_judge_agent(
            &self.model_ref,
            &id,
            DEFAULT_JUDGE_INSTRUCTIONS,
        )));
        let agent_tool = Arc::new(HostAgentTool {
            llm: self.llm.clone(),
            provider: LocalProvider::new(sub_base("judge")),
            catalog,
            seq: AtomicU64::new(0),
        });
        self.grader = Arc::new(AgentToolGrader::new(agent_tool, id));
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

    /// Bind `model_ref` to `thread` (R2/R5), staged before its first turn.
    pub fn register_thread_model(&self, thread: &str, model_ref: impl Into<String>) {
        self.inference_routing.register(thread, model_ref);
    }

    /// Deny network egress for `thread`'s sandbox (from its environment's networking
    /// policy), staged before its first turn and consumed by `sandbox_spec` (and by a
    /// sandboxed ACP channel source wired via [`SharedHost::thread_egress`]).
    pub fn register_thread_egress(&self, thread: &str, deny: bool) {
        self.thread_egress.set(thread, deny);
    }

    /// The shared per-thread egress-registration handle, for wiring a
    /// [`crate::SandboxChannelSource`] before `with_acp` consumes the builder.
    pub fn thread_egress(&self) -> crate::sandbox_source::ThreadEgress {
        self.thread_egress.clone()
    }

    /// Stage `thread`'s environment sandbox overlay (isolation/network/limits), from its
    /// `config.sandbox`, consumed by `sandbox_spec` and a sandboxed ACP channel source.
    pub fn register_thread_sandbox(
        &self,
        thread: &str,
        over: awaken_provisioning_contract::SandboxOverride,
    ) {
        self.thread_sandbox.set(thread, over);
    }

    /// The shared per-thread sandbox-override handle, for wiring a sandboxed/container
    /// ACP channel source (like [`Self::thread_egress`]) before the builder is consumed.
    pub fn thread_sandbox(&self) -> crate::sandbox_source::ThreadSandbox {
        self.thread_sandbox.clone()
    }

    /// Stage MCP servers for `thread`, to be connected when the thread's context
    /// is first built (its first turn) — the Managed session-create path calls
    /// this from `prepare_session`, so the credential is materialized before the
    /// session exists but the network connect happens lazily (ADR-0043 Phase 3).
    /// Re-registering replaces the thread's staged set.
    pub fn register_thread_mcp(&self, thread: &str, servers: Vec<PreparedMcpServer>) {
        self.thread_mcp
            .lock()
            .expect("thread mcp mutex poisoned")
            .insert(thread.to_string(), servers);
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

    /// Register a delegate agent fulfilled over A2A: `agent_run` calls naming it
    /// are routed to `transport` (a remote agent). The id joins the advertised
    /// roster so the model can delegate to it.
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
        self.delegates.add_remote(agent_id, delegate);
        self
    }

    /// Build the delegation executor from the configured roster and remotes, or
    /// `None` when the host has no delegates. Injected into each thread's runtime.
    /// Build the per-session delegation executor. `sandbox` is the calling thread's
    /// live sandbox: a native delegate shares it by default (`默认共用`), so the parent
    /// and its subagent collaborate in one workspace; `agent_run_reuse_sandbox = false`
    /// gives each delegate a fresh, isolated root instead.
    pub(crate) fn run_delegation(
        &self,
        sandbox: Arc<LocalSandbox>,
        commit: Arc<crate::store::HostCommit>,
    ) -> Result<Option<Arc<dyn RunDelegationService>>, HostError> {
        if self.delegates.is_empty() {
            return Ok(None);
        }
        let scheduler = if self.deployment.durable {
            Some(crate::agent_runner::RunScheduler {
                store: crate::dispatch_backend::shared_durable_store(self.store_dir.as_deref())?,
                commit: commit.clone(),
                reader: commit,
                owner: crate::dispatch_backend::dispatch_owner(),
                claimed_commit: self.upstream.as_ref().map(|upstream| {
                    let mut commit =
                        crate::commit_ingest::RemoteClaimedRunCommit::new(upstream.base_url())
                            .with_client(upstream.client().clone());
                    if let Some(identity) = upstream.worker_identity() {
                        commit = commit.with_worker_identity(identity.clone());
                    }
                    Arc::new(commit) as Arc<dyn awaken_run_ingress::ClaimedRunCommit>
                }),
            })
        } else {
            None
        };
        Ok(Some(Arc::new(HostRunDelegationService::new(
            self.llm.clone(),
            self.model_ref.clone(),
            Arc::new(LocalProvider::new(sub_base("deleg"))),
            sandbox,
            self.agent_run_reuse_sandbox,
            self.delegates.clone(),
            scheduler,
        ))))
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
        let Ok(store) = crate::dispatch_backend::shared_durable_store(self.store_dir.as_deref())
        else {
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
