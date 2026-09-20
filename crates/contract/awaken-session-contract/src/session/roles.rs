//! The ten required roles of [`SessionRuntime`]: a runtime that answers `Some` from an
//! accessor owns every method of that role (ADR-0081 capability ladder); neutral behaviour lives in [`super::neutral`].

use super::*;

/// Session realization: projections, leases and binding sinks a Coordinator installs on a Worker-side runtime.
///
/// A role of [`SessionRuntime`]; every method is required, so a runtime that
/// owns the role owns all of it, and one that does not answers `None` from
/// [`SessionRuntime::realization`].
#[async_trait]
pub trait RealizationRuntime: Send + Sync {
    /// Sample the execution host's elapsed clock before a Control request.
    /// A Runtime without this port cannot claim local timed execution proof.
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_runtime_contract::authority_lease::AuthorityProofStarted, RunError>;

    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn crate::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_runtime_contract::authority_lease::AuthorityLeaseTiming,
    ) -> Result<usize, RunError>;

    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &crate::SessionSandboxLayout,
    ) -> Result<(), RunError>;

    /// Install one complete Control-frozen Session projection. Dispatch and
    /// realization are explicit modes of this single port; callers must never
    /// lower publication, baseline, request context, Environment, or lease
    /// fields independently.
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: crate::FrozenSessionProjection,
        mode: crate::SessionProjectionInstallMode,
    ) -> Result<(), RunError>;

    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: crate::ObservedSessionRealizationLease,
    ) -> Result<(), RunError>;

    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &crate::SessionRealizationLease,
    ) -> Result<(), RunError>;

    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(&self, sink: Arc<dyn SessionEnvironmentBindingSink>);
}

/// Session Run coordination: reservations, activation, child threads, tool replies and delegation quiescence.
///
/// A role of [`SessionRuntime`]; every method is required, so a runtime that
/// owns the role owns all of it, and one that does not answers `None` from
/// [`SessionRuntime::coordination`].
#[async_trait]
pub trait CoordinationRuntime: Send + Sync {
    /// Persist one complete self-affine Session Run as an unclaimable reservation.
    /// Implementations must use their existing durable Run dispatch authority;
    /// the default fails closed rather than executing inline.
    async fn reserve_session_run(
        &self,
        command: crate::AdmitSessionRun,
    ) -> Result<crate::SessionRunReservation, RunError>;

    /// Publish one exact reservation only after the Session root committed its
    /// activity epoch. Ordinary Worker claim/execution remains the sole runner.
    async fn activate_session_run(
        &self,
        delivery: crate::SessionRunDelivery,
    ) -> Result<crate::SessionRunActivation, RunError>;

    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        admission: crate::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError>;

    /// Read the exact Run lifecycle from existing committed Thread truth. This
    /// is the recovery observation for batch advancement, not a completion
    /// registry or protocol event cache.
    async fn session_run_state(
        &self,
        session_id: &crate::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, RunError>;

    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError>;

    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: crate::CoordinatedRunCommand,
    ) -> Result<crate::SessionAgentMessageReceipt, RunError>;

    /// Rebuild the Session's coordinated child-Thread links from existing
    /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
    /// relationship repository.
    /// `observed_root` may borrow an atomic prefix already read for this Session
    /// during a larger read-only projection. Implementations must reject a
    /// different root identity and fence enrichment to that prefix. `None`
    /// reads the current prefix; command admission and final validation use it.
    async fn coordinated_threads(
        &self,
        session_id: &crate::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<crate::CoordinatedThreadLink>, RunError>;

    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &crate::SessionId,
        thread_id: &str,
    ) -> Result<Option<Box<dyn SessionThreadLiveSubscription>>, RunError>;

    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        command: crate::SessionAgentReportContinuation,
    ) -> Result<(), RunError>;

    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        session_id: &crate::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), RunError>;

    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        command: &crate::SessionThreadToolReplyCommand,
    ) -> Result<crate::SessionThreadToolReplyFence, RunError>;

    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: crate::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError>;

    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: crate::SessionThreadToolReplyDelivery,
    ) -> Result<StepOutcome, RunError>;

    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<DelegatedRunSnapshot, RunError>;

    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. Success returns the command reconstructed from
    /// that committed prefix; Session policy consumes that value instead of
    /// trusting a second copy supplied by the caller. The fail-closed default
    /// keeps lightweight runtimes from silently becoming a second trusted
    /// command source.
    async fn validate_session_agent_message_source(
        &self,
        command: &crate::SessionAgentMessageCommand,
    ) -> Result<crate::SessionAgentMessageCommand, RunError>;
}

