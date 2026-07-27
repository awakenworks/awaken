//! Session/context management for [`SharedHost`]: building a thread's commit
//! boundary, stream-checkpoint store, run-delivery ingress, and `ctx_for`.

use super::*;

fn pre_authorized_tool_ids(
    mcp_ids: &[String],
    admin_ids: &[String],
    has_authored_permission: bool,
) -> Vec<String> {
    if has_authored_permission {
        admin_ids.to_vec()
    } else {
        mcp_ids
            .iter()
            .cloned()
            .chain(admin_ids.iter().cloned())
            .collect()
    }
}

impl SharedHost {
    /// Evict only the rebuildable runtime context while retaining the
    /// independently-owned Session environment and its live resource projection.
    /// Terminal cleanup remains the single responsibility of [`Self::end_session`].
    pub(crate) async fn evict_session_for_rebuild(&self, thread: &str) {
        self.session_slots
            .modify(thread, |slot| slot.runtime = None);
    }

    /// Build a thread's commit boundary under the configured store directory: a
    /// durable SQLite database (default) or the filesystem append-log backend when
    /// `DeploymentConfig::store=Fs`, or an in-memory coordinator when no store dir is set.
    pub(crate) async fn build_commit(&self, thread: &str) -> Result<HostCommit, HostError> {
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
            CommitPlan::Remote(_) => Ok(HostCommit::Remote(crate::store::RemoteHostCommit::new())),
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
                "DeploymentConfig::store=Fs requires DeploymentConfig::storage_dir: the filesystem append-log \
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
    /// execution (`DirectRunIngress`, slice C). With `typed durable ingress` the
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
        attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>,
        commit: Arc<HostCommit>,
        stream_checkpoint: Arc<dyn StreamCheckpointStore>,
        terminal_observers: &[Arc<dyn awaken_runtime_contract::terminal::RunTerminalObserver>],
        session_plugins: &[Arc<dyn awaken_runtime_contract::plugin::Plugin>],
    ) -> Result<
        (
            Arc<dyn RunIngress>,
            Option<Arc<DurableRunIngress<AnyDispatchStore>>>,
        ),
        HostError,
    > {
        if !self.deployment.durable {
            return Ok((
                Arc::new(DirectRunIngress::with_attempt_executor(
                    runtime,
                    attempt_executor,
                )),
                None,
            ));
        }
        // The ONE process-shared dispatch queue (shared SQLite file, or the shared
        // Postgres pool for a fleet) plus this process's unique claim owner — both
        // live in `dispatch_backend`, which owns backend selection (ADR-0019/0024).
        // Every session's worker shares this queue; the process-level `DispatchPool`
        // is its sole claimer and routes each run back to its owning session.
        let store = self.dispatch_store()?;
        // Carry the host's InferenceExecutorMaterializer into the worker as a neutral model→executor
        // closure, so a worker-driven run resolves its own configured model per attempt
        // (R1). `None` when no provider is installed — the worker stays on the host
        // default.
        let inference_materializer = self.worker_inference_materializer();
        let local_credential_capabilities = self
            .upstream
            .is_none()
            .then(|| self.local_credential_realization_capabilities());
        let recovery_projection = commit.recovery_projection();
        // The recovered dispatch a crash left mid-flight is re-executed by this
        // worker; giving it the same checkpoint store lets that re-execution resume
        // the interrupted step from its flushed partial (Phase 3 cross-process).
        let run_context = terminal_observers.iter().cloned().fold(
            awaken_runtime_contract::RuntimeRunContext::new(),
            awaken_runtime_contract::RuntimeRunContext::with_terminal_observer,
        );
        let run_context = session_plugins.iter().cloned().fold(
            run_context,
            awaken_runtime_contract::RuntimeRunContext::with_session_plugin,
        );
        let mut ingress = DurableRunIngress::with_owner_and_resolver(
            runtime,
            store,
            commit,
            self.deployment.dispatch_owner.clone(),
            Some(stream_checkpoint),
            inference_materializer,
        )
        .with_context(run_context);
        if let Some(capabilities) = local_credential_capabilities {
            ingress = ingress.with_local_credential_capabilities(capabilities);
        }
        if let Some(resolver) = &self.worker_credential_resolver {
            ingress = ingress.with_worker_credential_resolver(resolver.clone());
        }
        ingress.install_attempt_executor(attempt_executor);
        if let Some(upstream) = &self.upstream {
            ingress =
                ingress.with_claimed_commit(crate::commit_ingest::remote_claimed_commit(upstream)?);
        }
        if let Some(projection) = recovery_projection {
            ingress = ingress.with_recovery_projection(projection);
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
    /// published-access → executor closure a worker's `WorkerContext` carries, so a
    /// database-less worker materializes the run's configured model per attempt (R1).
    /// `None` when no provider is installed — the worker stays on the runtime's bound
    /// default (a single-model deployment is unaffected).
    pub(crate) fn worker_inference_materializer(
        &self,
    ) -> Option<awaken_run_ingress::InferenceMaterializerFn> {
        self.inference_routing.materializer().map(|materializer| {
            let resolve: awaken_run_ingress::InferenceMaterializerFn =
                Arc::new(move |activation, context| materializer.materialize(activation, context));
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
        adopted: Option<crate::session_environment::SessionEnvironment>,
    ) -> Result<Arc<SessionCtx>, HostError> {
        self.ctx_for_snapshot_with_sandbox(thread, agent, None, adopted)
            .await
    }

    /// Open a session from the executable snapshot carried by a claimed dispatch.
    /// The snapshot is the publication output and therefore authoritative for the
    /// worker; local Session creation reads the current immutable publication.
    pub(crate) async fn ctx_for_snapshot_with_sandbox(
        &self,
        thread: &str,
        agent: Option<&str>,
        published_snapshot: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
        adopted: Option<crate::session_environment::SessionEnvironment>,
    ) -> Result<Arc<SessionCtx>, HostError> {
        let lifecycle = self
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        if let Some(ctx) = self
            .session_slots
            .read(thread, |slot| slot.runtime.clone())
            .flatten()
        {
            if let Some(adopted) = adopted {
                if ctx.env.handle() != adopted.handle() {
                    return Err(HostError::internal(format!(
                        "thread {thread} is already bound to a different sandbox"
                    )));
                }
                // A concurrent cold resolver may have adopted while this context was
                // becoming resident. Stop only that wrapper's hand process; disposing
                // it would tear down the shared underlying sandbox.
                adopted.stop_bound_processes().await;
            }
            let _ = ctx
                .runtime
                .reconcile_delegation_cancellations(&ctx.thread_id, ctx.commit.as_ref())
                .await;
            return Ok(ctx);
        }
        let retained = self
            .session_slots
            .read(thread, |slot| slot.environment.clone())
            .flatten();
        let (env, needs_provision, needs_registration) = match (retained, adopted) {
            (Some(existing), Some(adopted)) => {
                if existing.handle() != adopted.handle() {
                    return Err(HostError::internal(format!(
                        "thread {thread} is already bound to a different sandbox"
                    )));
                }
                adopted.stop_bound_processes().await;
                (existing, false, false)
            }
            (Some(existing), None) => (existing, false, false),
            // The adopted environment already contains its Session workspace and
            // repositories. Re-cloning would both fail and destroy continuity.
            (None, Some(adopted)) => (Arc::new(adopted), false, true),
            (None, None) => (
                Arc::new(
                    self.session_provider
                        .create(&self.sandbox_spec(thread))
                        .await
                        .map_err(|e| HostError::internal(e.to_string()))?,
                ),
                true,
                true,
            ),
        };
        if needs_provision {
            // Clone staged repositories only for a physically new environment.
            // Rebuilding SessionCtx must not re-clone over a live Session workspace.
            if let Err(error) = self.realize_thread_repositories(thread, env.as_ref()).await {
                let _ = env.dispose().await;
                return Err(error);
            }
        }
        if needs_registration {
            self.session_slots
                .update(thread, |slot| slot.environment = Some(env.clone()));
        }
        let thread_id = ThreadId(thread.to_string());
        let commit = Arc::new(self.build_commit(thread).await?);
        // Durable interrupted-stream checkpoints follow the commit's durability
        // (Phase 3): a mid-recovery crash resumes from the flushed partial.
        let stream_checkpoint = self.build_stream_checkpoint(thread)?;
        // A remote worker consumes the exact snapshot distributed in the claim;
        // it must not reopen the config registry and reconstruct current state.
        // Local Session creation has no claimed snapshot yet, so it resolves the
        // current publication once.
        let workspace = self.thread_workspace(thread);
        let installed = published_snapshot.or_else(|| {
            self.agent_publications.as_ref().and_then(|source| {
                source.current(
                    &workspace,
                    &awaken_runtime_contract::snapshot::AgentId(
                        agent.unwrap_or("assistant").to_string(),
                    ),
                )
            })
        });
        // Runtime selection is known before MCP realization. Native execution
        // connects staged servers as in-process McpPlugins; ACP hands the same
        // typed server set to the CLI's own MCP client and must not open a second
        // competing host-side connection.
        let execution_backend = self
            .acp
            .as_ref()
            .and_then(|acp| acp.adapter_for(thread))
            .map(|adapter| awaken_runtime_contract::resolved::Backend::from_ref(&adapter))
            .or_else(|| {
                installed.as_ref().map(|snapshot| {
                    awaken_runtime_contract::resolved::Backend::from_ref(
                        &snapshot.resolved_spec.model_binding.backend_ref,
                    )
                })
            })
            .unwrap_or(awaken_runtime_contract::resolved::Backend::Native);
        let is_acp = execution_backend.is_acp();
        // This thread's staged MCP servers (ADR-0043 Phase 3), registered by the
        // managed adapter's `prepare_session` before the first turn; the wire
        // composition (connect + discover, fail closed) lives in `crate::mcp`.
        // Read, not removed, so a retry re-attempts (and re-fails) the connect.
        let active_mcp = self.active_mcp_projections(thread);
        let mcp = if is_acp {
            crate::mcp::McpWiring::empty()
        } else {
            let mut combined = crate::mcp::McpWiring::empty();
            for projection in &active_mcp {
                let wiring = projection.native_wiring.as_ref().ok_or_else(|| {
                    HostError::internal(format!(
                        "native MCP generation {}:{} has no staged connection",
                        projection.generation.attachment_id.0, projection.generation.generation.0
                    ))
                })?;
                combined.plugins.extend(wiring.plugins.clone());
                combined.tool_ids.extend(wiring.tool_ids.clone());
            }
            combined
        };
        // An authored permission policy is the sole authority for MCP confirmation.
        // Without one, selecting the MCP server pre-authorizes its discovered tools;
        // with one, its rules/default decide every MCP call. Do not project a second
        // confirmation list through Session state: that path cannot survive recovery
        // without duplicating the published policy.
        let authored_permission = config_permission_ruleset(
            installed
                .as_ref()
                .map(|c| c.resolved_spec.plugin_config.plugins())
                .unwrap_or(&self.plugin_config),
        );
        let published_toolsets = installed
            .as_ref()
            .map(|snapshot| snapshot.resolved_spec.plugin_config.agent.toolsets.clone())
            .unwrap_or_default();
        let toolsets = self
            .session_slots
            .read(thread, |slot| slot.toolsets.clone())
            .flatten()
            .unwrap_or(published_toolsets);
        // Management tools (ADR-0052) remain pre-authorized: they are read-only,
        // and only the reserved-scope assistant's config names them.
        let admin_ids: Vec<String> = self
            .admin_tools
            .iter()
            .map(|t| t.id().to_string())
            .collect();
        let has_explicit_tool_policy = authored_permission.is_some() || !toolsets.is_empty();
        let pre_authorized =
            pre_authorized_tool_ids(&mcp.tool_ids, &admin_ids, has_explicit_tool_policy);
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
        let apply_base_gate = !pre_authorized.is_empty() || has_explicit_tool_policy;
        let permission = crate::config::server_permission_policy_with_toolsets(
            authored_permission.clone(),
            &pre_authorized,
            &toolsets,
        );
        let base_gate = server_gate_with_toolsets(authored_permission, &pre_authorized, &toolsets);
        // R1/R2: the runtime is built with the host default executor; each run then
        // resolves its *effective* model (its `model_ref_override`, else its snapshot
        // binding) to an executor at the resolve seam and sets it on the run context.
        // Resolving per run — not once at session build — means a per-turn model
        // switch needs no session rebuild, and a database-less worker runs the
        // configured model without a session-level registry.
        let mut runtime = build_runtime(self.llm.clone(), env.as_ref());
        if apply_base_gate {
            runtime = runtime.with_gate(base_gate.clone());
        }
        // Register the management tool executables globally (ADR-0052 D3): the
        // registry stays global, the compile-time scope fence is what restricts them.
        for tool in &self.admin_tools {
            runtime = runtime.with_tool(tool.clone());
        }
        // Delegation is a runtime concern: inject the executor so the kernel runs
        // `agent_run` as a sub-agent (native or remote), not the tool registry.
        let published_delegate_targets = installed
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .resolved_spec
                    .plugin_config
                    .agent
                    .delegate_ids
                    .iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if let Some(service) = self.run_delegation(
            thread,
            env.clone(),
            commit.clone(),
            published_delegate_targets,
        )? {
            runtime = runtime.with_run_delegation(service);
        }
        // Skills are fronted by two stable tools (ADR-0036); all skill behavior is
        // in `awaken-ext-skills`. The host only wires the pieces it alone owns —
        // the sandbox env, the sub-run capability, and the base gate — via
        // `skills::wire_skills`.
        let mut skill_descriptors = Vec::new();
        let mut skill_registry: Option<Arc<dyn SkillRegistry>> = None;
        // A managed Session consumes its exact frozen Skill versions. An embedded
        // direct Session without a manifest reads its configured Skill catalog.
        let frozen = self
            .session_slots
            .read(thread, |slot| slot.skills.clone())
            .flatten();
        let delivered = if frozen.is_some() {
            frozen
        } else {
            self.skills
                .reload_cache_in(&workspace)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
            self.skills
                .has_store()
                .then(|| self.skills.cache_snapshot_in(&workspace))
        };
        // A published Agent receives exactly its selected Skills. Embedded direct
        // Sessions without a publication use the host-configured catalog.
        let selected_skills = installed.as_ref().map(|config| {
            config
                .resolved_spec
                .plugin_config
                .agent
                .skill_ids
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
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
                    .filter(|version| selected.contains(version.skill_id.as_str()))
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
            self.skill_fork_placement,
            &skills_subdir,
        )
        .await
        .map_err(HostError::internal)?
        {
            runtime = runtime
                .with_gate(wiring.gate)
                .with_tool(wiring.list_tool)
                .with_tool(wiring.activate_tool);
            skill_descriptors = wiring.descriptors;
            skill_registry = Some(wiring.registry);
        }
        // This is the sole final gate replacement point. Skill wiring may decorate
        // the ordinary base gate above; an explicit host override intentionally
        // replaces that complete default chain (the scheduled-action scenario).
        if let Some(gate) = &self.gate_override {
            runtime = runtime.with_gate(gate.clone());
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
        // slice into a summary and injects it request-only. A successful fold
        // activates its matching Run-scoped window; before that history stays whole.
        let context_policy = match &self.compaction {
            Some(compaction) => {
                let agent_tool =
                    build_compact_runner(self.llm.clone(), &self.model_ref, commit.clone());
                let backend = build_compact_backend(agent_tool, self.memory.background());
                let plugin =
                    CompactPlugin::new(compaction.config.clone()).with_backend(thread, backend);
                runtime = runtime.with_plugin(Arc::new(plugin));
                plugin_ids.push(awaken_ext_compact::COMPACT_PLUGIN_ID.to_string());
                awaken_runtime_contract::resolved::ContextPolicy::KeepAll
            }
            None => awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        };
        // Authored/default Session configuration still advertises Skill tools.
        // MCP tools are live Session plugins and therefore do not rewrite either
        // this generated snapshot or an immutable published snapshot.
        let dynamic_descriptors = skill_descriptors;
        let session_delegates: HashSet<String> = installed
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .resolved_spec
                    .plugin_config
                    .agent
                    .delegate_ids
                    .iter()
                    .map(|id| id.0.clone())
                    .collect()
            })
            .unwrap_or_default();
        let mut config = installed.unwrap_or_else(|| {
            server_config(
                "assistant",
                &self.inference_routing.model_ref(thread, &self.model_ref),
                &self.client_tools,
                &session_delegates,
                &plugin_ids,
                &self.plugin_config,
                &dynamic_descriptors,
                context_policy,
            )
        });
        if self
            .session_slots
            .read(thread, |slot| slot.toolsets.is_some())
            .unwrap_or(false)
        {
            config.resolved_spec.plugin_config.agent.toolsets = toolsets;
        }
        // A session-selected ACP/A2A runtime is an execution backend choice, not
        // merely an environment hint. Reflect it into the neutral resolved
        // snapshot so the shared AttemptExecutorRegistry routes the activation
        // instead of silently using the native fallback.
        if let Some(adapter) = self.acp.as_ref().and_then(|acp| acp.adapter_for(thread))
            && awaken_runtime_contract::resolved::Backend::from_ref(&adapter).is_acp()
        {
            config.resolved_spec.model_binding.binding.backend_ref = adapter;
        }
        // D6: for an ACP run, hand the session's staged MCP servers to the CLI's own MCP
        // client via `plugin_config.acp.mcp_servers`. Whether this run executes on ACP is
        // the host's runtime registration (`AcpBackend::is_acp`), not the config's
        // `backend_ref` — the managed `server_config` stamps a fixed backend_ref, so the
        // routing decision is the only reliable signal. The credential form is the host's
        // isolation decision: the raw bearer never reaches the CLI; every authenticated
        // ACP server uses the Worker-held exact-generation relay. A native run is untouched
        // (its MCP servers are already the in-process tools connected above).
        // Runtime construction consumes effects staged by the one public MCP
        // realization lifecycle.  It must not start a relay or recreate routes:
        // after restart, durable rehydration stages and publishes them first.
        let relay = self.mcp_relay.get();
        let acp_mcp_servers = if is_acp {
            active_mcp
                .iter()
                .filter_map(|projection| {
                    projection
                        .server
                        .as_ref()
                        .map(|server| (projection, server))
                })
                .map(|(projection, server)| {
                    crate::mcp::project_mcp_transport(server, &projection.generation, relay)
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        // Recover the session's position from committed truth: a durable store may
        // already hold this thread's history and an awaiting run after a restart.
        let mut state = SessionState::default();
        if let Some((run_id, _)) = commit.open_wait_for_thread(&thread_id) {
            // Prime the fresh runtime so the awaiting run's snapshot resolves on
            // resume — `start_run` would normally have installed it.
            runtime.register_snapshot(config.clone());
            state.awaiting_run = Some(run_id);
        }
        let runtime = Arc::new(runtime);
        let acp_executor = self.acp.as_ref().map(|acp| {
            acp.executor_for(
                env.clone(),
                permission,
                execution_backend.clone(),
                acp_mcp_servers,
            )
        });
        let attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor> =
            Arc::new(crate::run_exec::SessionAttemptExecutor::new(
                runtime.clone(),
                acp_executor,
                self.remote_attempt_executor.clone(),
                &config.resolved_spec,
                self.judge_snapshot
                    .as_ref()
                    .map(|snapshot| &snapshot.resolved_spec),
            ));
        let attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor> =
            Arc::new(crate::application::SessionPromptAttemptExecutor::new(
                attempt_executor,
                self.thread_session_prompts(thread),
            ));
        let attempt_executor = self
            .application_attempt_decorator
            .as_ref()
            .map_or(attempt_executor.clone(), |decorate| {
                decorate(attempt_executor)
            });
        // The foreground delivery seam (slice C/D): a turn's execution goes through
        // `RunIngress` rather than calling `runtime.start_run` directly. Direct
        // ingress runs inline on the same `runtime`; durable ingress queues the run
        // through a dispatch store first. Both share this thread's `runtime`/`commit`.
        let terminal_observers: Vec<_> = self
            .memory_terminal_observer(thread, &config, commit.clone())
            .await
            .into_iter()
            .collect();
        let (ingress, durable_ingress) = self
            .build_ingress(
                runtime.clone(),
                attempt_executor,
                commit.clone(),
                stream_checkpoint.clone(),
                &terminal_observers,
                &mcp.plugins,
            )
            .await?;
        let durable = durable_ingress.is_some();
        let mut hand_placement = self.hand_placement.clone();
        if let Some(hand) = env.bound_tool_executor() {
            hand_placement.bind_environment_hand(hand);
        }
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
            terminal_observers,
            stream_checkpoint,
            session_plugins: mcp.plugins,
            hand_placement,
            capture_sink: self.capture_sink.clone(),
            thread_id,
            env,
            skill_registry,
            cancel: Arc::new(std::sync::Mutex::new(None)),
            active_run: std::sync::Mutex::new(None),
            reschedule: std::sync::Mutex::new(None),
            live_inbox: std::sync::Mutex::new(crate::live_inbox::LiveInboxSlot::default()),
            state: tokio::sync::Mutex::new(state),
            outcome: tokio::sync::Mutex::new(()),
            execution: tokio::sync::Mutex::new(()),
        });
        self.session_slots
            .update(thread, |slot| slot.runtime = Some(ctx.clone()));
        // A prior process may have crashed after atomically ending the parent
        // and before delivering its remote child cancellations. Re-entering the
        // session redelivers those idempotent outbox entries.
        let _ = ctx
            .runtime
            .reconcile_delegation_cancellations(&ctx.thread_id, ctx.commit.as_ref())
            .await;
        // Recover the generic after-commit observer gap from committed Run truth.
        if let Some(run) = ctx.commit.latest_run(&ctx.thread_id)
            && run.state.is_terminal()
        {
            let _ = awaken_runtime_contract::terminal::redeliver_committed_terminal(
                ctx.commit.as_ref(),
                &ctx.terminal_observers,
                &run.id,
                &ctx.thread_id,
            )
            .await;
        }
        Ok(ctx)
    }

    pub(crate) async fn session_environment(
        &self,
        thread: &str,
    ) -> Option<Arc<crate::session_environment::SessionEnvironment>> {
        self.session_slots
            .read(thread, |slot| slot.environment.clone())
            .flatten()
    }

    pub(crate) async fn session_environment_handle(
        &self,
        thread: &str,
    ) -> Option<awaken_provisioning_contract::SandboxHandle> {
        self.session_environment(thread)
            .await
            .map(|env| env.handle())
    }

    /// Resolve an opaque durable binding through the one Session environment
    /// provider. Both claimed-worker recovery and foreground Managed-session
    /// restoration use this path, so ownership/status validation cannot drift.
    /// `rebuild_unavailable` is the dispatch recovery policy: when set, an
    /// unavailable binding is fenced and reported to the caller for replacement.
    pub(crate) async fn adopt_bound_session_environment(
        &self,
        thread: &str,
        encoded: Option<&str>,
        rebuild_unavailable: bool,
    ) -> Result<(Option<crate::session_environment::SessionEnvironment>, bool), HostError> {
        let Some(encoded) = encoded else {
            return Ok((None, false));
        };
        let handle: awaken_provisioning_contract::SandboxHandle = serde_json::from_str(encoded)
            .map_err(|error| {
                HostError::internal(format!("invalid Session sandbox binding: {error}"))
            })?;
        if handle.sandbox_id != thread {
            return Err(HostError::internal(format!(
                "sandbox {} does not belong to Session {thread}",
                handle.sandbox_id
            )));
        }
        if let Some(environment) = self.session_environment(thread).await {
            let resident = environment.handle();
            if resident != handle {
                return Err(HostError::internal(format!(
                    "Session {thread} is already bound to sandbox {}, not {}",
                    resident.sandbox_id, handle.sandbox_id
                )));
            }
            match environment.status().await {
                Ok(awaken_provisioning_contract::SandboxStatus::Ready) => {
                    return Ok((None, false));
                }
                Ok(_) | Err(_) if rebuild_unavailable => {
                    if !self.discard_session_environment(thread, &environment).await {
                        return Err(HostError::internal(format!(
                            "lost the sandbox recovery fence for Session {thread}"
                        )));
                    }
                    return Ok((None, true));
                }
                Ok(status) => {
                    return Err(HostError::internal(format!(
                        "Session sandbox {} is not ready ({status:?})",
                        handle.sandbox_id
                    )));
                }
                Err(error) => {
                    return Err(HostError::internal(format!(
                        "could not inspect Session sandbox {}: {error}",
                        handle.sandbox_id
                    )));
                }
            }
        }
        let adoption = async {
            let sandbox = self
                .session_provider
                .adopt(&handle)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
            if sandbox
                .status()
                .await
                .map_err(|error| HostError::internal(error.to_string()))?
                != awaken_provisioning_contract::SandboxStatus::Ready
            {
                return Err(HostError::internal(format!(
                    "Session sandbox {} is no longer available",
                    handle.sandbox_id
                )));
            }
            Ok(sandbox)
        }
        .await;
        match adoption {
            Ok(sandbox) => Ok((Some(sandbox), false)),
            Err(_) if rebuild_unavailable => Ok((None, true)),
            Err(error) => Err(error),
        }
    }

    /// Forget a dead environment only when it is still the exact `Arc` observed
    /// by the recovery attempt. Object identity plus the full durable handle is
    /// the ABA fence: a stale recovery task must never evict a replacement that
    /// has already been installed for the same thread/sandbox id.
    pub(crate) async fn discard_session_environment(
        &self,
        thread: &str,
        expected: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> bool {
        let removed = self
            .session_slots
            .modify(thread, |slot| {
                let still_observed = slot.environment.as_ref().is_some_and(|current| {
                    Arc::ptr_eq(current, expected) && current.handle() == expected.handle()
                });
                if !still_observed {
                    return None;
                }
                let removed = slot.environment.take();
                if slot.runtime.as_ref().is_some_and(|ctx| {
                    Arc::ptr_eq(&ctx.env, expected) && ctx.env.handle() == expected.handle()
                }) {
                    slot.runtime = None;
                }
                removed
            })
            .flatten();
        if let Some(environment) = removed {
            // The provider object may represent an unavailable external
            // sandbox. Only stop processes owned by this wrapper here; normal
            // terminal disposal remains the responsibility of `end_session`.
            environment.stop_bound_processes().await;
            true
        } else {
            false
        }
    }

    /// End a session's sandbox lifecycle at a terminal edge (managed session
    /// delete/archive): evict the cached context and dispose the sandbox at the OS
    /// boundary (shred materialized secrets, reap the per-thread workspace dir).
    /// Idempotent — a thread with no live session is a no-op.
    ///
    /// This is the ONLY place a Session-owned environment is reaped. Runtime-context
    /// rebuilds retain it in the Session runtime slot; a terminal end removes that
    /// owner and disposes exactly once. Repository publication and authored-Skill
    /// persistence run at the caller's release boundary before this method; Memory
    /// copy reconciliation is owned by `Sandbox::dispose` through its mount guard.
    pub(crate) async fn end_session(&self, thread: &str) -> Result<(), HostError> {
        let (ctx, env) = self.session_slots.update(thread, |slot| {
            (slot.runtime.take(), slot.environment.take())
        });
        let dispose_result = if let Some(env) = env.or_else(|| ctx.map(|ctx| ctx.env.clone())) {
            if env.needs_recovered_memory_reconciliation() {
                let mounter = self.memory_mounter().ok_or_else(|| {
                    HostError::internal("recovered Memory copy has no MemoryMounter")
                })?;
                for mount in self.thread_resources_snapshot(thread).mounts {
                    if let awaken_provisioning_contract::MountSource::MemoryStore { store_id } =
                        &mount.source
                    {
                        let files = env
                            .list_files(&mount.mount_path)
                            .await
                            .map_err(|error| HostError::internal(error.to_string()))?;
                        mounter
                            .reconcile_recovered_copy(store_id, &files, mount.access)
                            .await
                            .map_err(|error| HostError::internal(error.to_string()))?;
                    }
                }
            }
            env.dispose()
                .await
                .map_err(|e| HostError::internal(e.to_string()))
        } else {
            Ok(())
        };

        // Terminal cleanup removes every thread-scoped projection, including
        // credential-bearing MCP relay routes. A future Session reusing the opaque
        // thread id must start from an empty projection and be authorized/staged
        // again; resource state never outlives its Session boundary in these maps.
        let reference_result = self
            .clear_session_references(thread)
            .await
            .map_err(|error| HostError::internal(error.to_string()));
        self.session_slots.remove(thread);
        if let Some(relay) = self.mcp_relay.get() {
            relay.remove_routes(thread);
        }

        dispose_result.and(reference_result)
    }
}

#[cfg(test)]
mod permission_projection_tests {
    use super::pre_authorized_tool_ids;

    #[test]
    fn authored_permission_is_the_only_mcp_confirmation_authority() {
        let mcp = vec!["mcp__calc__add".to_string()];
        let admin = vec!["awaken_admin_get".to_string()];

        assert_eq!(
            pre_authorized_tool_ids(&mcp, &admin, false),
            vec!["mcp__calc__add", "awaken_admin_get"]
        );
        assert_eq!(
            pre_authorized_tool_ids(&mcp, &admin, true),
            vec!["awaken_admin_get"]
        );
    }
}
