//! Session/context management for [`SharedHost`]: building a thread's commit
//! boundary, stream-checkpoint store, run-delivery ingress, and `ctx_for`.

use super::*;

impl SharedHost {
    /// Evict a cached runtime context while retaining the Session workspace.
    /// Memory mount guards must be released explicitly; dropping their trait
    /// objects cannot run async unmount/harvest. When inputs changed, clear the
    /// runtime-owned `.mnt` projection as well so detach is an actual revocation.
    pub(crate) async fn evict_session_for_rebuild(
        &self,
        thread: &str,
        clear_resources: bool,
    ) -> Result<(), HostError> {
        let ctx = self.sessions.lock().await.remove(thread);
        if let Some(ctx) = ctx {
            ctx.env.release_memory_mounts().await;
            if clear_resources {
                ctx.env
                    .clear_resource_projection()
                    .map_err(|error| HostError::internal(error.to_string()))?;
            }
        }
        Ok(())
    }

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
            self.upstream.as_ref().map(|upstream| upstream.base_url()),
            thread,
        ) {
            // Database-less worker: every thread commits to the cell server's ingest.
            CommitPlan::Remote(url) => {
                let upstream = self
                    .upstream
                    .as_ref()
                    .expect("remote commit plan has an upstream");
                Ok(HostCommit::Remote(
                    crate::commit_ingest::RemoteCoordinator::new(url)
                        .with_client(upstream.client().clone())
                        .with_worker_id(upstream.worker_id()),
                ))
            }
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
        // Carry the host's InferenceExecutorMaterializer into the worker as a neutral model→executor
        // closure, so a worker-driven run resolves its own configured model per attempt
        // (R1). `None` when no provider is installed — the worker stays on the host
        // default.
        let inference_materializer = self.worker_inference_materializer();
        // The recovered dispatch a crash left mid-flight is re-executed by this
        // worker; giving it the same checkpoint store lets that re-execution resume
        // the interrupted step from its flushed partial (Phase 3 cross-process).
        let mut ingress = DurableRunIngress::with_owner_and_resolver(
            runtime,
            store,
            commit,
            crate::dispatch_backend::dispatch_owner(),
            Some(stream_checkpoint),
            inference_materializer,
        );
        if let Some(upstream) = &self.upstream {
            let mut commit = crate::commit_ingest::RemoteClaimedRunCommit::new(upstream.base_url())
                .with_client(upstream.client().clone());
            if let Some(identity) = upstream.worker_identity() {
                commit = commit.with_worker_identity(identity.clone());
            }
            ingress = ingress.with_claimed_commit(Arc::new(commit));
        }
        let ingress = Arc::new(ingress);
        // No per-session recovery sweep here: this session's worker shares one queue
        // with every other, so a claim would grab foreign threads' runs. The
        // process-level `DispatchPool` owns recovery — it claims each crashed run and
        // routes it to the session (this one included) that owns its thread.
        let boxed: Arc<dyn RunIngress> = ingress.clone();
        Ok((boxed, Some(ingress)))
    }

    /// Wrap this host's `InferenceExecutorMaterializer` (if installed) into the neutral
    /// `model_ref → executor` closure a worker's `WorkerContext` carries, so a
    /// database-less worker resolves the run's configured model per attempt (R1).
    /// `None` when no provider is installed — the worker stays on the runtime's bound
    /// default (a single-model deployment is unaffected).
    pub(crate) fn worker_inference_materializer(
        &self,
    ) -> Option<awaken_run_ingress::InferenceMaterializerFn> {
        self.inference_routing.materializer().map(|materializer| {
            let resolve: awaken_run_ingress::InferenceMaterializerFn =
                Arc::new(move |activation| {
                    let legacy_access = awaken_runtime_contract::InferenceAccess::host_executor(
                        activation.effective_model_ref(),
                    );
                    let access = activation
                        .snapshot
                        .metadata
                        .inference_access
                        .as_ref()
                        .unwrap_or(&legacy_access);
                    materializer.materialize(activation, access)
                });
            resolve
        })
    }

    pub(crate) async fn ctx_for(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<Arc<SessionCtx>, HostError> {
        self.ctx_for_with_sandbox(thread, agent, None).await
    }

    /// Open a session over an already-adopted sandbox, or create one when this is
    /// the first placement. The recovery adapter owns parsing/provider selection;
    /// session construction only enforces that a resident thread cannot be rebound
    /// to a different environment.
    pub(crate) async fn ctx_for_with_sandbox(
        &self,
        thread: &str,
        agent: Option<&str>,
        adopted: Option<LocalSandbox>,
    ) -> Result<Arc<SessionCtx>, HostError> {
        self.ctx_for_snapshot_with_sandbox(thread, agent, None, adopted)
            .await
    }

    /// Open a session from the executable snapshot carried by a claimed dispatch.
    /// The snapshot is the publication output and therefore authoritative for the
    /// worker; the config service is only a local/session-create compatibility path.
    pub(crate) async fn ctx_for_snapshot_with_sandbox(
        &self,
        thread: &str,
        agent: Option<&str>,
        published_snapshot: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
        adopted: Option<LocalSandbox>,
    ) -> Result<Arc<SessionCtx>, HostError> {
        let mut sessions = self.sessions.lock().await;
        if let Some(ctx) = sessions.get(thread) {
            if let Some(adopted) = &adopted
                && ctx.env.handle() != adopted.handle()
            {
                return Err(HostError::internal(format!(
                    "thread {thread} is already bound to a different sandbox"
                )));
            }
            let ctx = ctx.clone();
            drop(sessions);
            let _ = ctx
                .runtime
                .reconcile_delegation_cancellations(&ctx.thread_id, ctx.commit.as_ref())
                .await;
            return Ok(ctx);
        }
        let env = Arc::new(match adopted {
            Some(env) => env,
            None => self
                .provider
                .create_sandbox(&self.sandbox_spec(thread))
                .await
                .map_err(|e| HostError::internal(e.to_string()))?,
        });
        // Clone any staged github_repository resources into the fresh sandbox,
        // host-side (ADR-0038); fail-closed so a bad repo aborts session start.
        self.realize_thread_repositories(thread, env.as_ref())?;
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
        // A remote worker consumes the exact snapshot distributed in the claim;
        // it must not reopen the config registry and reconstruct current state.
        // Local session creation has no claimed snapshot yet, so it uses the
        // installed publication as the compatibility path.
        let installed = published_snapshot.or_else(|| {
            self.config_service
                .as_ref()
                .zip(agent)
                .and_then(|(svc, agent)| svc.installed(agent))
        });
        // The workspace skill dir is negotiated by the agent/hand definition: its
        // `plugin_config.skills_dir` (ADR-0036) overrides the default `skills` subdir,
        // so a hand that authors skills elsewhere is discovered where it says — not a
        // hardcoded path. Absent/blank → the default.
        let skills_subdir = installed
            .as_ref()
            .and_then(|c| {
                c.resolved_spec
                    .plugin_config
                    .get("skills_dir")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| crate::skills::DEFAULT_SKILLS_SUBDIR.to_string());
        // An authored permission policy (the agent's `permission` config section)
        // shapes the gate: its rules layer over the built-in baseline (perception +
        // the pre-authorized MCP/admin tools) and its default governs unmatched calls.
        // A malformed/absent section → None → the strict built-in default (mutations
        // asked), so a bad policy never fails open.
        let authored_permission = config_permission_ruleset(
            installed
                .as_ref()
                .map(|c| &c.resolved_spec.plugin_config)
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
        // Delegation is a runtime concern: inject the executor so the kernel runs
        // `agent_run` as a sub-agent (native or remote), not the tool registry.
        if let Some(service) = self.run_delegation(env.clone(), commit.clone())? {
            runtime = runtime.with_run_delegation(service);
        }
        // Skills are fronted by two stable tools (ADR-0036); all skill behavior is
        // in `awaken-ext-skills`. The host only wires the pieces it alone owns —
        // the sandbox env, the sub-run capability, and the base gate — via
        // `skills::wire_skills`.
        let mut skill_descriptors = Vec::new();
        let mut skill_registry: Option<Arc<dyn SkillRegistry>> = None;
        // A managed Session consumes its exact frozen Skill versions. Direct/legacy
        // threads without a frozen manifest retain the latest-catalog compatibility
        // path. Presence of an empty frozen vector explicitly offers no Skills.
        let workspace = self.thread_workspace(thread);
        let frozen = self
            .thread_skills
            .lock()
            .expect("thread skills mutex poisoned")
            .get(thread)
            .cloned();
        let delivered = if frozen.is_some() {
            frozen
        } else {
            self.skills.reload_cache_in(&workspace).await;
            self.skills
                .has_store()
                .then(|| self.skills.cache_snapshot_in(&workspace))
        };
        // A newly published Agent receives exactly its selected Skills. Older
        // publications without the normalized binding section retain the legacy
        // global catalog behavior, which makes the migration backward compatible.
        let selected_skills = installed.as_ref().and_then(|config| {
            awaken_runtime_contract::agent_bindings::AgentBindings::from_config(
                &config.resolved_spec.plugin_config,
            )
            .map(|bindings| {
                bindings
                    .skill_ids
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>()
            })
        });
        let filtered_specs: Vec<SkillSpec> = match &selected_skills {
            Some(selected) => self
                .skills
                .specs()
                .iter()
                .filter(|skill| selected.contains(&skill.id))
                .cloned()
                .collect(),
            None => self.skills.specs().to_vec(),
        };
        let filtered_delivered = match (delivered, &selected_skills) {
            (Some(skills), Some(selected)) => Some(
                skills
                    .into_iter()
                    .filter(|version| selected.contains(&version.skill_id))
                    .collect(),
            ),
            (skills, None) => skills,
            (None, Some(_)) => None,
        };
        if let Some(wiring) = crate::skills::wire_skills(
            &filtered_specs,
            filtered_delivered,
            env.clone(),
            self.llm.clone(),
            &self.model_ref,
            thread,
            // The MCP-aware base gate, so a skill-wrapped gate keeps the thread's
            // pre-authorized MCP tools (identical to `server_gate()` without MCP).
            base_gate.clone(),
            sub_base("skill-fork"),
            self.agent_run_reuse_sandbox,
            &skills_subdir,
        )
        .map_err(HostError::internal)?
        {
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
        if let Some(mem) = self
            .memory_for_thread(thread)
            .filter(|memory| memory.recall_enabled())
        {
            let mut plugin =
                awaken_ext_memory::MemoryPlugin::from_handle(mem.store(), mem.bounds());
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
        let context_policy = match &self.compaction {
            Some(compaction) => {
                let keep_last = compaction.config.keep_last;
                let plugin = CompactPlugin::new(compaction.config.clone())
                    .with_agent_tool(compaction.agent_tool.clone());
                runtime = runtime.with_plugin(Arc::new(plugin));
                plugin_ids.push(awaken_ext_compact::COMPACT_PLUGIN_ID.to_string());
                awaken_runtime_contract::resolved::ContextPolicy::KeepLast { keep_last }
            }
            None => awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        };
        // Everything dynamically provisioned on this thread that the config must
        // advertise: the skill tools plus the discovered MCP tools.
        let mut dynamic_descriptors = skill_descriptors;
        dynamic_descriptors.extend(mcp_descriptors);
        let config = installed.unwrap_or_else(|| {
            server_config(
                "assistant",
                &self.inference_routing.model_ref(thread, &self.model_ref),
                &self.client_tools,
                self.delegates.ids_set(),
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
        // For a sandboxed (α) ACP run with authenticated MCP servers, resolve the α reference
        // through the host's loopback relay: point each server at the relay and register its
        // real bearer there, so the sandbox reaches the MCP server via loopback and the token
        // is injected host-side (never in the sandbox). Started lazily, once per host.
        let relay = if is_acp && !self.mcp_trusted_inline && !staged_mcp.is_empty() {
            match self
                .mcp_relay
                .get_or_try_init(crate::mcp_relay::McpRelay::start)
                .await
            {
                Ok(r) => {
                    r.set_routes(thread, &staged_mcp);
                    Some(r)
                }
                // A relay that cannot bind falls back to the (unresolved) α reference rather
                // than failing the session — the raw bearer still never enters the sandbox.
                Err(_) => None,
            }
        } else {
            None
        };
        let config = crate::mcp::overlay_acp_mcp(
            config,
            &staged_mcp,
            is_acp,
            self.mcp_trusted_inline,
            relay,
            thread,
        );
        // Recover the session's position from committed truth: a durable store may
        // already hold this thread's history and an awaiting run (e.g. after a
        // restart). `consumed_rounds` starts past any prior outcome rounds so a new
        // `define_outcome` reports only the rounds it produces.
        let mut state = SessionState {
            consumed_rounds: commit.continuation_payloads(&thread_id).len(),
            ..SessionState::default()
        };
        if let Some((run_id, _)) = commit.open_wait_for_thread(&thread_id) {
            // Prime the fresh runtime so the awaiting run's snapshot resolves on
            // resume — `start_run` would normally have installed it.
            runtime.register_snapshot(config.clone());
            state.awaiting_run = Some(run_id);
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
            hand_placement: self.hand_placement.clone(),
            capture_sink: self.capture_sink.clone(),
            thread_id,
            env,
            skill_registry,
            cancel: std::sync::Mutex::new(None),
            reschedule: std::sync::Mutex::new(None),
            live_inbox: std::sync::Mutex::new(crate::live_inbox::LiveInboxSlot::default()),
            state: tokio::sync::Mutex::new(state),
        });
        sessions.insert(thread.to_string(), ctx.clone());
        drop(sessions);
        // A prior process may have crashed after atomically ending the parent
        // and before delivering its remote child cancellations. Re-entering the
        // session redelivers those idempotent outbox entries.
        let _ = ctx
            .runtime
            .reconcile_delegation_cancellations(&ctx.thread_id, ctx.commit.as_ref())
            .await;
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

    /// End a session's sandbox lifecycle at a terminal edge (managed session
    /// delete/archive): evict the cached context and dispose the sandbox at the OS
    /// boundary (shred materialized secrets, reap the per-thread workspace dir).
    /// Idempotent — a thread with no live session is a no-op.
    ///
    /// This is the ONLY place a session's sandbox is reaped. The evict-to-rebuild
    /// edges (`rebind_model`/`apply_session_inputs`)
    /// remove the cached context WITHOUT disposing, so the next turn's `ctx_for`
    /// rebuilds over the same `base/<thread>` workspace (ADR-0038 continuity); a
    /// terminal end must instead reap it. Repository publication and authored-Skill
    /// persistence run at the caller's release boundary before this method; Memory
    /// copy reconciliation is owned by `Sandbox::dispose` through its mount guard.
    pub(crate) async fn end_session(&self, thread: &str) -> Result<(), HostError> {
        let ctx = self.sessions.lock().await.remove(thread);
        let dispose_result = if let Some(ctx) = ctx {
            awaken_provisioning_contract::Sandbox::dispose(&*ctx.env)
                .await
                .map_err(|e| HostError::internal(e.to_string()))
        } else {
            Ok(())
        };

        // Terminal cleanup removes every thread-scoped projection, including
        // credential-bearing MCP relay routes. A future Session reusing the opaque
        // thread id must start from an empty projection and be authorized/staged
        // again; resource state never outlives its Session boundary in these maps.
        self.thread_workspaces
            .lock()
            .expect("thread workspaces")
            .remove(thread);
        self.thread_memory
            .lock()
            .expect("thread memory mutex poisoned")
            .remove(thread);
        self.thread_mcp
            .lock()
            .expect("thread MCP mutex poisoned")
            .remove(thread);
        self.thread_resources
            .lock()
            .expect("thread resources mutex poisoned")
            .remove(thread);
        self.thread_egress.remove(thread);
        self.thread_sandbox.remove(thread);
        self.inference_routing.remove(thread);
        if let Some(relay) = self.mcp_relay.get() {
            relay.remove_routes(thread);
        }

        dispose_result
    }
}
