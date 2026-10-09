// @formal entrypoint acp.tool_export_requires_claim_execution_environment
// @formal entrypoint background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection
// @formal entrypoint memory.recall_config_contributes_authority_exactly_when_enabled
// @formal entrypoint session_run.reservation_never_executes
// @formal evidence session_run.execution_requires_dispatch_work_and_realization_owner
//! Session/context management for [`SharedHost`]: building a thread's commit
//! boundary, stream-checkpoint store, run-delivery ingress, and `ctx_for`.

mod child_substrate;
mod content_delivery;
mod context;
mod environment_lifecycle;
mod environment_realization;
mod input_projection;
mod run_ingress;

use super::*;
pub(super) use crate::config::{SessionToolsetProjection, project_session_tool_override};
use crate::host::session_ctx::{RuntimeContextPurpose, runtime_context_cache_reuse_admitted};
pub(crate) use content_delivery::SkillDeliveryEvidence;
pub(super) use input_projection::{ManagedCoordinationRole, project_managed_coordination_surface};

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
    /// Store-read proof that the root binding committed before a later
    /// derivative dispatch-cache edge failed. The lifecycle owner consumes
    /// this only to retain the same hidden Candidate under durable identity.
    pub(crate) committed_identity: Option<crate::session_slot::BoundSessionEnvironmentIdentity>,
}

pub(crate) struct AuthorizedSessionEnvironmentEffect {
    pub(crate) intent: awaken_session_state_contract::SessionEnvironmentEffectIntent,
    pub(crate) authorization: awaken_session_state_contract::SessionEnvironmentEffectAuthorization,
    pub(crate) provider_fence: Option<awaken_provisioning_contract::SandboxEffectFence>,
    /// Captured before provider I/O; renewal can advance expiry but cannot
    /// lend this operation another slot allocation or Worker generation.
    pub(crate) provider_authority:
        Option<Arc<dyn awaken_provisioning_contract::CurrentExecutionAuthority>>,
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
    /// The aggregate's terminal Repository command may reuse its exact bound
    /// terminal-retiring owner while the provider is still Ready. It never
    /// reopens ordinary execution. A validated terminal/absent source may only
    /// produce negative publication evidence, never Git I/O; disposing stays
    /// refused without inventing an outcome of an earlier publication.
    TerminalRepositoryPublication,
    Terminal,
    /// Re-observe/reconstruct only the physical owner after the aggregate has
    /// durably admitted terminal Preparation. An existing terminal Retiring
    /// owner stays under its original preparation fence; this mode adds no
    /// source-dependent work and exists only for the Disposal boundary.
    TerminalDisposal,
}

impl BoundEnvironmentPreparationMode {
    fn requires_live_source(self) -> bool {
        matches!(self, Self::LiveSource | Self::TerminalRepositoryPublication)
    }
}

/// Borrowed coordinates consumed by the one Session publication/provider
/// selector. Baseline, prospective layout, and genuinely legacy callers differ
/// only in where these immutable facts are stored; selection policy must not.
pub(crate) enum CanonicalSessionProjection<'a> {
    Baseline(&'a awaken_session_configuration_contract::SessionBaseline),
    Layout(&'a awaken_session_configuration_contract::SessionSandboxLayout),
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

    fn model_override(
        &self,
    ) -> Option<&awaken_session_configuration_contract::SessionModelOverride> {
        match self {
            Self::Baseline(baseline) => baseline.model_override.as_ref(),
            Self::Layout(layout) => layout.model_override.as_ref(),
            Self::LegacyAgent(_) => None,
        }
    }

    fn publication_decision(
        &self,
        publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> awaken_session_realization_contract::FrozenAgentPublicationDecision {
        match self {
            Self::Baseline(baseline) => {
                awaken_session_realization_contract::frozen_agent_publication_decision(
                    baseline,
                    publication,
                )
            }
            Self::Layout(layout) => {
                awaken_session_realization_contract::frozen_sandbox_layout_publication_decision(
                    layout,
                    publication,
                )
            }
            Self::LegacyAgent(agent) => {
                awaken_session_realization_contract::legacy_agent_publication_decision(
                    agent,
                    publication,
                )
            }
        }
    }
}

impl SharedHost {
    /// Open only the authoritative commit/read boundary for a query. A resident
    /// context already owns the exact adapter (including a Worker's recovery
    /// projection); otherwise reconstruct the configured durable adapter without
    /// selecting, creating, or adopting a Session environment.
    pub(crate) async fn commit_for_read(&self, thread: &str) -> Result<Arc<HostCommit>, HostError> {
        let _lifecycle = self.session_slots.lock_lifecycle(thread).await;
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
            .read_run(thread, |run| {
                run.runtime.as_ref().map(|context| context.commit.clone())
            })
            .flatten()
        {
            return Ok(commit);
        }
        self.authority.open_commit(thread).await.map(Arc::new)
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

    /// Build the Coordinator's projection/commit context for Session admission
    /// or foreground observation without consuming the physical Environment.
    /// That substrate belongs to the Worker that claims the activated row; a
    /// Coordinator may know its durable binding but must neither adopt it nor
    /// require a process-local owner. Existing resident contexts are reused.
    pub(crate) async fn ctx_for_session_coordination(
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

    /// Build an ordinary Thread context from one exact immutable snapshot while
    /// deliberately omitting physical Environment realization. Callers must
    /// first prove that the snapshot has no executable capability that needs a
    /// Sandbox; this adapter only selects the existing deferred branch and does
    /// not create a second context-construction path.
    pub(crate) async fn ctx_for_environment_free_snapshot(
        &self,
        thread: &str,
        agent: &str,
        published_snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
    ) -> Result<Arc<SessionCtx>, HostError> {
        self.ctx_for_snapshot_with_attempt(
            thread,
            Some(agent),
            SessionOpenRequest {
                published_snapshot: Some(published_snapshot),
                claimed_attempt: None,
                force_defer_environment: true,
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
            let requested_purpose = if force_defer_environment {
                RuntimeContextPurpose::CoordinationEnvelope
            } else {
                RuntimeContextPurpose::Execution
            };
            let _lifecycle = self.session_slots.lock_lifecycle(thread).await;
            self.retry_unpublished_session_environment_cleanup(thread)
                .await?;
            self.retry_session_environment_candidate_under_lifecycle(thread)
                .await?;
            if let Some(ctx) = self
                .session_slots
                .read_run(thread, |run| run.runtime.clone())
                .flatten()
            {
                let cache_matches_attempt = claimed_attempt.as_ref().is_none_or(|attempt| {
                    ctx.runtime_publication_identity.as_ref() == Some(&attempt.identity)
                });
                if !runtime_context_cache_reuse_admitted(
                    cache_matches_attempt,
                    requested_purpose,
                    ctx.purpose,
                ) {
                    // A deferred durable context can survive the crash gap after the
                    // dispatch claim bound a Sandbox but before Session persistence;
                    // likewise, a claim carrying different frozen Runtime inputs
                    // must rebuild around its exact publication source. A
                    // coordination-only envelope is never an executable cache hit,
                    // even when its publication fingerprint is identical.
                    self.session_slots
                        .update_run(thread, |run| run.runtime = None);
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
            context::construct(
                self,
                thread,
                agent,
                SessionOpenRequest {
                    published_snapshot,
                    claimed_attempt,
                    force_defer_environment,
                    recover_revoked_legacy,
                },
                requested_purpose,
            )
            .await
        })
    }
}
