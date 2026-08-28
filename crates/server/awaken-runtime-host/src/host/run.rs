//! Run driving for [`SharedHost`]: the neutral `run`/`resume`/`define_outcome`
//! entry points, step finalization, queries, and the durable-ops verbs.

use super::types::VerifiedStepProjection;
use super::*;

mod projection;

pub(super) use projection::session_thread_reply_result;
use projection::{
    StepCommitExpectation, client_result_for_ticket, delegation_registry_from_snapshot,
    project_delegated_runs, recovery_ticket,
};

impl SharedHost {
    pub(crate) async fn session_budget_resume_tickets(
        &self,
        session_id: &str,
    ) -> Result<Vec<awaken_session_contract::SessionBudgetResumeTicket>, HostError> {
        let parent = ThreadId(session_id.to_string());
        let mut thread_ids = vec![parent.clone()];
        let dispatch_rows = if let Some(store) = self.optional_dispatch_store() {
            store
                .list_dispatches()
                .await
                .map_err(|error| HostError::internal(error.to_string()))?
        } else {
            Vec::new()
        };
        for thread_id in dispatch_rows
            .iter()
            // Queue phase is not pause truth: a Worker commits Awaiting
            // before it settles a Leased row. Every live parent-affined row
            // is therefore a candidate and committed recovery truth below
            // performs the sole BudgetReached classification.
            .filter(|row| row.session_thread_id.as_ref() == Some(&parent))
            .map(|row| row.thread_id.clone())
        {
            if !thread_ids.contains(&thread_id) {
                thread_ids.push(thread_id);
            }
        }
        let commit = self.commit_for_read(session_id).await?;
        let mut tickets = Vec::new();
        for thread_id in thread_ids {
            let Some(latest) = commit.latest_run(&thread_id) else {
                continue;
            };
            let snapshot = commit
                .recovery_snapshot(&thread_id, &latest.id)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
            if let Some(ticket) = recovery_ticket(&snapshot, &latest.id)
                && ticket.reason() == AwaitReason::BudgetReached
            {
                tickets.push(awaken_session_contract::SessionBudgetResumeTicket {
                    ticket,
                    pause_generation: snapshot.next_commit_ordinal,
                    prior_session_activity_epoch: dispatch_rows
                        .iter()
                        .find(|row| row.run_id == latest.id)
                        .and_then(|row| row.session_activity_epoch),
                });
            }
        }
        Ok(tickets)
    }

    /// All messages committed on `thread` so far (the source of history). Empty
    /// when the thread has not run yet. Opens only the configured commit adapter,
    /// so a fresh process can read durable history without provisioning the
    /// Session execution environment (ADR-0039).
    /// True when the durable store already holds `thread` — WITHOUT building a
    /// session context (the layout probe lives with the commit boundary in
    /// [`crate::store`]).
    pub async fn has_durable_thread(&self, thread: &str) -> Result<bool, HostError> {
        match self.authority.as_ref() {
            Some(authority) => authority
                .durable_thread_exists(thread)
                .await
                .map_err(HostError::from),
            None => Ok(false),
        }
    }

    pub async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, HostError> {
        let commit = self.commit_for_read(thread).await?;
        commit
            .authoritative_committed_messages(&ThreadId(thread.to_string()))
            .await
            .map_err(HostError::internal)
    }

    /// Rebuild the neutral child-Run projection from the one durable owner:
    /// `RunDelegations` entries committed in each parent Run's ordinary state log.
    pub async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, HostError> {
        self.delegated_run_snapshot(thread)
            .await
            .map(|snapshot| snapshot.delegated_runs)
    }

    async fn delegated_run_snapshot(
        &self,
        thread: &str,
    ) -> Result<awaken_session_contract::DelegatedRunSnapshot, HostError> {
        let commit = self.commit_for_read(thread).await?;
        let root_thread = ThreadId(thread.to_string());
        let recovery = match commit
            .authoritative_latest_run(&root_thread)
            .await
            .map_err(HostError::internal)?
        {
            Some(run) => Some(
                commit
                    .recovery_snapshot(&root_thread, &run.id)
                    .await
                    .map_err(|error| HostError::internal(error.to_string()))?,
            ),
            None => None,
        };
        let commands = recovery
            .as_ref()
            .map(|snapshot| snapshot.state.as_slice())
            .unwrap_or_default();
        let watermark = u64::try_from(commands.len())
            .map_err(|_| HostError::internal("delegation watermark exceeds u64"))?;
        let runtime_commit_cursor = recovery
            .as_ref()
            .map_or(0, |snapshot| snapshot.store_cursor);
        let mut stores: HashMap<RunId, Store> = HashMap::new();
        for command in commands {
            let Some(run_id) = command
                .run_id
                .clone()
                .filter(|_| command.scope == Scope::Run)
            else {
                continue;
            };
            stores.entry(run_id).or_default().apply(command);
        }
        let mut projected = Vec::new();
        for store in stores.values() {
            let registry = RunDelegations::load(store)
                .map_err(|error| HostError::internal(error.to_string()))?;
            projected.extend(project_delegated_runs(registry.as_ref()));
        }
        projected.sort_by(|left, right| left.run_id.0.cmp(&right.run_id.0));
        projected.dedup_by(|left, right| left.run_id == right.run_id);
        Ok(awaken_session_contract::DelegatedRunSnapshot {
            delegated_runs: projected,
            coordinated_thread_ids: Vec::new(),
            watermark,
            runtime_commit_cursor,
        })
    }

