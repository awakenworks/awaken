//! Neutral behaviour for [`SessionRuntime`] roles a runtime does not own: [`neutral_role!`]
//! writes it once per role and [`NeutralRuntime`] is that behaviour as a value partial doubles forward to.

use super::*;

/// Implement one [`SessionRuntime`] role for a type with the **neutral** behaviour a
/// runtime that does not own the role used to get from the trait defaults: ownership
/// queries answer nothing, reads are empty, writes are refused as unavailable, and
/// streaming delegates to `run`. Test doubles and narrow in-process runtimes use it
/// so the neutral behaviour is written once here instead of copied per fake.
#[macro_export]
macro_rules! neutral_role {
    (RealizationRuntime for $ty:ty) => {
        #[$crate::__macro_support::async_trait::async_trait]
        impl $crate::RealizationRuntime for $ty {
            /// Sample the execution host's elapsed clock before a Control request.
            /// A Runtime without this port cannot claim local timed execution proof.
            fn observe_realization_request(
                &self,
            ) -> Result<
                awaken_runtime_contract::authority_lease::AuthorityProofStarted,
                $crate::RunError,
            > {
                Err($crate::RunError::unavailable(
                    "runtime does not provide an execution authority clock",
                ))
            }

            /// Reconcile the existing Runtime projections through the given Control
            /// owner. Local and Worker deployment use one slot-based selection and
            /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
            async fn renew_session_realization_projections(
                &self,
                _control: &dyn $crate::SessionRealizationControl,
                _owner: &str,
                _runtime_incarnation: &str,
                _timing: awaken_runtime_contract::authority_lease::AuthorityLeaseTiming,
            ) -> Result<usize, $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "runtime does not reconcile realization time proofs",
                ))
            }

            /// Validate the exact provider-effective Sandbox layout without publishing
            /// process-local state or performing provider, cache, Skill, File, Vault,
            /// Repository, or Git I/O. Session creation and Resource mutation call this
            /// before their durable/effect boundaries; Runtime repeats it at the final
            /// realization edge as defense in depth.
            fn validate_session_sandbox_layout(
                &self,
                _thread: &str,
                layout: &$crate::SessionSandboxLayout,
            ) -> Result<(), $crate::RunError> {
                if layout.resources.iter().any(|binding| {
                    matches!(
                        binding.target,
                        awaken_resource_contract::InputResourceId::Repository(_)
                    )
                }) {
                    return Err($crate::RunError::unavailable(
                        "runtime does not implement Repository Sandbox-layout validation",
                    ));
                }
                Ok(())
            }

            /// Install one complete Control-frozen Session projection. Dispatch and
            /// realization are explicit modes of this single port; callers must never
            /// lower publication, baseline, request context, Environment, or lease
            /// fields independently.
            async fn install_session_projection(
                &self,
                _thread: &str,
                _projection: $crate::FrozenSessionProjection,
                _mode: $crate::SessionProjectionInstallMode,
            ) -> Result<(), $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "runtime does not implement complete Session projection installation",
                ))
            }

            /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
            /// an already-installed realization fence. This is the only lease-only
            /// Runtime mutation; it deliberately carries no projection or phase action.
            async fn renew_session_realization_lease(
                &self,
                _thread: &str,
                _lease: $crate::ObservedSessionRealizationLease,
            ) -> Result<(), $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "runtime does not implement Session realization lease renewal",
                ))
            }

            /// Forget one failed realization fence from disposable Runtime state. The
            /// aggregate remains the sole lease authority: this hook only prevents a
            /// later retry from authorizing effects with a lease that Control has
            /// already failed or may have failed before losing its response.
            async fn invalidate_session_realization_lease(
                &self,
                _thread: &str,
                _lease: &$crate::SessionRealizationLease,
            ) -> Result<(), $crate::RunError> {
                Ok(())
            }

            /// Install the durable callback used at the exact sandbox creation boundary.
            fn install_environment_binding_sink(
                &self,
                _sink: Arc<dyn $crate::SessionEnvironmentBindingSink>,
            ) {
            }
        }
    };
    (CoordinationRuntime for $ty:ty) => {
        #[$crate::__macro_support::async_trait::async_trait]
        impl $crate::CoordinationRuntime for $ty {
            /// Persist one complete self-affine Session Run as an unclaimable reservation.
            /// Implementations must use their existing durable Run dispatch authority;
            /// the default fails closed rather than executing inline.
            async fn reserve_session_run(
                &self,
                _command: $crate::AdmitSessionRun,
            ) -> Result<$crate::SessionRunReservation, $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "runtime does not implement durable Session Run reservations",
                ))
            }

            /// Publish one exact reservation only after the Session root committed its
            /// activity epoch. Ordinary Worker claim/execution remains the sole runner.
            async fn activate_session_run(
                &self,
                _delivery: $crate::SessionRunDelivery,
            ) -> Result<$crate::SessionRunActivation, $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "runtime does not implement durable Session Run activation",
                ))
            }

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
                _admission: $crate::AdmittedSessionRun,
                _input_message_ids: Vec<String>,
                _sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
            ) -> Result<$crate::StepOutcome, $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "runtime does not implement durable Session Run observation",
                ))
            }

            /// Read the exact Run lifecycle from existing committed Thread truth. This
            /// is the recovery observation for batch advancement, not a completion
            /// registry or protocol event cache.
            async fn session_run_state(
                &self,
                _session_id: &$crate::SessionId,
                _run_id: &awaken_agent_contract::agent::run::Id,
            ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "runtime does not implement Session Run recovery",
                ))
            }

            /// Read the runtime-owned, durable child-Run relationships for `thread`.
            /// Protocol adapters use this only to rebuild disposable projections after a
            /// restart; the runtime relationship registry remains the sole authority.
            async fn delegated_runs(
                &self,
                _thread: &str,
            ) -> Result<Vec<$crate::DelegatedRun>, $crate::RunError> {
                Ok(Vec::new())
            }

            /// Admit one ordinary child Run after the Session application has validated
            /// the frozen roster and lifecycle. Implementations must use their canonical
            /// durable Run ingress; the default fails closed rather than spawning a
            /// process-local task.
            async fn admit_coordinated_run(
                &self,
                _command: $crate::CoordinatedRunCommand,
            ) -> Result<$crate::SessionAgentMessageReceipt, $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "asynchronous Agent coordination is unsupported",
                ))
            }

            /// Rebuild the Session's coordinated child-Thread links from existing
            /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
            /// relationship repository.
            /// `observed_root` may borrow an atomic prefix already read for this Session
            /// during a larger read-only projection. Implementations must reject a
            /// different root identity and fence enrichment to that prefix. `None`
            /// reads the current prefix; command admission and final validation use it.
            async fn coordinated_threads(
                &self,
                session_id: &$crate::SessionId,
                observed_root: Option<
                    &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
                >,
            ) -> Result<Vec<$crate::CoordinatedThreadLink>, $crate::RunError> {
                if observed_root.is_some_and(|snapshot| snapshot.thread_id.0 != session_id.as_str())
                {
                    return Err($crate::RunError::internal(
                        "coordination query received a snapshot for a different Session root",
                    ));
                }
                Ok(Vec::new())
            }

            /// Subscribe to neutral live observations for one already-admitted root or
            /// child logical Thread. `None` is the fail-closed default: committed events
            /// remain available, but no Runtime without an exact Thread-scoped observer
            /// can accidentally leak previews across Threads. This port is a view over
            /// the Runtime's existing live hub, not another fan-out or replay owner.
            async fn subscribe_session_thread_live(
                &self,
                _session_id: &$crate::SessionId,
                _thread_id: &str,
            ) -> Result<Option<Box<dyn $crate::SessionThreadLiveSubscription>>, $crate::RunError>
            {
                Ok(None)
            }

            /// Admit one Session-approved child report as immutable input of the
            /// deterministic primary Run. Implementations must not stage a parallel
            /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
            async fn continue_session_agent_report(
                &self,
                _command: $crate::SessionAgentReportContinuation,
            ) -> Result<(), $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "asynchronous Agent report continuation is unsupported",
                ))
            }

            /// Interrupt one logical child through its parent Session's physical commit
            /// and dispatch partition. Implementations must not open child-named storage.
            async fn interrupt_session_thread(
                &self,
                _session_id: &$crate::SessionId,
                _child_thread_id: &awaken_agent_contract::agent::thread::Id,
            ) -> Result<(), $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "coordinated Session Thread interruption is unsupported",
                ))
            }

            /// Read and validate the exact committed Awaiting coordinate that a Primary
            /// or child Thread tool reply intends to answer. The command already carries
            /// its admission-time Run/correlation identity; this returns only the current
            /// dispatch activity coordinate. Implementations revalidate the command when
            /// applying the reply so a concurrent resume or later Run fails closed.
            async fn session_thread_tool_reply_fence(
                &self,
                _command: &$crate::SessionThreadToolReplyCommand,
            ) -> Result<$crate::SessionThreadToolReplyFence, $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "Session Thread tool reply fencing is unsupported",
                ))
            }

            /// Reply to the exact pending tool of a Primary or logical child Thread
            /// through the Session-affine claim-fenced resume/commit boundary.
            async fn reply_session_thread_tool(
                &self,
                _delivery: $crate::SessionThreadToolReplyDelivery,
            ) -> Result<(), $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "Session Thread tool replies are unsupported",
                ))
            }

            /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
            /// reply ingress remains identical; implementations additionally register
            /// before publication and project the next committed Step from Thread
            /// truth. No inline executor is authorized by this observation policy.
            async fn reply_and_observe_session_thread_tool(
                &self,
                _delivery: $crate::SessionThreadToolReplyDelivery,
            ) -> Result<$crate::StepOutcome, $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "Session Thread tool reply observation is unsupported",
                ))
            }

            /// Stop the parent Run, wait until it can no longer commit a delegation, and
            /// then read the complete child set from durable runtime authority.
            async fn quiesce_terminal_delegations(
                &self,
                thread: &str,
            ) -> Result<$crate::DelegatedRunSnapshot, $crate::RunError> {
                $crate::StreamingRunRuntime::interrupt(self, thread).await?;
                Ok($crate::DelegatedRunSnapshot {
                    delegated_runs: $crate::CoordinationRuntime::delegated_runs(self, thread)
                        .await?,
                    coordinated_thread_ids: Vec::new(),
                    watermark: 0,
                    runtime_commit_cursor:
                        $crate::ThreadReadRuntime::session_thread_recovery_snapshot(
                            self,
                            &$crate::SessionId::from(thread),
                            thread,
                        )
                        .await?
                        .map(|snapshot| snapshot.store_cursor)
                        .unwrap_or_default(),
                })
            }

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
                _command: &$crate::SessionAgentMessageCommand,
            ) -> Result<$crate::SessionAgentMessageCommand, $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "durable Agent message source validation is unsupported",
                ))
            }
        }
    };
    (BudgetRuntime for $ty:ty) => {
        #[$crate::__macro_support::async_trait::async_trait]
        impl $crate::BudgetRuntime for $ty {
            /// Resume an exact `BudgetReached` ticket without injecting transcript
            /// content. Implementations reuse the foreground Run driver or the existing
            /// durable dispatch row; they must not enqueue a replacement Run.
            async fn resume_budget_reached(
                &self,
                _delivery: $crate::SessionBudgetResumeDelivery,
            ) -> Result<$crate::SessionBudgetResumeDisposition, $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "budget-paused Run resumption is unsupported",
                ))
            }

            /// Discover current budget-paused continuations from committed Run tickets
            /// and existing Session-affine dispatch rows. This is a recovery query over
            /// those authorities, never a pause registry.
            async fn session_budget_resume_tickets(
                &self,
                _session_id: &$crate::SessionId,
            ) -> Result<Vec<$crate::SessionBudgetResumeTicket>, $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "budget-paused Run discovery is unsupported",
                ))
            }

            /// The Session's accumulated token usage across all Runs, surfaced on the
            /// session's `usage` field. The default is empty — a runtime that reports no usage
            /// (the deterministic in-process models).
            async fn session_usage(
                &self,
                _thread: &str,
            ) -> Result<$crate::SessionUsage, $crate::RunError> {
                Ok($crate::SessionUsage::default())
            }

            /// Read usage for one logical Thread from its parent Session partition.
            /// Global stores may use `thread_id` directly; filesystem/SQLite adapters
            /// must retain `session_id` as the physical read owner.
            async fn session_thread_usage(
                &self,
                _session_id: &$crate::SessionId,
                thread_id: &str,
            ) -> Result<$crate::SessionUsage, $crate::RunError> {
                $crate::BudgetRuntime::session_usage(self, thread_id).await
            }
        }
    };
    (StreamingRunRuntime for $ty:ty) => {
        #[$crate::__macro_support::async_trait::async_trait]
        impl $crate::StreamingRunRuntime for $ty {
            /// Run one request with its protocol-projected, backend-neutral content
            /// owner. Adapters that do not support attributed capture may use the
            /// compatibility default; runtime hosts override it and persist attribution
            /// on the durable activation.
            async fn run_attributed(
                &self,
                agent: &str,
                thread: &str,
                content: Vec<
                    $crate::__macro_support::awaken_agent_contract::agent::content::ContentBlock,
                >,
                _data_subject_id: Option<String>,
            ) -> Result<$crate::StepOutcome, $crate::RunError> {
                $crate::SessionRuntime::run(self, agent, thread, content).await
            }

            /// Run one user request, installing `sink` as the Run's best-effort live-progress
            /// channel so the adapter can project in-flight `stream::Kind` into
            /// `event_start`/`event_delta` previews. The committed [`$crate::StepOutcome`] is
            /// identical to [`run`](Self::run) — the sink only mirrors in-flight events. The
            /// default ignores the sink and delegates to `run`, so a host without a
            /// streaming path (or a test double) is unaffected.
            async fn run_streaming(
                &self,
                agent: &str,
                thread: &str,
                content: Vec<
                    $crate::__macro_support::awaken_agent_contract::agent::content::ContentBlock,
                >,
                _sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
            ) -> Result<$crate::StepOutcome, $crate::RunError> {
                $crate::SessionRuntime::run(self, agent, thread, content).await
            }

            /// Streaming counterpart of [`run_attributed`](Self::run_attributed).
            async fn run_streaming_attributed(
                &self,
                agent: &str,
                thread: &str,
                content: Vec<
                    $crate::__macro_support::awaken_agent_contract::agent::content::ContentBlock,
                >,
                _data_subject_id: Option<String>,
                sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
            ) -> Result<$crate::StepOutcome, $crate::RunError> {
                $crate::StreamingRunRuntime::run_streaming(self, agent, thread, content, sink).await
            }

            /// Interrupt the run in flight on `thread` (a `user.interrupt`): cancel it so
            /// an in-progress outcome ends `interrupted`. A no-op when nothing is running.
            async fn interrupt(&self, _thread: &str) -> Result<(), $crate::RunError> {
                Ok(())
            }

            /// Rebind `thread` to `model` for its subsequent Runs (R5, per-Run override).
            /// The default is a no-op, so a host without per-thread model routing is
            /// unaffected; the server impl re-stages the thread's model and evicts the
            /// cached context so the next Run resolves the new executor.
            async fn rebind_model(
                &self,
                _thread: &str,
                _model: &str,
            ) -> Result<(), $crate::RunError> {
                Ok(())
            }

            /// Whether the thread's selected model accepts a system message after the
            /// conversation has started. Runtimes whose models do not support this can
            /// fail admission before the Managed adapter persists the inbound event.
            async fn supports_mid_conversation_system(&self, _thread: &str) -> bool {
                true
            }

            /// The tool currently blocking `thread`, if any. Admission uses this
            /// read-only projection to reject events that cannot legally precede the
            /// matching result; resume remains the sole mutating authority.
            async fn pending_tool(
                &self,
                _thread: &str,
            ) -> Result<Option<$crate::Pending>, $crate::RunError> {
                Ok(None)
            }
        }
    };
    (EnvironmentRuntime for $ty:ty) => {
        #[$crate::__macro_support::async_trait::async_trait]
        impl $crate::EnvironmentRuntime for $ty {
            /// Replace the complete Session-local tool configuration after an
            /// idle-session update.
            /// Implementations rebuild the disposable runtime context; durable desired
            /// state has already committed before this projection call.
            async fn replace_session_tools(
                &self,
                _thread: &str,
                _tools: $crate::SessionToolConfiguration,
            ) -> Result<(), $crate::RunError> {
                Err($crate::RunError::internal(
                    "session tool runtime is unsupported",
                ))
            }

            /// Adopt a previously persisted environment before reopening a Session.
            /// Implementations must validate that the binding belongs to `thread` and
            /// fail closed when it is malformed, unavailable, or owned elsewhere.
            async fn adopt_session_environment(
                &self,
                _agent: &str,
                _thread: &str,
                _binding: &str,
            ) -> Result<(), $crate::RunError> {
                Ok(())
            }

            /// Close Runtime admission and prove that no primary, delegated,
            /// shared-background, MCP, tool, Hand, or child-process effect can mutate
            /// this exact environment generation.
            async fn quiesce_session_environment(
                &self,
                _thread: &str,
                _operation: &$crate::SessionEnvironmentOperation,
                _source_effect_id: &str,
                _source_binding: &str,
                _generation: &$crate::SandboxGeneration,
                _expected_mcp_generations: &[$crate::McpGenerationRef],
            ) -> Result<$crate::QuiescenceReceipt, $crate::RunError> {
                Err($crate::RunError::unavailable_classified(
                    "session_environment_checkpoint_unsupported",
                    "Runtime does not implement Session environment quiescence",
                ))
            }

            /// Persist one verified filesystem checkpoint. Replays with the same
            /// operation id must return the same authoritative object receipt.
            async fn checkpoint_session_environment(
                &self,
                _thread: &str,
                _request: $crate::SandboxCheckpointRequest,
            ) -> Result<$crate::CheckpointReceipt, $crate::RunError> {
                Err($crate::RunError::unavailable_classified(
                    "session_environment_checkpoint_unsupported",
                    "Runtime does not implement Session environment checkpointing",
                ))
            }

            /// Complete every live source-durability effect without deleting the
            /// source. The aggregate must durably admit the returned receipt before it
            /// invokes physical disposal.
            async fn prepare_checkpoint_source_disposal(
                &self,
                _thread: &str,
                _preparation: &$crate::SourceReleasePreparationEffect,
                _generation: &$crate::SandboxGeneration,
                _source_binding: &str,
            ) -> Result<$crate::SourceReleasePreparedReceipt, $crate::RunError> {
                Err($crate::RunError::unavailable_classified(
                    "session_environment_checkpoint_unsupported",
                    "Runtime does not implement checkpoint source release preparation",
                ))
            }

            /// Physically dispose the source only after the aggregate has durably
            /// committed the exact source-release preparation receipt.
            async fn dispose_prepared_checkpoint_source(
                &self,
                _thread: &str,
                _disposal: &$crate::SourceReleaseDisposal,
            ) -> Result<$crate::SourceDisposedReceipt, $crate::RunError> {
                Err($crate::RunError::unavailable_classified(
                    "session_environment_checkpoint_unsupported",
                    "Runtime does not implement prepared checkpoint source disposal",
                ))
            }

            /// Restore a distinct environment from the exact durable checkpoint.
            async fn restore_checkpointed_session_environment(
                &self,
                _request: $crate::SandboxRestoreRequest,
            ) -> Result<$crate::RestoreReceipt, $crate::RunError> {
                Err($crate::RunError::unavailable_classified(
                    "session_environment_checkpoint_unsupported",
                    "Runtime does not implement Session environment restore",
                ))
            }

            /// Idempotently delete checkpoint bytes at a terminal edge. It never
            /// restores the Environment merely to clean it up.
            async fn delete_session_checkpoint(
                &self,
                _thread: &str,
                _checkpoint: &$crate::SandboxCheckpointRef,
            ) -> Result<(), $crate::RunError> {
                Err($crate::RunError::unavailable_classified(
                    "session_environment_checkpoint_unsupported",
                    "Runtime does not implement Session environment checkpoint deletion",
                ))
            }

            /// Resolve the current versions of already-authorized Skill resource ids once
            /// at Session creation. The implementation receives only a trusted Workspace
            /// and resource ids; it performs no principal/role/policy decision.
            async fn resolve_session_skills(
                &self,
                _workspace_id: &str,
                _skills: &[awaken_agent_contract::AgentSkillBinding],
            ) -> Result<Vec<$crate::ResolvedSkillBinding>, $crate::RunError> {
                Ok(Vec::new())
            }

            /// Apply one complete, aggregate-authored Resource transition. Live
            /// add/update/delete, rollback, and restart restoration converge here;
            /// Runtime never guesses the prior generation from a process-local cache or
            /// re-resolves Agent defaults/current Resource configuration.
            async fn apply_session_inputs(
                &self,
                _thread: &str,
                _transition: &$crate::SessionResourceTransition,
            ) -> Result<(), $crate::RunError> {
                Ok(())
            }
        }
    };
    (ThreadReadRuntime for $ty:ty) => {
        #[$crate::__macro_support::async_trait::async_trait]
        impl $crate::ThreadReadRuntime for $ty {
            /// True when durable truth already exists for `thread`. Session-id minting
            /// consults this to skip ids a previous process persisted; implementations
            /// MUST answer without materializing any per-thread state (no context
            /// build, no cache entry) — probing must be free of side effects. The
            /// default reports nothing, so an ephemeral host mints densely from 0.
            async fn owns_thread(&self, _thread: &str) -> Result<bool, $crate::RunError> {
                Ok(false)
            }

            /// The committed transcript for `thread`, in commit order. Used to rehydrate a
            /// session whose in-memory record was lost (e.g. after a process restart) from
            /// durable truth: a non-empty result means the thread exists in the store. The
            /// default reports nothing, so an ephemeral host never rehydrates.
            async fn committed_messages(
                &self,
                _thread: &str,
            ) -> Result<
                Vec<$crate::__macro_support::awaken_agent_contract::agent::message::Message>,
                $crate::RunError,
            > {
                Ok(Vec::new())
            }

            /// Read one internally consistent logical Thread prefix through the parent
            /// Session's physical commit partition. The primary Thread is addressed by
            /// `session_id == thread_id`; coordinated children use their own logical
            /// Thread id. Implementations must reuse
            /// the Runtime's authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
            /// rather than assembling messages, state, and tickets from separate reads.
            /// `None` means the logical Thread has no committed Run.
            async fn session_thread_recovery_snapshot(
                &self,
                _session_id: &$crate::SessionId,
                _thread_id: &str,
            ) -> Result<
                Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
                $crate::RunError,
            > {
                Err($crate::RunError::unavailable(
                    "consistent coordinated Thread recovery is unsupported",
                ))
            }

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
                session_id: &$crate::SessionId,
                thread_id: &str,
                run_id: &$crate::__macro_support::awaken_agent_contract::agent::run::Id,
            ) -> Result<
                Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
                $crate::RunError,
            > {
                let Some(mut snapshot) =
                    $crate::ThreadReadRuntime::session_thread_recovery_snapshot(
                        self, session_id, thread_id,
                    )
                    .await?
                else {
                    return Ok(None);
                };
                if !snapshot.runs.iter().any(|run| &run.id == run_id) {
                    return Ok(None);
                }
                snapshot.claimed_run_id = run_id.clone();
                Ok(Some(snapshot))
            }

            /// Read committed Run lifecycle facts after `cursor`. The commit log remains
            /// the sole authority; Managed uses this projection to observe Runs accepted
            /// through AI SDK, AG-UI, A2A, or another Coordinator replica.
            async fn committed_run_lifecycle(
                &self,
                _thread: &str,
                cursor: $crate::__macro_support::awaken_agent_contract::RunLifecycleCursor,
                _limit: usize,
            ) -> Result<
                $crate::__macro_support::awaken_agent_contract::RunLifecyclePage,
                $crate::RunError,
            > {
                Ok(
                    $crate::__macro_support::awaken_agent_contract::RunLifecyclePage {
                        events: Vec::new(),
                        next_cursor: cursor,
                    },
                )
            }

            /// Read the durable disposition of one logical child Thread through its
            /// parent Session's physical commit partition. Legacy Threads without the
            /// state cell remain Active.
            async fn session_thread_disposition(
                &self,
                _session_id: &$crate::SessionId,
                _thread_id: &str,
            ) -> Result<awaken_agent_contract::ThreadDisposition, $crate::RunError> {
                Ok(awaken_agent_contract::ThreadDisposition::Active)
            }

            /// Commit the absorbing archive disposition on one logical child Thread
            /// through its parent Session partition. Implementations must not mutate a
            /// protocol cache or open child-named storage.
            async fn archive_session_thread(
                &self,
                _session_id: &$crate::SessionId,
                _thread_id: &str,
            ) -> Result<(), $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "durable coordinated Thread archive is unsupported",
                ))
            }

            /// Read one exact terminal Outcome projection from committed Thread truth.
            /// `None` means the id is absent or still active. Implementations must not
            /// resume the Outcome or reconstruct a terminal from an in-process
            /// [`$crate::StepOutcome`].
            async fn committed_outcome_projection(
                &self,
                _thread: &str,
                _outcome_id: &str,
            ) -> Result<Option<$crate::CommittedOutcomeProjection>, $crate::RunError> {
                Ok(None)
            }
        }
    };
    (TerminalCleanupRuntime for $ty:ty) => {
        #[$crate::__macro_support::async_trait::async_trait]
        impl $crate::TerminalCleanupRuntime for $ty {
            /// Execute terminal Repository publication under the same realization
            /// generation that owns the cleanup command set and return either its
            /// secret-free receipt or one permanent compare-and-swap rejection. The
            /// caller verifies and durably records that outcome before ordinary root
            /// cleanup may dispose the shared environment. Runtimes must override this
            /// edge and carry the lease to the Repository transport boundary; falling
            /// back would erase the terminal ownership proof.
            async fn execute_terminal_repository_publication_for_lease(
                &self,
                _command: $crate::SessionRepositoryPublicationCommand,
                _lease: &$crate::SessionRealizationLease,
            ) -> Result<$crate::SessionRepositoryPublicationEffect, $crate::RunError> {
                Err($crate::RunError::unavailable_classified(
                    "session_repository_publication_effect_runtime_unsupported",
                    "runtime does not implement realization-fenced terminal Repository publication",
                ))
            }

            /// Make every source-dependent effect durable under the exact aggregate
            /// preparation fence without deleting the physical realization.
            async fn prepare_terminal_cleanup_for_effect(
                &self,
                _effect: $crate::SessionTerminalCleanupEffect,
                _authorization: $crate::SessionTerminalCleanupPreparationAuthorization,
            ) -> Result<$crate::SessionCleanupPreparation, $crate::RunError> {
                Err($crate::RunError::unavailable_classified(
                    "session_terminal_cleanup_preparation_runtime_unsupported",
                    "runtime does not implement realization-fenced Session cleanup preparation",
                ))
            }

            /// Retire only a prepared child projection after Control durably accepts
            /// its exact preparation. The shared root Environment must remain until the
            /// aggregate-wide physical disposal is complete.
            async fn acknowledge_terminal_cleanup_preparation(
                &self,
                _effect: &$crate::SessionTerminalCleanupEffect,
            ) {
            }

            /// Execute the one aggregate-wide physical disposal after every
            /// preparation is durable. The typed effect carries the exact prepared
            /// predecessor plus the current live successor lease.
            async fn dispose_terminal_cleanup_for_effect(
                &self,
                _effect: $crate::SessionTerminalCleanupDisposalEffect,
            ) -> Result<$crate::SessionCleanupDisposalReceipt, $crate::RunError> {
                Err($crate::RunError::unavailable_classified(
                    "session_terminal_cleanup_disposal_runtime_unsupported",
                    "runtime does not implement realization-fenced Session cleanup disposal",
                ))
            }

            /// Retire only the process-local teardown projection after Control has
            /// durably accepted the exact disposal receipt. No provider or Resource
            /// effect is permitted at this edge.
            async fn acknowledge_terminal_cleanup_disposal(
                &self,
                _effect: &$crate::SessionTerminalCleanupDisposalEffect,
            ) {
            }

            /// Retire a process-local terminal projection after Control reports that
            /// the aggregate has no remaining cleanup work. This closes final receipt
            /// response loss without replaying provider I/O or inventing another
            /// completion registry.
            async fn acknowledge_completed_terminal_cleanup(
                &self,
                _session_id: &$crate::SessionId,
                _lease: &$crate::SessionRealizationLease,
            ) {
            }

            /// Install the frozen facts carried by one terminal assignment without
            /// realizing Resources, adopting an Environment, or writing an ordinary
            /// Environment receipt. Cleanup effects remain gated by the exact action
            /// and lease passed to the preparation/disposal ports above.
            async fn install_terminal_cleanup_assignment(
                &self,
                _assignment: &$crate::SessionTerminalCleanupAssignment,
                _proof: $crate::ObservedSessionRealizationLease,
            ) -> Result<(), $crate::RunError> {
                Err($crate::RunError::unavailable_classified(
                    "session_terminal_projection_runtime_unsupported",
                    "runtime does not implement frozen terminal Session projection installation",
                ))
            }
        }
    };
    (OutcomeRuntime for $ty:ty) => {
        #[$crate::__macro_support::async_trait::async_trait]
        impl $crate::OutcomeRuntime for $ty {
            /// Persist one Outcome aggregate without executing a Worker or Grader Run.
            /// The opaque id is supplied by the application command so crash replay and
            /// activity admission share one stable operation identity.
            async fn prepare_outcome(
                &self,
                _thread: &str,
                _outcome_id: &str,
                _description: &str,
                _rubric: &str,
                _max_iterations: u32,
            ) -> Result<u64, $crate::RunError> {
                Err($crate::RunError::unavailable(
                    "runtime does not implement durable Outcome preparation",
                ))
            }

            /// Convenience composition over the canonical prepare/continue phases.
            async fn define_outcome(
                &self,
                thread: &str,
                description: &str,
                rubric: &str,
                max_iterations: u32,
            ) -> Result<$crate::OutcomeDrive, $crate::RunError> {
                let outcome_id = $crate::session_outcome_convenience_id(
                    thread,
                    description,
                    rubric,
                    max_iterations,
                );
                let _ = $crate::OutcomeRuntime::prepare_outcome(
                    self,
                    thread,
                    &outcome_id,
                    description,
                    rubric,
                    max_iterations,
                )
                .await?;
                $crate::OutcomeRuntime::continue_outcome(self, thread)
                    .await?
                    .ok_or_else(|| $crate::RunError::internal("prepared Outcome is not active"))
            }

            /// Continue the active Outcome after the ordinary Run resume has committed.
            /// `None` proves there is no active aggregate; implementations must rebuild
            /// continuation exclusively from the Thread-owned Outcome state.
            async fn continue_outcome(
                &self,
                _thread: &str,
            ) -> Result<Option<$crate::OutcomeDrive>, $crate::RunError> {
                Ok(None)
            }
        }
    };
    (LiveInboxRuntime for $ty:ty) => {
        #[$crate::__macro_support::async_trait::async_trait]
        impl $crate::LiveInboxRuntime for $ty {
            /// The live-inbox queue on `thread`'s in-flight Run. The default reports
            /// an inactive queue, so a host without live-inbox wiring is unaffected.
            async fn live_inbox_snapshot(&self, _thread: &str) -> $crate::LiveInboxSnapshot {
                $crate::LiveInboxSnapshot::inactive()
            }

            /// Queue a message onto `thread`'s in-flight Run; it is folded into the
            /// running transcript at the next safe boundary. Fails `Inactive` when no
            /// native Run is executing (the caller should send a normal event instead).
            async fn live_inbox_queue(
                &self,
                _thread: &str,
                _content: Vec<
                    $crate::__macro_support::awaken_agent_contract::agent::content::ContentBlock,
                >,
            ) -> Result<u64, $crate::LiveInboxError> {
                Err($crate::LiveInboxError::Inactive)
            }

            /// Delete one queued (not yet consumed) message.
            async fn live_inbox_remove(
                &self,
                _thread: &str,
                _id: u64,
            ) -> Result<(), $crate::LiveInboxError> {
                Err($crate::LiveInboxError::Inactive)
            }

            /// Replace one queued message's content, keeping its id and position.
            async fn live_inbox_replace(
                &self,
                _thread: &str,
                _id: u64,
                _content: Vec<
                    $crate::__macro_support::awaken_agent_contract::agent::content::ContentBlock,
                >,
            ) -> Result<(), $crate::LiveInboxError> {
                Err($crate::LiveInboxError::Inactive)
            }

            /// Reorder the queue to exactly `order` (a full permutation of current ids).
            async fn live_inbox_reorder(
                &self,
                _thread: &str,
                _order: Vec<u64>,
            ) -> Result<(), $crate::LiveInboxError> {
                Err($crate::LiveInboxError::Inactive)
            }
        }
    };
    (CapabilityRuntime for $ty:ty) => {
        #[$crate::__macro_support::async_trait::async_trait]
        impl $crate::CapabilityRuntime for $ty {
            /// The advertised capability surface echoed in the session's agent object. The
            /// default reports nothing; a real host overrides it with its built-in tools,
            /// custom tools, skills, and delegate roster so the session enumerates what it does.
            fn capabilities(&self) -> $crate::AgentCapabilities {
                $crate::AgentCapabilities::default()
            }

            /// The capability surface visible to one prepared session. Runtimes whose
            /// catalogs are workspace-scoped override this; simple runtimes inherit the
            /// process-wide view for backwards compatibility.
            fn capabilities_for(&self, _thread: &str) -> $crate::AgentCapabilities {
                $crate::CapabilityRuntime::capabilities(self)
            }
        }
    };
}