/// Budget and usage accounting for a Session and its threads.
///
/// A role of [`SessionRuntime`]; every method is required, so a runtime that
/// owns the role owns all of it, and one that does not answers `None` from
/// [`SessionRuntime::budget`].
#[async_trait]
pub trait BudgetRuntime: Send + Sync {
    /// Resume an exact `BudgetReached` ticket without injecting transcript
    /// content. Implementations reuse the foreground Run driver or the existing
    /// durable dispatch row; they must not enqueue a replacement Run.
    async fn resume_budget_reached(
        &self,
        delivery: SessionBudgetResumeDelivery,
    ) -> Result<SessionBudgetResumeDisposition, RunError>;

    /// Discover current budget-paused continuations from committed Run tickets
    /// and existing Session-affine dispatch rows. This is a recovery query over
    /// those authorities, never a pause registry.
    async fn session_budget_resume_tickets(
        &self,
        session_id: &crate::SessionId,
    ) -> Result<Vec<SessionBudgetResumeTicket>, RunError>;

    /// The Session's accumulated token usage across all Runs, surfaced on the
    /// session's `usage` field. The default is empty — a runtime that reports no usage
    /// (the deterministic in-process models).
    async fn session_usage(&self, thread: &str) -> Result<SessionUsage, RunError>;

    /// Read usage for one logical Thread from its parent Session partition.
    /// Global stores may use `thread_id` directly; filesystem/SQLite adapters
    /// must retain `session_id` as the physical read owner.
    async fn session_thread_usage(
        &self,
        session_id: &crate::SessionId,
        thread_id: &str,
    ) -> Result<SessionUsage, RunError>;
}

/// Streaming and attributed Run execution, interruption, model rebinding and pending-tool readback.
///
/// A role of [`SessionRuntime`]; every method is required, so a runtime that
/// owns the role owns all of it, and one that does not answers `None` from
/// [`SessionRuntime::streaming_runs`].
#[async_trait]
pub trait StreamingRunRuntime: Send + Sync {
    /// Run one request with its protocol-projected, backend-neutral content
    /// owner. Adapters that do not support attributed capture may use the
    /// compatibility default; runtime hosts override it and persist attribution
    /// on the durable activation.
    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
    ) -> Result<StepOutcome, RunError>;

    /// Run one user request, installing `sink` as the Run's best-effort live-progress
    /// channel so the adapter can project in-flight `stream::Kind` into
    /// `event_start`/`event_delta` previews. The committed [`StepOutcome`] is
    /// identical to [`run`](Self::run) — the sink only mirrors in-flight events. The
    /// default ignores the sink and delegates to `run`, so a host without a
    /// streaming path (or a test double) is unaffected.
    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError>;

    /// Streaming counterpart of [`run_attributed`](Self::run_attributed).
    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError>;

    /// Interrupt the run in flight on `thread` (a `user.interrupt`): cancel it so
    /// an in-progress outcome ends `interrupted`. A no-op when nothing is running.
    async fn interrupt(&self, thread: &str) -> Result<(), RunError>;

    /// Rebind `thread` to `model` for its subsequent Runs (R5, per-Run override).
    /// The default is a no-op, so a host without per-thread model routing is
    /// unaffected; the server impl re-stages the thread's model and evicts the
    /// cached context so the next Run resolves the new executor.
    async fn rebind_model(&self, thread: &str, model: &str) -> Result<(), RunError>;

    /// Whether the thread's selected model accepts a system message after the
    /// conversation has started. Runtimes whose models do not support this can
    /// fail admission before the Managed adapter persists the inbound event.
    async fn supports_mid_conversation_system(&self, thread: &str) -> bool;

    /// The tool currently blocking `thread`, if any. Admission uses this
    /// read-only projection to reject events that cannot legally precede the
    /// matching result; resume remains the sole mutating authority.
    async fn pending_tool(&self, thread: &str) -> Result<Option<Pending>, RunError>;
}