    /// Fence the parent Runtime at its durable dispatch authority, wait for any
    /// foreground projection to observe settlement, and only then read the child
    /// registry plus its committed-state watermark.
    pub async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<awaken_session_contract::DelegatedRunSnapshot, HostError> {
        // Terminal control is deliberately Environment-free. A cold projection
        // must never materialize the sandbox, MCP connections, or current Agent
        // configuration merely to tear the Session down.
        let resident = self
            .session_slots
            .read(thread, |slot| slot.runtime.clone())
            .flatten();
        let mut coordinated_thread_ids = std::collections::HashSet::new();
        if let Ok(store) = self.dispatch_store() {
            let thread_id = ThreadId(thread.to_string());
            // Cause/effect decision table: P1 root dispatch and P2 every row
            // whose trusted parent affinity names this Session are the complete
            // execution set; C1 before the resident-run fence and C2 after it
            // close the last-admission race. Each pass records logical child ids
            // before cancellation; no process-local child registry participates.
            for pass in 0..2 {
                let dispatches = store
                    .list_dispatches()
                    .await
                    .map_err(|error| HostError::internal(error.to_string()))?;
                for dispatch in dispatches.iter().filter(|dispatch| {
                    dispatch.thread_id == thread_id
                        || dispatch.session_thread_id.as_ref() == Some(&thread_id)
                }) {
                    if dispatch.session_thread_id.as_ref() == Some(&thread_id)
                        && dispatch.thread_id != thread_id
                    {
                        coordinated_thread_ids.insert(dispatch.thread_id.clone());
                    }
                }
                for dispatch in dispatches.into_iter().filter(|dispatch| {
                    (dispatch.thread_id == thread_id
                        || dispatch.session_thread_id.as_ref() == Some(&thread_id))
                        && matches!(
                            dispatch.state,
                            awaken_run_ingress_contract::DispatchState::Reserved
                                | awaken_run_ingress_contract::DispatchState::ReservationLeased
                                | awaken_run_ingress_contract::DispatchState::Pending
                                | awaken_run_ingress_contract::DispatchState::Leased
                                | awaken_run_ingress_contract::DispatchState::Awaiting
                                | awaken_run_ingress_contract::DispatchState::DeadLetter
                        )
                }) {
                    // The root's resident attempt receives the same post-intent
                    // accelerator as an explicit interrupt. Child/cold/remote
                    // attempts retain the durable claim path without a second
                    // process-local registry.
                    let live_runtime = resident
                        .as_ref()
                        .filter(|_| dispatch.thread_id == thread_id)
                        .map(|ctx| ctx.runtime.as_ref());
                    self.persist_dispatch_cancellation(&dispatch.run_id, live_runtime)
                        .await?;
                }

                // Direct ACP and Outcome attempts may own only the foreground
                // token. Any durable rows have crossed the intent boundary above,
                // so this legacy-compatible nudge cannot precede durable truth.
                if pass == 0
                    && let Some(ctx) = &resident
                    && let Some(token) = ctx.cancel.lock().expect("cancel mutex poisoned").as_ref()
                {
                    token.cancel();
                }
                if pass == 0
                    && let Some(ctx) = &resident
                {
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        if ctx
                            .active_run
                            .lock()
                            .expect("active run mutex poisoned")
                            .is_none()
                        {
                            break;
                        }
                        if tokio::time::Instant::now() >= deadline {
                            return Err(HostError::internal(format!(
                                "terminal quiescence timed out for Thread `{thread}`"
                            )));
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
                // Cancellation is complete only after every local or remote
                // Worker settles all root/parent-affined runnable rows. A local
                // root join does not cover recovered child Workers.
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    let still_active = store
                        .list_dispatches()
                        .await
                        .map_err(|error| HostError::internal(error.to_string()))?
                        .into_iter()
                        .any(|dispatch| {
                            (dispatch.thread_id == thread_id
                                || dispatch.session_thread_id.as_ref() == Some(&thread_id))
                                && matches!(
                                    dispatch.state,
                                    awaken_run_ingress_contract::DispatchState::Reserved
                                        | awaken_run_ingress_contract::DispatchState::ReservationLeased
                                        | awaken_run_ingress_contract::DispatchState::Pending
                                        | awaken_run_ingress_contract::DispatchState::Leased
                                        | awaken_run_ingress_contract::DispatchState::Awaiting
                                )
                        });
                    if !still_active {
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(HostError::internal(format!(
                            "terminal dispatch quiescence timed out for Session `{thread}`"
                        )));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        } else if let Some(ctx) = resident {
            // A direct/non-dispatch attempt has no durable cancellation intent
            // to order before this legacy foreground signal.
            if let Some(token) = ctx.cancel.lock().expect("cancel mutex poisoned").as_ref() {
                token.cancel();
            }
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if ctx
                    .active_run
                    .lock()
                    .expect("active run mutex poisoned")
                    .is_none()
                {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(HostError::internal(format!(
                        "terminal quiescence timed out for Thread `{thread}`"
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        // Rebuild committed links only after the dispatch fence. Cold or
        // malformed projection data may make enrichment fail, but it can never
        // prevent cancellation of already-admitted Session children.
        coordinated_thread_ids.extend(
            self.coordinated_threads(thread)
                .await?
                .into_iter()
                .map(|link| link.thread_id),
        );
        let mut snapshot = self.delegated_run_snapshot(thread).await?;
        snapshot.coordinated_thread_ids = coordinated_thread_ids.into_iter().collect();
        snapshot
            .coordinated_thread_ids
            .sort_by(|left, right| left.0.cmp(&right.0));
        Ok(snapshot)
    }

    /// Durable committed-truth lifecycle feed for the partition containing
    /// `thread`. A database-less Worker has only a non-authoritative recovery
    /// projection and therefore cannot expose this Control-side feed.
    pub(crate) async fn run_lifecycle_feed(
        &self,
        thread: &str,
    ) -> Result<std::sync::Arc<crate::store::HostCommit>, HostError> {
        let commit = self.commit_for_read(thread).await?;
        if matches!(commit.as_ref(), crate::store::HostCommit::Remote(_)) {
            return Err(HostError::bad_request(
                "run lifecycle feed is available only from committed-truth authority",
            ));
        }
        Ok(commit)
    }

    /// A thread's accumulated token usage, attributed per model (the run loop records
    /// it as committed thread state under [`THREAD_USAGE_STATE_KEY`]; each write is the
    /// running cumulative, so the last `Set` is the whole tally). Empty for a thread
    /// that has never run a real Run or whose provider reported no usage (the
    /// deterministic models). Callers use `.total()` for the session-level sum.
    pub async fn thread_usage(&self, thread: &str) -> awaken_runtime_contract::llm::ThreadUsage {
        use awaken_runtime_contract::llm::ThreadUsage;
        let Ok(commit) = self.commit_for_read(thread).await else {
            return ThreadUsage::default();
        };
        ThreadUsage::from_committed_state(&commit.committed_state(&ThreadId(thread.to_string())))
    }

    /// True when `thread` has a run awaiting a decision.
    pub async fn is_awaiting(&self, thread: &str) -> bool {
        let commit = match self.commit_for_read(thread).await {
            Ok(commit) => commit,
            Err(_) => return false,
        };
        match commit
            .open_wait_for_thread(&ThreadId(thread.to_string()))
            .await
        {
            Ok(waiting) => waiting.is_some(),
            Err(error) => {
                tracing::warn!(thread, %error, "failed to read authoritative awaiting position");
                true
            }
        }
    }

    /// The tool an awaiting run on `thread` is awaiting on, if any.
    pub async fn pending_tool(&self, thread: &str) -> Result<Option<Pending>, HostError> {
        self.pending_tool_in_partition(thread, &ThreadId(thread.to_string()))
            .await
    }

    /// Canonical pending-tool projection for one logical Thread in a Session
    /// partition. Both root and child reads use this implementation so local
    /// durable stores never infer a physical database from a child id.
    pub(crate) async fn pending_tool_in_partition(
        &self,
        partition: &str,
        logical_thread: &ThreadId,
    ) -> Result<Option<Pending>, HostError> {
        // The committed ticket is the lifecycle authority. In particular, an
        // observer on another protocol can see the committed tool-call message
        // before the foreground caller reaches `finish_step` and updates its
        // disposable `SessionState`; consulting that cache here would briefly
        // misclassify a client-executed call as an ordinary executed tool. Open
        // committed truth before rebuilding derived context: a malformed A2A
        // continuation must not hide its still-authoritative input/auth wait and
        // change a legitimate resume into a client-id error.
        let commit = self.commit_for_read(partition).await?;
        let (run_id, ticket) = match commit.open_wait_for_thread(logical_thread).await {
            Ok(Some(waiting)) => waiting,
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(HostError::internal(format!(
                    "failed to read authoritative pending tool for {}: {error}",
                    logical_thread.0
                )));
            }
        };
        if matches!(ticket.target(), AwaitTarget::RemoteInput { .. }) {
            return Ok(Pending::from_resume_ticket(&ticket));
        }
        if ticket.reason() == AwaitReason::Delegation {
            let snapshot = match commit.recovery_snapshot(logical_thread, &run_id).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(thread = %logical_thread.0, %error, "failed to read pending delegation snapshot");
                    return Err(HostError::internal(format!(
                        "failed to read pending delegation snapshot: {error}"
                    )));
                }
            };
            let registry = match delegation_registry_from_snapshot(&snapshot, &run_id) {
                Ok(registry) => registry,
                Err(error) => {
                    tracing::warn!(thread = %logical_thread.0, %error, "failed to rebuild pending delegation registry");
                    return Err(HostError::internal(format!(
                        "failed to rebuild pending delegation registry: {error}"
                    )));
                }
            };
            return self
                .visible_pending_tool(commit.as_ref(), &ticket, registry.as_ref())
                .await;
        }
        self.visible_pending_tool(commit.as_ref(), &ticket, None)
            .await
    }

    /// Interrupt the run in flight on `thread`, if any: cancel its token so the
    /// runtime observes it at the next step boundary and ends the run `Cancelled`
    /// (an outcome loop then reports `interrupted`). A no-op when nothing is
    /// running. Never blocks on the run's own state lock — it only touches the
    /// separate cancel slot — so it works from a concurrent request.
    pub async fn interrupt(&self, thread: &str) -> Result<(), HostError> {
        // Cancellation is a control-plane operation. Resolve only an already
        // resident Runtime accelerator; creating a context here would make an
        // operator unable to recover the exact Runs whose Environment cannot be
        // realized. Durable dispatch and committed Thread truth remain the
        // authorities for cold Managed Sessions.
        let resident = self
            .session_slots
            .read(thread, |slot| slot.runtime.clone())
            .flatten();
        let active_run = resident.as_ref().and_then(|ctx| {
            ctx.active_run
                .lock()
                .expect("active run mutex poisoned")
                .clone()
        });
        if let (Some(ctx), Some(ingress), Some(run_id)) = (
            resident.as_ref(),
            resident
                .as_ref()
                .and_then(|ctx| ctx.durable_ingress.as_ref()),
            active_run.as_ref(),
        ) {
            if self.dispatch_pool.get().is_some() {
                // The process pool is the sole durable claim driver. This edge
                // accepts the cancellation intent and returns; its drainer owns
                // the claim-fenced Runtime commit and settlement.
                self.persist_dispatch_cancellation(run_id, Some(ctx.runtime.as_ref()))
                    .await?;
            } else {
                // A standalone Host has no autonomous pool. Preserve its direct
                // ingress behavior for tests/embedders while served deployments
                // always take the non-blocking process-pool branch above.
                ingress
                    .cancel(run_id)
                    .await
                    .map_err(|error| HostError::internal(error.to_string()))?;
            }
            if let Some(token) = ctx.cancel.lock().expect("cancel mutex poisoned").as_ref() {
                token.cancel();
            }
            return Ok(());
        }

        // Managed Session Event Runs are reserved and activated directly in the
        // dispatch authority even when ordinary foreground delivery is direct.
        // They intentionally bypass BoundRunExecutor and therefore have no
        // process-local `active_run` hint. Fall back only when that existing
        // authority is installed, and require the Session's one executable root
        // row so an interrupt cannot broaden into unrelated queued work.
        if self.optional_dispatch_store().is_some() {
            let logical_thread_id = resident
                .as_ref()
                .map_or_else(|| ThreadId(thread.to_string()), |ctx| ctx.thread_id.clone());
            let executable = self
                .executable_session_dispatch_runs(thread, &logical_thread_id)
                .await?;
            match executable.as_slice() {
                [] => {}
                [run_id] => {
                    self.persist_recoverable_dispatch_cancellation(
                        run_id,
                        resident.as_ref().map(|ctx| ctx.runtime.as_ref()),
                    )
                    .await?;
                }
                _ => {
                    return Err(HostError::internal(format!(
                        "Session `{thread}` has multiple executable primary dispatches"
                    )));
                }
            }
        }
        // Direct/foreground ACP attempts retain this exact token outside the
        // Runtime registry. Durable paths reach it only after intent persistence;
        // direct paths have no durable ordering precondition.
        if let Some(ctx) = resident
            && let Some(token) = ctx.cancel.lock().expect("cancel mutex poisoned").as_ref()
        {
            token.cancel();
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
    ) -> Result<CommittedStepReceipt, HostError> {
        self.deliver_run(agent, thread, input, false, None, None)
            .await
    }

    /// Run one request with its neutral, request-grained content owner.
    pub async fn run_attributed(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
        data_subject_id: Option<awaken_runtime_contract::DataSubjectId>,
    ) -> Result<CommittedStepReceipt, HostError> {
        self.deliver_run(agent, thread, input, false, None, data_subject_id)
            .await
    }

    /// Like [`SharedHost::run`] but forwards the engine's best-effort live
    /// progress to `sink` as the Run executes (the streaming protocol path). The
    /// committed result is identical; the sink only mirrors in-flight events.
    pub async fn run_streaming(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<CommittedStepReceipt, HostError> {
        self.deliver_run(agent, thread, input, false, Some(sink), None)
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
    ) -> Result<CommittedStepReceipt, HostError> {
        self.deliver_run(agent, thread, input, false, Some(sink), data_subject_id)
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
    ) -> Result<CommittedStepReceipt, HostError> {
        self.deliver_run(agent, thread, input, true, None, None)
            .await
    }

    async fn deliver_run(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
        supersede: bool,
        sink: Option<Arc<dyn StreamSink>>,
        data_subject_id: Option<awaken_runtime_contract::DataSubjectId>,
    ) -> Result<CommittedStepReceipt, HostError> {
        let ctx = self.ctx_for(thread, agent).await?;
        let _execution = ctx.execution.lock().await;
        let st = ctx.state.lock().await;
        if ctx
            .commit
            .open_wait_for_thread(&ctx.thread_id)
            .await
            .map_err(HostError::internal)?
            .is_some()
            && !supersede
        {
            return Err(HostError::bad_request("thread is awaiting a tool decision"));
        }
        if supersede && ctx.durable_ingress.is_none() {
            return Err(HostError::bad_request(
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
        drop(st);
        activation.run_id = run_id.clone();
        activation.model_ref_override = self.inference_routing.override_for(thread);
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
        let state = match awaken_runtime_contract::execution::RunExecutor::execute(
            &executor,
            activation,
            awaken_runtime_contract::RuntimeRunContext::new(),
        )
        .await
        {
            Ok(state) => state,
            Err(error) => {
                Self::clear_active_run(&ctx, &run_id);
                return Err(HostError::internal(error.to_string()));
            }
        };
        let mut st = ctx.state.lock().await;
        let result = self
            .finish_active_step(
                &ctx,
                &mut st,
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
    ) -> Result<RunState, HostError> {
        let run_id = activation.run_id.clone();
        *ctx.active_run.lock().expect("active run mutex poisoned") = Some(run_id.clone());
        if ctx.durable {
            let result = self.resume_durable_foreground(ctx, command).await;
            if result.is_err() {
                Self::clear_active_run(ctx, &run_id);
            }
            return result;
        }
        // Direct execution rematerializes an attempt-scoped executor (and a fresh
        // grant when brokered). Durable execution returned above and resumes only
        // through the dispatch worker's claimed path.
        let context = self.native_attempt_context(ctx, &activation).await?;
        let result = ctx.ingress.resume(activation, command, context).await;
        if result.is_err() {
            Self::clear_active_run(ctx, &run_id);
        }
        result.map_err(|error| HostError::internal(error.to_string()))
    }

    /// The durable ingress for `thread`, building the session if needed. Errors
    /// unless the server runs in durable mode (`typed durable ingress`). This is
    /// the operational entry for the ADR-0009 follow-on verbs (slice E): recover
    /// (ADR-0011), manual quarantine / GC (ADR-0015), and superseding submit
    /// (ADR-0022).
    pub(crate) async fn durable_ingress(
        &self,
        thread: &str,
    ) -> Result<Arc<DurableRunIngress<AnyDispatchStore>>, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        ctx.durable_ingress.clone().ok_or_else(|| {
            HostError::bad_request("durable ingress not enabled (set typed durable ingress)")
        })
    }

    /// Read this Thread's committed dispatch rows from the one process dispatch
    /// authority. Operational monitoring observes queue truth only: it must not
    /// materialize a Session context or reopen its frozen Agent publication.
    async fn durable_dispatch_rows(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_run_ingress::DispatchSummary>, HostError> {
        if !self.deployment.durable {
            return Err(HostError::bad_request(
                "durable ingress not enabled (set typed durable ingress)",
            ));
        }
        let thread_id = ThreadId(thread.to_owned());
        self.dispatch_store()?
            .list_dispatches()
            .await
            .map_err(|error| HostError::internal(error.to_string()))
            .map(|rows| {
                rows.into_iter()
                    .filter(|row| row.thread_id == thread_id)
                    .collect()
            })
    }

    /// Reconcile `thread`'s dispatch queue (ADR-0011, slice E): reclaim and re-run
    /// any dispatch left runnable by a crash. Returns the recovered run ids.
    pub async fn reconcile(&self, thread: &str) -> Result<Vec<String>, HostError> {
        let processed = self
            .durable_ingress(thread)
            .await?
            .recover(Arc::new(awaken_run_ingress::SystemClock))
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
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
    ) -> Result<usize, HostError> {
        self.durable_ingress(thread)
            .await?
            .quarantine_retry_exhausted(max_attempts, now_ms)
            .await
            .map_err(|e| HostError::internal(e.to_string()))
    }

    /// The run ids currently dead-lettered on `thread` (ADR-0015, slice E).
    pub async fn dead_letters(&self, thread: &str) -> Result<Vec<String>, HostError> {
        let rows = self.durable_dispatch_rows(thread).await?;
        Ok(rows
            .into_iter()
            .filter(|row| row.state == awaken_run_ingress::DispatchState::DeadLetter)
            .map(|row| row.run_id.0)
            .collect())
    }

    /// Return one dead-lettered run to the durable queue with a fresh retry
    /// budget after the operator has repaired the external failure.
    pub async fn requeue_dead_letter(&self, thread: &str, run_id: &str) -> Result<bool, HostError> {
        let ingress = self.durable_ingress(thread).await?;
        let belongs_to_thread = ingress
            .list_dispatches()
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
            .into_iter()
            .any(|row| {
                row.run_id.0 == run_id
                    && row.thread_id.0 == thread
                    && row.state == awaken_run_ingress::DispatchState::DeadLetter
            });
        if !belongs_to_thread {
            return Ok(false);
        }
        ingress
            .requeue(&RunId(run_id.to_owned()))
            .await
            .map_err(|error| HostError::internal(error.to_string()))
    }

    /// Operator GC: purge every dead-lettered dispatch on `thread` (ADR-0015,
    /// slice E). Returns how many were removed.
    pub async fn purge_dead_letters(&self, thread: &str) -> Result<usize, HostError> {
        self.durable_ingress(thread)
            .await?
            .purge_dead_letters()
            .await
            .map_err(|e| HostError::internal(e.to_string()))
    }

    /// An operational snapshot of `thread`'s dispatch queue (ADR-0025): every row
    /// in enqueue order with its status and attempt count — the monitoring surface.
    pub async fn list_dispatches(
        &self,
        thread: &str,
    ) -> Result<Vec<(String, String, u64, bool)>, HostError> {
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
    pub async fn superseded(&self, thread: &str) -> Result<Vec<String>, HostError> {
        let rows = self.durable_dispatch_rows(thread).await?;
        Ok(rows
            .into_iter()
            .filter(|row| row.state == awaken_run_ingress::DispatchState::Superseded)
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
    ) -> Result<String, HostError> {
        let ctx = self.ctx_for(thread, agent).await?;
        let mut messages: Vec<Message> = Vec::new();
        messages.extend(input);
        let (uid, mut activation) = ctx
            .runtime
            .prepare(&ctx.config, thread.to_string(), messages);
        // Stamp the Thread's per-Run model override (R2/R5) off the fingerprinted
        // snapshot, so the claiming worker resolves the effective model itself.
        activation.model_ref_override = self.inference_routing.override_for(thread);
        activation.run_id = uid.clone();
        let request = self.resolved_dispatch(activation)?;
        match self.dispatch_pool_or_err() {
            // Normal server: the local pool claims and drives it.
            Ok(pool) => pool
                .submit_dispatch(request)
                .await
                .map_err(|e| HostError::internal(e.to_string()))?,
            // Coordinator-only durable server (no local pool): enqueue straight into
            // the shared store so a remote database-less worker drains it over the
            // dispatch transport. Non-durable keeps the original "enable the pool" error.
            Err(e) if self.deployment.durable => {
                use awaken_run_ingress::DispatchQueue;
                let _ = e;
                let store = self.dispatch_store()?;
                store
                    .enqueue(request)
                    .await
                    .map_err(|e| HostError::internal(e.to_string()))?;
            }
            Err(e) => return Err(e),
        }
        Ok(uid.0)
    }

    /// Rebuild the sole durable resume target for a foreground root Run whose
    /// committed Awaiting ticket predates dispatch ownership. Budget and tool
    /// replies share this handoff; child Runs must already have a durable row.
    pub(super) async fn enqueue_foreground_session_resume_dispatch(
        &self,
        session_id: &str,
        ticket: &ResumeTicket,
        session_activity_epoch: u64,
    ) -> Result<Arc<awaken_run_ingress::AnyDispatchStore>, HostError> {
        use awaken_run_ingress::DispatchQueue as _;

        if ticket.thread_id.0 != session_id || session_activity_epoch == 0 {
            return Err(HostError::internal(
                "only an exact foreground Session root can enter durable resume",
            ));
        }
        let store = self.dispatch_store()?;
        let ctx = self.ctx_for(session_id, None).await?;
        let activation = ctx
            .resume_activation(ticket)
            .with_model_ref_override(self.inference_routing.override_for(session_id));
        let request = self
            .resolved_dispatch(activation)?
            .for_session(ThreadId(session_id.to_string()))
            .with_session_activity_epoch(session_activity_epoch);
        // One exact enqueue closes the foreground-to-durable crash window. A
        // concurrent exact retry is idempotent; a changed Run payload is rejected
        // by the existing dispatch identity fence before any resume is staged.
        store
            .enqueue(request)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
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
    ) -> Result<(), HostError> {
        if child_thread_id.0 == session_id {
            return Err(HostError::bad_request(
                "a coordinated child Thread must differ from its parent Session",
            ));
        }
        let child_runs = self
            .executable_session_dispatch_runs(session_id, child_thread_id)
            .await?;
        for run_id in child_runs {
            self.persist_recoverable_dispatch_cancellation(&run_id, None)
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
    ) -> Result<Vec<RunId>, HostError> {
        let store = self.dispatch_store()?;
        let parent = ThreadId(session_id.to_string());
        let commit = self.commit_for_read(session_id).await?;
        let rows = store
            .list_dispatches()
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(rows
            .into_iter()
            .filter(|dispatch| {
                dispatch.thread_id == *logical_thread_id
                    && dispatch.session_thread_id.as_ref() == Some(&parent)
                    && matches!(
                        dispatch.state,
                        awaken_run_ingress::DispatchState::Reserved
                            | awaken_run_ingress::DispatchState::ReservationLeased
                            | awaken_run_ingress::DispatchState::Pending
                            | awaken_run_ingress::DispatchState::Leased
                            | awaken_run_ingress::DispatchState::Awaiting
                            | awaken_run_ingress::DispatchState::DeadLetter
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

    /// RunResume the run awaiting on `thread`, answering `tool_use_id` with `resume`.
    /// Fails closed unless `tool_use_id` names the pending tool and its binding
    /// (built-in vs client-executed) matches the resume variant.
    pub(crate) async fn resume_budget_reached(
        &self,
        delivery: awaken_session_contract::SessionBudgetResumeDelivery,
    ) -> Result<awaken_session_contract::SessionBudgetResumeDisposition, HostError> {
        use awaken_run_ingress::Outbox as _;

        if delivery.session_activity_epoch == 0 {
            return Err(HostError::bad_request(
                "budget resume requires a newly admitted Session activity",
            ));
        }
        let commit = self.commit_for_read(&delivery.session_id).await?;
        let snapshot = commit
            .recovery_snapshot(&delivery.thread_id, &delivery.run_id)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let Some(ticket) = recovery_ticket(&snapshot, &delivery.run_id) else {
            return Ok(awaken_session_contract::SessionBudgetResumeDisposition::Stale);
        };
        if ticket.reason() != AwaitReason::BudgetReached
            || ticket.correlation_id != delivery.correlation_id
            || ticket.thread_id != delivery.thread_id
            || snapshot.next_commit_ordinal != delivery.pause_generation
        {
            return Ok(awaken_session_contract::SessionBudgetResumeDisposition::Stale);
        }

        let store = self.optional_dispatch_store();
        let dispatch = match &store {
            Some(store) => store
                .list_dispatches()
                .await
                .map_err(|error| HostError::internal(error.to_string()))?
                .into_iter()
                .find(|row| row.run_id == delivery.run_id),
            None => None,
        };
        let store = if let Some(dispatch) = dispatch {
            if dispatch.thread_id != delivery.thread_id
                || dispatch.session_thread_id.as_ref()
                    != Some(&ThreadId(delivery.session_id.clone()))
            {
                return Err(HostError::bad_request(
                    "budget resume dispatch affinity is inconsistent",
                ));
            }
            let activity_matches = match dispatch.session_activity_epoch {
                Some(epoch) => {
                    Some(epoch) == delivery.prior_session_activity_epoch
                        || epoch == delivery.session_activity_epoch
                }
                None => delivery.prior_session_activity_epoch.is_none(),
            };
            if !activity_matches {
                return Ok(awaken_session_contract::SessionBudgetResumeDisposition::Stale);
            }
            store
                .as_ref()
                .expect("dispatch was read from the installed store")
                .clone()
        } else {
            if delivery.thread_id.0 != delivery.session_id
                || delivery.prior_session_activity_epoch.is_some()
            {
                return Err(HostError::internal(
                    "budget-paused child Run has no durable dispatch row",
                ));
            }
            self.enqueue_foreground_session_resume_dispatch(
                &delivery.session_id,
                &ticket,
                delivery.session_activity_epoch,
            )
            .await?
        };
        // Cause/effect decision table for foreground→durable handoff: a crash
        // before enqueue leaves the committed ticket discoverable; a crash
        // after enqueue but before staging leaves an idempotently reusable row;
        // a Worker that claims first observes the committed Awaiting Run and
        // cannot execute it as fresh work. Once staged, the existing claim and
        // settlement observer owns Completed, required action, and a later
        // budget pause, including crash repair of the Session activity epoch.
        let input = PendingInput {
            message_id: format!(
                "budget-resume-{}",
                awaken_session_contract::stable_fingerprint(&(
                    delivery.session_id.as_str(),
                    delivery.thread_id.0.as_str(),
                    delivery.run_id.0.as_str(),
                    delivery.correlation_id.as_str(),
                    delivery.pause_generation,
                ))
            ),
            run_id: delivery.run_id,
            thread_id: delivery.thread_id,
            correlation_id: delivery.correlation_id,
            available_at_ms: None,
            context_messages: Vec::new(),
            result: ResumeResult::Continue,
        };
        store
            .stage_session_resume(
                input,
                &ThreadId(delivery.session_id),
                delivery.prior_session_activity_epoch,
                delivery.session_activity_epoch,
            )
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        if let Some(pool) = self.dispatch_pool.get() {
            pool.notify().await;
        } else {
            store
                .relay()
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
        }
        Ok(awaken_session_contract::SessionBudgetResumeDisposition::Dispatched)
    }

    pub async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: HostResume,
    ) -> Result<CommittedStepReceipt, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        let _execution = ctx.execution.lock().await;
        let (run_id, ticket) = ctx
            .commit
            .open_wait_for_thread(&ctx.thread_id)
            .await
            .map_err(HostError::internal)?
            .ok_or_else(|| HostError::bad_request("no awaiting run to resume"))?;
        let awaiting_snapshot = self.authoritative_step_snapshot(&ctx, &run_id).await?;

        if matches!(ticket.target(), AwaitTarget::RemoteInput { .. }) {
            if ticket.call_id() != Some(tool_use_id) {
                return Err(HostError::bad_request(format!(
                    "tool_use_id {tool_use_id:?} does not match the pending remote input"
                )));
            }
            let HostResume::ClientResult { content, .. } = resume else {
                return Err(HostError::bad_request(
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
            let mut st = ctx.state.lock().await;
            let result = self
                .finish_active_step(
                    &ctx,
                    &mut st,
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
            let child = self
                .authoritative_child_ticket(
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
                    return Err(HostError::bad_request(format!(
                        "tool_use_id {tool_use_id:?} does not match the pending delegate"
                    )));
                }
                let HostResume::ClientResult { content, .. } = resume else {
                    return Err(HostError::bad_request(
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
            let mut st = ctx.state.lock().await;
            let result = self
                .finish_active_step(
                    &ctx,
                    &mut st,
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
        let (call_id, _) = ticket
            .tool_call()
            .ok_or_else(|| HostError::internal("awaiting run has no pending tool call"))?;
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
        let mut st = ctx.state.lock().await;
        let result = self
            .finish_active_step(
                &ctx,
                &mut st,
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

    fn clear_active_run(ctx: &SessionCtx, run_id: &RunId) {
        let mut active = ctx.active_run.lock().expect("active run mutex poisoned");
        if active.as_ref() == Some(run_id) {
            *active = None;
        }
    }

    async fn finish_active_step(
        &self,
        ctx: &SessionCtx,
        st: &mut SessionState,
        run_id: RunId,
        state: RunState,
        expectation: StepCommitExpectation<'_>,
        thread: &str,
    ) -> Result<CommittedStepReceipt, HostError> {
        let active_run = run_id.clone();
        let result = self
            .finish_step(ctx, st, run_id, state, expectation, thread)
            .await;
        Self::clear_active_run(ctx, &active_run);
        result
    }

    /// Reuse the canonical claim-recovery snapshot as the exact thread read for
    /// foreground projection. PostgreSQL implements this from one repeatable-read
    /// transaction; local stores expose the same facts without a parallel model.
    async fn authoritative_step_snapshot(
        &self,
        ctx: &SessionCtx,
        run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, HostError> {
        ctx.commit
            .recovery_snapshot(&ctx.thread_id, run_id)
            .await
            .map_err(|error| HostError::internal(error.to_string()))
    }

    /// Join the parent-thread delegation relationship with the child thread's
    /// authoritative recovery snapshot. Child Runs intentionally own a separate
    /// thread, so the parent's consistent snapshot cannot contain their ticket.
    async fn authoritative_child_ticket(
        &self,
        commit: &HostCommit,
        registry: Option<&awaken_agent_contract::agent::delegation::DelegationRegistry>,
        parent_call_id: Option<&str>,
    ) -> Result<Option<ResumeTicket>, HostError> {
        let Some(child_run_id) = parent_call_id.and_then(|parent_call_id| {
            registry?
                .delegations()
                .find(|relationship| relationship.parent_call_id == parent_call_id)
                .map(|relationship| relationship.child_run_id.clone())
        }) else {
            return Ok(None);
        };
        let child_thread_id = ThreadId(child_run_id.0.clone());
        let committed = commit
            .recovery_snapshot(&child_thread_id, &child_run_id)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(recovery_ticket(&committed, &child_run_id))
    }

    /// Project the one externally answerable tool from a committed ticket. A
    /// delegation owns a child ticket when the child is local; remote A2A may
    /// instead expose opaque input through the parent ticket. Both foreground
    /// completion and later protocol reads must use this same classification.
    async fn visible_pending_tool(
        &self,
        commit: &HostCommit,
        ticket: &ResumeTicket,
        registry: Option<&awaken_agent_contract::agent::delegation::DelegationRegistry>,
    ) -> Result<Option<Pending>, HostError> {
        if ticket.reason() != AwaitReason::Delegation {
            return Ok(Pending::from_resume_ticket(ticket));
        }
        let visible_child = self
            .authoritative_child_ticket(commit, registry, ticket.call_id())
            .await?;
        match visible_child {
            Some(child) => Ok(Pending::from_resume_ticket(&child)),
            None => Ok(Pending::from_resume_ticket(ticket).map(|mut pending| {
                pending.client_executed = true;
                pending
            })),
        }
    }

    /// Project the step's delta, update the awaiting position, and publish the
    /// delta to the thread hub for any observing protocol.
    async fn finish_step(
        &self,
        ctx: &SessionCtx,
        st: &mut SessionState,
        run_id: RunId,
        state: RunState,
        expectation: StepCommitExpectation<'_>,
        thread: &str,
    ) -> Result<CommittedStepReceipt, HostError> {
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
                st.awaiting_run = Some(run_id.clone());
                let ticket = recovery_ticket(&committed, &run_id);
                let await_reason = ticket.as_ref().map(ResumeTicket::reason);
                let pending = if let Some(ticket) = ticket {
                    // A parent waiting on a child exposes the CHILD's ordinary
                    // interaction request. The protocol still addresses the
                    // parent session; it never obtains a bypass around the
                    // parent-child relationship.
                    self.visible_pending_tool(
                        ctx.commit.as_ref(),
                        &ticket,
                        delegation_registry.as_ref(),
                    )
                    .await?
                } else {
                    None
                };
                (pending, true, await_reason)
            }
            _ => {
                st.awaiting_run = None;
                (None, false, None)
            }
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

    /// Fail closed before resuming: the asserted `tool_use_id` must name the
    /// run's pending tool, and that tool's binding must match the inbound resume
    /// — a client result may only answer a client-executed tool, a confirmation
    /// only a built-in one.
    pub(super) fn check_pending(
        &self,
        ticket: &ResumeTicket,
        tool_use_id: &str,
        want_client: bool,
    ) -> Result<(), HostError> {
        let pending = Pending::from_resume_ticket(ticket)
            .ok_or_else(|| HostError::internal("awaiting run has no pending tool"))?;
        if pending.tool_use_id != tool_use_id {
            return Err(HostError::bad_request(format!(
                "tool_use_id {tool_use_id:?} does not match the pending tool"
            )));
        }
        if pending.client_executed != want_client {
            let (got, expected) = if want_client {
                ("built-in", "a confirmation")
            } else {
                ("client-executed", "a client tool result")
            };
            return Err(HostError::bad_request(format!(
                "pending tool is {got}; answer it with {expected}"
            )));
        }
        Ok(())
    }
}

/// Convert one consistent committed prefix into the only step result allowed to
/// cross the Host boundary. Executor completion is merely a claim until this
/// proof binds the exact Thread, Run, input identities, and lifecycle state.
fn verify_committed_step(
    committed: &RunRecoverySnapshot,
    thread_id: &ThreadId,
    run_id: &RunId,
    returned_state: &RunState,
    before: usize,
    expected_input_ids: &[String],
) -> Result<Vec<Message>, HostError> {
    if &committed.thread_id != thread_id || &committed.claimed_run_id != run_id {
        return Err(HostError::internal(
            "committed step proof names another Thread or Run",
        ));
    }
    if committed.latest_run_id.as_ref() != Some(run_id) {
        return Err(HostError::internal(
            "committed step proof is not the latest Run on its Thread",
        ));
    }
    let committed_state = committed
        .runs
        .iter()
        .find(|run| &run.id == run_id && &run.thread_id == thread_id)
        .map(|run| &run.state)
        .ok_or_else(|| HostError::internal("committed step proof has no Run record"))?;
    if committed_state != returned_state {
        return Err(HostError::internal(
            "executor result does not match the committed Run state",
        ));
    }
    if matches!(returned_state, RunState::Running) {
        return Err(HostError::internal("committed step proof is not settled"));
    }
    if committed.messages.len() < before {
        return Err(HostError::internal(
            "committed Thread message prefix moved backwards",
        ));
    }
    if committed.thread_version == 0 || committed.next_commit_ordinal == 0 {
        return Err(HostError::internal(
            "committed step proof has no durable commit identity",
        ));
    }
    for expected in expected_input_ids {
        if !committed
            .messages
            .iter()
            .any(|message| message.id.0 == *expected)
        {
            return Err(HostError::internal(format!(
                "committed step proof is missing input message `{expected}`"
            )));
        }
    }
    let suffix = committed.messages[before..].to_vec();
    // Natural completion must be evidenced by committed assistant output. An
    // error completion already carries its typed explanation in the sole
    // committed RunState and is projected from that authority; requiring a
    // duplicate assistant transcript entry would hide the real provider/runtime
    // failure behind a proof error.
    if matches!(returned_state, RunState::Ended(EndCause::NaturalEnd))
        && !suffix.iter().any(|message| message.role == Role::Assistant)
    {
        return Err(HostError::internal(
            "committed natural terminal step has no assistant output",
        ));
    }
    Ok(suffix)
}

#[cfg(test)]
mod committed_step_proof_tests {
    use super::*;
    use awaken_agent_contract::agent::message::Role;
    use awaken_agent_contract::agent::run::Record as RunRecord;

    fn snapshot() -> RunRecoverySnapshot {
        let thread_id = ThreadId("thread-proof".into());
        let run_id = RunId("run-proof".into());
        RunRecoverySnapshot {
            thread_id: thread_id.clone(),
            claimed_run_id: run_id.clone(),
            runs: vec![RunRecord {
                id: run_id.clone(),
                thread_id,
                state: RunState::Ended(EndCause::NaturalEnd),
            }],
            latest_run_id: Some(run_id),
            messages: vec![
                Message::text(MessageId("input-proof".into()), Role::User, "question"),
                Message::text(MessageId("output-proof".into()), Role::Assistant, "answer"),
            ],
            message_commit_cursors: Vec::new(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 1,
            store_cursor: 1,
            next_commit_ordinal: 1,
        }
    }

    #[test]
    fn committed_step_proof_follows_the_fmeca_decision_table() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // FMECA causes: C1 exact Thread/claimed Run; C2 latest Run identity;
        // C3 committed RunRecord exists; C4 executor/committed state agree;
        // C5 committed message count does not regress; C6 every accepted input
        // MessageId exists; C7 the snapshot carries a nonzero commit identity;
        // C8 a natural terminal step has committed assistant output, while an
        // error terminal carries its explanation in the committed RunState; C9
        // the state is settled rather than Running. Effects: E1 issue the
        // unforgeable receipt and project only the committed suffix; E2 fail
        // closed before StepOutcome/SSE completion. Severity is critical: any
        // false rule would report completion without authoritative history.
        //
        // | Rule | Failed cause | Effect |
        // | P1   | none         | E1     |
        // | P2   | C1           | E2     |
        // | P3   | C2           | E2     |
        // | P4   | C3           | E2     |
        // | P5   | C4           | E2     |
        // | P6   | C5           | E2     |
        // | P7   | C6           | E2     |
        // | P8   | C7           | E2     |
        // | P9   | C8           | E2     |
        // | P10  | C9           | E2     |
        let thread = ThreadId("thread-proof".into());
        let run = RunId("run-proof".into());
        let state = RunState::Ended(EndCause::NaturalEnd);
        let expected = vec!["input-proof".to_string()];

        let suffix =
            verify_committed_step(&snapshot(), &thread, &run, &state, 1, &expected).expect("P1/E1");
        assert_eq!(suffix.len(), 1, "P1/E1");
        assert_eq!(suffix[0].id.0, "output-proof", "P1/E1");
        let receipt = CommittedStepReceipt::from_verified(
            VerifiedStepProjection {
                run_id: run.clone(),
                new_messages: suffix,
                state: state.clone(),
                pending: None,
                await_reason: None,
                delegated_runs: Vec::new(),
            },
            &snapshot(),
        );
        assert_eq!(receipt.thread_id().0, "thread-proof", "P1/E1");
        assert_eq!(receipt.commit_sequence(), 1, "P1/E1");
        assert_eq!(receipt.store_cursor(), 1, "P1/E1");
        assert_eq!(receipt.operation_ordinal(), 0, "P1/E1");
        assert_eq!(
            receipt.first_message_id().map(|id| id.0.as_str()),
            Some("output-proof"),
            "P1/E1"
        );
        assert_eq!(
            receipt.last_message_id().map(|id| id.0.as_str()),
            Some("output-proof"),
            "P1/E1"
        );

        let mut cases = Vec::new();
        let mut wrong_thread = snapshot();
        wrong_thread.thread_id = ThreadId("other".into());
        cases.push(("P2", wrong_thread, state.clone(), 1, expected.clone()));
        let mut stale_latest = snapshot();
        stale_latest.latest_run_id = Some(RunId("older".into()));
        cases.push(("P3", stale_latest, state.clone(), 1, expected.clone()));
        let mut missing_run = snapshot();
        missing_run.runs.clear();
        cases.push(("P4", missing_run, state.clone(), 1, expected.clone()));
        cases.push((
            "P5",
            snapshot(),
            RunState::Ended(EndCause::Cancelled),
            1,
            expected.clone(),
        ));
        cases.push(("P6", snapshot(), state.clone(), 3, expected.clone()));
        cases.push((
            "P7",
            snapshot(),
            state.clone(),
            1,
            vec!["missing-input".into()],
        ));
        let mut missing_commit_identity = snapshot();
        missing_commit_identity.thread_version = 0;
        missing_commit_identity.next_commit_ordinal = 0;
        cases.push((
            "P8",
            missing_commit_identity,
            state.clone(),
            1,
            expected.clone(),
        ));
        let mut missing_output = snapshot();
        missing_output.messages.pop();
        cases.push(("P9", missing_output, state.clone(), 0, expected.clone()));
        let mut running = snapshot();
        running.runs[0].state = RunState::Running;
        cases.push(("P10", running, RunState::Running, 0, expected.clone()));
        for (rule, snapshot, returned, before, inputs) in cases {
            assert!(
                verify_committed_step(&snapshot, &thread, &run, &returned, before, &inputs)
                    .is_err(),
                "{rule}/E2"
            );
        }

        let mut committed_error = snapshot();
        committed_error.messages.pop();
        let error_state = RunState::Ended(EndCause::Error(
            awaken_agent_contract::agent::run::Failure::Inference {
                code: "provider_error".into(),
                message: "provider connection closed".into(),
            },
        ));
        committed_error.runs[0].state = error_state.clone();
        assert!(
            verify_committed_step(&committed_error, &thread, &run, &error_state, 0, &expected,)
                .is_ok(),
            "P11/E1 committed typed error is its own explanation"
        );
    }
}

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
                    &awaken_session_contract::SessionThreadToolReply::Confirm(decision.clone()),
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
                awaken_session_contract::SessionThreadToolReply::Custom {
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
                awaken_session_contract::SessionThreadToolReply::Custom {
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
                awaken_session_contract::SessionThreadToolReply::Result {
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
                awaken_session_contract::SessionThreadToolReply::Result {
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

        let permission =
            awaken_session_contract::SessionThreadToolReply::Confirm(PermissionDecision::Allow {
                note: None,
            });
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
        let supplied = awaken_session_contract::SessionThreadToolReply::Result {
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
