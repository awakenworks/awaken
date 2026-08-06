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

fn merge_acp_mcp_servers(
    publication: Vec<awaken_runtime_contract::resolved::AcpMcpServer>,
    staged: impl IntoIterator<Item = awaken_runtime_contract::resolved::AcpMcpServer>,
) -> Result<Vec<awaken_runtime_contract::resolved::AcpMcpServer>, HostError> {
    let mut names = std::collections::BTreeSet::new();
    let mut merged = Vec::new();
    for server in publication.into_iter().chain(staged) {
        if !names.insert(server.name.clone()) {
            return Err(HostError::bad_request(format!(
                "duplicate ACP MCP server name `{}`",
                server.name
            )));
        }
        merged.push(server);
    }
    Ok(merged)
}

impl SharedHost {
    /// Resolve the one immutable publication selected for a Session and enforce
    /// its projected Agent/backend fences. Context construction and cold
    /// environment adoption share this boundary so provider selection cannot
    /// drift from execution selection.
    pub(crate) fn resolve_session_publication(
        &self,
        thread: &str,
        agent: Option<&str>,
        published_snapshot: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<
        (
            String,
            String,
            Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
        ),
        HostError,
    > {
        let workspace = self.thread_workspace(thread);
        let projected_agent = self.thread_agent_projection(thread);
        if let (Some(asserted), Some(projected)) = (agent, projected_agent.as_deref())
            && asserted != projected
        {
            return Err(HostError::internal(format!(
                "session Agent projection `{projected}` does not match requested Agent `{asserted}`"
            )));
        }
        let selected_agent = agent.or(projected_agent.as_deref()).unwrap_or("assistant");
        let installed = published_snapshot.or_else(|| {
            self.agent_publications.as_ref().and_then(|source| {
                source.current(
                    &workspace,
                    &awaken_runtime_contract::snapshot::AgentId(selected_agent.to_string()),
                )
            })
        });
        let published_backend_ref = installed
            .as_ref()
            .map(|snapshot| snapshot.resolved_spec.model_binding.backend_ref.clone());
        let projected_backend_ref = self
            .session_slots
            .read(thread, |slot| slot.backend_ref.clone())
            .flatten();
        if let (Some(published), Some(projected)) = (&published_backend_ref, &projected_backend_ref)
            && published != projected
        {
            return Err(HostError::internal(format!(
                "session backend projection `{projected}` does not match publication `{published}`"
            )));
        }
        if installed.is_none() && projected_backend_ref.is_some() {
            return Err(HostError::internal(
                "session backend projection has no immutable Agent publication",
            ));
        }
        Ok((workspace, selected_agent.to_string(), installed))
    }

    fn session_has_local_environment_inputs(
        &self,
        thread: &str,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> bool {
        let slot_requires = self
            .session_slots
            .read(thread, |slot| {
                !slot.delegates.is_empty()
                    || slot.memory.is_some()
                    || !slot.resources.mounts.is_empty()
                    || !slot.resources.repositories.is_empty()
                    || slot.baseline.as_ref().is_some_and(|baseline| {
                        !baseline.mounts.is_empty() || !baseline.env.is_empty()
                    })
                    || slot.skills.as_ref().is_some_and(|versions| {
                        versions
                            .iter()
                            .any(crate::skills::version_requires_environment)
                    })
            })
            .unwrap_or(false);
        let selected_skills = published_snapshot.map(|snapshot| {
            snapshot
                .resolved_spec
                .plugin_config
                .agent
                .skills
                .iter()
                .map(|skill| skill.skill_id.clone())
                .collect::<std::collections::BTreeSet<_>>()
        });
        let workspace = self.thread_workspace(thread);
        slot_requires
            || self
                .skills
                .requires_environment_in(&workspace, selected_skills.as_ref())
    }

    pub(crate) fn session_environment_provider(
        &self,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    ) -> Result<&crate::session_environment::SessionEnvironmentProvider, HostError> {
        match provisioning {
            awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { .. } => {
                self.backend_owned_session_provider.as_ref().ok_or_else(|| {
                    HostError::internal(
                        "BackendOwned provisioning requires a trusted-host Session provider",
                    )
                })
            }
            awaken_runtime_contract::resolved::ModelProvisioning::Provider { .. }
            | awaken_runtime_contract::resolved::ModelProvisioning::Remote { .. }
            | awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor => {
                Ok(&self.session_provider)
            }
        }
    }

    fn can_defer_session_environment(
        &self,
        thread: &str,
        agent: Option<&str>,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> bool {
        // A Coordinator-only Host constructs the durable dispatch envelope but
        // never executes it. Creating an eager sandbox here would make the
        // Coordinator a second physical owner beside the registered Worker.
        if self.deployment.disable_local_pool {
            return true;
        }
        let slot_allows = self
            .session_slots
            .read(thread, |slot| {
                slot.deferred_executor.is_some()
                    && slot
                        .environment_projection
                        .as_ref()
                        .is_some_and(|projection| {
                            projection.provisioning
                                == awaken_session_contract::SandboxProvisioning::OnToolUse
                        })
            })
            .unwrap_or(false);
        let workspace = self.thread_workspace(thread);
        let published_backend_is_acp = published_snapshot
            .cloned()
            .or_else(|| {
                self.agent_publications.as_ref().and_then(|source| {
                    source.current(
                        &workspace,
                        &awaken_runtime_contract::snapshot::AgentId(
                            agent.unwrap_or("assistant").to_string(),
                        ),
                    )
                })
            })
            .is_some_and(|snapshot| {
                awaken_runtime_contract::resolved::Backend::from_ref(
                    &snapshot.resolved_spec.model_binding.backend_ref,
                )
                .is_acp()
            });
        let selected_backend_is_acp = self
            .session_slots
            .read(thread, |slot| slot.backend_ref.clone())
            .flatten()
            .is_some_and(|backend_ref| {
                awaken_runtime_contract::resolved::Backend::from_ref(&backend_ref).is_acp()
            });
        slot_allows
            && !self.session_has_local_environment_inputs(thread, published_snapshot)
            && !published_backend_is_acp
            && !selected_backend_is_acp
    }

    async fn persist_environment_before_publish(
        &self,
        thread: &str,
        env: &crate::session_environment::SessionEnvironment,
    ) -> Result<(), HostError> {
        let binding = serde_json::to_string(&env.handle())
            .map_err(|error| HostError::internal(error.to_string()))?;
        let sink = self
            .environment_binding_sink
            .read()
            .expect("environment binding sink lock poisoned")
            .clone();
        if let Some(sink) = sink {
            if !sink.owns(thread).await {
                return Ok(());
            }
            let realization = self
                .session_slots
                .read(thread, |slot| slot.realization_lease.clone())
                .flatten();
            sink.persist(thread, &binding, realization.as_ref())
                .await
                .map_err(|error| {
                    HostError::internal(format!(
                        "persist Session environment binding before use: {error}"
                    ))
                })?;
        }
        Ok(())
    }

    async fn bind_deferred_dispatch_before_publish(
        &self,
        thread: &str,
        binding: &str,
    ) -> Result<bool, HostError> {
        let claim = self
            .session_slots
            .read(thread, |slot| slot.deferred_claim.clone())
            .flatten();
        let Some(claim) = claim else {
            return Ok(false);
        };
        let outcome = self
            .dispatch_store()
            .map_err(|error| HostError::internal(error.to_string()))?
            .bind_sandbox(&claim, binding)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        if !outcome.applied() {
            return Err(HostError::internal(
                "deferred sandbox binding was fenced by a replacement claim",
            ));
        }
        Ok(true)
    }

    /// One physical Session-environment creation path. CacheVolume preparation
    /// happens before either the authoritative or BackendOwned provider binds the
    /// mount, so eager warmup and first-use realization cannot diverge.
    pub(crate) async fn create_session_environment(
        &self,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<crate::session_environment::SessionEnvironment, HostError> {
        self.cache_volume_prewarmer
            .prepare_mounts(&spec.mounts)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        provider
            .create(spec)
            .await
            .map_err(|error| HostError::internal(error.to_string()))
    }

    /// Materialize the deferred environment at the first Sandbox-target tool.
    /// The same lifecycle mutex used by context construction guarantees one
    /// creator, and publication follows resource realization + durable binding.
    pub(crate) async fn ensure_session_environment_for_tool(
        &self,
        thread: &str,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        let lifecycle = self
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        if let Some(environment) = self
            .session_slots
            .read(thread, |slot| slot.environment.clone())
            .flatten()
        {
            return Ok(environment);
        }
        let environment = Arc::new(
            self.create_session_environment(&self.session_provider, &self.sandbox_spec(thread))
                .await?,
        );
        if let Err(error) = self
            .realize_thread_repositories(thread, environment.as_ref())
            .await
        {
            let _ = environment.dispose().await;
            return Err(error);
        }
        let binding = serde_json::to_string(&environment.handle())
            .map_err(|error| HostError::internal(error.to_string()))?;
        let dispatch_bound = match self
            .bind_deferred_dispatch_before_publish(thread, &binding)
            .await
        {
            Ok(bound) => bound,
            Err(error) => {
                let _ = environment.dispose().await;
                return Err(error);
            }
        };
        if let Err(error) = self
            .persist_environment_before_publish(thread, environment.as_ref())
            .await
        {
            // Once the durable claim owns this handle, keep the physical
            // environment available for adoption. A retry repairs the Session
            // aggregate before publishing it to the runtime.
            if !dispatch_bound {
                let _ = environment.dispose().await;
            }
            return Err(error);
        }
        self.session_slots.update(thread, |slot| {
            slot.environment = Some(environment.clone());
            slot.deferred_claim = None;
        });
        Ok(environment)
    }

    /// Evict only the rebuildable runtime context while retaining the
    /// independently-owned Session environment and its live resource projection.
    /// Terminal cleanup remains the single responsibility of [`Self::end_session`].
    pub(crate) async fn evict_session_for_rebuild(&self, thread: &str) {
        self.session_slots
            .modify(thread, |slot| slot.runtime = None);
    }

    /// Build a thread's commit boundary under the configured store directory: a
    /// durable SQLite database (default) or the filesystem append-log backend when
    /// `DeploymentConfig::store=Fs`. Only test-support composition may omit the
    /// storage directory and select an in-memory coordinator.
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
            #[cfg(any(test, feature = "test-support"))]
            CommitPlan::Memory => Ok(HostCommit::Local(std::sync::Arc::new(
                MemoryCommitCoordinator::new(),
            ))),
            #[cfg(not(any(test, feature = "test-support")))]
            CommitPlan::SqliteNeedsStorageDir => Err(HostError::internal(
                "product SQLite commit requires DeploymentConfig::storage_dir; refusing to drop committed history on restart",
            )),
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

    /// Open only the authoritative commit/read boundary for a query. A resident
    /// context already owns the exact adapter (including a Worker's recovery
    /// projection); otherwise reconstruct the configured durable adapter without
    /// selecting, creating, or adopting a Session environment.
    pub(crate) async fn commit_for_read(&self, thread: &str) -> Result<Arc<HostCommit>, HostError> {
        let lifecycle = self
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        if let Some(commit) = self
            .session_slots
            .read(thread, |slot| {
                slot.runtime.as_ref().map(|context| context.commit.clone())
            })
            .flatten()
        {
            return Ok(commit);
        }
        self.build_commit(thread).await.map(Arc::new)
    }

    /// Build a thread's interrupted-stream checkpoint store, mirroring
    /// `build_commit`'s durability choice: a filesystem store under the configured
    /// directory (so a partial survives a process crash and resumes), or the
    /// checkpoint adapter paired with the shared Postgres dispatch authority.
    /// Only explicit test-support composition may select a process-local store.
    fn build_stream_checkpoint(
        &self,
        thread: &str,
    ) -> Result<Option<Arc<dyn StreamCheckpointStore>>, HostError> {
        // A database-less Worker persists checkpoints through its authenticated,
        // claim-fenced dispatch transport. Installing a local store here would
        // create a second authority that cannot survive Worker replacement.
        if self.upstream.is_some() {
            return Ok(None);
        }
        if self.deployment.store == crate::deployment_config::StoreKind::Postgres {
            return self
                .dispatch_store()?
                .stream_checkpoint_store()
                .map(Some)
                .ok_or_else(|| {
                    HostError::internal(
                        "Postgres runtime requires the checkpoint store paired with its dispatch authority",
                    )
                });
        }
        let Some(dir) = &self.store_dir else {
            #[cfg(any(test, feature = "test-support"))]
            return Ok(Some(Arc::new(MemoryStreamCheckpointStore::new())));
            #[cfg(not(any(test, feature = "test-support")))]
            return Err(HostError::internal(
                "product stream checkpoints require durable storage; refusing a process-local fallback",
            ));
        };
        let checkpoint_dir = dir.join(sanitize_thread(thread)).join("stream-checkpoints");
        let store = FsStreamCheckpointStore::open(&checkpoint_dir)
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(Some(Arc::new(store)))
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
        stream_checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
        run_context: awaken_runtime_contract::RuntimeRunContext,
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
        let mut ingress = DurableRunIngress::with_owner_and_resolver(
            runtime,
            store,
            commit,
            self.deployment.dispatch_owner.clone(),
            stream_checkpoint,
            inference_materializer,
        )
        .with_context(run_context);
        ingress = match &self.worker_stream_publisher {
            Some(publisher) => ingress.with_claimed_stream_publisher(publisher.clone()),
            None => ingress.with_stream_sink(self.completion.clone()),
        };
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
            if adopted.is_some() && ctx.env.is_none() {
                // A deferred durable context can survive the crash gap after the
                // dispatch claim bound a Sandbox but before Session persistence.
                // Rebuild that sandbox-free context around the adopted handle.
                self.session_slots
                    .update(thread, |slot| slot.runtime = None);
            } else {
                if let Some(adopted) = adopted {
                    if ctx.env.as_ref().map(|env| env.handle()) != Some(adopted.handle()) {
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
        }
        // Resolve the immutable publication before selecting an environment.
        // Environment identity is a consequence of provisioning, not of the ACP
        // executor or a process-wide sandbox default.
        let (workspace, selected_agent, installed) =
            self.resolve_session_publication(thread, agent, published_snapshot)?;
        // Legacy Session manifests carry no frozen Skill list. Refresh their
        // canonical delivered catalog before deciding whether `on_tool_use` may
        // defer the Environment; doing this later in Skill wiring can classify
        // a cold cache as instruction-only and then discover support files after
        // the sandbox-free decision has already been made.
        let frozen_skill_versions = self
            .session_slots
            .read(thread, |slot| slot.skills.clone())
            .flatten();
        if frozen_skill_versions.is_none() {
            self.skills
                .reload_cache_in(&workspace)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
        }
        let published_backend_ref = installed
            .as_ref()
            .map(|snapshot| snapshot.resolved_spec.model_binding.backend_ref.clone());
        let provisioning = installed
            .as_ref()
            .map(|snapshot| &snapshot.resolved_spec.model_binding.provisioning)
            .unwrap_or(&awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor);
        let execution_backend = published_backend_ref
            .as_deref()
            .map(awaken_runtime_contract::resolved::Backend::from_ref)
            .unwrap_or(awaken_runtime_contract::resolved::Backend::Native);
        let a2a_only = installed.as_ref().is_some_and(|snapshot| {
            !crate::host::completion::requires_local_environment(&snapshot.resolved_spec)
        });
        if a2a_only && self.session_has_local_environment_inputs(thread, installed.as_ref()) {
            return Err(HostError::bad_request(
                "remote A2A execution cannot consume local Session Environment inputs",
            ));
        }
        let environment_provider = self.session_environment_provider(provisioning)?;
        let retained = self
            .session_slots
            .read(thread, |slot| slot.environment.clone())
            .flatten();
        if a2a_only && (retained.is_some() || adopted.is_some()) {
            return Err(HostError::bad_request(
                "remote A2A execution cannot bind a local Session Environment",
            ));
        }
        let deferred = retained.is_none()
            && adopted.is_none()
            && self.can_defer_session_environment(
                thread,
                Some(selected_agent.as_str()),
                installed.as_ref(),
            );
        let (env, needs_provision, needs_registration) = match (retained, adopted) {
            (Some(existing), Some(adopted)) => {
                if existing.handle() != adopted.handle() {
                    return Err(HostError::internal(format!(
                        "thread {thread} is already bound to a different sandbox"
                    )));
                }
                adopted.stop_bound_processes().await;
                (Some(existing), false, false)
            }
            (Some(existing), None) => (Some(existing), false, false),
            // The adopted environment already contains its Session workspace and
            // repositories. Re-cloning would both fail and destroy continuity.
            (None, Some(adopted)) => (Some(Arc::new(adopted)), false, true),
            (None, None) if a2a_only || deferred => (None, false, false),
            (None, None) => (
                Some(Arc::new(
                    self.create_session_environment(
                        environment_provider,
                        &self.sandbox_spec(thread),
                    )
                    .await?,
                )),
                true,
                true,
            ),
        };
        if needs_provision {
            let env = env.as_ref().expect("new environment exists");
            // Clone staged repositories only for a physically new environment.
            // Rebuilding SessionCtx must not re-clone over a live Session workspace.
            if let Err(error) = self.realize_thread_repositories(thread, env.as_ref()).await {
                let _ = env.dispose().await;
                return Err(error);
            }
            if let Err(error) = self
                .persist_environment_before_publish(thread, env.as_ref())
                .await
            {
                let _ = env.dispose().await;
                return Err(error);
            }
        }
        if needs_registration {
            let env = env.as_ref().expect("registered environment exists");
            if !needs_provision {
                // A dispatch-owned sandbox may be adopted after a crash between
                // dispatch binding and Session aggregate persistence. Repair the
                // aggregate before publishing the adopted wrapper to this runtime.
                self.persist_environment_before_publish(thread, env.as_ref())
                    .await?;
            }
            self.session_slots
                .update(thread, |slot| slot.environment = Some(env.clone()));
        }
        let thread_id = ThreadId(thread.to_string());
        let commit = Arc::new(self.build_commit(thread).await?);
        // Durable interrupted-stream checkpoints follow the commit's durability
        // (Phase 3): a mid-recovery crash resumes from the flushed partial.
        let stream_checkpoint = self.build_stream_checkpoint(thread)?;
        // Runtime selection is known before MCP realization. Native execution
        // connects staged servers as in-process McpPlugins; ACP hands the same
        // typed server set to the CLI's own MCP client and must not open a second
        // competing host-side connection.
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
                        projection.request.generation.attachment_id.0,
                        projection.request.generation.generation.0
                    ))
                })?;
                combined.plugins.extend(wiring.plugins.clone());
                combined.tool_ids.extend(wiring.tool_ids.clone());
                combined
                    .skill_registries
                    .extend(wiring.skill_registries.clone());
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
        let mut runtime = match env.as_ref() {
            Some(env) => build_runtime(self.llm.clone(), env.as_ref()),
            None => build_runtime(self.llm.clone(), &crate::config::DeferredHandToolSource),
        };
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
        let has_published_delegates = installed.as_ref().is_some_and(|snapshot| {
            !snapshot
                .resolved_spec
                .plugin_config
                .agent
                .delegates
                .is_empty()
        });
        if has_published_delegates && env.is_none() {
            return Err(HostError::internal(
                "deferred Session environment cannot host delegate targets",
            ));
        }
        if let Some(env) = env.clone()
            && let Some(service) = self.run_delegation(
                thread,
                env,
                permission.clone(),
                commit.clone(),
                installed.as_ref(),
            )?
        {
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
        let delivered = if frozen_skill_versions.is_some() {
            frozen_skill_versions
        } else {
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
                .skills
                .iter()
                .map(|skill| skill.skill_id.clone())
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
            mcp.skill_registries.clone(),
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
            commit.clone(),
            // A coordinator-only host owns the Skill registry used to describe
            // the durable Run, but the exact claimed Worker owns filesystem
            // realization. Every execution-capable host must materialize here.
            !self.deployment.disable_local_pool,
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
        let authored_memory = installed
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .resolved_spec
                    .plugin_config
                    .get(awaken_ext_memory::MEMORY_PLUGIN_ID)
            })
            .and_then(|value| awaken_ext_memory::MemoryConfig::from_value(Some(value)).ok())
            .unwrap_or_default();
        let published_memory_selected = installed.as_ref().is_some_and(|snapshot| {
            snapshot
                .resolved_spec
                .plugin_ids
                .iter()
                .any(|id| id == awaken_ext_memory::MEMORY_PLUGIN_ID)
        });
        let memory_selected = if installed.is_some() {
            let binding_id = if published_memory_selected {
                Some(authored_memory.binding_id.as_deref().ok_or_else(|| {
                    HostError::bad_request(
                        "the Awaken memory extension requires an explicit `memory.binding_id`",
                    )
                })?)
            } else {
                None
            };
            let selected = self
                .select_thread_memory_binding(thread, binding_id)
                .map_err(HostError::bad_request)?;
            if let Some(memory) = &selected {
                memory.reconcile(thread).await;
            }
            selected.is_some()
        } else {
            // Direct embedders opt in through `bind_resolved_memory`; Managed
            // compatibility Sessions merely register standard mount bindings.
            self.memory_for_thread(thread).is_some()
        };
        let recalled_memory = self.memory_for_thread(thread).filter(|memory| {
            memory.recall_enabled() && memory_selected && authored_memory.recall_enabled
        });
        let memory_selector = recalled_memory.as_ref().map(|_| {
            let agent_id = authored_memory
                .selector_agent_id
                .as_deref()
                .unwrap_or(awaken_ext_memory::SELECTOR_AGENT_ID);
            let fallback = awaken_ext_memory::default_selector_agent(
                &self.model_ref,
                authored_memory
                    .selector_instructions
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(awaken_ext_memory::DEFAULT_SELECTOR_INSTRUCTIONS),
            );
            let snapshot = crate::agent_catalog::resolve_auxiliary_snapshot(
                self.agent_publications.as_deref(),
                &workspace,
                agent_id,
                fallback,
                authored_memory.selector_instructions.as_deref(),
            );
            Arc::new(crate::memory::AgentSelector::new(
                self.llm.clone(),
                snapshot,
                commit.clone(),
            )) as Arc<dyn awaken_ext_memory::RecallSelector>
        });
        let acp_memory_recall = recalled_memory.as_ref().map(|mem| {
            let recall =
                awaken_ext_memory::MemoryRecall::new(mem.store(), authored_memory.recall.clone());
            match &memory_selector {
                Some(selector) => recall.with_selector(selector.clone()),
                None => recall,
            }
        });
        if let Some(mem) = recalled_memory {
            let mut plugin = awaken_ext_memory::MemoryPlugin::from_handle(
                mem.store(),
                authored_memory.recall.clone(),
            );
            if let Some(selector) = &memory_selector {
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
                let compact_config = installed
                    .as_ref()
                    .and_then(|snapshot| {
                        snapshot
                            .resolved_spec
                            .plugin_config
                            .get(awaken_ext_compact::COMPACT_PLUGIN_ID)
                    })
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                    .unwrap_or_else(|| compaction.config.clone());
                let fallback = awaken_ext_compact::default_compact_agent(
                    &self.model_ref,
                    compact_config
                        .agent_instructions
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or(awaken_ext_compact::DEFAULT_COMPACT_INSTRUCTIONS),
                );
                let snapshot = crate::agent_catalog::resolve_auxiliary_snapshot(
                    self.agent_publications.as_deref(),
                    &workspace,
                    &compact_config.agent_id,
                    fallback,
                    compact_config.agent_instructions.as_deref(),
                );
                let agent_tool = build_compact_runner(self.llm.clone(), snapshot, commit.clone());
                let backend = build_compact_backend(agent_tool, self.memory.background());
                let plugin = CompactPlugin::new(compact_config).with_backend(thread, backend);
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
                    .delegate_ids()
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
        // WebSearch has one configuration/dispatch owner for both execution
        // backends. Native lets Runtime resolve the plugin once; ACP resolves
        // the same plugin once and exports that RawTool through MCP. A Session
        // never constructs both copies.
        let web_search = if config
            .resolved_spec
            .plugin_ids
            .iter()
            .any(|id| id == awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID)
        {
            let credentials = self.credential_materializer.clone().map(|materializer| {
                Arc::new(crate::web_search::HostWebSearchCredentialResolver::new(
                    materializer,
                    self.thread_workspace(thread),
                )) as Arc<dyn awaken_ext_builtin_tools::WebSearchCredentialResolver>
            });
            let plugin = Arc::new(awaken_ext_builtin_tools::WebSearchPlugin::new(
                self.web_search_providers.clone(),
                credentials,
            ));
            if is_acp {
                Some(
                    plugin
                        .configured_tool(
                            config
                                .resolved_spec
                                .plugin_config
                                .get(awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID),
                        )
                        .map_err(|error| HostError::bad_request(error.to_string()))?,
                )
            } else {
                runtime = runtime.with_plugin(plugin);
                None
            }
        } else {
            None
        };
        // D6: for an ACP run, merge publication-pinned secret-free stdio routes with
        // the Session's staged URL routes, then hand the exact set to the CLI's own MCP
        // client. The immutable publication's `backend_ref` is the routing authority.
        // Environment provisioning may install a stdio command, but it never owns the
        // route: the Agent publication declares it and the Session receives it here.
        // The credential form is the host's
        // isolation decision: the raw bearer never reaches the CLI; every authenticated
        // ACP server uses the Worker-held exact-generation relay. A native run is untouched
        // (its MCP servers are already the in-process tools connected above).
        // Runtime construction consumes effects staged by the one public MCP
        // realization lifecycle.  It must not start a relay or recreate routes:
        // after restart, durable rehydration stages and publishes them first.
        let relay = self.mcp_relay.get();
        let mut acp_mcp_servers = if is_acp {
            let publication = awaken_runtime_contract::resolved::AcpSpec::from_plugin_config(
                config.resolved_spec.plugin_config.plugins(),
            )
            .mcp_servers;
            let staged = active_mcp
                .iter()
                .filter_map(|projection| {
                    projection
                        .server
                        .as_ref()
                        .map(|server| (projection, server))
                })
                .map(|(projection, server)| {
                    crate::mcp::project_mcp_transport(server, &projection.request.generation, relay)
                })
                .collect::<Result<Vec<_>, _>>()?;
            merge_acp_mcp_servers(publication, staged)?
        } else {
            Vec::new()
        };
        let web_search_mcp = if is_acp {
            match web_search {
                Some((descriptor, tool)) => {
                    let export = self
                        .acp_tool_exporter
                        .as_ref()
                        .ok_or_else(|| {
                            HostError::internal(
                                "ACP WebSearch requires an installed tool-export adapter",
                            )
                        })?
                        .export("awaken_web_search", descriptor, tool)
                        .await
                        .map_err(HostError::internal)?;
                    acp_mcp_servers =
                        merge_acp_mcp_servers(acp_mcp_servers, [export.server.clone()])?;
                    Some(export)
                }
                None => None,
            }
        } else {
            None
        };
        // Recover the session's position from committed truth: a durable store may
        // already hold this thread's history and an awaiting run after a restart.
        let mut state = SessionState::default();
        if let Some((run_id, _)) = commit
            .open_wait_for_thread(&thread_id)
            .await
            .map_err(HostError::internal)?
        {
            // The activated resume boundary installs its exact snapshot; session
            // construction only restores the committed position.
            state.awaiting_run = Some(run_id);
        }
        let runtime = Arc::new(runtime);
        let acp_executor = self.acp.as_ref().zip(env.clone()).map(|(acp, env)| {
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
            Arc::new(crate::application::AcpContextAttemptExecutor::new(
                attempt_executor,
                skill_registry.clone(),
                acp_memory_recall,
                thread,
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
        // Privacy attribution is an attempt concern, not a delivery-topology
        // concern. This one decorator therefore wraps the final executor used by
        // both DirectRunIngress and DurableRunIngress (including recovered runs).
        let capture_sink = self
            .capture_sink
            .read()
            .expect("capture sink lock poisoned")
            .clone();
        let attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor> =
            Arc::new(crate::run_exec::CaptureContextAttemptExecutor::new(
                attempt_executor,
                self.capture_decision.clone(),
                capture_sink,
                self.data_subject_consent.clone(),
            ));
        // This is the one post-attempt output edge. Because it wraps the final
        // executor shared by DirectRunIngress and DurableRunIngress, claimed
        // recovery cannot bypass artifact publication.
        let attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor> =
            Arc::new(crate::run_exec::ArtifactHarvestAttemptExecutor::new(
                attempt_executor,
                self.artifact_harvester(),
            ));
        // The foreground delivery seam (slice C/D): a turn's execution goes through
        // `RunIngress` rather than calling `runtime.start_run` directly. Direct
        // ingress runs inline on the same `runtime`; durable ingress queues the run
        // through a dispatch store first. Both share this thread's `runtime`/`commit`.
        // A database-less Worker sends its terminal commit to the Coordinator.
        // The Coordinator's HostCommitApplier owns post-commit observation so the
        // durable extraction intent lands beside the authoritative Session. Local
        // execution observes here because its commit authority is in this Host.
        let terminal_observers: Vec<_> = if self.upstream.is_some() {
            Vec::new()
        } else {
            self.memory_terminal_observer(thread, &config, commit.clone())
                .await
                .into_iter()
                .collect()
        };
        let tool_executor = if a2a_only {
            None
        } else if let Some(environment) = env.as_ref() {
            // Every realized tier owns the Hand for its Session. Container uses
            // the channel-backed process; Workdir/Namespace use their rooted
            // implementations behind the same ToolExecutor port.
            Some(environment.tool_executor())
        } else {
            self.session_slots
                .read(thread, |slot| slot.deferred_executor.clone())
                .flatten()
        };
        // Cause/effect composition rules: terminal observers and Session plugins
        // are additive; an Environment/placement hand overrides only the tool
        // executor; one canonical RuntimeRunContext crosses the ingress boundary.
        let run_context = terminal_observers.iter().cloned().fold(
            awaken_runtime_contract::RuntimeRunContext::new(),
            awaken_runtime_contract::RuntimeRunContext::with_terminal_observer,
        );
        let run_context = mcp.plugins.iter().cloned().fold(
            run_context,
            awaken_runtime_contract::RuntimeRunContext::with_session_plugin,
        );
        let run_context = match tool_executor.as_ref() {
            Some(executor) => run_context.with_tool_executor(executor.clone()),
            None => run_context,
        };
        let run_context = match env.as_ref() {
            Some(environment) => run_context.with_tool_output_spiller(Arc::new(
                crate::tool_output_spill::SandboxToolOutputSpiller::new(environment.clone()),
            )),
            None => run_context,
        };
        let (ingress, durable_ingress) = self
            .build_ingress(
                runtime.clone(),
                attempt_executor,
                commit.clone(),
                stream_checkpoint.clone(),
                run_context.clone(),
            )
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
            attempt_context: run_context,
            terminal_observers,
            stream_checkpoint,
            _web_search_mcp: web_search_mcp,
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

    #[cfg(test)]
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
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
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
        let provider = self.session_environment_provider(provisioning)?;
        let adoption = async {
            let sandbox = provider
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
            sandbox
                .reconcile_adopted_mounts(&self.thread_session_mounts(thread))
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
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
                // Both eager contexts (which retain `env`) and deferred contexts
                // (which retain only the lazy executor) are coupled to this exact
                // slot environment once it is published.
                slot.runtime = None;
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
        self.stop_session_mcp_processes(thread).await;
        let (ctx, env) = self.session_slots.update(thread, |slot| {
            (slot.runtime.take(), slot.environment.take())
        });
        let dispose_result = if let Some(env) = env.or_else(|| ctx.and_then(|ctx| ctx.env.clone()))
        {
            if env.needs_recovered_memory_reconciliation() {
                let mounter = self.memory_mounter().ok_or_else(|| {
                    HostError::internal("recovered Memory copy has no MemoryMounter")
                })?;
                for mount in self.thread_resources_snapshot(thread).mounts {
                    if let awaken_provisioning_contract::MountSource::MemoryStore {
                        store_id,
                        materialization_reference,
                        ..
                    } = &mount.source
                    {
                        let files = env
                            .list_files(&mount.mount_path)
                            .await
                            .map_err(|error| HostError::internal(error.to_string()))?;
                        mounter
                            .reconcile_recovered_copy(
                                materialization_reference.as_deref().unwrap_or(store_id),
                                &files,
                                mount.access,
                            )
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
    use super::{merge_acp_mcp_servers, pre_authorized_tool_ids};
    use awaken_runtime_contract::resolved::{AcpMcpServer, AcpMcpTransport};

    fn stdio(name: &str) -> AcpMcpServer {
        AcpMcpServer {
            name: name.into(),
            transport: AcpMcpTransport::Stdio {
                command: "playwright-mcp".into(),
                args: vec!["--headless".into()],
            },
        }
    }

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

    #[test]
    fn publication_stdio_and_session_mcp_routes_merge_without_shadowing() {
        let merged = merge_acp_mcp_servers(vec![stdio("playwright")], [stdio("session")])
            .expect("distinct routes");
        assert_eq!(
            merged
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>(),
            ["playwright", "session"]
        );

        let error = merge_acp_mcp_servers(vec![stdio("playwright")], [stdio("playwright")])
            .expect_err("duplicate names must not silently shadow a publication route");
        assert!(
            error
                .to_string()
                .contains("duplicate ACP MCP server name `playwright`")
        );
    }
}