/// Session environment lifecycle: adoption, checkpoints, restore, tool and skill resolution, input application.
///
/// A role of [`SessionRuntime`]; every method is required, so a runtime that
/// owns the role owns all of it, and one that does not answers `None` from
/// [`SessionRuntime::environment`].
#[async_trait]
pub trait EnvironmentRuntime: Send + Sync {
    /// Replace the complete Session-local tool configuration after an
    /// idle-session update.
    /// Implementations rebuild the disposable runtime context; durable desired
    /// state has already committed before this projection call.
    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: crate::SessionToolConfiguration,
    ) -> Result<(), RunError>;

    /// Adopt a previously persisted environment before reopening a Session.
    /// Implementations must validate that the binding belongs to `thread` and
    /// fail closed when it is malformed, unavailable, or owned elsewhere.
    async fn adopt_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), RunError>;

    /// Close Runtime admission and prove that no primary, delegated,
    /// shared-background, MCP, tool, Hand, or child-process effect can mutate
    /// this exact environment generation.
    async fn quiesce_session_environment(
        &self,
        thread: &str,
        operation: &crate::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &crate::SandboxGeneration,
        expected_mcp_generations: &[crate::McpGenerationRef],
    ) -> Result<crate::QuiescenceReceipt, RunError>;

    /// Persist one verified filesystem checkpoint. Replays with the same
    /// operation id must return the same authoritative object receipt.
    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: crate::SandboxCheckpointRequest,
    ) -> Result<crate::CheckpointReceipt, RunError>;

    /// Complete every live source-durability effect without deleting the
    /// source. The aggregate must durably admit the returned receipt before it
    /// invokes physical disposal.
    async fn prepare_checkpoint_source_disposal(
        &self,
        thread: &str,
        preparation: &crate::SourceReleasePreparationEffect,
        generation: &crate::SandboxGeneration,
        source_binding: &str,
    ) -> Result<crate::SourceReleasePreparedReceipt, RunError>;

    /// Physically dispose the source only after the aggregate has durably
    /// committed the exact source-release preparation receipt.
    async fn dispose_prepared_checkpoint_source(
        &self,
        thread: &str,
        disposal: &crate::SourceReleaseDisposal,
    ) -> Result<crate::SourceDisposedReceipt, RunError>;

    /// Restore a distinct environment from the exact durable checkpoint.
    async fn restore_checkpointed_session_environment(
        &self,
        request: crate::SandboxRestoreRequest,
    ) -> Result<crate::RestoreReceipt, RunError>;

    /// Idempotently delete checkpoint bytes at a terminal edge. It never
    /// restores the Environment merely to clean it up.
    async fn delete_session_checkpoint(
        &self,
        thread: &str,
        checkpoint: &crate::SandboxCheckpointRef,
    ) -> Result<(), RunError>;

    /// Resolve the current versions of already-authorized Skill resource ids once
    /// at Session creation. The implementation receives only a trusted Workspace
    /// and resource ids; it performs no principal/role/policy decision.
    async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<crate::ResolvedSkillBinding>, RunError>;

    /// Apply one complete, aggregate-authored Resource transition. Live
    /// add/update/delete, rollback, and restart restoration converge here;
    /// Runtime never guesses the prior generation from a process-local cache or
    /// re-resolves Agent defaults/current Resource configuration.
    async fn apply_session_inputs(
        &self,
        thread: &str,
        transition: &crate::SessionResourceTransition,
    ) -> Result<(), RunError>;
}

/// Committed thread readback: messages, recovery snapshots, run lifecycle, disposition and outcome projections.
///
/// A role of [`SessionRuntime`]; every method is required, so a runtime that
/// owns the role owns all of it, and one that does not answers `None` from
/// [`SessionRuntime::thread_reads`].
#[async_trait]
pub trait ThreadReadRuntime: Send + Sync {
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(&self, thread: &str) -> Result<bool, RunError>;

    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError>;

    /// Read one internally consistent logical Thread prefix through the parent
    /// Session's physical commit partition. The primary Thread is addressed by
    /// `session_id == thread_id`; coordinated children use their own logical
    /// Thread id. Implementations must reuse
    /// the Runtime's authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// rather than assembling messages, state, and tickets from separate reads.
    /// `None` means the logical Thread has no committed Run.
    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &crate::SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>;

    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &crate::SessionId,
        thread_id: &str,
        run_id: &RunId,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>;

    /// Read committed Run lifecycle facts after `cursor`. The commit log remains
    /// the sole authority; Managed uses this projection to observe Runs accepted
    /// through AI SDK, AG-UI, A2A, or another Coordinator replica.
    async fn committed_run_lifecycle(
        &self,
        thread: &str,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunError>;

    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &crate::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, RunError>;

    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &crate::SessionId,
        thread_id: &str,
    ) -> Result<(), RunError>;

    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<CommittedOutcomeProjection>, RunError>;
}

