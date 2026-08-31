//! Session/context management for [`SharedHost`]: building a thread's commit
//! boundary, stream-checkpoint store, run-delivery ingress, and `ctx_for`.

mod child_substrate;
mod content_delivery;
mod environment_lifecycle;
mod environment_realization;
mod input_projection;

use super::*;
pub(super) use crate::config::{SessionToolsetProjection, project_session_tool_override};
use crate::host::session_ctx::ForegroundRunDelivery;
pub(super) use input_projection::{ManagedCoordinationRole, project_managed_coordination_surface};
use input_projection::{
    merge_acp_mcp_servers, merge_process_local_mcp_servers, pre_authorized_tool_ids,
};

/// One named value for the optional authority inputs that distinguish
/// reservation, fresh-snapshot, claimed, and legacy-recovery Session opens.
/// Keeping these fields together prevents positional `Option`/boolean arguments
/// from silently selecting a different physical-attempt lifecycle path.
struct SessionOpenRequest {
    published_snapshot: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
    claimed_attempt: Option<ClaimedRuntimeInput>,
    force_defer_environment: bool,
    recover_revoked_legacy: bool,
}

pub(crate) struct EnvironmentBindingPersistenceError {
    pub(crate) error: HostError,
}

pub(crate) struct AuthorizedSessionEnvironmentEffect {
    pub(crate) intent: awaken_session_contract::SessionEnvironmentEffectIntent,
    pub(crate) authorization: awaken_session_contract::SessionEnvironmentEffectAuthorization,
    pub(crate) provider_fence: Option<awaken_provisioning_contract::SandboxEffectFence>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionEnvironmentAdoptionDisposition {
    NoBinding,
    Ready,
    RebuildRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionEnvironmentUnavailablePolicy {
    Reject,
    Rebuild,
}

/// Select which side of the single bound-Environment recovery seam may run.
/// A live source effect may only adopt a `Ready` provider without changing its
/// lifecycle marker; terminal recovery may reconstruct the provider's closed
/// cleanup owner. Keeping the choice explicit prevents a pre-delete source
/// effect from accidentally entering terminal takeover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoundEnvironmentPreparationMode {
    LiveSource,
    Terminal,
    /// Re-observe/reconstruct only the physical owner after the aggregate has
    /// durably admitted terminal Preparation. An existing terminal Retiring
    /// owner stays under its original preparation fence; this mode adds no
    /// source-dependent work and exists only for the Disposal boundary.
    TerminalDisposal,
}

/// Borrowed coordinates consumed by the one Session publication/provider
/// selector. Baseline, prospective layout, and genuinely legacy callers differ
/// only in where these immutable facts are stored; selection policy must not.
pub(crate) enum CanonicalSessionProjection<'a> {
    Baseline(&'a awaken_session_contract::SessionBaseline),
    Layout(&'a awaken_session_contract::SessionSandboxLayout),
    LegacyAgent(&'a str),
}

impl CanonicalSessionProjection<'_> {
    fn agent(&self) -> &str {
        match self {
            Self::Baseline(baseline) => &baseline.agent_id,
            Self::Layout(layout) => &layout.agent_id,
            Self::LegacyAgent(agent) => agent,
        }
    }

    fn agent_revision(&self) -> Option<u64> {
        match self {
            Self::Baseline(baseline) => baseline.agent_revision,
            Self::Layout(layout) => layout.agent_revision,
            Self::LegacyAgent(_) => None,
        }
    }

    fn model_override(&self) -> Option<&awaken_session_contract::SessionModelOverride> {
        match self {
            Self::Baseline(baseline) => baseline.model_override.as_ref(),
            Self::Layout(layout) => layout.model_override.as_ref(),
            Self::LegacyAgent(_) => None,
        }
    }

    fn publication_decision(
        &self,
        publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> awaken_session_contract::FrozenAgentPublicationDecision {
        match self {
            Self::Baseline(baseline) => {
                awaken_session_contract::frozen_agent_publication_decision(baseline, publication)
            }
            Self::Layout(layout) => {
                awaken_session_contract::frozen_sandbox_layout_publication_decision(
                    layout,
                    publication,
                )
            }
            Self::LegacyAgent(agent) => {
                awaken_session_contract::legacy_agent_publication_decision(agent, publication)
            }
        }
    }
}

impl SharedHost {
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
        self.commit_for_read_under_lifecycle(thread).await
    }

    /// Read the authoritative commit while the caller already owns this
    /// Session's lifecycle guard. This is the only non-reentrant seam used by
    /// continuation quiescence; ordinary callers use [`Self::commit_for_read`].
    pub(crate) async fn commit_for_read_under_lifecycle(
        &self,
        thread: &str,
    ) -> Result<Arc<HostCommit>, HostError> {
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
    /// foreground-delivery policy. Durable foreground delivery retains the
    /// durable coordinator that owns that same Worker; direct foreground delivery
    /// keeps one concrete attempt driver while retaining the Worker for pool-routed claims.
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
            ForegroundRunDelivery,
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
        let mut worker = awaken_run_ingress::DispatchWorker::new(
            runtime.clone(),
            store,
            commit,
            self.deployment.dispatch_owner.clone(),
        );
        if let Some(checkpoint) = stream_checkpoint {
            worker = worker.with_stream_checkpoint(checkpoint);
        }
        if let Some(materializer) = inference_materializer {
            worker = worker.with_inference_materializer(materializer);
        }
        worker = worker.with_context(run_context);
        if let Some(observer) = settlement_observer {
            worker = worker.with_settlement_observer(observer);
        }
        worker = match &self.worker_stream_publisher {
            Some(publisher) => worker.with_claimed_stream_publisher(publisher.clone()),
            None => worker.with_stream_sink(self.completion.clone()),
        };
        if let Some(capabilities) = local_credential_capabilities {
            worker = worker.with_local_credential_capabilities(capabilities);
        }
        if let Some(resolver) = &self.worker_credential_resolver {
            worker = worker.with_worker_credential_resolver(resolver.clone());
        }
        if let Some(upstream) = &self.upstream {
            worker =
                worker.with_claimed_commit(crate::commit_ingest::remote_claimed_commit(upstream)?);
        }
        if let Some(projection) = recovery_projection {
            worker = worker.with_recovery_projection(projection);
        }
        let claimed_worker = Arc::new(worker);
        claimed_worker.install_attempt_executor(attempt_executor.clone());
        // No per-session recovery sweep here: this session's worker shares one queue
        // with every other, so a claim would grab foreign threads' runs. The
        // process-level `DispatchPool` owns recovery — it claims each crashed run and
        // routes it to the session (this one included) that owns its thread.
        if self.deployment.durable {
            Ok((ForegroundRunDelivery::Durable, claimed_worker))
        } else {
            let direct = Arc::new(DirectAttemptDriver::with_attempt_executor(
                runtime,
                attempt_executor,
            ));
            Ok((ForegroundRunDelivery::Direct(direct), claimed_worker))
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
        self.reconcile_direct_terminal_before_environment(thread, agent)
            .await?;
        self.ctx_for_snapshot(thread, agent, None).await
    }

    /// Build only the immutable dispatch envelope for a Session User Run. A
    /// first reservation must not realize the physical Environment: that effect
    /// belongs to the Worker that later claims the activated row. Existing
    /// resident contexts are reused unchanged.
    pub(crate) async fn ctx_for_session_reservation(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<Arc<SessionCtx>, HostError> {
        self.ctx_for_snapshot_with_attempt(
            thread,
            agent,
            SessionOpenRequest {
                published_snapshot: None,
                claimed_attempt: None,
                force_defer_environment: true,
                recover_revoked_legacy: false,
            },
        )
        .await
    }

    /// Open a session from the executable snapshot carried by a claimed dispatch.
    /// The snapshot is authoritative for the Worker; Environment adoption has
    /// already completed through the lifecycle owner and is read from the slot.
    pub(crate) async fn ctx_for_snapshot(
        &self,
        thread: &str,
        agent: Option<&str>,
        published_snapshot: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<Arc<SessionCtx>, HostError> {
        self.ctx_for_snapshot_with_attempt(
            thread,
            agent,
            SessionOpenRequest {
                published_snapshot,
                claimed_attempt: None,
                force_defer_environment: false,
                recover_revoked_legacy: false,
            },
        )
        .await
    }

    pub(crate) async fn ctx_for_claimed_snapshot(
        &self,
        thread: &str,
        agent: Option<&str>,
        published_snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
        attempt: ClaimedRuntimeInput,
    ) -> Result<Arc<SessionCtx>, HostError> {
        self.ctx_for_snapshot_with_attempt(
            thread,
            agent,
            SessionOpenRequest {
                published_snapshot: Some(published_snapshot),
                claimed_attempt: Some(attempt),
                force_defer_environment: false,
                // Claimed projection synchronization owns typed legacy
                // reconstruction before context construction. Re-entering it
                // here would create a second physical-effect path.
                recover_revoked_legacy: false,
            },
        )
        .await
    }

    pub(crate) async fn ctx_for_resume(&self, thread: &str) -> Result<Arc<SessionCtx>, HostError> {
        self.ctx_for_snapshot_with_attempt(
            thread,
            None,
            SessionOpenRequest {
                published_snapshot: None,
                claimed_attempt: None,
                force_defer_environment: false,
                recover_revoked_legacy: true,
            },
        )
        .await
    }

    fn ctx_for_snapshot_with_attempt<'a>(
        &'a self,
        thread: &'a str,
        agent: Option<&'a str>,
        request: SessionOpenRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Arc<SessionCtx>, HostError>> + Send + 'a>,
    > {
        // Context-construction stack cause/effect table: C1=the caller enters
        // through reservation, snapshot, or claimed execution; C2=the frozen
        // projection requires the full Environment/Runtime realization chain.
        // R1=C1+!C2 may finish from the resident cache; R2=C1+C2 runs the same
        // single realization owner from a heap-backed future. Both rules keep
        // identical lifecycle locking and effects; R2 must not require a larger
        // process/test thread stack merely because more typed stages were added.
        Box::pin(async move {
            let SessionOpenRequest {
                published_snapshot,
                claimed_attempt,
                force_defer_environment,
                recover_revoked_legacy,
            } = request;
            let lifecycle = self
                .session_slots
                .update(thread, |slot| slot.lifecycle.clone());
            let _lifecycle = lifecycle.lock().await;
            self.retry_unpublished_session_environment_cleanup(thread)
                .await?;
            self.retry_prepared_session_environment_publication_under_lifecycle(thread)
                .await?;
            if let Some(ctx) = self
                .session_slots
                .read(thread, |slot| slot.runtime.clone())
                .flatten()
            {
                let cache_matches_attempt = claimed_attempt.as_ref().is_none_or(|attempt| {
                    ctx.runtime_publication_identity.as_ref() == Some(&attempt.identity)
                });
                // A reservation preflight can briefly leave an envelope-only ACP
                // context resident before the claiming Worker reaches this lookup.
                // ACP execution requires its exact physical Environment; Native
                // on-tool-use deferral and A2A remain intentionally environment-free.
                let deferred_acp_context = !force_defer_environment
                    && ctx.env.is_none()
                    && ctx
                        .config
                        .resolved_spec
                        .attempt_candidates(None)
                        .into_iter()
                        .any(|candidate| {
                            awaken_runtime_contract::resolved::Backend::from_ref(
                                &candidate.binding().backend_ref,
                            )
                            .is_acp()
                        });
                if !cache_matches_attempt || deferred_acp_context {
                    // A deferred durable context can survive the crash gap after the
                    // dispatch claim bound a Sandbox but before Session persistence;
                    // likewise, a claim carrying different frozen Runtime inputs
                    // must rebuild around its exact publication source rather than
                    // reuse a warm Runtime.
                    self.session_slots
                        .update(thread, |slot| slot.runtime = None);
                } else {
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
            let (workspace, selected_agent, installed, model_candidate) =
                self.resolve_session_publication(thread, agent, published_snapshot)?;
            self.retain_session_publication(thread, installed.as_ref())?;
            let fallback_model_ref = claimed_attempt.as_ref().map_or_else(
                || {
                    let frozen_model_ref =
                        installed.as_ref().map_or(self.model_ref.as_str(), |root| {
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
            // Managed dispatch is the profile boundary for every Skill source, not
            // merely for its eventual presentation. Read it before any catalog I/O
            // so an empty Managed projection never depends on or reloads the live
            // direct-compatibility catalog.
            let session_dispatch = self
                .session_slots
                .read(thread, |slot| slot.session_dispatch)
                .unwrap_or(false);
            // Direct protocol Sessions have no aggregate Resource projection, but
            // Environment realization still consumes the same exact transition as
            // Managed and claimed execution. Project the direct empty generation
            // once instead of teaching the provider a second baseline-only create
            // path. A Managed/claimed caller must already carry its aggregate-owned
            // transition and therefore remains fail-closed when that fact is absent.
            if claimed_attempt.is_none() && !session_dispatch {
                let empty = awaken_session_contract::SessionResourceManifest::at_revision(
                    workspace.clone(),
                    0,
                    awaken_session_contract::ResolvedSessionResources::default(),
                );
                let transition =
                    awaken_session_contract::SessionResourceTransition::new(empty.clone(), empty)
                        .map_err(|error| HostError::internal(error.to_string()))?;
                self.session_slots.update(thread, |slot| {
                    if slot.resource_transition.is_none() {
                        slot.resource_transition = Some(transition);
                    }
                });
            }
            // Legacy direct Session manifests carry no frozen Skill list. Refresh
            // their canonical delivered catalog before deciding whether
            // `on_tool_use` may defer the Environment; doing this later in Skill
            // wiring can classify a cold cache as instruction-only and then discover
            // support files after the sandbox-free decision has already been made.
            let frozen_skill_versions = self
                .session_slots
                .read(thread, |slot| slot.skills.clone())
                .flatten();
            if !session_dispatch && frozen_skill_versions.is_none() {
                self.skills
                    .reload_cache_in(&workspace)
                    .await
                    .map_err(|error| HostError::internal(error.to_string()))?;
            }
            let published_backend_ref = model_candidate
                .as_ref()
                .map(|candidate| candidate.binding().backend_ref.clone());
            let provisioning = model_candidate
                .as_ref()
                .map(awaken_runtime_contract::resolved::ResolvedModelCandidate::provisioning)
                .unwrap_or(&awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor);
            let execution_backend = published_backend_ref
                .as_deref()
                .map(awaken_runtime_contract::resolved::Backend::from_ref)
                .unwrap_or(awaken_runtime_contract::resolved::Backend::Native);
            // BackgroundTask completion is a process-local fenced projection. Reject
            // unsupported execution placement immediately after resolving the frozen
            // publication and before Environment, MCP, tool, or commit realization.
            // A late rejection would be semantically correct but would still leak a
            // physical Session side effect for work that can never be admitted.
            let background_tasks_enabled = installed.as_ref().map_or_else(
                || {
                    self.plugin_ids
                        .iter()
                        .any(|id| id == awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID)
                },
                |snapshot| {
                    snapshot
                        .resolved_spec
                        .plugin_ids
                        .iter()
                        .any(|id| id == awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID)
                },
            );
            if background_tasks_enabled && execution_backend.is_acp() {
                return Err(HostError::bad_request(
                    "background_task requires the Native backend so the canonical tool executor remains process-addressable",
                ));
            }
            if background_tasks_enabled && self.upstream.is_some() {
                return Err(HostError::bad_request(
                    "background_task requires a co-located Session application until completion attention has a claim-fenced Worker transport",
                ));
            }
            let background_attention = if background_tasks_enabled {
                self.session_background_runs
                    .read()
                    .expect("Session background Run application lock poisoned")
                    .clone()
                    .filter(|application| application.strong_count() > 0)
            } else {
                None
            };
            if background_tasks_enabled && session_dispatch && background_attention.is_none() {
                return Err(HostError::internal(
                    "Managed BackgroundTask Session has no background Run application authority",
                ));
            }
            let a2a_only = installed.as_ref().is_some_and(|snapshot| {
                !crate::host::completion::requires_local_environment(&snapshot.resolved_spec)
            });
            if a2a_only && self.session_has_local_environment_inputs(thread, installed.as_ref()) {
                return Err(HostError::bad_request(
                    "remote A2A execution cannot consume local Session Environment inputs",
                ));
            }
            // A legacy/direct Environment has no encoded durable binding to feed
            // through ordinary adoption. Claimed recovery is owned by the Worker
            // projection synchronizer; only an explicit resume may consume the
            // quiesced owner here, after frozen publication and local-placement
            // validation. Ordinary and remote-only contexts remain fail-closed.
            if recover_revoked_legacy && !a2a_only {
                let provider = self.session_environment_provider(provisioning)?;
                self.rebuild_claimed_legacy_environment_after_revocation(thread, provider)
                    .await?;
            }
            let retained = self
                .session_slots
                .read(thread, |slot| slot.environment_owner.resident())
                .flatten();
            let environment_owner_is_vacant = self.session_environment_owner_is_vacant(thread);
            let environment_owner_has_unpublished_candidate = self
                .session_slots
                .read(thread, |slot| {
                    slot.environment_owner.unpublished_candidate().is_some()
                })
                .unwrap_or(false);
            if a2a_only && !environment_owner_is_vacant {
                return Err(HostError::bad_request(
                    "remote A2A execution cannot bind a local Session Environment",
                ));
            }
            let expected_binding = self.durable_session_environment_binding(thread);
            // Recovery cause/effect decision table: C1 durable binding exists; C2 a
            // matching lifecycle-owned environment is resident. R1 !C1 may create
            // the first environment; R2 C1+C2 reuses it; R3 C1+!C2 fails
            // closed. A failed recovery must never erase C1 by creating a substitute.
            if retained.is_none()
                && expected_binding.is_none()
                && !environment_owner_is_vacant
                && !environment_owner_has_unpublished_candidate
            {
                return Err(HostError::internal(format!(
                    "Session {thread} Environment transition must be recovered before context construction"
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
            let has_published_advisor = installed.as_ref().is_some_and(|snapshot| {
                snapshot.resolved_spec.plugin_config.agent.advisor.is_some()
            });
            let has_published_multiagent = has_published_delegates || has_published_advisor;
            // Managed Sessions expose Anthropic's fixed asynchronous coordination
            // tools. Ordinary/direct SDK sessions retain the existing synchronous
            // `agent_run` path. `session_dispatch` is the projection of the durable
            // Session admission fact; this is not a process-local feature mode.
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
                frozen_skill_versions.as_deref(),
            )?;
            let can_defer = self.can_defer_session_environment(
                thread,
                Some(selected_agent.as_str()),
                installed.as_ref(),
                has_published_multiagent,
            );
            // Reservation cannot create a physical Environment before its immutable
            // dispatch is durable. It must still reject a deterministic substrate
            // mismatch before the Session activity opens; otherwise an impossible
            // mount remains `running` while the queue repeatedly relinquishes it.
            if force_defer_environment
                && !self.deployment.disable_local_pool
                && !a2a_only
                && !can_defer
            {
                let provider = self.session_environment_provider(provisioning)?;
                let spec = self.sandbox_spec_for_provider(thread, provider);
                let _ = self.validate_session_environment_capabilities(provider, &spec)?;
            }
            let deferred = retained.is_none()
                && expected_binding.is_none()
                && (force_defer_environment || can_defer);
            let env = match retained {
                Some(existing) => Some(existing),
                None if a2a_only || deferred => None,
                None if expected_binding.is_some() => {
                    return Err(HostError::internal(format!(
                        "Session {thread} durable Environment was not adopted before context construction"
                    )));
                }
                None => {
                    let provider = self.session_environment_provider(provisioning)?;
                    Some(
                        self.create_reserved_session_environment_under_lifecycle(thread, provider)
                            .await?,
                    )
                }
            };
            let env = match env {
                Some(environment) => Some(
                    self.ensure_published_environment_reconciled_under_lifecycle(
                        thread,
                        environment,
                    )
                    .await?,
                ),
                None => None,
            };
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
                    awaken_ext_builtin_tools::SEND_MESSAGE.to_string(),
                ]);
            }
            let has_explicit_tool_policy =
                config_permission_ruleset(published_configuration.plugins()).is_some()
                    || !toolsets.is_empty();
            let pre_authorized =
                pre_authorized_tool_ids(&mcp.tool_ids, &admin_ids, has_explicit_tool_policy);
            // `plugin_config.skills_dir` is the direct/non-Managed authored-workspace
            // compatibility setting. Managed repository discovery always uses the
            // provider-fixed `.claude/skills` path under each realized mount, while
            // attached Managed bundles materialize under runtime-owned `.skills`.
            let authored_skills_subdir = if session_dispatch {
                crate::skills::MANAGED_SKILLS_SUBDIR.to_string()
            } else {
                installed
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
                    })
            };
            let skill_source_roots = self.session_skill_source_roots(
                thread,
                installed.as_ref(),
                &authored_skills_subdir,
            );
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
                let coordinator = coordination_endpoint.clone().map(|endpoint| {
                    Arc::new(crate::coordination::HostAgentCoordinator::new(endpoint))
                });
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
            // One registry owns Skill discovery/body resolution. Managed Sessions
            // project exact attached bytes plus the realized-repository snapshot
            // only through the filesystem; direct callers may adapt the same
            // registry to the legacy two-tool contract.
            let mut skill_descriptors = Vec::new();
            let mut semantic_skill_tools = None;
            let mut skill_registry: Option<Arc<dyn SkillRegistry>> = None;
            let mut session_content_plugins: Vec<Arc<dyn awaken_runtime_contract::plugin::Plugin>> =
                Vec::new();
            // A Managed Session consumes exact frozen versions for attached Skills.
            // Its separate admitted repository source is derived below from the
            // already-realized Resource projection. Only a direct Session without a
            // frozen manifest may fall back to the live compatibility catalog.
            let delivered = frozen_skill_versions.or_else(|| {
                (!session_dispatch && self.skills.has_application())
                    .then(|| self.skills.cache_snapshot_in(&workspace))
            });
            // A published Agent receives exactly its selected Skills. Embedded direct
            // Sessions without a publication use the host-configured catalog.
            let selected_skills = content_delivery::published_skill_ids(installed.as_ref());
            let filtered_specs: Vec<SkillSpec> = if session_dispatch {
                // Managed attached-Skill truth is the resolved binding plus its
                // exact frozen version bytes. Host-static specs have neither and
                // remain a direct-session compatibility source only; realized
                // repository files enter through `skill_source_roots` instead.
                Vec::new()
            } else {
                match &selected_skills {
                    Some(selected) => self
                        .skills
                        .specs()
                        .iter()
                        .filter(|skill| selected.contains(&skill.id))
                        .cloned()
                        .collect(),
                    None => self.skills.specs().to_vec(),
                }
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
            // An embedded/default Agent has no immutable publication to carry its
            // Session-selected host catalog. Freeze the exact configured/delivered
            // ids into the generated snapshot that crosses durable dispatch, so the
            // claimed rebuild applies the same selection filter instead of treating
            // an empty binding list as an explicit denial.
            let generated_skill_bindings = installed.is_none().then(|| {
                let ids = filtered_specs
                    .iter()
                    .map(|skill| skill.id.clone())
                    .chain(
                        filtered_delivered
                            .iter()
                            .flatten()
                            .map(|version| version.skill_id.to_string()),
                    )
                    .collect::<std::collections::BTreeSet<_>>();
                ids.iter()
                    .map(|id| awaken_agent_contract::AgentSkillBinding::custom(id.clone()))
                    .collect::<Vec<_>>()
            });
            if let Some(registry) = crate::skills::build_skill_registry(
                &filtered_specs,
                if session_dispatch {
                    // Managed admission rejects prompts-as-skills, but keep the
                    // construction boundary closed as well: lazy remote registries
                    // cannot become a second Managed Skill authority after recovery.
                    Vec::new()
                } else {
                    mcp.skill_registries.clone()
                },
                filtered_delivered,
                env.clone(),
                &authored_skills_subdir,
                skill_source_roots.as_deref(),
                content_delivery == crate::session_slot::ManagedContentDelivery::ManagedFilesystem,
            )
            .await
            .map_err(HostError::internal)?
            {
                if content_delivery
                    == crate::session_slot::ManagedContentDelivery::ManagedFilesystem
                {
                    let prompt = crate::skills::filesystem_skill_prompt(registry.as_ref())
                        .map_err(HostError::internal)?;
                    self.session_slots
                        .update(thread, |slot| slot.skill_prompt = prompt);
                } else {
                    if session_dispatch {
                        return Err(HostError::bad_request(
                            "Managed Agent Skills require filesystem progressive disclosure, but this Session disables every filesystem tool",
                        ));
                    }
                    // The MCP-aware base gate keeps this direct Session's
                    // pre-authorized MCP tools while the adapter narrows active
                    // Skill tools. Managed Sessions never construct this value.
                    let adapter = crate::skills::semantic_skill_adapter(
                        registry.clone(),
                        env.clone(),
                        self.llm.clone(),
                        &self.model_ref,
                        thread,
                        base_gate.clone(),
                        sub_base("skill-fork"),
                        self.skill_fork_placement,
                        commit.clone(),
                    );
                    if installed.is_some() && !is_acp {
                        session_content_plugins.push(Arc::new(
                            crate::session_tools::SessionToolPlugin::new(
                                "awaken.session.skills",
                                adapter.descriptors.clone(),
                                vec![adapter.list_tool.clone(), adapter.activate_tool.clone()],
                            )
                            .map_err(HostError::internal)?,
                        ));
                    }
                    if !is_acp {
                        runtime = runtime
                            .with_gate(adapter.gate)
                            .with_tool(adapter.list_tool.clone())
                            .with_tool(adapter.activate_tool.clone());
                    }
                    skill_descriptors = adapter.descriptors.clone();
                    semantic_skill_tools = Some((
                        adapter.descriptors,
                        vec![adapter.list_tool, adapter.activate_tool],
                    ));
                }
                skill_registry = Some(registry);
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
                if let Some(wiring) = crate::session_memory_tools::SessionMemoryTools::new(bindings)
                {
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
                let recall = awaken_ext_memory::MemoryRecall::new(
                    mem.store(),
                    authored_memory.recall.clone(),
                );
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
            // The selected plugin id/config pair is the sole compaction authority in
            // both an immutable publication and the generated dispatch snapshot. In
            // particular, a claimed Worker must not reconstruct host defaults after
            // the coordinator froze a non-default threshold.
            let compact_source = installed
                .as_ref()
                .map(|snapshot| &snapshot.resolved_spec)
                .filter(|spec| {
                    spec.plugin_ids
                        .iter()
                        .any(|id| id == awaken_ext_compact::COMPACT_PLUGIN_ID)
                });
            let compact_config = match compact_source {
                Some(spec) => Some(
                    spec.plugin_config
                        .get(awaken_ext_compact::COMPACT_PLUGIN_ID)
                        .cloned()
                        .map(serde_json::from_value::<awaken_ext_compact::CompactConfig>)
                        .transpose()
                        .map_err(|error| {
                            HostError::bad_request(format!(
                                "invalid published compact plugin config: {error}"
                            ))
                        })?
                        .unwrap_or_default(),
                ),
                None if installed.is_none()
                    && self
                        .plugin_ids
                        .iter()
                        .any(|id| id == awaken_ext_compact::COMPACT_PLUGIN_ID) =>
                {
                    Some(
                        self.plugin_config
                            .get(awaken_ext_compact::COMPACT_PLUGIN_ID)
                            .cloned()
                            .map(serde_json::from_value::<awaken_ext_compact::CompactConfig>)
                            .transpose()
                            .map_err(|error| {
                                HostError::bad_request(format!(
                                    "invalid host compact plugin config: {error}"
                                ))
                            })?
                            .unwrap_or_default(),
                    )
                }
                None => None,
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
                    let agent_tool =
                        build_compact_runner(self.llm.clone(), snapshot, commit.clone());
                    let backend = build_compact_backend(agent_tool, self.memory.background());
                    let plugin = CompactPlugin::new(compact_config).with_backend(thread, backend);
                    runtime = runtime.with_plugin(Arc::new(plugin));
                    if !plugin_ids
                        .iter()
                        .any(|id| id == awaken_ext_compact::COMPACT_PLUGIN_ID)
                    {
                        plugin_ids.push(awaken_ext_compact::COMPACT_PLUGIN_ID.to_string());
                    }
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
            if let Some(skills) = generated_skill_bindings {
                config.resolved_spec.plugin_config.agent.skills = skills;
                config_changed = true;
            }
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
                config_changed |= project_managed_coordination_surface(
                    &mut config,
                    ManagedCoordinationRole::Primary,
                );
            }
            if config_changed {
                config.recompute_fingerprint().map_err(|error| {
                    HostError::internal(format!("fingerprint Session tool projection: {error}"))
                })?;
            }
            debug_assert_eq!(
                background_tasks_enabled,
                config
                    .resolved_spec
                    .plugin_ids
                    .iter()
                    .any(|id| id == awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID),
                "Session projections must not rewrite BackgroundTask activation",
            );
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
                        acp_mcp_servers =
                            merge_process_local_mcp_servers(acp_mcp_servers, [server])?;
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
                Arc::new(crate::application::SessionContextAttemptExecutor::new(
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
            // both direct and claimed durable attempts (including recovered runs).
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
            // executor shared by direct delivery and the durable Worker, claimed
            // recovery cannot bypass artifact publication.
            let attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor> =
                Arc::new(crate::run_exec::ArtifactHarvestAttemptExecutor::new(
                    attempt_executor,
                    self.artifact_harvester(),
                ));
            // The foreground delivery seam (slice C/D): a Run's execution goes through
            // the selected delivery path rather than calling the Runtime loop directly. Direct
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
            } else if content_delivery == crate::session_slot::ManagedContentDelivery::SemanticTools
            {
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
            let environment_generation = match env.as_ref() {
                Some(environment) => {
                    self.resident_environment_activity_generation_id(thread, environment)?
                }
                None => "brain".to_string(),
            };
            let run_context = awaken_runtime_contract::RuntimeRunContext::new()
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from(workspace_id.as_str()),
                ))
                .with_tool_execution_admission(
                    self.memory
                        .background()
                        .tool_execution_admission(thread, &environment_generation),
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
                terminal_observers.push(Arc::new(
                    crate::background_task::BackgroundTaskTerminalObserver::new(
                        runtime.clone(),
                        config.clone(),
                        run_context.clone(),
                        commit.clone(),
                        self.memory.background(),
                        awaken_ext_background_task::process_supervisor(),
                        thread.to_string(),
                        environment_generation,
                        background_attention.clone(),
                    ),
                ));
            }
            let run_context = terminal_observers.iter().cloned().fold(
                run_context,
                awaken_runtime_contract::RuntimeRunContext::with_terminal_observer,
            );
            let (delivery, claimed_worker) = self
                .build_ingress(
                    runtime.clone(),
                    attempt_executor,
                    commit.clone(),
                    stream_checkpoint.clone(),
                    run_context.clone(),
                )
                .await?;
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
                delivery,
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
                projection: tokio::sync::Mutex::new(()),
                outcome: tokio::sync::Mutex::new(()),
                command: tokio::sync::Mutex::new(()),
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
            if !ctx.delivery.is_durable()
                && let Some(run) = ctx
                    .commit
                    .authoritative_latest_run(&ctx.thread_id)
                    .await
                    .map_err(HostError::internal)?
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
        })
    }
}
