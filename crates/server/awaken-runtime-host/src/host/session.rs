//! Session/context management for [`SharedHost`]: building a thread's commit
//! boundary, stream-checkpoint store, run-delivery ingress, and `ctx_for`.

use super::*;

impl SharedHost {
    /// Build a thread's commit boundary under the configured store directory: a
    /// durable SQLite database (default) or the filesystem append-log backend when
    /// `AWAKEN_STORE=fs`, or an in-memory coordinator when no store dir is set.
    async fn build_commit(&self, thread: &str) -> Result<HostCommit, HostError> {
        use crate::store::{CommitPlan, plan_commit};
        // The backend-selection decision is pure config (see `plan_commit`): worker
        // upstream wins first, then the shared Postgres backend, then the on-disk
        // fs/sqlite layout. This match only performs the resulting I/O.
        match plan_commit(
            self.deployment.store,
            self.store_dir.as_deref(),
            self.upstream.as_deref(),
            thread,
        ) {
            // Database-less worker: every thread commits to the cell server's ingest.
            CommitPlan::Remote(url) => Ok(HostCommit::Remote(
                crate::commit_ingest::RemoteCoordinator::new(url),
            )),
            // Shared Postgres commit backend (ADR-0022 D6): one coordinator keyed by
            // thread, connected once at startup (the non-Send sqlx connect stays out of
            // the run loop). Fails closed when uninitialised, independent of a store dir.
            CommitPlan::Postgres => crate::store::postgres_commit_or_err(),
            CommitPlan::Memory => Ok(HostCommit::Local(std::sync::Arc::new(
                MemoryCommitCoordinator::new(),
            ))),
            // Fail closed: an explicit fs backend with no storage dir would otherwise
            // silently degrade to an ephemeral in-memory store and drop committed
            // history on restart (the filesystem append-log has no in-memory form).
            CommitPlan::FsNeedsStorageDir => Err(HostError::internal(
                "AWAKEN_STORE=fs requires AWAKEN_STORAGE_DIR: the filesystem append-log \
                 backend has no in-memory form, so serving it without a storage dir would \
                 silently use an ephemeral store and drop committed history on restart. \
                 Refusing to serve a durable 'fs' store on a volatile backing.",
            )),
            CommitPlan::Fs(thread_dir) => {
                if let Some(parent) = thread_dir.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| HostError::internal(e.to_string()))?;
                }
                let fs = FsCommitCoordinator::open(&thread_dir)
                    .await
                    .map_err(|e| HostError::internal(e.to_string()))?;
                Ok(HostCommit::Local(std::sync::Arc::new(fs)))
            }
            CommitPlan::Sqlite(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| HostError::internal(e.to_string()))?;
                }
                let sqlite = SqliteCommitCoordinator::open(&path.to_string_lossy())
                    .map_err(|e| HostError::internal(e.to_string()))?;
                Ok(HostCommit::Local(std::sync::Arc::new(sqlite)))
            }
        }
    }

    /// Build a thread's interrupted-stream checkpoint store, mirroring
    /// `build_commit`'s durability choice: a filesystem store under the configured
    /// directory (so a partial survives a process crash and resumes), or an
    /// in-memory store when no store dir is set. Always filesystem when durable —
    /// the checkpoint is a small `run_id`-keyed blob, so it needs no SQLite/fs
    /// backend axis; it simply follows the commit boundary's durability.
    fn build_stream_checkpoint(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn StreamCheckpointStore>, HostError> {
        let Some(dir) = &self.store_dir else {
            return Ok(Arc::new(MemoryStreamCheckpointStore::new()));
        };
        let checkpoint_dir = dir.join(sanitize_thread(thread)).join("stream-checkpoints");
        let store = FsStreamCheckpointStore::open(&checkpoint_dir)
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(Arc::new(store))
    }

    /// Build a thread's run-delivery ingress. Default is direct in-process
    /// execution (`DirectRunIngress`, slice C). With `AWAKEN_INGRESS=durable` the
    /// turn is delivered through a `DurableRunIngress`: every accepted run is
    /// persisted to a dispatch queue before it executes (so it survives a crash),
    /// and on session (re)build any dispatch a prior process crashed on is
    /// recovered (slice D). The durable ingress shares this thread's `runtime` and
    /// `commit`, so execution and committed truth are identical to the direct path
    /// (G6) — only the delivery guarantee differs. Returns the ingress plus the
    /// flag that tells `run` to submit through the durable (queued) path.
    async fn build_ingress(
        &self,
        runtime: Arc<Runtime>,
        commit: Arc<HostCommit>,
        stream_checkpoint: Arc<dyn StreamCheckpointStore>,
    ) -> Result<
        (
            Arc<dyn RunIngress>,
            Option<Arc<DurableRunIngress<AnyDispatchStore>>>,
        ),
        HostError,
    > {
        if !self.deployment.durable {
            return Ok((Arc::new(DirectRunIngress::new(runtime)), None));
        }
        // The ONE process-shared dispatch queue (shared SQLite file, or the shared
        // Postgres pool for a fleet) plus this process's unique claim owner — both
        // live in `dispatch_backend`, which owns backend selection (ADR-0019/0024).
        // Every session's worker shares this queue; the process-level `DispatchPool`
        // is its sole claimer and routes each run back to its owning session.
        let store = crate::dispatch_backend::shared_durable_store(self.store_dir.as_deref())?;
        // Carry the host's ExecutorProvider into the worker as a neutral model→executor
        // closure, so a worker-driven run resolves its own configured model per attempt
        // (R1). `None` when no provider is installed — the worker stays on the host
        // default.
        let model_resolver = self.worker_model_resolver();
        // The recovered dispatch a crash left mid-flight is re-executed by this
        // worker; giving it the same checkpoint store lets that re-execution resume
        // the interrupted step from its flushed partial (Phase 3 cross-process).
        let ingress = Arc::new(DurableRunIngress::with_owner_and_resolver(
            runtime,
            store,
            commit,
            crate::dispatch_backend::dispatch_owner(),
            Some(stream_checkpoint),
            model_resolver,
        ));
        // No per-session recovery sweep here: this session's worker shares one queue
        // with every other, so a claim would grab foreign threads' runs. The
        // process-level `DispatchPool` owns recovery — it claims each crashed run and
        // routes it to the session (this one included) that owns its thread.
        let boxed: Arc<dyn RunIngress> = ingress.clone();
        Ok((boxed, Some(ingress)))
    }

    /// Wrap this host's `ExecutorProvider` (if installed) into the neutral
    /// `model_ref → executor` closure a worker's `RunExecutionContext` carries, so a
    /// database-less worker resolves the run's configured model per attempt (R1).
    /// `None` when no provider is installed — the worker stays on the runtime's bound
    /// default (a single-model deployment is unaffected).
    pub(crate) fn worker_model_resolver(&self) -> Option<awaken_run_ingress::ModelResolverFn> {
        self.model_route.provider().map(|provider| {
            let resolve: awaken_run_ingress::ModelResolverFn =
                Arc::new(move |model_ref: &str| provider.executor_for(model_ref));
            resolve
        })
    }

    pub(crate) async fn ctx_for(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<Arc<SessionCtx>, HostError> {
        let mut sessions = self.sessions.lock().await;
        if let Some(ctx) = sessions.get(thread) {
            return Ok(ctx.clone());
        }
        let env = Arc::new(
            self.provider
                .create_sandbox(&self.sandbox_spec(thread))
                .await
                .map_err(|e| HostError::internal(e.to_string()))?,
        );
        // Clone any staged github_repository resources into the fresh sandbox,
        // host-side (ADR-0038); fail-closed so a bad repo aborts session start.
        self.provision_thread_repos(thread, &env)?;
        let thread_id = ThreadId(thread.to_string());
        let commit = Arc::new(self.build_commit(thread).await?);
        // Durable interrupted-stream checkpoints follow the commit's durability
        // (Phase 3): a mid-recovery crash resumes from the flushed partial.
        let stream_checkpoint = self.build_stream_checkpoint(thread)?;
        // This thread's staged MCP servers (ADR-0043 Phase 3), registered by the
        // managed adapter's `prepare_session` before the first turn; the wire
        // composition (connect + discover, fail closed) lives in `crate::mcp`.
        // Read, not removed, so a retry re-attempts (and re-fails) the connect.
        let staged_mcp: Vec<PreparedMcpServer> = self
            .thread_mcp
            .lock()
            .expect("thread mcp mutex poisoned")
            .get(thread)
            .cloned()
            .unwrap_or_default();
        let mcp = crate::mcp::connect_staged(&staged_mcp).await?;
        // MCP tools are pre-authorized on this thread's gate: the session creator
        // explicitly configured the server (with its credential), which is the
        // authorization decision — the ask-gate keeps covering the built-in
        // mutation tools. `server_gate_allowing(&[])` is the plain server gate,
        // so threads without MCP keep the exact default policy.
        // The management tools (ADR-0052) are pre-authorized like the MCP tools: they
        // are read-only, and only the reserved-scope assistant's config names them.
        let admin_ids: Vec<String> = self
            .admin_tools
            .iter()
            .map(|t| t.id().to_string())
            .collect();
        let pre_authorized: Vec<String> = mcp
            .tool_ids
            .iter()
            .cloned()
            .chain(admin_ids.iter().cloned())
            .collect();
        // A published agent runs with its own installed config (slice A); an
        // unknown/unpublished agent falls back to the server's built-in default.
        // Fetched here because its `plugin_config["permission"]` shapes the gate.
        let installed = self
            .config_service
            .as_ref()
            .zip(agent)
            .and_then(|(svc, agent)| svc.installed(agent));
        // An authored permission policy (the agent's `permission` config section)
        // shapes the gate: its rules layer over the built-in baseline (perception +
        // the pre-authorized MCP/admin tools) and its default governs unmatched calls.
        // A malformed/absent section → None → the strict built-in default (mutations
        // asked), so a bad policy never fails open.
        let authored_permission = config_permission_ruleset(
            installed
                .as_ref()
                .map(|c| &c.snapshot().resolved_spec.plugin_config)
                .unwrap_or(&self.plugin_config),
        );
        let apply_base_gate = !pre_authorized.is_empty() || authored_permission.is_some();
        let base_gate = server_gate_with(authored_permission, &pre_authorized);
        // R1/R2: the runtime is built with the host default executor; each run then
        // resolves its *effective* model (its `model_ref_override`, else its snapshot
        // binding) to an executor at the resolve seam and sets it on the run context.
        // Resolving per run — not once at session build — means a per-turn model
        // switch needs no session rebuild, and a database-less worker runs the
        // configured model without a session-level registry.
        let mut runtime = build_runtime(self.llm.clone(), &env);
        if apply_base_gate {
            runtime = runtime.with_gate(base_gate.clone());
        }
        // Register the discovered MCP tools; their descriptors join the advertised
        // config below so the model sees them.
        for tool in mcp.tools {
            runtime = runtime.with_tool(tool);
        }
        // Register the management tool executables globally (ADR-0052 D3): the
        // registry stays global, the compile-time scope fence is what restricts them.
        for tool in &self.admin_tools {
            runtime = runtime.with_tool(tool.clone());
        }
        let mcp_descriptors = mcp.descriptors;
        // A gate override (slice E) replaces the default authorization gate — e.g.
        // a scheduling gate that defers tool calls as `ScheduledAction`s so the
        // durable worker performs them out of band (ADR-0020).
        if let Some(gate) = &self.gate_override {
            runtime = runtime.with_gate(gate.clone());
        }
        // Delegation is a runtime concern: inject the resolver so the kernel runs
        // `agent_run` as a sub-agent (native or remote), not the tool registry.
        if let Some(resolver) = self.agent_resolver(env.clone()) {
            runtime = runtime.with_resolver(resolver);
        }
        // Skills are fronted by two stable tools (ADR-0036); all skill behavior is
        // in `awaken-ext-skills`. The host only wires the pieces it alone owns —
        // the sandbox env, the sub-run capability, and the base gate — via
        // `skills::wire_skills`.
        let mut skill_descriptors = Vec::new();
        let mut skill_registry: Option<Arc<dyn SkillRegistry>> = None;
        // Refresh the delivered-catalog snapshot from the (async) store for this
        // session; `Some` (possibly empty) exactly when a durable store is wired.
        self.reload_skill_cache().await;
        let delivered = self.has_skill_store().then(|| self.skill_cache_snapshot());
        if let Some(wiring) = crate::skills::wire_skills(
            &self.skills,
            delivered,
            env.clone(),
            self.llm.clone(),
            &self.model_ref,
            thread,
            // The MCP-aware base gate, so a skill-wrapped gate keeps the thread's
            // pre-authorized MCP tools (identical to `server_gate()` without MCP).
            base_gate.clone(),
            sub_base("skill-fork"),
            self.subagent_reuse_sandbox,
        ) {
            runtime = runtime
                .with_gate(wiring.gate)
                .with_tool(wiring.list_tool)
                .with_tool(wiring.activate_tool);
            skill_descriptors = wiring.descriptors;
            skill_registry = Some(wiring.registry);
        }
        // Memory recall is a plugin: it contributes a BeforeInference hook that
        // injects bounded recall as request-only context (never committed). Install
        // it and list its id so it is active for the run (G30).
        // Seed with host-registered plugins (e.g. the tool state machine via
        // `with_state_machine`), then append the per-run memory/compact plugins.
        let mut plugin_ids: Vec<String> = self.plugin_ids.clone();
        if let Some(mem) = &self.memory {
            let mut plugin = awaken_ext_memory::MemoryPlugin::new(mem.store(), mem.bounds());
            if let Some(selector) = &self.memory_selector {
                plugin = plugin.with_selector(selector.clone());
            }
            runtime = runtime.with_plugin(Arc::new(plugin));
            plugin_ids.push(awaken_ext_memory::MEMORY_PLUGIN_ID.to_string());
        }
        // Compaction is a plugin too: a BeforeInference hook that folds the older
        // slice into a summary and injects it request-only. The main agent runs a
        // rolling window matching the config's `keep_last`, so summarized older turns
        // leave the model view.
        let context_policy = match (&self.compact_config, &self.compact_runner) {
            (Some(config), Some(runner)) => {
                let keep_last = config.keep_last;
                let plugin = CompactPlugin::new(config.clone()).with_runner(runner.clone());
                runtime = runtime.with_plugin(Arc::new(plugin));
                plugin_ids.push(awaken_ext_compact::COMPACT_PLUGIN_ID.to_string());
                awaken_runtime_contract::resolved::ContextPolicy::KeepLast { keep_last }
            }
            _ => awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        };
        // Everything dynamically provisioned on this thread that the config must
        // advertise: the skill tools plus the discovered MCP tools.
        let mut dynamic_descriptors = skill_descriptors;
        dynamic_descriptors.extend(mcp_descriptors);
        let config = installed.unwrap_or_else(|| {
            server_config(
                &self.model_route.model_ref(thread, &self.model_ref),
                &self.client_tools,
                &self.delegates,
                &plugin_ids,
                &self.plugin_config,
                &dynamic_descriptors,
                context_policy,
            )
        });
        // D6: for an ACP run, hand the session's staged MCP servers to the CLI's own MCP
        // client via `plugin_config.acp.mcp_servers`. Whether this run executes on ACP is
        // the host's runtime registration (`AcpBackend::is_acp`), not the config's
        // `backend_ref` — the managed `server_config` stamps a fixed backend_ref, so the
        // routing decision is the only reliable signal. The credential form is the host's
        // isolation decision: a managed/sandboxed host keeps the α default (secretless
        // reference; the raw bearer never reaches the CLI), while a trusted-local host may
        // opt into β (`with_trusted_acp_mcp`) and hand the bearer inline. A native run is
        // untouched (its MCP servers are already the in-process tools connected above).
        let is_acp = self.acp.as_ref().is_some_and(|a| a.is_acp(thread));
        let config =
            crate::mcp::overlay_acp_mcp(config, &staged_mcp, is_acp, self.mcp_trusted_inline);
        // Install this session's catalog on its runtime NOW, so any node can resolve a
        // run it drives — not only the node that submitted it. The in-process path
        // installs via `prepare` on submit, but a durable run is driven by whichever
        // pool node CLAIMS it (ADR-0019), and that node calls `execute` directly (no
        // `prepare`); without a locally-installed catalog, `resolve` fails closed with
        // `NoActiveCatalog` and the claimed run strands. Idempotent — a later `prepare`
        // on the submitting node re-installs the same catalog harmlessly.
        runtime
            .install_catalog(config.install().clone())
            .map_err(|e| HostError::internal(format!("install session catalog: {e}")))?;
        // Recover the session's position from committed truth: a durable store may
        // already hold this thread's history and a parked run (e.g. after a
        // restart). `consumed_rounds` starts past any prior outcome rounds so a new
        // `define_outcome` reports only the rounds it produces.
        let mut state = SessionState {
            consumed_rounds: commit.continuation_payloads(&thread_id).len(),
            ..SessionState::default()
        };
        if let Some((run_id, _)) = commit.open_wait_for_thread(&thread_id) {
            // Prime the fresh runtime so the parked run's snapshot resolves on
            // resume — `start_run` would normally have installed it.
            runtime
                .install_for_resume(&config)
                .map_err(|e| HostError::internal(e.to_string()))?;
            state.parked = Some(run_id);
        }
        let runtime = Arc::new(runtime);
        // The foreground delivery seam (slice C/D): a turn's execution goes through
        // `RunIngress` rather than calling `runtime.start_run` directly. Direct
        // ingress runs inline on the same `runtime`; durable ingress queues the run
        // through a dispatch store first. Both share this thread's `runtime`/`commit`.
        let (ingress, durable_ingress) = self
            .build_ingress(runtime.clone(), commit.clone(), stream_checkpoint.clone())
            .await?;
        let durable = durable_ingress.is_some();
        // No per-session dispatch daemon: the process-level `DispatchPool` (spawned
        // once by `mount`) is the sole claimer of the shared queue and drives this
        // session's runs by routing claimed work back to its worker (O2).
        let ctx = Arc::new(SessionCtx {
            runtime,
            ingress,
            durable,
            durable_ingress,
            config,
            commit,
            stream_checkpoint,
            remote_hand: self.remote_hand.clone(),
            tool_executor_provider: self.tool_executor_provider.clone(),
            capture_sink: self.capture_sink.clone(),
            thread_id,
            env,
            skill_registry,
            cancel: std::sync::Mutex::new(None),
            live_inbox: std::sync::Mutex::new(crate::live_inbox::LiveInboxSlot::default()),
            state: tokio::sync::Mutex::new(state),
        });
        sessions.insert(thread.to_string(), ctx.clone());
        // Deliver the session's staged resource prompts (ADR-0038 A3a) as system
        // context on the first turn, so the model knows what it has mounted and where.
        let prompts = self.thread_resource_prompts(thread);
        if !prompts.is_empty() {
            let mut st = ctx.state.lock().await;
            for prompt in prompts {
                st.pending_system.push(prompt);
            }
        }
        Ok(ctx)
    }
}