/// Terminal cleanup effects a Worker performs under a realization lease.
///
/// A role of [`SessionRuntime`]; every method is required, so a runtime that
/// owns the role owns all of it, and one that does not answers `None` from
/// [`SessionRuntime::terminal_cleanup`].
#[async_trait]
pub trait TerminalCleanupRuntime: Send + Sync {
    /// Execute terminal Repository publication under the same realization
    /// generation that owns the cleanup command set and return either its
    /// secret-free receipt or one permanent compare-and-swap rejection. The
    /// caller verifies and durably records that outcome before ordinary root
    /// cleanup may dispose the shared environment. Runtimes must override this
    /// edge and carry the lease to the Repository transport boundary; falling
    /// back would erase the terminal ownership proof.
    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: crate::SessionRepositoryPublicationCommand,
        lease: &crate::SessionRealizationLease,
    ) -> Result<crate::SessionRepositoryPublicationEffect, RunError>;

    /// Make every source-dependent effect durable under the exact aggregate
    /// preparation fence without deleting the physical realization.
    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: crate::SessionTerminalCleanupEffect,
        authorization: crate::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<crate::SessionCleanupPreparation, RunError>;

    /// Retire only a prepared child projection after Control durably accepts
    /// its exact preparation. The shared root Environment must remain until the
    /// aggregate-wide physical disposal is complete.
    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        effect: &crate::SessionTerminalCleanupEffect,
    );

    /// Execute the one aggregate-wide physical disposal after every
    /// preparation is durable. The typed effect carries the exact prepared
    /// predecessor plus the current live successor lease.
    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: crate::SessionTerminalCleanupDisposalEffect,
    ) -> Result<crate::SessionCleanupDisposalReceipt, RunError>;

    /// Retire only the process-local teardown projection after Control has
    /// durably accepted the exact disposal receipt. No provider or Resource
    /// effect is permitted at this edge.
    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &crate::SessionTerminalCleanupDisposalEffect,
    );

    /// Retire a process-local terminal projection after Control reports that
    /// the aggregate has no remaining cleanup work. This closes final receipt
    /// response loss without replaying provider I/O or inventing another
    /// completion registry.
    async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &crate::SessionId,
        lease: &crate::SessionRealizationLease,
    );

    /// Install the frozen facts carried by one terminal assignment without
    /// realizing Resources, adopting an Environment, or writing an ordinary
    /// Environment receipt. Cleanup effects remain gated by the exact action
    /// and lease passed to the preparation/disposal ports above.
    async fn install_terminal_cleanup_assignment(
        &self,
        assignment: &crate::SessionTerminalCleanupAssignment,
        proof: crate::ObservedSessionRealizationLease,
    ) -> Result<(), RunError>;
}

/// Outcome preparation, definition and continuation.
///
/// A role of [`SessionRuntime`]; every method is required, so a runtime that
/// owns the role owns all of it, and one that does not answers `None` from
/// [`SessionRuntime::outcomes`].
#[async_trait]
pub trait OutcomeRuntime: Send + Sync {
    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, RunError>;

    /// Convenience composition over the canonical prepare/continue phases.
    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeDrive, RunError>;

    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(&self, thread: &str) -> Result<Option<OutcomeDrive>, RunError>;
}

/// Live inbox mutation and snapshot for a running thread.
///
/// A role of [`SessionRuntime`]; every method is required, so a runtime that
/// owns the role owns all of it, and one that does not answers `None` from
/// [`SessionRuntime::live_inbox`].
#[async_trait]
pub trait LiveInboxRuntime: Send + Sync {
    /// The live-inbox queue on `thread`'s in-flight Run. The default reports
    /// an inactive queue, so a host without live-inbox wiring is unaffected.
    async fn live_inbox_snapshot(&self, thread: &str) -> LiveInboxSnapshot;

    /// Queue a message onto `thread`'s in-flight Run; it is folded into the
    /// running transcript at the next safe boundary. Fails `Inactive` when no
    /// native Run is executing (the caller should send a normal event instead).
    async fn live_inbox_queue(
        &self,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxError>;

    /// Delete one queued (not yet consumed) message.
    async fn live_inbox_remove(&self, thread: &str, id: u64) -> Result<(), LiveInboxError>;

    /// Replace one queued message's content, keeping its id and position.
    async fn live_inbox_replace(
        &self,
        thread: &str,
        id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxError>;

    /// Reorder the queue to exactly `order` (a full permutation of current ids).
    async fn live_inbox_reorder(&self, thread: &str, order: Vec<u64>)
    -> Result<(), LiveInboxError>;
}

/// Capability advertisement for the runtime and for one Session.
///
/// A role of [`SessionRuntime`]; every method is required, so a runtime that
/// owns the role owns all of it, and one that does not answers `None` from
/// [`SessionRuntime::capability`].
#[async_trait]
pub trait CapabilityRuntime: Send + Sync {
    /// The advertised capability surface echoed in the session's agent object. The
    /// default reports nothing; a real host overrides it with its built-in tools,
    /// custom tools, skills, and delegate roster so the session enumerates what it does.
    fn capabilities(&self) -> AgentCapabilities;

    /// The capability surface visible to one prepared session. Runtimes whose
    /// catalogs are workspace-scoped override this; simple runtimes inherit the
    /// process-wide view for backwards compatibility.
    fn capabilities_for(&self, thread: &str) -> AgentCapabilities;
}
