// @formal entrypoint worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal entrypoint worker_drain.grace_expiry_retains_cancellation_observer
//! Run driving for [`ExecutorComponents`]: the neutral `run`/`resume`/`define_outcome`
//! entry points, step finalization, queries, and the durable-ops verbs.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

mod quiescence;

use awaken_session_run_delivery::StepCommitExpectation;
#[cfg(test)]
use awaken_session_run_delivery::client_result_for_ticket;
use awaken_session_run_delivery::message_prefix_before_exact_inputs;
#[cfg(test)]
use awaken_session_run_delivery::session_thread_reply_result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunCommandOrigin {
    DirectCommand,
    ThreadExtension,
}

/// Complete input for one ordinary Host-owned Run delivery. This private value
/// is distinct from `SessionRunDelivery`, whose session activity receipt belongs
/// to the durable Session admission boundary.
struct HostRunDelivery<'a> {
    agent: Option<&'a str>,
    thread: &'a str,
    input: Vec<Message>,
    sink: Option<Arc<dyn StreamSink>>,
    data_subject_id: Option<awaken_runtime_contract::DataSubjectId>,
    origin: RunCommandOrigin,
}

impl<'a> HostRunDelivery<'a> {
    fn command(agent: Option<&'a str>, thread: &'a str, input: Vec<Message>) -> Self {
        Self {
            agent,
            thread,
            input,
            sink: None,
            data_subject_id: None,
            origin: RunCommandOrigin::DirectCommand,
        }
    }

    #[cfg(test)]
    fn thread_extension(agent: Option<&'a str>, thread: &'a str, input: Vec<Message>) -> Self {
        Self {
            origin: RunCommandOrigin::ThreadExtension,
            ..Self::command(agent, thread, input)
        }
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
        self.interrupt_with_commit_read(thread, async {
            Ok(self.thread_reads.commit_for_read(thread).await?)
        })
        .await
    }

    /// Use the lock-aware authoritative reader when the caller owns the
    /// lifecycle guard. Cancellation selection and delivery remain identical;
    /// reacquiring that guard through commit_for_read would deadlock revocation.
    pub(crate) async fn interrupt_with_commit_read(
        &self,
        thread: &str,
        commit_read: impl std::future::Future<
            Output = Result<Arc<SessionCommit>, SessionExecutionError>,
        >,
    ) -> Result<(), SessionExecutionError> {
        // Snapshot original controls under the canonical installed Session lock before
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
                self.threads
                    .durable_run_control()?
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
                .threads
                .executable_session_dispatch_runs(thread, &logical_thread_id, &commit)
                .await?;
            for run_id in executable {
                self.threads
                    .durable_run_control()?
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

    /// One foreground Run delivery, heap-owned at this one boundary: every Host
    /// ingress (`run`, its attributed/streamed forms and Thread extension)
    /// awaits a pointer, so an instrumented ingress frame never embeds the
    /// whole Run attempt and a nested auxiliary Run still fits a 2 MiB stack.
    fn deliver_run<'a>(
        &'a self,
        delivery: HostRunDelivery<'a>,
    ) -> futures_util::future::BoxFuture<'a, Result<CommittedStepReceipt, SessionExecutionError>>
    {
        Box::pin(deliver_foreground_run(self, delivery))
    }

    /// RunResume through the same in-flight identity installed Session as a fresh foreground
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
        let result = awaken_session_run_delivery::resume_result(
            ctx.commit.as_ref(),
            &awaiting_snapshot,
            &run_id,
            &ticket,
            tool_use_id,
            resume.into(),
        )
        .await?;
        let activation = ctx.resume_activation(&ticket);
        let before = self
            .authoritative_step_snapshot(&ctx, &activation.run_id)
            .await?
            .messages
            .len();
        let command = ResumeCommand::from_ticket(&ticket, result, 0);
        let state = self.drive_resume(&ctx, activation, command).await?;
        let _projection = ctx.projection.lock().await;
        self.finish_active_step(
            &ctx,
            run_id,
            state,
            StepCommitExpectation {
                messages_before: before,
                input_ids: &[],
            },
            thread,
        )
        .await
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
        Ok(awaken_session_run_delivery::authoritative_step_snapshot(
            ctx.commit.as_ref(),
            &ctx.thread_id,
            run_id,
        )
        .await?)
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
        Ok(awaken_session_run_delivery::project_committed_step(
            ctx.commit.as_ref(),
            &self.hub,
            &ctx.thread_id,
            run_id,
            state,
            expectation,
            thread,
        )
        .await?)
    }
}

async fn deliver_foreground_run(
    runs: &crate::SessionRuns,
    delivery: HostRunDelivery<'_>,
) -> Result<CommittedStepReceipt, SessionExecutionError> {
    let HostRunDelivery {
        agent,
        thread,
        input,
        sink,
        data_subject_id,
        origin,
    } = delivery;
    if origin == RunCommandOrigin::DirectCommand
        && runs
            .installed_sessions
            .read_run(thread, |run| run.session_dispatch)?
            .unwrap_or(false)
    {
        return Err(SessionExecutionError::bad_request(
            "Managed Session roots must use Session-owned Run ingress",
        ));
    }
    let ctx = runs.run_assembly.ctx_for(thread, agent).await?;
    let _command = ctx.command.lock().await;
    if ctx
        .commit
        .open_wait_for_thread(&ctx.thread_id)
        .await
        .map_err(SessionExecutionError::internal)?
        .is_some()
    {
        return Err(SessionExecutionError::bad_request(
            "thread is awaiting a tool decision",
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
    let baseline = runs.authoritative_step_snapshot(&ctx, &run_id).await?;
    let before = baseline.messages.len();
    activation.run_id = run_id.clone();
    activation.model_ref_override = runs.agent.inference_routing.override_for(thread)?;
    activation.data_subject_id = data_subject_id;
    let expected_input_ids = activation
        .input
        .iter()
        .map(|message| message.id.0.clone())
        .collect::<Vec<_>>();
    let executor = crate::run_exec::BoundRunExecutor::new(runs, ctx.clone())
        .with_stream_sink(sink)
        .retain_active_until_settled();
    let executor = if origin == RunCommandOrigin::ThreadExtension {
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
            crate::SessionRuns::clear_active_run(&ctx, &run_id)?;
            return Err(SessionExecutionError::internal(error.to_string()));
        }
    };
    let _projection = ctx.projection.lock().await;
    let result = runs
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

use awaken_dispatch_contract::{
    DispatchAdministration as _, DispatchMaintenance as _, DispatchReads as _,
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
