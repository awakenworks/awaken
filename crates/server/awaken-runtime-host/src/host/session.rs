//! Session/context management for [`SharedHost`]: building a thread's commit
//! boundary, stream-checkpoint store, run-delivery ingress, and `ctx_for`.

mod child_substrate;
mod content_delivery;
mod environment_lifecycle;
mod input_projection;

use super::*;
pub(super) use crate::config::{SessionToolsetProjection, project_session_tool_override};
pub(super) use input_projection::{
    ManagedCoordinationRole, project_frozen_session_model_override,
    project_managed_coordination_surface,
};
use input_projection::{
    merge_acp_mcp_servers, merge_process_local_mcp_servers, pre_authorized_tool_ids,
};

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
        let baseline = self
            .session_slots
            .read(thread, |slot| slot.baseline.clone())
            .flatten();
        let frozen_revision = baseline
            .as_ref()
            .and_then(|baseline| baseline.agent_revision);
        let installed = published_snapshot.or_else(|| {
            self.agent_publications.as_ref().and_then(|source| {
                let agent_id =
                    awaken_runtime_contract::snapshot::AgentId(selected_agent.to_string());
                match frozen_revision {
                    Some(revision) => source.at_revision(&workspace, &agent_id, revision),
                    None => source.current(&workspace, &agent_id),
                }
            })
        });
        if installed.is_none() && frozen_revision.is_some() {
            return Err(HostError::internal(
                "frozen Session Agent publication is unavailable",
            ));
        }
        let model_override = baseline
            .as_ref()
            .and_then(|baseline| baseline.model_override.clone());
        let installed = installed
            .map(|snapshot| {
                project_frozen_session_model_override(snapshot, model_override.as_ref(), &workspace)
            })
            .transpose()?;
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

    async fn persist_environment_before_publish(
        &self,
        thread: &str,
        env: &crate::session_environment::SessionEnvironment,
        kind: awaken_session_contract::SessionEnvironmentEffectKind,
    ) -> Result<(), HostError> {
        let binding = serde_json::to_string(&env.handle())
            .map_err(|error| HostError::internal(error.to_string()))?;
        let sink = self
            .environment_binding_sink
            .read()
            .expect("environment binding sink lock poisoned")
            .clone();
        if let Some(sink) = sink {
            if !sink
                .owns(thread)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?
            {
                return Ok(());
            }
            let mut realization = self
                .session_slots
                .read(thread, |slot| slot.realization_lease.clone())
                .flatten();
            // FMECA/causal graph: C1 a slow K8s realization finishes under lease L1;
            // C2 heartbeat/reclaim projects L2 before its receipt commits; C3 a
            // second renewal projects L3 while the L2 retry is in flight. E1 never
            // publishes under L1/L2 after either fence changed; E2 follows each
            // exact slot notification; E3 commits only under the repository-current
            // lease; E4 bounded churn/absence fails closed. One replacement retry
            // is insufficient because renewal and reclaim are independent clocks.
            const FENCE_CATCH_UP_ATTEMPTS: usize = 4;
            let mut persisted = None;
            for attempt in 0..FENCE_CATCH_UP_ATTEMPTS {
                let result = sink
                    .persist(awaken_session_contract::SessionEnvironmentReceipt::new(
                        thread,
                        kind,
                        binding.clone(),
                        realization.clone(),
                    ))
                    .await;
                match result {
                    Ok(()) => {
                        persisted = Some(Ok(()));
                        break;
                    }
                    Err(error)
                        if error.code == "session_realization_stale"
                            && attempt + 1 < FENCE_CATCH_UP_ATTEMPTS =>
                    {
                        let changed = self
                            .session_slots
                            .update(thread, |slot| slot.realization_changed.clone());
                        let replacement =
                            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                                loop {
                                    let notified = changed.notified();
                                    let current = self
                                        .session_slots
                                        .read(thread, |slot| slot.realization_lease.clone())
                                        .flatten();
                                    if current.is_some() && current != realization {
                                        break current;
                                    }
                                    notified.await;
                                }
                            })
                            .await;
                        match replacement {
                            Ok(replacement) => realization = replacement,
                            Err(_) => {
                                persisted = Some(Err(error));
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        persisted = Some(Err(error));
                        break;
                    }
                }
            }
            let persisted = persisted.expect("bounded binding persistence produces a result");
            persisted.map_err(|error| {
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
            .read(thread, |slot| slot.dispatch_claim.clone())
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
        if spec.isolation >= awaken_provisioning_contract::IsolationClass::Namespace {
            let required = awaken_provisioning_contract::SandboxRequirements::from_spec(spec, true);
            let capabilities = provider.capabilities();
            if !capabilities.satisfies_requirements(&required) {
                return Err(HostError::internal(format!(
                    "Session environment cannot preserve one sandbox-absolute workspace path across Hand, Bash, Git, and Agent processes: required={required:?}, provider={capabilities:?}",
                )));
            }
        }
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
            .persist_environment_before_publish(
                thread,
                environment.as_ref(),
                awaken_session_contract::SessionEnvironmentEffectKind::Create,
            )
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
        self.session_slots
            .update(thread, |slot| slot.environment = Some(environment.clone()));
        Ok(environment)
    }

    /// Evict only the rebuildable runtime context while retaining the
    /// independently-owned Session environment and its live resource projection.
    /// Terminal cleanup remains the single responsibility of [`Self::end_session`].
    pub(crate) async fn evict_session_for_rebuild(&self, thread: &str) {
        self.session_slots
            .modify(thread, |slot| slot.runtime = None);
    }

    /// Select the only commit boundary available to this process: a database-less
    /// Worker's claim-fenced remote projection or the Coordinator-injected local
    /// authority. Runtime Host never selects a Store backend.
    pub(crate) async fn build_commit(&self, thread: &str) -> Result<HostCommit, HostError> {
        if self.upstream.is_some() {
            return Ok(HostCommit::Remote(crate::store::RemoteHostCommit::new()));
        }
        self.authority
            .as_ref()
            .ok_or_else(|| HostError::internal("local Runtime requires an injected authority"))?
            .open_commit(thread)
            .await
            .map(HostCommit::Local)
            .map_err(HostError::from)
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
    /// Only explicit test-support startup may select a process-local store.
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
        self.authority
            .as_ref()
            .ok_or_else(|| {
                HostError::internal(
                    "database-less Worker build cannot open a local checkpoint authority",
                )
            })?
            .stream_checkpoint(thread)
            .map(Some)
            .map_err(HostError::from)
    }

    /// Build the one claimed Worker for this Thread, then select its independent
    /// foreground-delivery policy. Durable foreground delivery exposes the
    /// `DurableRunIngress` that owns that same Worker; direct foreground delivery
    /// keeps `DirectRunIngress` while retaining the Worker for pool-routed claims.
    /// Runtime, commit, executor, observers, credentials, and recovery wiring are
    /// therefore configured once regardless of delivery durability.
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
            Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>,
        ),
        HostError,
    > {
        // The ONE process-shared dispatch queue (shared SQLite file, or the shared
        // Postgres pool for a fleet) plus this process's unique claim owner — both
        // arrive through the Coordinator-owned RuntimeAuthority (ADR-0019/0024).
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
        let settlement_observer = self.dispatch_settlement_observer();
        // The recovered dispatch a crash left mid-flight is re-executed by this
        // worker; giving it the same checkpoint store lets that re-execution resume
        // the interrupted step from its flushed partial (Phase 3 cross-process).
        let mut ingress = DurableRunIngress::with_owner_and_resolver(
            runtime.clone(),
            store,
            commit,
            self.deployment.dispatch_owner.clone(),
            stream_checkpoint,
            inference_materializer,
        )
        .with_context(run_context);
        if let Some(observer) = settlement_observer {
            ingress = ingress.with_settlement_observer(observer);
        }
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
        ingress.install_attempt_executor(attempt_executor.clone());
        if let Some(upstream) = &self.upstream {
            ingress =
                ingress.with_claimed_commit(crate::commit_ingest::remote_claimed_commit(upstream)?);
        }
        if let Some(projection) = recovery_projection {
            ingress = ingress.with_recovery_projection(projection);
        }
        let ingress = Arc::new(ingress);
        let claimed_worker = ingress.worker_handle();
        // No per-session recovery sweep here: this session's worker shares one queue
        // with every other, so a claim would grab foreign threads' runs. The
        // process-level `DispatchPool` owns recovery — it claims each crashed run and
        // routes it to the session (this one included) that owns its thread.
        if self.deployment.durable {
            let boxed: Arc<dyn RunIngress> = ingress.clone();
            Ok((boxed, Some(ingress), claimed_worker))
        } else {
            let direct: Arc<dyn RunIngress> = Arc::new(DirectRunIngress::with_attempt_executor(
                runtime,
                attempt_executor,
            ));
            Ok((direct, None, claimed_worker))
        }
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
        self.ctx_for_snapshot_with_attempt(thread, agent, published_snapshot, adopted, None)
            .await
    }

    pub(crate) async fn ctx_for_claimed_snapshot_with_sandbox(
        &self,
        thread: &str,
        agent: Option<&str>,
        published_snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
        adopted: Option<crate::session_environment::SessionEnvironment>,
        attempt: ClaimedRuntimeInput,
    ) -> Result<Arc<SessionCtx>, HostError> {
        self.ctx_for_snapshot_with_attempt(
            thread,
            agent,
            Some(published_snapshot),
            adopted,
            Some(attempt),
        )
        .await
    }

    async fn ctx_for_snapshot_with_attempt(
        &self,
        thread: &str,
        agent: Option<&str>,
        published_snapshot: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
        adopted: Option<crate::session_environment::SessionEnvironment>,
        claimed_attempt: Option<ClaimedRuntimeInput>,
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
            let cache_matches_attempt = claimed_attempt.as_ref().is_none_or(|attempt| {
                ctx.runtime_publication_identity.as_ref() == Some(&attempt.identity)
            });
            if !cache_matches_attempt || (adopted.is_some() && ctx.env.is_none()) {
                // A deferred durable context can survive the crash gap after the
                // dispatch claim bound a Sandbox but before Session persistence;
                // likewise, a claim carrying different frozen Runtime inputs
                // must rebuild around its exact publication source rather than
                // reuse a warm Runtime.
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
                // A resident Session is also a recovery wake. Rebind every
                // frozen writable Memory resource through the one controller so
                // a prior transient repository outage cannot strand durable
                // parent- or child-Thread extraction work behind the cache hit.
                self.bind_thread_memory_recovery(thread, ctx.commit.clone())
                    .await?;
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
        let fallback_model_ref = claimed_attempt.as_ref().map_or_else(
            || {
                let frozen_model_ref = installed.as_ref().map_or(self.model_ref.as_str(), |root| {
                    root.resolved_spec.model_binding.model_ref.as_str()
                });
                self.inference_routing.model_ref(thread, frozen_model_ref)
            },
            |attempt| attempt.effective_model_ref.clone(),
        );
        let (publication_source, unclaimed_runtime_publications): (
            Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
            Option<Vec<awaken_runtime_contract::ExecutableAgentSnapshot>>,
        ) = if let Some(attempt) = &claimed_attempt {
            (attempt.publications.clone(), None)
        } else {
            let frozen = installed
                .as_ref()
                .map(|root| {
                    crate::agent_catalog::freeze_run_publications(
                        root,
                        self.agent_publications.as_deref(),
                        &workspace,
                    )
                })
                .transpose()
                .map_err(HostError::bad_request)?
                .unwrap_or_default();
            let source = Arc::new(
                awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new(frozen.clone())
                    .map_err(|error| {
                        HostError::bad_request(format!(
                            "invalid Agent publication closure: {error}"
                        ))
                    })?,
            );
            (source, Some(frozen))
        };
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
            .map(|snapshot| snapshot.resolved_spec.model_binding.provisioning())
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
        let retained = self
            .session_slots
            .read(thread, |slot| slot.environment.clone())
            .flatten();
        if a2a_only && (retained.is_some() || adopted.is_some()) {
            return Err(HostError::bad_request(
                "remote A2A execution cannot bind a local Session Environment",
            ));
        }
        let expected_binding = self
            .session_slots
            .read(thread, |slot| slot.expected_environment_binding.clone())
            .flatten();
        // Recovery cause/effect decision table: C1 durable binding exists; C2 a
        // matching resident/adopted environment is available. R1 !C1 may create
        // the first environment; R2 C1+C2 reuses/adopts it; R3 C1+!C2 fails
        // closed. A failed recovery must never erase C1 by creating a substitute.
        if expected_binding.is_some() && retained.is_none() && adopted.is_none() {
            return Err(HostError::internal(format!(
                "Session {thread} has a durable environment binding that was not adopted"
            )));
        }
        let has_published_delegates = installed.as_ref().is_some_and(|snapshot| {
            !snapshot
                .resolved_spec
                .plugin_config
                .agent
                .delegates
                .is_empty()
        });
        let has_published_advisor = installed
            .as_ref()
            .is_some_and(|snapshot| snapshot.resolved_spec.plugin_config.agent.advisor.is_some());
        let has_published_multiagent = has_published_delegates || has_published_advisor;
        // Managed Sessions expose Anthropic's fixed asynchronous coordination
        // tools. Ordinary/direct SDK sessions retain the existing synchronous
        // `agent_run` path. `session_dispatch` is the projection of the durable
        // Session admission fact; this is not a process-local feature mode.
        let session_dispatch = self
            .session_slots
            .read(thread, |slot| slot.session_dispatch)
            .unwrap_or(false);
        let managed_coordination = has_published_multiagent && session_dispatch;
        // `session_dispatch` is also the sole scope selector for per-request
        // budget admission. A Managed single-Agent Session needs that authority
        // even though it has no coordination tools, while direct AI-SDK/AG-UI
        // Threads must remain independent of the Managed application port.
        let coordination_endpoint = self.coordination_endpoint();
        if session_dispatch && coordination_endpoint.is_none() {
            return Err(HostError::internal(
                "Managed Session has no Session application authority",
            ));
        }
        let content_delivery = self.select_content_delivery(
            thread,
            installed.as_ref(),
            frozen_skill_versions.as_ref(),
        )?;
        let deferred = retained.is_none()
            && adopted.is_none()
            && self.can_defer_session_environment(
                thread,
                Some(selected_agent.as_str()),
                installed.as_ref(),
                has_published_multiagent,
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
                        self.session_environment_provider(provisioning)?,
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
                .persist_environment_before_publish(
                    thread,
                    env.as_ref(),
                    awaken_session_contract::SessionEnvironmentEffectKind::Create,
                )
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
                self.persist_environment_before_publish(
                    thread,
                    env.as_ref(),
                    awaken_session_contract::SessionEnvironmentEffectKind::Adopt,
                )
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
        // Managed adapter's `prepare_session` before the first Run; the wire
        // startup (connect + discover, fail closed) lives in `crate::mcp`.
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
        let published_configuration = installed
            .as_ref()
            .map(|snapshot| snapshot.resolved_spec.plugin_config.clone())
            .unwrap_or_else(|| {
                awaken_runtime_contract::agent_bindings::ResolvedConfiguration::new(
                    Default::default(),
                    self.plugin_config.clone(),
                )
            });
        let published_toolsets = installed
            .as_ref()
            .map(|snapshot| snapshot.resolved_spec.plugin_config.agent.toolsets.clone())
            .unwrap_or_default();
        let session_tools = self
            .session_slots
            .read(thread, |slot| slot.tools.clone())
            .flatten();
        let toolsets = session_tools
            .as_ref()
            .map(|tools| tools.toolsets.clone())
            .unwrap_or(published_toolsets);
        // Management tools (ADR-0052) remain pre-authorized: they are read-only,
        // and only the reserved-scope assistant's config names them.
        let mut admin_ids: Vec<String> = self
            .admin_tools
            .iter()
            .map(|t| t.id().to_string())
            .collect();
        if managed_coordination {
            admin_ids.extend([
                awaken_ext_builtin_tools::LIST_AGENTS.to_string(),
                awaken_ext_builtin_tools::SEND_TO_AGENT.to_string(),
            ]);
        }
        let has_explicit_tool_policy = config_permission_ruleset(published_configuration.plugins())
            .is_some()
            || !toolsets.is_empty();
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
            .unwrap_or_else(|| {
                if installed.is_some() {
                    crate::skills::MANAGED_SKILLS_SUBDIR.to_string()
                } else {
                    crate::skills::DEFAULT_SKILLS_SUBDIR.to_string()
                }
            });
        let repository_skill_roots = if installed.is_some()
            && self.session_allows_repository_skill_discovery(thread, installed.as_ref())
        {
            let mut roots = vec![skills_subdir.clone()];
            roots.extend(
                self.session_slots
                    .read(thread, |slot| {
                        slot.resources
                            .repositories
                            .iter()
                            .map(|repository| {
                                format!(
                                    "{}/{}",
                                    repository
                                        .plan
                                        .mount_path
                                        .trim_start_matches('/')
                                        .trim_end_matches('/'),
                                    crate::skills::MANAGED_SKILLS_SUBDIR
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            );
            roots.sort();
            roots.dedup();
            roots
        } else {
            Vec::new()
        };
        let authorization =
            effective_tool_authorization(&published_configuration, &pre_authorized, &toolsets);
        let permission = authorization.policy.clone();
        let base_gate = authorization.gate.clone();
        // R1/R2: the runtime is built with the host default executor; each run then
        // resolves its *effective* model (its `model_ref_override`, else its snapshot
        // binding) to an executor at the resolve seam and sets it on the run context.
        // Resolving per Run — not once at Session build — means a per-Run model
        // switch needs no session rebuild, and a database-less worker runs the
        // configured model without a session-level registry.
        let mut runtime = match env.as_ref() {
            Some(env) => {
                build_runtime_with_authorization(self.llm.clone(), env.as_ref(), &authorization)
            }
            None => build_runtime_with_authorization(
                self.llm.clone(),
                &crate::config::DeferredHandToolSource,
                &authorization,
            ),
        };
        // Register the management tool executables globally (ADR-0052 D3): the
        // registry stays global, the compile-time scope fence is what restricts them.
        for tool in &self.admin_tools {
            runtime = runtime.with_tool(tool.clone());
        }
        // Delegation is a runtime concern: inject the executor so the kernel runs
        // `agent_run` as a sub-agent (native or remote), not the tool registry.
        if has_published_multiagent
            && !managed_coordination
            && env.is_none()
            && !self.deployment.disable_local_pool
        {
            return Err(HostError::internal(
                "remote-only execution cannot host local delegate targets",
            ));
        }
        if managed_coordination {
            let coordinator = coordination_endpoint
                .clone()
                .map(|endpoint| Arc::new(crate::coordination::HostAgentCoordinator::new(endpoint)));
            if let Some(coordinator) = coordinator {
                for tool in awaken_ext_builtin_tools::coordination_tools(coordinator) {
                    runtime = runtime.with_tool(tool);
                }
            }
            // Advisor is an internal target of the existing durable child-Run
            // service. Installing the shared service does not re-enable
            // `agent_run`: Runtime dispatch additionally requires the exact
            // AgentDelegation descriptor, which the Managed surface removed.
            if has_published_advisor
                && let Some(env) = env.clone()
                && let Some(service) = self.run_delegation(
                    thread,
                    env,
                    commit.clone(),
                    installed.as_ref(),
                    publication_source.clone(),
                )?
            {
                runtime = runtime.with_run_delegation(service);
            }
        } else if let Some(env) = env.clone()
            && let Some(service) = self.run_delegation(
                thread,
                env,
                commit.clone(),
                installed.as_ref(),
                publication_source.clone(),
            )?
        {
            runtime = runtime.with_run_delegation(service);
        }
        // Skills are fronted by two stable tools (ADR-0036); all skill behavior is
        // in `awaken-ext-skills`. The host only wires the pieces it alone owns —
        // the sandbox env, the sub-run capability, and the base gate — via
        // `skills::wire_skills`.
        let mut skill_descriptors = Vec::new();
        let mut semantic_skill_tools = None;
        let mut skill_registry: Option<Arc<dyn SkillRegistry>> = None;
        let mut session_content_plugins: Vec<Arc<dyn awaken_runtime_contract::plugin::Plugin>> =
            Vec::new();
        // A managed Session consumes its exact frozen Skill versions. An embedded
        // direct Session without a manifest reads its configured Skill catalog.
        let delivered = if frozen_skill_versions.is_some() {
            frozen_skill_versions
        } else {
            self.skills
                .has_application()
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
            &repository_skill_roots,
            commit.clone(),
            content_delivery == crate::session_slot::ManagedContentDelivery::ManagedFilesystem
                && !self.deployment.disable_local_pool,
        )
        .await
        .map_err(HostError::internal)?
        {
            if content_delivery == crate::session_slot::ManagedContentDelivery::ManagedFilesystem {
                let prompt = crate::skills::managed_filesystem_prompt(wiring.registry.as_ref());
                self.session_slots
                    .update(thread, |slot| slot.skill_prompt = prompt);
            } else {
                if installed.is_some() && !is_acp {
                    session_content_plugins.push(Arc::new(
                        crate::session_tools::SessionToolPlugin::new(
                            "awaken.session.skills",
                            wiring.descriptors.clone(),
                            vec![wiring.list_tool.clone(), wiring.activate_tool.clone()],
                        )
                        .map_err(HostError::internal)?,
                    ));
                }
                if !is_acp {
                    runtime = runtime
                        .with_gate(wiring.gate)
                        .with_tool(wiring.list_tool.clone())
                        .with_tool(wiring.activate_tool.clone());
                }
                skill_descriptors = wiring.descriptors.clone();
                semantic_skill_tools = Some((
                    wiring.descriptors,
                    vec![wiring.list_tool, wiring.activate_tool],
                ));
            }
            skill_registry = Some(wiring.registry);
        } else {
            self.session_slots
                .update(thread, |slot| slot.skill_prompt = None);
        }
        let mut memory_descriptors = Vec::new();
        let mut semantic_memory_tools = None;
        if content_delivery == crate::session_slot::ManagedContentDelivery::SemanticTools {
            let bindings = self
                .session_slots
                .read(thread, |slot| slot.memory_bindings.clone())
                .unwrap_or_default();
            if let Some(wiring) = crate::session_memory_tools::SessionMemoryTools::new(bindings) {
                if installed.is_some() && !is_acp {
                    session_content_plugins.push(Arc::new(
                        crate::session_tools::SessionToolPlugin::new(
                            "awaken.session.memory",
                            wiring.descriptors.clone(),
                            wiring.executors.clone(),
                        )
                        .map_err(HostError::internal)?,
                    ));
                }
                if !is_acp {
                    for tool in &wiring.executors {
                        runtime = runtime.with_tool(tool.clone());
                    }
                }
                memory_descriptors = wiring.descriptors.clone();
                semantic_memory_tools = Some(wiring);
            }
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
        let selected_memory = if installed.is_some() {
            let binding_id = if published_memory_selected {
                Some(authored_memory.binding_id.as_deref().ok_or_else(|| {
                    HostError::bad_request(
                        "the Awaken memory extension requires an explicit `memory.binding_id`",
                    )
                })?)
            } else {
                None
            };
            self.select_thread_memory_binding(thread, binding_id)
                .map_err(HostError::bad_request)?
        } else {
            // Direct embedders opt in through `bind_resolved_memory`; Managed
            // compatibility Sessions merely register standard mount bindings.
            self.memory_for_thread(thread)
        };
        // Recovery is owned by the frozen Resource bindings, not by whichever
        // Memory plugin the parent Agent selects. This also resumes delegated
        // child intents when the parent itself has no automatic Memory plugin.
        self.bind_thread_memory_recovery(thread, commit.clone())
            .await?;
        let recalled_memory = selected_memory
            .clone()
            .filter(|memory| memory.recall_enabled() && authored_memory.recall_enabled);
        let memory_selector = recalled_memory
            .as_ref()
            .map(
                |_| -> Result<Arc<dyn awaken_ext_memory::RecallSelector>, HostError> {
                    let agent_id = authored_memory
                        .selector_agent_id
                        .as_deref()
                        .unwrap_or(awaken_ext_memory::SELECTOR_AGENT_ID);
                    let fallback = awaken_ext_memory::default_selector_agent(
                        &fallback_model_ref,
                        authored_memory
                            .selector_instructions
                            .as_deref()
                            .filter(|value| !value.trim().is_empty())
                            .unwrap_or(awaken_ext_memory::DEFAULT_SELECTOR_INSTRUCTIONS),
                    );
                    let snapshot = crate::agent_catalog::resolve_auxiliary_snapshot(
                        Some(publication_source.as_ref()),
                        &workspace,
                        agent_id,
                        fallback,
                        authored_memory.selector_instructions.as_deref(),
                    )
                    .map_err(HostError::bad_request)?;
                    Ok(Arc::new(crate::memory::AgentSelector::new(
                        self.llm.clone(),
                        snapshot,
                        commit.clone(),
                    ))
                        as Arc<dyn awaken_ext_memory::RecallSelector>)
                },
            )
            .transpose()?;
        let acp_memory_recall = recalled_memory.as_ref().map(|mem| {
            let recall =
                awaken_ext_memory::MemoryRecall::new(mem.store(), authored_memory.recall.clone());
            match &memory_selector {
                Some(selector) => recall.with_selector(selector.clone()),
                None => recall,
            }
        });
        if let Some(mem) = selected_memory {
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
        let published_compact = installed.as_ref().and_then(|snapshot| {
            snapshot
                .resolved_spec
                .plugin_ids
                .iter()
                .any(|id| id == awaken_ext_compact::COMPACT_PLUGIN_ID)
                .then_some(snapshot)
        });
        let compact_config = match published_compact {
            Some(snapshot) => Some(
                snapshot
                    .resolved_spec
                    .plugin_config
                    .get(awaken_ext_compact::COMPACT_PLUGIN_ID)
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|error| {
                        HostError::bad_request(format!(
                            "invalid published compact plugin config: {error}"
                        ))
                    })?
                    .unwrap_or_default(),
            ),
            None => self
                .compaction
                .as_ref()
                .map(|compaction| compaction.config.clone()),
        };
        let context_policy = match compact_config {
            Some(compact_config) => {
                let fallback = awaken_ext_compact::default_compact_agent(
                    &fallback_model_ref,
                    compact_config
                        .agent_instructions
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or(awaken_ext_compact::DEFAULT_COMPACT_INSTRUCTIONS),
                );
                let snapshot = crate::agent_catalog::resolve_auxiliary_snapshot(
                    Some(publication_source.as_ref()),
                    &workspace,
                    &compact_config.agent_id,
                    fallback,
                    compact_config.agent_instructions.as_deref(),
                )
                .map_err(HostError::bad_request)?;
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
        // A direct embedded Session may author its generated snapshot locally.
        // A published snapshot is immutable, so its Resource-derived Skill
        // descriptors enter through `session_skill_plugin` instead.
        let dynamic_descriptors = if installed.is_some() {
            Vec::new()
        } else {
            skill_descriptors
                .into_iter()
                .chain(memory_descriptors)
                .collect()
        };
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
        let generated_config = installed.is_none();
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
        // The durable Session configuration is a complete execution overlay. It
        // is applied to this per-Session clone, never to the retained immutable
        // Agent publication. Replacing all ClientExecuted descriptors prevents a
        // removed published custom tool from leaking beside an official
        // `agent_with_overrides` surface, while preserving built-in, Skill, MCP,
        // and delegation ownership.
        let mut config_changed = false;
        if let Some(session_tools) = &session_tools {
            config_changed |= project_session_tool_override(
                &mut config,
                session_tools,
                if generated_config {
                    SessionToolsetProjection::ProjectIntoSnapshot
                } else {
                    SessionToolsetProjection::PreservePublished
                },
            );
        }
        if managed_coordination {
            config_changed |=
                project_managed_coordination_surface(&mut config, ManagedCoordinationRole::Primary);
        }
        if config_changed {
            config.recompute_fingerprint().map_err(|error| {
                HostError::internal(format!("fingerprint Session tool projection: {error}"))
            })?;
        }
        let background_tasks_enabled = config
            .resolved_spec
            .plugin_ids
            .iter()
            .any(|id| id == awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID);
        if background_tasks_enabled && is_acp {
            return Err(HostError::bad_request(
                "background_task requires the Native backend so the canonical tool executor remains process-addressable",
            ));
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
            let execution_configuration =
                awaken_ext_builtin_tools::web_search_execution_configuration(&toolsets)
                    .map_err(HostError::bad_request)?;
            let plugin = self.web_search_plugin(thread, execution_configuration);
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
        let web_fetch = if config
            .resolved_spec
            .plugin_ids
            .iter()
            .any(|id| id == awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID)
        {
            let plugin = self.web_fetch_plugin(
                thread,
                awaken_ext_builtin_tools::web_fetch_execution_configuration(&toolsets)
                    .map_err(HostError::bad_request)?,
            );
            if is_acp {
                Some(
                    plugin
                        .configured_tool(
                            config
                                .resolved_spec
                                .plugin_config
                                .get(awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID),
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
            let adapter = match &execution_backend {
                awaken_runtime_contract::resolved::Backend::Acp(backend) => {
                    awaken_run_executor_acp::acp_cli(backend.cli()).ok_or_else(|| {
                        HostError::internal(format!("unknown ACP adapter `{backend}`"))
                    })?
                }
                _ => unreachable!("is_acp is derived from the execution backend"),
            };
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
                    crate::mcp::project_mcp_session_transport(
                        server,
                        &projection.request,
                        relay,
                        &projection.receipt,
                        adapter,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            merge_acp_mcp_servers(publication, staged)?
        } else {
            Vec::new()
        };
        let mut acp_tool_exports = Vec::new();
        if is_acp {
            for (name, label, configured) in [
                ("awaken_web_search", "WebSearch", web_search),
                ("awaken_web_fetch", "WebFetch", web_fetch),
            ] {
                if let Some((export, server)) = self
                    .export_web_tool_for_acp(name, label, configured)
                    .await?
                {
                    acp_mcp_servers = merge_process_local_mcp_servers(acp_mcp_servers, [server])?;
                    acp_tool_exports.push(export);
                }
            }

            let mut descriptors = Vec::new();
            let mut executors = Vec::new();
            if let Some((skill_descriptors, skill_executors)) = semantic_skill_tools.as_ref() {
                descriptors.extend(skill_descriptors.iter().cloned());
                executors.extend(skill_executors.iter().cloned());
            }
            if let Some(memory) = semantic_memory_tools.as_ref() {
                descriptors.extend(memory.descriptors.iter().cloned());
                executors.extend(memory.executors.iter().cloned());
            }
            if !descriptors.is_empty() {
                let (servers, exports) = crate::acp_tool_export::export_tools(
                    self.acp_tool_exporter
                        .as_ref()
                        .ok_or_else(|| {
                            HostError::internal(
                                "ACP Session tools require an installed tool-export adapter",
                            )
                        })?
                        .as_ref(),
                    "awaken_session",
                    descriptors,
                    executors,
                )
                .await
                .map_err(HostError::internal)?;
                acp_mcp_servers = merge_process_local_mcp_servers(acp_mcp_servers, servers)?;
                acp_tool_exports.extend(exports);
            }
        }
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
                acp_memory_recall,
            ));
        let attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor> =
            Arc::new(crate::application::SessionPromptAttemptExecutor::new(
                attempt_executor,
                self.session_slots.clone(),
                thread,
            ));
        let attempt_executor = self
            .attempt_decorator
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
        // The foreground delivery seam (slice C/D): a Run's execution goes through
        // `RunIngress` rather than calling `runtime.start_run` directly. Direct
        // ingress runs inline on the same `runtime`; durable ingress queues the run
        // through a dispatch store first. Both share this thread's `runtime`/`commit`.
        // A database-less Worker sends its terminal commit to the Coordinator.
        // The authenticated dispatch-settlement observer owns remote post-commit
        // Memory observation, after Coordinator commit and before settlement, so
        // the durable intent lands beside authoritative Session truth. Local
        // execution observes here because its commit authority is in this Host.
        let mut terminal_observers: Vec<_> = if self.upstream.is_some() {
            Vec::new()
        } else {
            self.memory_terminal_observer(
                thread,
                &config,
                &fallback_model_ref,
                None,
                publication_source.as_ref(),
                commit.clone(),
            )
            .await?
            .into_iter()
            .collect()
        };
        let tool_executor = if a2a_only {
            None
        } else if content_delivery == crate::session_slot::ManagedContentDelivery::SemanticTools {
            let mut tools = Vec::new();
            if let Some((_, skill_tools)) = semantic_skill_tools.as_ref() {
                tools.extend(skill_tools.iter().cloned());
            }
            if let Some(memory) = semantic_memory_tools.as_ref() {
                tools.extend(memory.executors.iter().cloned());
            }
            Some(Arc::new(
                crate::config::FilesystemFreeAgentToolExecutor::try_new(tools)
                    .map_err(HostError::internal)?,
            )
                as Arc<dyn awaken_runtime_contract::tool::ToolExecutor>)
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
        // Cause/effect rules: terminal observers and Session plugins
        // are additive; an Environment/placement hand overrides only the tool
        // executor; one canonical RuntimeRunContext crosses the ingress boundary.
        let workspace_id = self.thread_workspace(thread).to_owned();
        let run_context = awaken_runtime_contract::RuntimeRunContext::new().with_execution_scope(
            awaken_tenancy::ExecutionScopeRef(awaken_tenancy::ScopeId::from(workspace_id.as_str())),
        );
        let dispatch_claim = self
            .session_slots
            .read(thread, |slot| slot.dispatch_claim.clone())
            .flatten();
        let mut run_context = run_context.with_model_content_materializer(Arc::new(
            crate::model_content_materializer::ResourceModelContentMaterializer::new(
                self.file_content_source.clone(),
                workspace_id,
                thread,
                dispatch_claim,
            ),
        ));
        run_context.request_context = self
            .session_slots
            .read(thread, |slot| slot.request_context.clone())
            .unwrap_or_default();
        let run_context = mcp.plugins.iter().cloned().fold(
            run_context,
            awaken_runtime_contract::RuntimeRunContext::with_session_plugin,
        );
        let run_context = session_content_plugins.into_iter().fold(
            run_context,
            awaken_runtime_contract::RuntimeRunContext::with_session_plugin,
        );
        let run_context = match (session_dispatch, coordination_endpoint) {
            (true, Some(endpoint)) => run_context.with_model_request_gate(Arc::new(
                crate::coordination::HostModelRequestGate::new(endpoint, thread),
            )),
            (false, _) => run_context,
            (true, None) => unreachable!("Session application authority was checked above"),
        };
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
        if background_tasks_enabled {
            let generation = env.as_ref().map_or_else(
                || "brain".to_string(),
                |environment| environment.handle().sandbox_id,
            );
            terminal_observers.push(Arc::new(
                crate::background_task::BackgroundTaskTerminalObserver::new(
                    runtime.clone(),
                    config.clone(),
                    run_context.clone(),
                    commit.clone(),
                    self.memory.background(),
                    awaken_ext_background_task::process_supervisor(),
                    thread.to_string(),
                    generation,
                ),
            ));
        }
        let run_context = terminal_observers.iter().cloned().fold(
            run_context,
            awaken_runtime_contract::RuntimeRunContext::with_terminal_observer,
        );
        let (ingress, durable_ingress, claimed_worker) = self
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
        let runtime_publication_identity = claimed_attempt
            .as_ref()
            .map(|attempt| attempt.identity.clone())
            .or_else(|| {
                unclaimed_runtime_publications
                    .as_deref()
                    .map(|publications| {
                        RuntimePublicationIdentity::from_publications(
                            &config,
                            publications,
                            fallback_model_ref.as_str(),
                        )
                    })
            });
        let ctx = Arc::new(SessionCtx {
            runtime,
            ingress,
            durable,
            durable_ingress,
            claimed_worker,
            runtime_publication_identity,
            config,
            commit,
            attempt_context: run_context,
            terminal_observers,
            stream_checkpoint,
            _acp_tool_exports: acp_tool_exports,
            thread_id,
            env,
            skill_registry,
            cancel: Arc::new(std::sync::Mutex::new(None)),
            active_run: std::sync::Mutex::new(None),
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
        // Direct ingress has no later settlement edge, so reopening it repairs
        // the generic after-commit observer gap from committed Run truth. A
        // durable Runtime must leave this delivery to its guarded Worker/HTTP
        // settlement owner, including ordinary cold contexts opened without a
        // claimed identity.
        if !ctx.durable
            && let Some(run) = ctx.commit.latest_run(&ctx.thread_id)
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
}
