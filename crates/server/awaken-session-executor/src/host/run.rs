// @formal entrypoint worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal entrypoint worker_drain.grace_expiry_retains_cancellation_observer
//! Run driving for [`ExecutorComponents`]: the neutral `run`/`resume`/`define_outcome`
//! entry points, step finalization, queries, and the durable-ops verbs.

use super::types::VerifiedStepProjection;
use super::*;

mod projection;
mod quiescence;
mod step_verification;

use crate::thread_reads::{
    delegation_registry_from_snapshot, project_delegated_runs, recovery_ticket,
    visible_pending_tool,
};
pub(super) use projection::session_thread_reply_result;
use projection::{StepCommitExpectation, client_result_for_ticket};
use step_verification::{message_prefix_before_exact_inputs, verify_committed_step};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunIngressScope {
    HostCommand,
    ThreadExtension,
}

/// Complete input for one ordinary Host-owned Run delivery. This private value
/// is distinct from `SessionRunDelivery`, whose session activity receipt belongs
/// to the durable Session admission boundary.
struct HostRunDelivery<'a> {
    agent: Option<&'a str>,
    thread: &'a str,
    input: Vec<Message>,
    supersede: bool,
    sink: Option<Arc<dyn StreamSink>>,
    data_subject_id: Option<awaken_runtime_contract::DataSubjectId>,
    ingress_scope: RunIngressScope,
}

impl<'a> HostRunDelivery<'a> {
    fn command(agent: Option<&'a str>, thread: &'a str, input: Vec<Message>) -> Self {
        Self {
            agent,
            thread,
            input,
            supersede: false,
            sink: None,
            data_subject_id: None,
            ingress_scope: RunIngressScope::HostCommand,
        }
    }

    #[cfg(test)]
    fn thread_extension(agent: Option<&'a str>, thread: &'a str, input: Vec<Message>) -> Self {
        Self {
            ingress_scope: RunIngressScope::ThreadExtension,
            ..Self::command(agent, thread, input)
        }
    }

    fn superseding(mut self) -> Self {
        self.supersede = true;
        self
    }

    fn with_stream_sink(mut self, sink: Arc<dyn StreamSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    fn with_data_subject_id(
        mut self,
        data_subject_id: Option<awaken_runtime_contract::DataSubjectId>,
    ) -> Self {
        self.data_subject_id = data_subject_id;
        self
    }
}
impl crate::SessionRuns {
    /// Interrupt the run in flight on `thread`, if any: cancel its token so the
    /// runtime observes it at the next step boundary and ends the run `Cancelled`
    /// (an outcome loop then reports `interrupted`). A no-op when nothing is
    /// running. Never blocks on the run's own execution lock — it only touches the
    /// separate cancel slot — so it works from a concurrent request.
    pub async fn interrupt(&self, thread: &str) -> Result<(), SessionExecutionError> {
        self.interrupt_with_commit_read(thread, self.thread_reads.commit_for_read(thread))
            .await
    }

    /// Use the lock-aware authoritative reader when the caller owns the
    /// lifecycle guard. Cancellation selection and delivery remain identical;
    /// reacquiring that guard through commit_for_read would deadlock revocation.
    pub(crate) async fn interrupt_with_commit_read(
        &self,
        thread: &str,
        commit_read: impl std::future::Future<Output = Result<Arc<HostCommit>, SessionExecutionError>>,
    ) -> Result<(), SessionExecutionError> {
        // Snapshot original controls under the canonical Slot lock before
        // durable I/O. Physical revocation consumes this same selection kernel.
        // An unreadable foreground record refuses the request before any effect.
        let selected = self
            .installed_sessions
            .read_run(
                thread,
                crate::installed_session::SessionRunBinding::select_interruption,
            )?
            .transpose()?
            .unwrap_or_default();
        if !self.authority.cancels_through_durable_intent() {
            // A Worker revokes its resident attempt, not the Coordinator's
            // durable cancellation intent: its transport cannot list, requeue
            // or cancel the global queue. It signals the Runtime registration
            // selected above (by foreground Run or installed claim) and the
            // foreground signal, never a later registration of the same Run.
            if let Some(local) = &selected.local {
                local.interrupt();
            }
            return Ok(());
        }
        let active_run = selected.local.as_ref().and_then(|local| local.active_run());
        let resident = selected.resident;
        if let (Some(ctx), Some(ingress), Some(run_id)) = (
            resident.as_ref(),
            resident
                .as_ref()
                .and_then(|ctx| ctx.delivery.is_durable().then_some(&ctx.claimed_worker)),
            active_run,
        ) {
            if self.dispatch.pool().is_some() {
                // The process pool is the sole durable claim driver. This edge
                // accepts the cancellation intent and returns; its drainer owns
                // the claim-fenced Runtime commit and settlement.
                self.durable_run_control()?
                    .persist_cancellation(run_id, Some(ctx.live_control()))
                    .await?;
            } else {
                // A standalone Host has no autonomous pool. Preserve its direct
                // ingress behavior for tests/embedders while served deployments
                // always take the non-blocking process-pool branch above.
                awaken_dispatch_execution::LiveRunControlService::from_local_owner(
                    (*ingress).clone(),
                    self.coordinator_dispatch()?,
                    self.run_assembly.clock(),
                )
                .map_err(|error| SessionExecutionError::internal(error.to_string()))?
                .cancel(&run_id.0)
                .await
                .map_err(|error| SessionExecutionError::internal(error.to_string()))?;
            }
            if let Some(local) = &selected.local {
                local.interrupt();
            }
            return Ok(());
        }

        // Managed Session Event Runs are reserved and activated directly in the
        // dispatch authority even when ordinary foreground delivery is direct.
        // They intentionally bypass BoundRunExecutor and therefore have no
        // process-local `active_run` hint. Fall back only when that existing
        // authority is installed. A primary Thread can legally have an Awaiting
        // customer-tool Run and queued child-report continuations at once;
        // Session interruption covers every executable row with this exact
        // primary Session affinity, just as child interruption does.
        if self.optional_dispatch_store().is_some() {
            let commit = commit_read.await?;
            let logical_thread_id = resident
                .as_ref()
                .map_or_else(|| ThreadId(thread.to_string()), |ctx| ctx.thread_id.clone());
            let executable = self
                .executable_session_dispatch_runs(thread, &logical_thread_id, &commit)
                .await?;
            for run_id in executable {
                self.durable_run_control()?
                    .persist_recoverable_cancellation(
                        &run_id,
                        resident.as_ref().map(|ctx| {
                            ctx.runtime.as_ref()
                                as &(
                                     dyn awaken_runtime_contract::control::LiveRunControl
                                         + Send
                                         + Sync
                                 )
                        }),
                    )
                    .await?;
            }
        }
        // The selected control owns both original registration and direct/ACP
        // signals. Durable paths reach it only after intent persistence; it
        // never looks up a later registration by mutable Run identity.
        if let Some(local) = &selected.local {
            local.interrupt();
        }
        Ok(())
    }