/// The neutral role behaviour as a value: every [`SessionRuntime`] role with the
/// behaviour a runtime that does not own that role used to get from the former
/// trait defaults. Partial test doubles forward the methods they do not script
/// to it, so that behaviour is defined once and read in one place.
#[derive(Clone, Copy, Debug, Default)]
pub struct NeutralRuntime;

#[async_trait]
impl SessionRuntime for NeutralRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::unavailable(
            "the neutral runtime executes nothing",
        ))
    }
    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::unavailable("the neutral runtime resumes nothing"))
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::unavailable("the neutral runtime resumes nothing"))
    }
    fn model(&self) -> String {
        String::new()
    }
    fn realization(&self) -> Option<&dyn RealizationRuntime> {
        Some(self)
    }
    fn coordination(&self) -> Option<&dyn CoordinationRuntime> {
        Some(self)
    }
    fn budget(&self) -> Option<&dyn BudgetRuntime> {
        Some(self)
    }
    fn streaming_runs(&self) -> Option<&dyn StreamingRunRuntime> {
        Some(self)
    }
    fn environment(&self) -> Option<&dyn EnvironmentRuntime> {
        Some(self)
    }
    fn thread_reads(&self) -> Option<&dyn ThreadReadRuntime> {
        Some(self)
    }
    fn terminal_cleanup(&self) -> Option<&dyn TerminalCleanupRuntime> {
        Some(self)
    }
    fn outcomes(&self) -> Option<&dyn OutcomeRuntime> {
        Some(self)
    }
    fn live_inbox(&self) -> Option<&dyn LiveInboxRuntime> {
        Some(self)
    }
    fn capability(&self) -> Option<&dyn CapabilityRuntime> {
        Some(self)
    }
}

crate::neutral_role!(RealizationRuntime for NeutralRuntime);
crate::neutral_role!(CoordinationRuntime for NeutralRuntime);
crate::neutral_role!(BudgetRuntime for NeutralRuntime);
crate::neutral_role!(StreamingRunRuntime for NeutralRuntime);
crate::neutral_role!(EnvironmentRuntime for NeutralRuntime);
crate::neutral_role!(ThreadReadRuntime for NeutralRuntime);
crate::neutral_role!(TerminalCleanupRuntime for NeutralRuntime);
crate::neutral_role!(OutcomeRuntime for NeutralRuntime);
crate::neutral_role!(LiveInboxRuntime for NeutralRuntime);
crate::neutral_role!(CapabilityRuntime for NeutralRuntime);
