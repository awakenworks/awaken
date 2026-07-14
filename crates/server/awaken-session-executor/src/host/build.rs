//! Builder / configuration wiring for [`SharedHost`]: `new`, the chainable
//! `with_*` methods, per-thread registration, and the process dispatch pool.

use super::*;

impl SharedHost {
    /// A host over `llm`. Configure it with the chainable `with_*` builders
    /// (client tools, delegates, a judge grader, a durable store).
    pub fn new(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Self {
        // Composition root: the deployment axes are parsed once from the environment
        // into one typed config. `AWAKEN_STORAGE_DIR` set → durable SQLite commit
        // store + a durable memory blob store under it (both survive a restart);
        // unset → ephemeral.
        let deployment = crate::deployment_config::DeploymentConfig::from_env();
        let store_dir = deployment.storage_dir.clone();
        // The ADR-0038 memory_store family persists under the storage dir when set so
        // a harvested write-back outlives the process; otherwise a per-process temp dir
        // (unit tests / ephemeral use) keeps it in-run only.
        let memory_store_root = match &store_dir {
            Some(dir) => dir.join("memory_stores"),
            None => std::env::temp_dir().join(format!("awaken-memstore-{}", std::process::id())),
        };
        let memory_stores: Arc<dyn awaken_memory_store::MemoryBlobStore> = Arc::new(
            awaken_memory_store::FsMemoryBlobStore::open(&memory_store_root)
                .expect("open durable memory-store root"),
        );
        // ADR-0053 path-addressed memory files persist alongside, under the same
        // durability rule. Backed by the SQLite store so rename-replace and CAS are
        // **crash-atomic** (one transaction) — the plain-file backend is only no-loss
        // (a crash mid-rename can leave a transient duplicate source).
        let memory_fs: Arc<dyn awaken_memory_store::MemoryFs> = Arc::new(match &store_dir {
            Some(dir) => {
                let db = dir.join("memory_fs.db");
                awaken_memory_store::SqliteMemoryFs::open(
                    db.to_str().expect("memory-fs db path is valid UTF-8"),
                )
                .expect("open durable memory-fs sqlite store")
            }
            // No durable dir → an ephemeral in-memory database (dies with the process).
            None => awaken_memory_store::SqliteMemoryFs::open_in_memory()
                .expect("open ephemeral memory-fs sqlite store"),
        });
        Self {
            llm,
            model_ref: model_ref.into(),
            model_route: crate::model_route::ThreadModelBinding::new(),
            acp: None,
            provider: LocalSandboxProvider::new(sub_base("")),
            grader: Arc::new(KeywordGrader),
            client_tools: HashSet::new(),
            skills: Vec::new(),
            skill_store: None,
            skill_cache: std::sync::Mutex::new(Vec::new()),
            delegates: HashSet::new(),
            plugin_ids: Vec::new(),
            plugin_config: std::collections::BTreeMap::new(),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            hub: Arc::new(ThreadEventHub::new()),
            // `with_store_dir` still overrides this environment-derived default.
            store_dir,
            // A shared root (e.g. a networked mount) enables cross-machine ACP
            // session recovery; unset means single-machine (stable config home).
            session_blob_root: std::env::var("AWAKEN_ACP_SESSION_BLOBS")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            upstream: None,
            deployment,
            remote_agents: HashMap::new(),
            memory: None,
            memory_selector: None,
            compact_config: None,
            compact_runner: None,
            config_service: None,
            thread_mcp: std::sync::Mutex::new(HashMap::new()),
            thread_resources: std::sync::Mutex::new(HashMap::new()),
            thread_egress: crate::sandbox_source::ThreadEgress::new(),
            file_store: Arc::new(InMemoryFileStore::new()),
            memory_stores,
            memory_fs,
            gate_override: None,
            dispatch_pool: std::sync::OnceLock::new(),
            completion: Arc::new(CompletionRegistry::default()),
            remote_hand: None,
            tool_executor_provider: None,
            gateway_executor_factory: None,
            capture_sink: None,
            admin_tools: Vec::new(),
        }
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
        self.compact_config = Some(config);
        self.compact_runner = Some(build_compact_runner(self.llm.clone(), &self.model_ref));
        self
    }

    /// Enable out-of-band memory extraction, writing memories under `mem_dir`. After
    /// each turn that reaches a natural end, a background `memory-extractor` sub-agent
    /// reads the conversation and saves durable memories via `write_memory` (scoped
    /// to `mem_dir`), without blocking the turn. The extractor runs the default
    /// memory agent over this host's model; drain it before shutdown with
    /// [`drain_memory`](Self::drain_memory).
    /// Make this host a database-less **worker** of the cell server at `url`: every
    /// thread's commit posts facts to the server's commit ingest instead of a local
    /// store (paired with an `HttpDispatchQueue` for claim/settle). The worker holds
    /// no store; the server stays the single writer.
    #[must_use]
    pub fn with_upstream(mut self, url: impl Into<String>) -> Self {
        self.upstream = Some(url.into());
        self
    }

    pub fn with_memory(mut self, mem_dir: impl Into<PathBuf>) -> Self {
        let catalog = Arc::new(AgentCatalog::new().with_agent(default_memory_agent(
            &self.model_ref,
            DEFAULT_MEMORY_INSTRUCTIONS,
        )));
        let extraction = MemoryExtraction::new(
            self.llm.clone(),
            Arc::new(LocalSandboxProvider::new(sub_base("mem"))),
            catalog,
            Arc::new(BackgroundRuns::new()),
            mem_dir.into(),
        );
        self.memory = Some(Arc::new(extraction));
        // The recall plugin uses this selector once the store grows: a single-step
        // `memory-selector` sub-agent picks the memories relevant to the user's
        // message.
        self.memory_selector = Some(Arc::new(crate::memory::AgentSelector::new(
            self.llm.clone(),
            &self.model_ref,
        )));
        self
    }

    /// Await in-flight background memory extractions up to `timeout` (shutdown
    /// flush). Returns `true` if all finished. A no-op returning `true` when memory
    /// is disabled.
    pub async fn drain_memory(&self, timeout: std::time::Duration) -> bool {
        match &self.memory {
            Some(mem) => mem.drain(timeout).await,
            None => true,
        }
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
    /// a call parks and the client supplies the result.
    pub fn with_client_tools(mut self, client_tools: HashSet<String>) -> Self {
        self.client_tools.extend(client_tools);
        self
    }

    /// Add local delegate agents callable via `agent_run`.
    pub fn with_delegates(mut self, delegates: HashSet<String>) -> Self {
        self.delegates.extend(delegates);
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
        self.skills.extend(skills);
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
        self.skill_store = Some(Arc::new(store));
        self
    }

    /// Back the delivered skill catalog with an arbitrary [`SkillStore`] backend
    /// (e.g. `PgSkillStore` for a multi-node deployment). Sibling of
    /// [`with_skill_store`](Self::with_skill_store), which wires the filesystem one.
    pub fn with_skill_store_backend(
        mut self,
        store: Arc<dyn awaken_skill_store::SkillStore>,
    ) -> Self {
        self.skill_store = Some(store);
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
        let runner = Arc::new(HostSubagentRunner {
            llm: self.llm.clone(),
            provider: LocalSandboxProvider::new(sub_base("judge")),
            catalog,
            seq: AtomicU64::new(0),
        });
        self.grader = Arc::new(DelegateGrader::new(runner, id));
        self
    }

    /// Persist every thread's committed truth to a durable SQLite database under
    /// `dir` (one file per thread). A run parked on a thread survives a restart:
    /// a host rebuilt over the same directory recovers the parked position and can
    /// resume it. Without this, sessions are in-memory and lost on restart.
    pub fn with_store_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.store_dir = Some(dir.into());
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

    /// Install the model→executor resolver (R1). Without one, every thread uses `llm`.
    pub fn with_executor_provider(
        mut self,
        provider: Arc<dyn crate::model_route::ExecutorProvider>,
    ) -> Self {
        self.model_route.set_provider(provider);
        self
    }

    /// Bind `model_ref` to `thread` (R2/R5), staged before its first turn.
    pub fn register_thread_model(&self, thread: &str, model_ref: impl Into<String>) {
        self.model_route.register(thread, model_ref);
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
        self.remote_hand = Some(hand);
        self
    }

    /// Install a hand-placement provider (ADR-0046): per run it selects the
    /// `ToolExecutor` (the in-process default or a placed hand). Takes precedence
    /// over [`with_remote_hand`] for any run it places; runs it declines (`None`)
    /// fall back to `remote_hand`/in-process. This is the seam a config-driven
    /// self-hosted brain–hand split — or a host's own richer policy — plugs into.
    pub fn with_tool_executor_provider(mut self, provider: Arc<dyn ToolExecutorProvider>) -> Self {
        self.tool_executor_provider = Some(provider);
        self
    }

    /// Install the cloud-managed-gateway egress builder (ADR-0004). With it, a run
    /// carrying a `ModelAccessGrant::CloudManagedGateway` is honored natively: the
    /// grant is materialized and `factory` builds the executor that dials the gateway
    /// with the lease token (the real provider credential is injected at the gateway,
    /// out of this process). Without it, a gateway grant fails closed on the native
    /// path. This is the seam a secretless worker — or the closed awaken-cloud layer —
    /// plugs its provider stack into; the host stays provider-agnostic.
    pub fn with_gateway_executor_factory(
        mut self,
        factory: Arc<dyn crate::gateway_executor::GatewayExecutorFactory>,
    ) -> Self {
        self.gateway_executor_factory = Some(factory);
        self
    }

    pub fn with_remote_a2a(
        mut self,
        agent_id: impl Into<String>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        let agent_id = agent_id.into();
        self.delegates.insert(agent_id.clone());
        self.remote_agents.insert(agent_id, transport);
        self
    }

    /// Build the delegation resolver from the configured roster and remotes, or
    /// `None` when the host has no delegates. Injected into each thread's runtime.
    pub(crate) fn agent_resolver(&self) -> Option<Arc<dyn AgentResolver>> {
        if self.delegates.is_empty() {
            return None;
        }
        // Native delegates are the roster ids that are not remotes.
        let native: HashSet<String> = self
            .delegates
            .iter()
            .filter(|id| !self.remote_agents.contains_key(*id))
            .cloned()
            .collect();
        Some(Arc::new(DelegationResolver::new(
            self.llm.clone(),
            self.model_ref.clone(),
            LocalSandboxProvider::new(sub_base("deleg")),
            native,
            self.remote_agents.clone(),
        )))
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
}