    /// Run `thread` once: buffered system messages first, then `input`.
    /// Runs to the first pause (an awaiting tool) or the natural end.
    #[tracing::instrument(
        name = "host.run",
        skip_all,
        fields(awaken.thread.id = %thread)
    )]
    pub async fn run(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        self.deliver_run(HostRunDelivery::command(agent, thread, input))
            .await
    }

    /// Run one request with its neutral, request-grained content owner.
    pub async fn run_attributed(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
        data_subject_id: Option<awaken_runtime_contract::DataSubjectId>,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        self.deliver_run(
            HostRunDelivery::command(agent, thread, input).with_data_subject_id(data_subject_id),
        )
        .await
    }

    /// Like [`ExecutorComponents::run`] but forwards the engine's best-effort live
    /// progress to `sink` as the Run executes (the streaming protocol path). The
    /// committed result is identical; the sink only mirrors in-flight events.
    pub async fn run_streaming(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        self.deliver_run(HostRunDelivery::command(agent, thread, input).with_stream_sink(sink))
            .await
    }

    /// Streaming counterpart of [`run_attributed`](Self::run_attributed).
    pub async fn run_streaming_attributed(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
        sink: Arc<dyn StreamSink>,
        data_subject_id: Option<awaken_runtime_contract::DataSubjectId>,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        self.deliver_run(
            HostRunDelivery::command(agent, thread, input)
                .with_stream_sink(sink)
                .with_data_subject_id(data_subject_id),
        )
        .await
    }

    /// Submit a Run that *supersedes* the Thread's prior pending/awaiting work
    /// (ADR-0022, slice E): the newest submission wins, stale dispatches are marked
    /// superseded and never claimed again, then the new run is driven. Requires
    /// durable ingress. Unlike `run` it does not fail closed on an awaiting
    /// thread — superseding an awaiting run is the point.
    pub async fn supersede_run(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        self.deliver_run(HostRunDelivery::command(agent, thread, input).superseding())
            .await
    }

    /// Test-only projection seam for Host behavior whose precondition is an
    /// already-admitted Session. It reuses the complete ordinary delivery
    /// implementation while withholding Session affinity; Session admission,
    /// activity, and Worker settlement are tested by their owning contexts.
    #[cfg(test)]
    pub(super) async fn run_thread_extension_after_admission(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        self.deliver_run(HostRunDelivery::thread_extension(agent, thread, input))
            .await
    }

    async fn deliver_run(
        &self,
        delivery: HostRunDelivery<'_>,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        let HostRunDelivery {
            agent,
            thread,
            input,
            supersede,
            sink,
            data_subject_id,
            ingress_scope,
        } = delivery;
        if ingress_scope == RunIngressScope::HostCommand
            && self
                .installed_sessions
                .read_run(thread, |run| run.session_dispatch)?
                .unwrap_or(false)
        {
            return Err(SessionExecutionError::bad_request(
                "Managed Session roots must use Session-owned Run ingress",
            ));
        }
        let ctx = self.run_assembly.ctx_for(thread, agent).await?;
        let _command = ctx.command.lock().await;
        if ctx
            .commit
            .open_wait_for_thread(&ctx.thread_id)
            .await
            .map_err(SessionExecutionError::internal)?
            .is_some()
            && !supersede
        {
            return Err(SessionExecutionError::bad_request(
                "thread is awaiting a tool decision",
            ));
        }
        if supersede && !ctx.delivery.is_durable() {
            return Err(SessionExecutionError::bad_request(
                "supersede requires durable ingress (set typed durable ingress)",
            ));
        }
        // Recall is injected by the memory plugin's BeforeInference hook (request-only,
        // never committed), so the host does not touch it here.
        let mut messages: Vec<Message> = Vec::new();
        // Expand a user `/skill-name` into the skill's instructions before the Run.
        let input = match &ctx.skill_registry {
            Some(registry) => {
                awaken_ext_skills::expand_slash_commands(registry.as_ref(), thread, input)
                    .map_err(SessionExecutionError::internal)?
            }
            None => input,
        };
        messages.extend(input);
        let (generated_run_id, mut activation) =
            ctx.runtime
                .prepare(&ctx.config, ctx.thread_id.0.clone(), messages);
        // Runtime owns the one restart-unique Run id scheme. Durable execution
        // persists that id before dispatch; a second timestamp/counter mint here
        // would collide across active-active PID-1 containers in the same millisecond.
        let run_id = generated_run_id;
        // Read the baseline from the authoritative recovery contract. In an
        // active-active Coordinator tier this process's synchronous projection
        // may lag commits made by a peer; using it here would project an older
        // Run again when the current Run settles on another replica.
        let baseline = self.authoritative_step_snapshot(&ctx, &run_id).await?;
        let before = baseline.messages.len();
        activation.run_id = run_id.clone();
        activation.model_ref_override = self.agent.inference_routing.override_for(thread)?;
        activation.data_subject_id = data_subject_id;
        let expected_input_ids = activation
            .input
            .iter()
            .map(|message| message.id.0.clone())
            .collect::<Vec<_>>();
        let executor = crate::run_exec::BoundRunExecutor::new(self, ctx.clone())
            .with_supersede(supersede)
            .with_stream_sink(sink)
            .retain_active_until_settled();
        let executor = if ingress_scope == RunIngressScope::ThreadExtension {
            executor.for_thread_extension()
        } else {
            executor
        };
        let state = match awaken_runtime_contract::execution::RunExecutor::execute(
            &executor,
            activation,
            awaken_runtime_contract::RuntimeRunContext::new(),
        )
        .await
        {
            Ok(state) => state,
            Err(error) => {
                Self::clear_active_run(&ctx, &run_id)?;
                return Err(SessionExecutionError::internal(error.to_string()));
            }
        };
        let _projection = ctx.projection.lock().await;
        let result = self
            .finish_active_step(
                &ctx,
                run_id,
                state,
                StepCommitExpectation {
                    messages_before: before,
                    input_ids: &expected_input_ids,
                },
                thread,
            )
            .await?;
        Ok(result)
    }

    /// RunResume through the same in-flight identity slot as a fresh foreground
    /// attempt, so `user.interrupt` is backend-independent while a resumed ACP or
    /// A2A task is executing.
    async fn drive_resume(
        &self,
        ctx: &Arc<SessionCtx>,
        activation: RunActivation,
        command: ResumeCommand,
    ) -> Result<RunState, SessionExecutionError> {
        let run_id = activation.run_id.clone();
        ctx.foreground.begin(run_id.clone())?;
        if ctx.delivery.is_durable() {
            let result = self.resume_durable_foreground(ctx, command).await;
            if result.is_err() {
                Self::clear_active_run(ctx, &run_id)?;
            }
            return result;
        }
        // Direct execution rematerializes an attempt-scoped executor (and a fresh
        // grant when brokered). Durable execution returned above and resumes only
        // through the dispatch worker's claimed path.
        let context = self.native_attempt_context(ctx, &activation).await?;
        let result = ctx
            .delivery
            .direct()
            .ok_or_else(|| {
                SessionExecutionError::internal("direct delivery driver is unavailable")
            })?
            .resume(activation, command, context)
            .await;
        if result.is_err() {
            Self::clear_active_run(ctx, &run_id)?;
        }
        result.map_err(|error| SessionExecutionError::internal(error.to_string()))
    }

    /// The exact Worker composition for a durable Thread. The Worker and its
    /// Dispatch store are the authority; no parallel durable-ingress façade is
    /// retained in Session state.
    pub(crate) async fn durable_worker(
        &self,
        thread: &str,
    ) -> Result<
        Arc<awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>>,
        SessionExecutionError,
    > {
        self.require_durable_delivery()?;
        let ctx = self.run_assembly.ctx_for(thread, None).await?;
        ctx.delivery
            .is_durable()
            .then(|| ctx.claimed_worker.clone())
            .ok_or_else(|| {
                SessionExecutionError::bad_request(
                    "durable delivery not enabled (set typed durable ingress)",
                )
            })
    }

    /// Admit every durable operational verb through the deployment's one typed
    /// capability before it can materialize a Session or query the dispatch
    /// store. The queue remains delivery truth; this guard only selects whether
    /// the deployment exposes that authority.
    pub(super) fn require_durable_delivery(&self) -> Result<(), SessionExecutionError> {
        if self.execution.ingress.is_durable() {
            Ok(())
        } else {
            Err(SessionExecutionError::bad_request(
                "durable ingress not enabled (set typed durable ingress)",
            ))
        }
    }

    /// Read this Thread's committed dispatch rows from the one process dispatch
    /// authority. Operational monitoring observes queue truth only: it must not
    /// materialize a Session context or reopen its frozen Agent publication.
    async fn durable_dispatch_rows(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_dispatch_contract::DispatchSummary>, SessionExecutionError> {
        self.require_durable_delivery()?;
        let thread_id = ThreadId(thread.to_owned());
        self.coordinator_dispatch()?
            .list_dispatches()
            .await
            .map_err(|error| SessionExecutionError::internal(error.to_string()))
            .map(|rows| {
                rows.into_iter()
                    .filter(|row| row.thread_id == thread_id)
                    .collect()
            })
    }

    /// Reconcile `thread`'s dispatch queue (ADR-0011, slice E): reclaim and re-run
    /// any dispatch left runnable by a crash. Returns the recovered run ids.
    pub async fn reconcile(&self, thread: &str) -> Result<Vec<String>, SessionExecutionError> {
        let worker = self.durable_worker(thread).await?;
        let clock = self.clock.clone();
        let mut processed = worker
            .reconcile_committed_terminals(256, self.coordinator_dispatch()?.as_ref())
            .await
            .map_err(|e| SessionExecutionError::internal(e.to_string()))?;
        processed.extend(
            worker
                .run_until_idle(clock)
                .await
                .map_err(|e| SessionExecutionError::internal(e.to_string()))?,
        );
        Ok(processed.into_iter().map(|(id, _)| id.0).collect())
    }

    /// Explicitly quarantine crashed dispatches on `thread` selected by an
    /// operator. Automatic retry exhaustion instead commits terminal Run truth.
    /// `now_ms` is the operator-selected as-of cutoff (ADR-0015, slice E).
    pub async fn quarantine_retry_exhausted(
        &self,
        thread: &str,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, SessionExecutionError> {
        self.durable_worker(thread).await?;
        self.coordinator_dispatch()?
            .quarantine_retry_exhausted(max_attempts, now_ms)
            .await
            .map_err(|e| SessionExecutionError::internal(e.to_string()))
    }

    /// The run ids currently dead-lettered on `thread` (ADR-0015, slice E).
    pub async fn dead_letters(&self, thread: &str) -> Result<Vec<String>, SessionExecutionError> {
        let rows = self.durable_dispatch_rows(thread).await?;
        Ok(rows
            .into_iter()
            .filter(|row| row.state == awaken_dispatch_contract::DispatchState::DeadLetter)
            .map(|row| row.run_id.0)
            .collect())
    }

    /// Return one dead-lettered run to the durable queue with a fresh retry
    /// budget after the operator has repaired the external failure.
    pub async fn requeue_dead_letter(
        &self,
        thread: &str,
        run_id: &str,
    ) -> Result<bool, SessionExecutionError> {
        self.require_durable_delivery()?;
        let store = self.coordinator_dispatch()?;
        let belongs_to_thread = store
            .list_dispatches()
            .await
            .map_err(|error| SessionExecutionError::internal(error.to_string()))?
            .into_iter()
            .any(|row| {
                row.run_id.0 == run_id
                    && row.thread_id.0 == thread
                    && row.state == awaken_dispatch_contract::DispatchState::DeadLetter
            });
        if !belongs_to_thread {
            return Ok(false);
        }
        store
            .requeue(&RunId(run_id.to_owned()))
            .await
            .map_err(|error| SessionExecutionError::internal(error.to_string()))
    }

    /// Operator GC: purge every dead-lettered dispatch on `thread` (ADR-0015,
    /// slice E). Returns how many were removed.
    pub async fn purge_dead_letters(&self, thread: &str) -> Result<usize, SessionExecutionError> {
        self.durable_worker(thread).await?;
        self.coordinator_dispatch()?
            .purge_dead_letters()
            .await
            .map_err(|e| SessionExecutionError::internal(e.to_string()))
    }

    /// An operational snapshot of `thread`'s dispatch queue (ADR-0025): every row
    /// in enqueue order with its status and attempt count — the monitoring surface.
    pub async fn list_dispatches(
        &self,
        thread: &str,
    ) -> Result<Vec<(String, String, u64, bool)>, SessionExecutionError> {
        let rows = self.durable_dispatch_rows(thread).await?;
        Ok(rows
            .into_iter()
            .map(|d| {
                (
                    d.run_id.0,
                    format!("{:?}", d.state),
                    d.attempt_count,
                    d.sandbox_bound,
                )
            })
            .collect())
    }

    /// The run ids superseded by a newer submission on `thread` (ADR-0022,
    /// slice E).
    pub async fn superseded(&self, thread: &str) -> Result<Vec<String>, SessionExecutionError> {
        let rows = self.durable_dispatch_rows(thread).await?;
        Ok(rows
            .into_iter()
            .filter(|row| row.state == awaken_dispatch_contract::DispatchState::Superseded)
            .map(|row| row.run_id.0)
            .collect())
    }

    /// Enqueue a run for the process dispatch pool to drive autonomously (ADR-0011,
    /// slice E follow-up): prepare the activation and hand it to the pool via
    /// `DispatchPool::submit` (durable enqueue + wake), returning immediately with
    /// the run id. The pool drains it out of band — no foreground request drives it
    /// — so the caller observes completion by polling committed truth. Requires
    /// durable ingress (`typed durable ingress`), which spawns the pool.
    pub async fn submit_background_async(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
    ) -> Result<String, SessionExecutionError> {
        // Cause/effect table for durable background admission:
        // B1 cold Thread + enqueue => freeze/validate an environment-free
        //    dispatch; only the later claim owner may realize the Environment.
        // B2 resident Thread + enqueue => reuse its exact retained projection.
        // B3 recovery scan racing either rule => the shared lifecycle owner
        //    still sees one durable identity; admission creates no competing
        //    physical candidate. Calling `ctx_for` here violated B1 by realizing
        //    before the queue commit and let the local pool become a second
        //    effect owner.
        let ctx = self
            .run_assembly
            .ctx_for_session_coordination(thread, agent)
            .await?;
        let mut messages: Vec<Message> = Vec::new();
        messages.extend(input);
        let (uid, mut activation) = ctx
            .runtime
            .prepare(&ctx.config, thread.to_string(), messages);
        // Stamp the Thread's per-Run model override (R2/R5) off the fingerprinted
        // snapshot, so the claiming worker resolves the effective model itself.
        activation.model_ref_override = self.agent.inference_routing.override_for(thread)?;
        activation.run_id = uid.clone();
        let request = self.run_assembly.prepare_dispatch(activation).fresh()?;
        match self.dispatch_pool_or_err() {
            // Normal server: the local pool claims and drives it.
            Ok(pool) => pool
                .submit_dispatch(request)
                .await
                .map_err(|e| SessionExecutionError::internal(e.to_string()))?,
            // Coordinator-only durable server (no local pool): enqueue straight into
            // the shared store so a remote database-less worker drains it over the
            // dispatch transport. Non-durable keeps the original "enable the pool" error.
            Err(e) if self.execution.ingress.is_durable() => {
                let _ = e;
                let store = self.coordinator_dispatch()?;
                store
                    .enqueue(request)
                    .await
                    .map_err(|e| SessionExecutionError::internal(e.to_string()))?;
            }
            Err(e) => return Err(e),
        }
        Ok(uid.0)
    }

    /// Rebuild the sole durable resume target for a foreground root Run whose
    /// committed Awaiting ticket predates dispatch ownership. Budget and tool
    /// replies share this handoff; child Runs must already have a durable row.
    pub(crate) async fn enqueue_foreground_session_resume_dispatch(
        &self,
        session_id: &str,
        ticket: &ResumeTicket,
        session_activity_epoch: u64,
    ) -> Result<Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>, SessionExecutionError> {
        if ticket.thread_id.0 != session_id || session_activity_epoch == 0 {
            return Err(SessionExecutionError::internal(
                "only an exact foreground Session root can enter durable resume",
            ));
        }
        let store = self.coordinator_dispatch()?;
        let ctx = self.run_assembly.ctx_for(session_id, None).await?;
        if ticket.inherited_toolsets.is_none()
            && !ctx
                .config
                .resolved_spec
                .plugin_config
                .agent
                .delegates
                .is_empty()
        {
            return Err(SessionExecutionError::bad_request(
                "legacy Run ticket cannot resume a delegation-capable root without frozen tool sources",
            ));
        }
        let activation = ctx
            .resume_activation(ticket)
            .with_model_ref_override(self.agent.inference_routing.override_for(session_id)?);
        let request = self
            .run_assembly
            .prepare_dispatch(activation)
            .resume()?
            .for_session(ThreadId(session_id.to_string()))
            .with_session_activity_epoch(session_activity_epoch);
        // One exact enqueue closes the foreground-to-durable crash window. A
        // concurrent exact retry is idempotent; a changed Run payload is rejected
        // by the existing dispatch identity fence before any resume is staged.
        store
            .enqueue(request)
            .await
            .map_err(|error| SessionExecutionError::internal(error.to_string()))?;
        Ok(store)
    }

    /// Persist interruption of every executable dispatch for one logical child.
    /// A row whose Run already committed `Ended` is the worker's current
    /// before-settle boundary, not executable work; skipping it lets a failed
    /// boundary cancel raced later continuations without fencing its own claim.
    /// Awaiting native children are resolved by the DispatchWorker's specialized
    /// claim-fenced interruption command; running/remote children retain the
    /// canonical cancellation path.
    pub(crate) async fn interrupt_session_thread(
        &self,
        session_id: &str,
        child_thread_id: &ThreadId,
    ) -> Result<(), SessionExecutionError> {
        if child_thread_id.0 == session_id {
            return Err(SessionExecutionError::bad_request(
                "a coordinated child Thread must differ from its parent Session",
            ));
        }
        let commit = self.thread_reads.commit_for_read(session_id).await?;
        let child_runs = self
            .executable_session_dispatch_runs(session_id, child_thread_id, &commit)
            .await?;
        for run_id in child_runs {
            self.durable_run_control()?
                .persist_recoverable_cancellation(&run_id, None)
                .await?;
        }
        Ok(())
    }

    /// Select interruptible work only from the one Session-affined dispatch
    /// authority. Retry-exhausted work remains interruptible because it still
    /// owns an accepted Session Event; the control path requeues that same row
    /// only to let the canonical cancellation claim settle it. Primary
    /// interruption requires one result; child interruption deliberately
    /// consumes every result so a raced continuation is fenced too.
    async fn executable_session_dispatch_runs(
        &self,
        session_id: &str,
        logical_thread_id: &ThreadId,
        commit: &HostCommit,
    ) -> Result<Vec<RunId>, SessionExecutionError> {
        let store = self.coordinator_dispatch()?;
        let parent = ThreadId(session_id.to_string());
        let rows = store
            .list_dispatches()
            .await
            .map_err(|error| SessionExecutionError::internal(error.to_string()))?;
        Ok(rows
            .into_iter()
            .filter(|dispatch| {
                dispatch.thread_id == *logical_thread_id
                    && dispatch.session_thread_id.as_ref() == Some(&parent)
                    && matches!(
                        dispatch.state,
                        awaken_dispatch_contract::DispatchState::Reserved
                            | awaken_dispatch_contract::DispatchState::ReservationLeased
                            | awaken_dispatch_contract::DispatchState::Pending
                            | awaken_dispatch_contract::DispatchState::Leased
                            | awaken_dispatch_contract::DispatchState::Awaiting
                            | awaken_dispatch_contract::DispatchState::DeadLetter
                    )
            })
            .filter(|dispatch| {
                !commit
                    .run_state(&dispatch.run_id)
                    .is_some_and(|state| state.is_terminal())
            })
            .map(|dispatch| dispatch.run_id)
            .collect())
    }

    pub async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: HostResume,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        let ctx = self.run_assembly.ctx_for_resume(thread).await?;
        let _command = ctx.command.lock().await;
        let (run_id, ticket) = ctx
            .commit
            .open_wait_for_thread(&ctx.thread_id)
            .await
            .map_err(SessionExecutionError::internal)?
            .ok_or_else(|| SessionExecutionError::bad_request("no awaiting run to resume"))?;
        let awaiting_snapshot = self.authoritative_step_snapshot(&ctx, &run_id).await?;

        if matches!(ticket.target(), AwaitTarget::RemoteInput { .. }) {
            if ticket.call_id() != Some(tool_use_id) {
                return Err(SessionExecutionError::bad_request(format!(
                    "tool_use_id {tool_use_id:?} does not match the pending remote input"
                )));
            }
            let HostResume::ClientResult { content, .. } = resume else {
                return Err(SessionExecutionError::bad_request(
                    "awaiting remote input requires a client result",
                ));
            };
            let activation = ctx.resume_activation(&ticket);
            let before = self
                .authoritative_step_snapshot(&ctx, &activation.run_id)
                .await?
                .messages
                .len();
            let command = ResumeCommand::from_ticket(
                &ticket,
                client_result_for_ticket(&ticket, tool_use_id, content, false),
                0,
            );
            let state = self.drive_resume(&ctx, activation, command).await?;
            let _projection = ctx.projection.lock().await;
            let result = self
                .finish_active_step(
                    &ctx,
                    run_id,
                    state,
                    StepCommitExpectation {
                        messages_before: before,
                        input_ids: &[],
                    },
                    thread,
                )
                .await?;
            return Ok(result);
        }

        // A awaiting delegation resumes through the kernel resolver with the user's
        // typed answer; the kernel routes it through the parent relationship to the
        // child's own Run service. The user never resumes the child directly.
        if ticket.reason() == AwaitReason::Delegation {
            let registry = delegation_registry_from_snapshot(&awaiting_snapshot, &run_id)?;
            let child = crate::thread_reads::authoritative_child_ticket(
                ctx.commit.as_ref(),
                registry.as_ref(),
                ticket.call_id(),
            )
            .await?;
            let result = if let Some(child_ticket) = child {
                self.check_pending(&child_ticket, tool_use_id, resume.wants_client())?;
                match resume {
                    HostResume::Permission(decision) => ResumeResult::Permission(decision),
                    HostResume::ClientResult { content, is_error } => {
                        client_result_for_ticket(&child_ticket, tool_use_id, content, is_error)
                    }
                }
            } else {
                // Remote adapters may expose an opaque follow-up without a locally
                // committed child ticket. Keep that adapter boundary as typed user
                // input while still validating the parent call identity.
                if ticket.call_id() != Some(tool_use_id) {
                    return Err(SessionExecutionError::bad_request(format!(
                        "tool_use_id {tool_use_id:?} does not match the pending delegate"
                    )));
                }
                let HostResume::ClientResult { content, .. } = resume else {
                    return Err(SessionExecutionError::bad_request(
                        "awaiting remote Agent input requires a client result",
                    ));
                };
                ResumeResult::Input(awaken_agent_contract::agent::content::extract_text(
                    &content,
                ))
            };
            let activation = ctx.resume_activation(&ticket);
            let before = self
                .authoritative_step_snapshot(&ctx, &activation.run_id)
                .await?
                .messages
                .len();
            let command = ResumeCommand::from_ticket(&ticket, result, 0);
            let state = self.drive_resume(&ctx, activation, command).await?;
            let _projection = ctx.projection.lock().await;
            let result = self
                .finish_active_step(
                    &ctx,
                    run_id,
                    state,
                    StepCommitExpectation {
                        messages_before: before,
                        input_ids: &[],
                    },
                    thread,
                )
                .await?;
            return Ok(result);
        }

        self.check_pending(&ticket, tool_use_id, resume.wants_client())?;
        let (call_id, _) = ticket.tool_call().ok_or_else(|| {
            SessionExecutionError::internal("awaiting run has no pending tool call")
        })?;
        let result = match resume {
            HostResume::Permission(decision) => ResumeResult::Permission(decision),
            HostResume::ClientResult { content, is_error } => {
                let output = if is_error {
                    ToolOutput::error_blocks(call_id, content)
                } else {
                    ToolOutput::ok_blocks(call_id, content)
                };
                ResumeResult::ToolResult(output)
            }
        };
        let activation = ctx.resume_activation(&ticket);
        let before = self
            .authoritative_step_snapshot(&ctx, &activation.run_id)
            .await?
            .messages
            .len();
        let command = ResumeCommand::from_ticket(&ticket, result, 0);
        let state = self.drive_resume(&ctx, activation, command).await?;
        let _projection = ctx.projection.lock().await;
        let result = self
            .finish_active_step(
                &ctx,
                run_id,
                state,
                StepCommitExpectation {
                    messages_before: before,
                    input_ids: &[],
                },
                thread,
            )
            .await?;
        Ok(result)
    }

    fn clear_active_run(ctx: &SessionCtx, run_id: &RunId) -> Result<(), SessionExecutionError> {
        ctx.foreground.release(run_id)?;
        Ok(())
    }

    async fn finish_active_step(
        &self,
        ctx: &SessionCtx,
        run_id: RunId,
        state: RunState,
        expectation: StepCommitExpectation<'_>,
        thread: &str,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        let active_run = run_id.clone();
        let result = self
            .finish_step(ctx, run_id, state, expectation, thread)
            .await;
        Self::clear_active_run(ctx, &active_run)?;
        result
    }

    /// Reuse the canonical claim-recovery snapshot as the exact thread read for
    /// foreground projection. PostgreSQL implements this from one repeatable-read
    /// transaction; local stores expose the same facts without a parallel model.
    pub(super) async fn authoritative_step_snapshot(
        &self,
        ctx: &SessionCtx,
        run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, SessionExecutionError> {
        ctx.commit
            .recovery_snapshot(&ctx.thread_id, run_id)
            .await
            .map_err(|error| SessionExecutionError::internal(error.to_string()))
    }

    /// Project an admitted Session Run from its exact committed input identities.
    /// This supports first delivery and response-loss replay without trusting a
    /// request-local transcript length or creating another committed reader.
    pub(super) async fn project_settled_session_run(
        &self,
        ctx: &Arc<SessionCtx>,
        run_id: RunId,
        state: RunState,
        input_message_ids: &[String],
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        let committed = self.authoritative_step_snapshot(ctx, &run_id).await?;
        let messages_before = message_prefix_before_exact_inputs(&committed, input_message_ids)?;
        let _projection = ctx.projection.lock().await;
        self.finish_step(
            ctx,
            run_id,
            state,
            StepCommitExpectation {
                messages_before,
                input_ids: input_message_ids,
            },
            &ctx.thread_id.0,
        )
        .await
    }

    /// Project the committed suffix after one exact Session-owned resume. The
    /// caller reads `messages_before` before publishing the durable Inbox item;
    /// the resulting proof is otherwise identical to the canonical Host resume
    /// projection.
    pub(super) async fn project_settled_session_resume(
        &self,
        ctx: &Arc<SessionCtx>,
        run_id: RunId,
        state: RunState,
        messages_before: usize,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        let _projection = ctx.projection.lock().await;
        self.finish_step(
            ctx,
            run_id,
            state,
            StepCommitExpectation {
                messages_before,
                input_ids: &[],
            },
            &ctx.thread_id.0,
        )
        .await
    }

    /// Project the step's delta and publish it to the thread hub. Awaiting
    /// position remains exclusively owned by committed Thread truth.
    async fn finish_step(
        &self,
        ctx: &SessionCtx,
        run_id: RunId,
        state: RunState,
        expectation: StepCommitExpectation<'_>,
        thread: &str,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        // Cause/effect: when peer P commits this Run, this Coordinator's local
        // projection is stale but the recovery snapshot contains P's messages,
        // state, and tickets. Project that one committed truth or fail closed;
        // never return an idle response with an empty/old assistant delta.
        let committed = self.authoritative_step_snapshot(ctx, &run_id).await?;
        let new_messages = verify_committed_step(
            &committed,
            &ctx.thread_id,
            &run_id,
            &state,
            expectation.messages_before,
            expectation.input_ids,
        )?;
        let delegation_registry = delegation_registry_from_snapshot(&committed, &run_id)?;
        let (pending, awaiting, await_reason) = match &state {
            RunState::Awaiting => {
                let ticket = recovery_ticket(&committed, &run_id)?;
                let await_reason = ticket.as_ref().map(ResumeTicket::reason);
                let pending = if let Some(ticket) = ticket {
                    // A parent waiting on a child exposes the CHILD's ordinary
                    // interaction request. The protocol still addresses the
                    // parent session; it never obtains a bypass around the
                    // parent-child relationship.
                    visible_pending_tool(ctx.commit.as_ref(), &ticket, delegation_registry.as_ref())
                        .await?
                } else {
                    None
                };
                (pending, true, await_reason)
            }
            _ => (None, false, None),
        };
        if !new_messages.is_empty() {
            self.hub
                .publish(thread, ThreadEvent::Committed(new_messages.clone()));
        }
        self.hub
            .publish(thread, ThreadEvent::StepEnded { awaiting });
        let delegated_runs = project_delegated_runs(delegation_registry.as_ref());
        Ok(CommittedStepReceipt::from_verified(
            VerifiedStepProjection {
                run_id,
                new_messages,
                state,
                pending,
                await_reason,
                delegated_runs,
            },
            &committed,
        ))
    }
}

use awaken_dispatch_contract::{
    DispatchAdministration as _, DispatchEnqueue as _, DispatchMaintenance as _, DispatchReads as _,
};

#[cfg(test)]
mod ticket_projection_tests {
    use super::*;
    use awaken_agent_contract::agent::awaiting::{RemoteInputReason, ToolAwaitReason};

    #[test]
    fn remote_input_wait_projects_as_a_client_executed_agent_input() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect decision table: no pending_tool + UserInput/ExternalEvent
        // -> synthetic client-executed agent_input + ResumeResult::Input;
        // concrete pending_tool -> preserve ordinary client/built-in binding and
        // a client result becomes ToolResult. This test owns the remote row.
        let ticket = ResumeTicket::new(
            "a2a:remote-7:InputRequired",
            RunId("run-7".into()),
            ThreadId("thread-7".into()),
            "snapshot-7",
            "fingerprint-7",
            AwaitTarget::RemoteInput {
                reason: RemoteInputReason::UserInput,
                call_id: "remote-7".into(),
            },
        );

        let pending = Pending::from_resume_ticket(&ticket).expect("visible input");
        assert_eq!(pending.tool_use_id, "remote-7");
        assert_eq!(pending.name, "agent_input");
        assert!(pending.client_executed);
        assert_eq!(pending.input["reason"], "user_input");
        assert_eq!(
            client_result_for_ticket(
                &ticket,
                "remote-7",
                vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "src/lib.rs",
                )],
                false,
            ),
            ResumeResult::Input("src/lib.rs".into())
        );

        // Committed-wait classification cause/effect table. The reason is the
        // authority selected when Runtime committed the ticket; a cold query
        // must not rebuild an Environment merely to inspect a second tool list.
        //
        // | Rule | pending tool | reason          | client executed |
        // | P1   | absent       | UserInput       | yes (agent_input) |
        // | P2   | present      | ExternalEvent   | yes              |
        // | P3   | present      | ToolPermission  | no               |
        let concrete = |reason| {
            ResumeTicket::new(
                "tool-correlation",
                RunId("run-7".into()),
                ThreadId("thread-7".into()),
                "snapshot-7",
                "fingerprint-7",
                AwaitTarget::ToolCall {
                    reason,
                    call_id: "remote-7".into(),
                    tool: awaken_agent_contract::agent::awaiting::PendingTool {
                        tool_id: "submit_answer".into(),
                        arguments: serde_json::json!({"answer": 42}),
                    },
                    authority: ToolCallAuthority::RuntimeToolBatch,
                },
            )
        };
        assert!(
            Pending::from_resume_ticket(&concrete(ToolAwaitReason::ClientExecution))
                .unwrap()
                .client_executed,
            "P2"
        );
        assert!(
            !Pending::from_resume_ticket(&concrete(ToolAwaitReason::Permission))
                .unwrap()
                .client_executed,
            "P3"
        );
    }

    #[test]
    fn session_thread_reply_variants_map_to_the_two_runtime_resume_kinds() {
        // Cause/effect graph: C1 the reply family is confirmation/custom/generic;
        // C2 confirmation is allow/deny with note absent/present; C3 supplied
        // content is normal/error; C4 the committed target is permission,
        // client-execution, or remote input. Effects: E1 confirmation preserves
        // the exact closed PermissionDecision; E2 custom/generic preserve exact
        // blocks and error state; E3 mismatched result/target kinds fail closed.
        // Constraint/invariant: wire provenance lowers once upstream; this Host
        // projection clones the closed decision and the runtime validator remains
        // the terminal target-kind authority. The integration tests named R6/R7
        // additionally prove a rejected mismatch does not consume the await.
        //
        // | Rule | Reply | Target | Effect |
        // |---|---|---|---|
        // | R1 | Confirm Allow, note absent/present | permission | E1 exact Permission |
        // | R2 | Confirm Deny, reason absent | permission | E1 exact Permission |
        // | R3 | Confirm Deny, reason present | permission | E1 exact Permission |
        // | R4 | Custom normal/error | client | E2 exact ToolResult |
        // | R5 | Generic normal/error | client | E2 exact ToolResult |
        // | R6 | Confirm | client/remote | E3 ResultKindMismatch |
        // | R7 | Custom/Result | permission | E3 ResultKindMismatch |
        let target = |reason| AwaitTarget::ToolCall {
            reason,
            call_id: "reply-call".into(),
            tool: awaken_agent_contract::agent::awaiting::PendingTool {
                tool_id: "reply-tool".into(),
                arguments: serde_json::json!({}),
            },
            authority: ToolCallAuthority::RuntimeToolBatch,
        };
        let permission_ticket = ResumeTicket::new(
            "reply-correlation",
            RunId("reply-run".into()),
            ThreadId("reply-thread".into()),
            "reply-snapshot",
            "reply-catalog",
            target(ToolAwaitReason::Permission),
        );
        for (rule, decision) in [
            ("R1a", PermissionDecision::Allow { note: None }),
            (
                "R1b",
                PermissionDecision::Allow {
                    note: Some("approved".into()),
                },
            ),
            ("R2", PermissionDecision::Deny { reason: None }),
            (
                "R3",
                PermissionDecision::Deny {
                    reason: Some("blocked".into()),
                },
            ),
        ] {
            assert_eq!(
                session_thread_reply_result(
                    &permission_ticket,
                    "reply-call",
                    &awaken_session_execution_contract::SessionThreadToolReply::Confirm(
                        decision.clone()
                    ),
                ),
                ResumeResult::Permission(decision),
                "{rule}/E1"
            );
        }

        let result_ticket = ResumeTicket::new(
            "reply-correlation",
            RunId("reply-run".into()),
            ThreadId("reply-thread".into()),
            "reply-snapshot",
            "reply-catalog",
            target(ToolAwaitReason::ClientExecution),
        );
        for (rule, reply, expected_error, expected_text) in [
            (
                "R4a",
                awaken_session_execution_contract::SessionThreadToolReply::Custom {
                    content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                        "custom ok",
                    )],
                    is_error: false,
                },
                false,
                "custom ok",
            ),
            (
                "R4b",
                awaken_session_execution_contract::SessionThreadToolReply::Custom {
                    content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                        "custom failed",
                    )],
                    is_error: true,
                },
                true,
                "custom failed",
            ),
            (
                "R5a",
                awaken_session_execution_contract::SessionThreadToolReply::Result {
                    content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                        "generic ok",
                    )],
                    is_error: false,
                },
                false,
                "generic ok",
            ),
            (
                "R5b",
                awaken_session_execution_contract::SessionThreadToolReply::Result {
                    content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                        "generic failed",
                    )],
                    is_error: true,
                },
                true,
                "generic failed",
            ),
        ] {
            match session_thread_reply_result(&result_ticket, "reply-call", &reply) {
                ResumeResult::ToolResult(output) => {
                    assert_eq!(output.call_id, "reply-call", "{rule}/E2");
                    assert_eq!(output.is_error, expected_error, "{rule}/E2");
                    assert_eq!(output.text(), expected_text, "{rule}/E2");
                }
                other => panic!("{rule} expected ToolResult, got {other:?}"),
            }
        }

        let permission = awaken_session_execution_contract::SessionThreadToolReply::Confirm(
            PermissionDecision::Allow { note: None },
        );
        let client_mismatch = ResumeCommand::from_ticket(
            &result_ticket,
            session_thread_reply_result(&result_ticket, "reply-call", &permission),
            0,
        );
        assert_eq!(
            awaken_runtime_contract::resume::validate_resume(&result_ticket, &client_mismatch),
            Err(awaken_runtime_contract::resume::ResumeError::ResultKindMismatch),
            "R6/E3 client target"
        );
        let remote_ticket = ResumeTicket::new(
            "remote-correlation",
            RunId("remote-run".into()),
            ThreadId("remote-thread".into()),
            "remote-snapshot",
            "remote-catalog",
            AwaitTarget::RemoteInput {
                reason: awaken_agent_contract::agent::awaiting::RemoteInputReason::UserInput,
                call_id: "remote-call".into(),
            },
        );
        let remote_mismatch = ResumeCommand::from_ticket(
            &remote_ticket,
            session_thread_reply_result(&remote_ticket, "remote-call", &permission),
            0,
        );
        assert_eq!(
            awaken_runtime_contract::resume::validate_resume(&remote_ticket, &remote_mismatch),
            Err(awaken_runtime_contract::resume::ResumeError::ResultKindMismatch),
            "R6/E3 remote target"
        );
        let supplied = awaken_session_execution_contract::SessionThreadToolReply::Result {
            content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                "forged",
            )],
            is_error: false,
        };
        let permission_mismatch = ResumeCommand::from_ticket(
            &permission_ticket,
            session_thread_reply_result(&permission_ticket, "reply-call", &supplied),
            0,
        );
        assert_eq!(
            awaken_runtime_contract::resume::validate_resume(
                &permission_ticket,
                &permission_mismatch,
            ),
            Err(awaken_runtime_contract::resume::ResumeError::ResultKindMismatch),
            "R7/E3"
        );
    }
}

mod committed_terminal_observation;
