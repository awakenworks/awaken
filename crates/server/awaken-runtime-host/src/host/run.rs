//! Run driving for [`SharedHost`]: the neutral `run`/`resume`/`define_outcome`
//! entry points, step finalization, queries, and the durable-ops verbs.

use super::types::VerifiedStepProjection;
use super::*;

#[derive(Clone, Copy)]
struct StepCommitExpectation<'a> {
    messages_before: usize,
    input_ids: &'a [String],
}

fn project_delegated_runs(
    registry: Option<&awaken_agent_contract::agent::delegation::DelegationRegistry>,
) -> Vec<DelegatedRun> {
    registry
        .into_iter()
        .flat_map(|registry| {
            registry.delegations().map(|delegation| DelegatedRun {
                run_id: delegation.child_run_id.clone(),
                parent_call_id: delegation.parent_call_id.clone(),
                agent_id: delegation.target_agent_id.clone(),
                status: delegation.status,
            })
        })
        .collect()
}

fn delegation_registry_from_snapshot(
    snapshot: &RunRecoverySnapshot,
    run_id: &RunId,
) -> Result<Option<awaken_agent_contract::agent::delegation::DelegationRegistry>, HostError> {
    let mut store = Store::new();
    for command in &snapshot.state {
        if command.scope == Scope::Run && command.run_id.as_ref() == Some(run_id) {
            store.apply(command);
        }
    }
    RunDelegations::load(&store).map_err(|error| HostError::internal(error.to_string()))
}

fn client_result_for_ticket(
    ticket: &ResumeTicket,
    tool_use_id: &str,
    content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    is_error: bool,
) -> ResumeResult {
    if ticket.pending_tool.is_none()
        && matches!(
            ticket.reason,
            AwaitReason::UserInput | AwaitReason::ExternalEvent
        )
    {
        ResumeResult::Input(awaken_agent_contract::agent::content::extract_text(
            &content,
        ))
    } else {
        let output = if is_error {
            ToolOutput::error_blocks(tool_use_id, content)
        } else {
            ToolOutput::ok_blocks(tool_use_id, content)
        };
        ResumeResult::ToolResult(output)
    }
}

impl SharedHost {
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
        let commands = commit.committed_state(&ThreadId(thread.to_string()));
        let watermark = u64::try_from(commands.len())
            .map_err(|_| HostError::internal("delegation watermark exceeds u64"))?;
        let mut stores: HashMap<RunId, Store> = HashMap::new();
        for command in &commands {
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
            watermark,
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
        if let Some(ctx) = &resident
            && let Some(token) = ctx.cancel.lock().expect("cancel mutex poisoned").as_ref()
        {
            token.cancel();
        }
        if self.deployment.durable {
            let store = self.dispatch_store()?;
            let thread_id = ThreadId(thread.to_string());
            let dispatches = store
                .list_dispatches()
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
            for dispatch in dispatches.into_iter().filter(|dispatch| {
                dispatch.thread_id == thread_id
                    && matches!(
                        dispatch.state,
                        awaken_run_ingress_contract::DispatchState::Pending
                            | awaken_run_ingress_contract::DispatchState::Leased
                            | awaken_run_ingress_contract::DispatchState::Awaiting
                    )
            }) {
                if let Some(pool) = self.dispatch_pool.get() {
                    // A local pool can synchronously drive the cancellation
                    // receipt. A Coordinator without a pool still commits the
                    // epoch-advancing cancellation below; its ordinary recovery
                    // worker settles the already-fenced row.
                    pool.cancel(&dispatch.run_id)
                        .await
                        .map_err(|error| HostError::internal(error.to_string()))?;
                } else {
                    store
                        .cancel(&dispatch.run_id)
                        .await
                        .map_err(|error| HostError::internal(error.to_string()))?;
                }
            }
        }

        if let Some(ctx) = resident {
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
        self.delegated_run_snapshot(thread).await
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
    /// that has never run a real turn or whose provider reported no usage (the
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
    pub async fn pending_tool(&self, thread: &str) -> Result<Option<PendingTool>, HostError> {
        // The committed ticket is the lifecycle authority. In particular, an
        // observer on another protocol can see the committed tool-call message
        // before the foreground caller reaches `finish_step` and updates its
        // disposable `SessionState`; consulting that cache here would briefly
        // misclassify a client-executed call as an ordinary executed tool. Open
        // committed truth before rebuilding derived context: a malformed A2A
        // continuation must not hide its still-authoritative input/auth wait and
        // turn a legitimate resume into a client-id error.
        let commit = self.commit_for_read(thread).await?;
        let (run_id, ticket) = match commit
            .open_wait_for_thread(&ThreadId(thread.to_string()))
            .await
        {
            Ok(Some(waiting)) => waiting,
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(HostError::internal(format!(
                    "failed to read authoritative pending tool: {error}"
                )));
            }
        };
        if ticket.pending_tool.is_none()
            && matches!(
                ticket.reason,
                AwaitReason::UserInput | AwaitReason::ExternalEvent
            )
        {
            return Ok(pending_from_ticket(&ticket));
        }
        if ticket.reason == AwaitReason::Delegation {
            let snapshot = match commit
                .recovery_snapshot(&ThreadId(thread.to_string()), &run_id)
                .await
            {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(thread, %error, "failed to read pending delegation snapshot");
                    return Err(HostError::internal(format!(
                        "failed to read pending delegation snapshot: {error}"
                    )));
                }
            };
            let registry = match delegation_registry_from_snapshot(&snapshot, &run_id) {
                Ok(registry) => registry,
                Err(error) => {
                    tracing::warn!(thread, %error, "failed to rebuild pending delegation registry");
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

    /// Buffer a system message; it is prepended to the next turn's input.
    pub async fn add_system(&self, thread: &str, text: &str) -> Result<(), HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        ctx.state.lock().await.pending_system.push(text.to_string());
        Ok(())
    }

    /// Interrupt the run in flight on `thread`, if any: cancel its token so the
    /// runtime observes it at the next step boundary and ends the run `Cancelled`
    /// (an outcome loop then reports `interrupted`). A no-op when nothing is
    /// running. Never blocks on the run's own state lock — it only touches the
    /// separate cancel slot — so it works from a concurrent request.
    pub async fn interrupt(&self, thread: &str) -> Result<(), HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        if let Some(token) = ctx.cancel.lock().expect("cancel mutex poisoned").as_ref() {
            token.cancel();
        }
        let active_run = ctx
            .active_run
            .lock()
            .expect("active run mutex poisoned")
            .clone();
        if let (Some(ingress), Some(run_id)) = (&ctx.durable_ingress, active_run) {
            ingress
                .cancel(&run_id)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
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
    /// progress to `sink` as the turn runs (the streaming protocol path). The
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

    /// Submit a turn that *supersedes* the thread's prior pending/awaiting work
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
        let mut st = ctx.state.lock().await;
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
        messages.extend(
            std::mem::take(&mut st.pending_system)
                .into_iter()
                .map(|text| {
                    Message::text(
                        MessageId(awaken_runtime::fresh_process_id("sys")),
                        Role::System,
                        text,
                    )
                }),
        );
        // Expand a user `/skill-name` into the skill's instructions before the turn.
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
        // turn again when the current turn settles on another replica.
        let baseline = self.authoritative_step_snapshot(&ctx, &run_id).await?;
        let before = baseline.messages.len();
        // Baseline the compaction-fold count at the turn's start; a fold during the
        // turn grows it and the terminal step surfaces the marker. Set here (not on
        // resume) so it spans an awaiting→resumed turn.
        st.compactions_before = awaken_ext_compact::compaction_count(&baseline.state);
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
    /// (ADR-0011), reap / dead-letter GC (ADR-0015), and superseding submit
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

    /// Reconcile `thread`'s dispatch queue (ADR-0011, slice E): reclaim and re-run
    /// any dispatch left runnable by a crash. Returns the recovered run ids.
    pub async fn reconcile(&self, thread: &str) -> Result<Vec<String>, HostError> {
        let processed = self
            .durable_ingress(thread)
            .await?
            .recover(now_ms())
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(processed.into_iter().map(|(id, _)| id.0).collect())
    }

    /// Reap crashed dispatches on `thread` that have exhausted `max_attempts`
    /// crash-recoveries as of `now_ms` (ADR-0015, slice E). Returns how many were
    /// dead-lettered. `now_ms` is an as-of cutoff so an operator (or a test) can
    /// reap against a chosen clock.
    pub async fn reap(
        &self,
        thread: &str,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, HostError> {
        self.durable_ingress(thread)
            .await?
            .reap(max_attempts, now_ms)
            .await
            .map_err(|e| HostError::internal(e.to_string()))
    }

    /// The run ids currently dead-lettered on `thread` (ADR-0015, slice E).
    pub async fn dead_letters(&self, thread: &str) -> Result<Vec<String>, HostError> {
        let rows = self
            .durable_ingress(thread)
            .await?
            .list_dispatches()
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                row.thread_id.0 == thread
                    && row.state == awaken_run_ingress::DispatchState::DeadLetter
            })
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
        let rows = self
            .durable_ingress(thread)
            .await?
            .list_dispatches()
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(rows
            .into_iter()
            .filter(|row| row.thread_id.0 == thread)
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
        let rows = self
            .durable_ingress(thread)
            .await?
            .list_dispatches()
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                row.thread_id.0 == thread
                    && row.state == awaken_run_ingress::DispatchState::Superseded
            })
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
        let mut messages: Vec<Message> = {
            let mut st = ctx.state.lock().await;
            std::mem::take(&mut st.pending_system)
                .into_iter()
                .map(|text| {
                    Message::text(
                        MessageId(awaken_runtime::fresh_process_id("sys")),
                        Role::System,
                        text,
                    )
                })
                .collect()
        };
        messages.extend(input);
        let (uid, mut activation) = ctx
            .runtime
            .prepare(&ctx.config, thread.to_string(), messages);
        // Stamp the thread's per-turn model override (R2/R5) off the fingerprinted
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

    /// RunResume the run awaiting on `thread`, answering `tool_use_id` with `resume`.
    /// Fails closed unless `tool_use_id` names the pending tool and its binding
    /// (built-in vs client-executed) matches the resume variant.
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

        if matches!(
            ticket.reason,
            AwaitReason::UserInput | AwaitReason::ExternalEvent
        ) && ticket.pending_tool.is_none()
        {
            if ticket.call_id.as_deref() != Some(tool_use_id) {
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
        if ticket.reason == AwaitReason::Delegation {
            let registry = delegation_registry_from_snapshot(&awaiting_snapshot, &run_id)?;
            let child = self
                .authoritative_child_ticket(
                    ctx.commit.as_ref(),
                    registry.as_ref(),
                    ticket.call_id.as_deref(),
                )
                .await?;
            let result = if let Some(child_ticket) = child {
                self.check_pending(&child_ticket, tool_use_id, resume.wants_client())?;
                match resume {
                    HostResume::ToolPermission { allow, note } => {
                        if allow {
                            ResumeResult::allow()
                        } else {
                            ResumeResult::deny(note)
                        }
                    }
                    HostResume::ClientResult { content, is_error } => {
                        client_result_for_ticket(&child_ticket, tool_use_id, content, is_error)
                    }
                }
            } else {
                // Remote adapters may expose an opaque follow-up without a locally
                // committed child ticket. Keep that adapter boundary as typed user
                // input while still validating the parent call identity.
                if ticket.call_id.as_deref() != Some(tool_use_id) {
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
        let result = match resume {
            HostResume::ToolPermission { allow, note } => {
                if allow {
                    ResumeResult::allow()
                } else {
                    ResumeResult::deny(note)
                }
            }
            HostResume::ClientResult { content, is_error } => {
                let call_id = ticket.call_id.clone().unwrap_or_default();
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
    ) -> Result<Option<PendingTool>, HostError> {
        if ticket.reason != AwaitReason::Delegation {
            if ticket.reason == AwaitReason::ToolPermission && ticket.pending_tool.is_none() {
                return Err(HostError::internal(
                    "awaiting tool-permission run has no pending tool",
                ));
            }
            return Ok(pending_from_ticket(ticket));
        }
        let visible_child = self
            .authoritative_child_ticket(commit, registry, ticket.call_id.as_deref())
            .await?;
        Ok(match visible_child {
            Some(child) => pending_from_ticket(&child),
            None => pending_from_ticket(ticket).map(|mut pending| {
                pending.client_executed = true;
                pending
            }),
        })
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
        let (pending, awaiting) = match &state {
            RunState::Awaiting => {
                st.awaiting_run = Some(run_id.clone());
                let pending = if let Some(ticket) = recovery_ticket(&committed, &run_id) {
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
                (pending, true)
            }
            _ => {
                st.awaiting_run = None;
                (None, false)
            }
        };
        if !new_messages.is_empty() {
            self.hub
                .publish(thread, ThreadEvent::Committed(new_messages.clone()));
        }
        self.hub
            .publish(thread, ThreadEvent::StepEnded { awaiting });
        // A fold grows the committed compaction-marker count; compare the turn's
        // start baseline (set in `deliver_run`, spanning an awaiting→resumed turn) to
        // the terminal-step count so the marker surfaces exactly once. Count-based,
        // not run-id-based, so it works under durable ingress (where the worker
        // mints its own run id). The compact extension owns the key (G16).
        let compacted = !awaiting
            && awaken_ext_compact::compaction_count(&committed.state) > st.compactions_before;
        // The run's transient-retry counter: non-zero ⇒ the inference seam
        // transparently retried at least once, so the turn was auto-recovered.
        let rescheduled = ctx
            .reschedule
            .lock()
            .expect("reschedule mutex poisoned")
            .as_ref()
            .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed) > 0);
        let model_requests = ctx
            .model_requests
            .lock()
            .expect("model requests mutex poisoned")
            .as_ref()
            .map(|observations| {
                observations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            })
            .unwrap_or_default();
        let rescheduled_delegated_run_ids = ctx
            .rescheduled_runs
            .lock()
            .expect("rescheduled runs mutex poisoned")
            .as_ref()
            .map(|runs| {
                runs.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .iter()
                    .filter(|run| *run != &run_id.0)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let delegated_runs = project_delegated_runs(delegation_registry.as_ref());
        Ok(CommittedStepReceipt::from_verified(
            VerifiedStepProjection {
                run_id,
                new_messages,
                state,
                pending,
                compacted,
                rescheduled,
                model_requests,
                rescheduled_delegated_run_ids,
                delegated_runs,
            },
            &committed,
        ))
    }

    /// Fail closed before resuming: the asserted `tool_use_id` must name the
    /// run's pending tool, and that tool's binding must match the inbound resume
    /// — a client result may only answer a client-executed tool, a confirmation
    /// only a built-in one.
    fn check_pending(
        &self,
        ticket: &ResumeTicket,
        tool_use_id: &str,
        want_client: bool,
    ) -> Result<(), HostError> {
        let pending = pending_from_ticket(ticket)
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

fn recovery_ticket(committed: &RunRecoverySnapshot, run_id: &RunId) -> Option<ResumeTicket> {
    committed
        .resume_tickets
        .iter()
        .find(|entry| &entry.run_id == run_id)
        .map(|entry| entry.ticket.clone())
}

/// Read the pending tool off the committed awaiting ticket. `AwaitReason` is the
/// durable execution contract: `ExternalEvent` expects a client result, while
/// `ToolPermission` expects an allow/deny decision. Reopening a Runtime context
/// merely to rediscover that distinction would make a read perform Environment
/// realization before Session admission.
fn pending_from_ticket(ticket: &ResumeTicket) -> Option<PendingTool> {
    let tool_use_id = ticket.call_id.clone()?;
    let Some(tool) = ticket.pending_tool.clone() else {
        if matches!(
            ticket.reason,
            AwaitReason::UserInput | AwaitReason::ExternalEvent
        ) {
            return Some(PendingTool {
                tool_use_id,
                name: "agent_input".to_string(),
                input: serde_json::json!({ "reason": ticket.reason.as_stream_str() }),
                client_executed: true,
            });
        }
        return None;
    };
    let client_executed = ticket.reason == AwaitReason::ExternalEvent;
    Some(PendingTool {
        tool_use_id,
        name: tool.tool_id,
        input: tool.arguments,
        client_executed,
    })
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
            state: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 1,
            store_cursor: 1,
            next_commit_ordinal: 1,
        }
    }

    #[test]
    fn committed_step_proof_follows_the_fmeca_decision_table() {
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
                compacted: false,
                rescheduled: false,
                model_requests: Vec::new(),
                rescheduled_delegated_run_ids: Default::default(),
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

    #[test]
    fn remote_input_wait_projects_as_a_client_executed_agent_input() {
        // Cause/effect decision table: no pending_tool + UserInput/ExternalEvent
        // -> synthetic client-executed agent_input + ResumeResult::Input;
        // concrete pending_tool -> preserve ordinary client/built-in binding and
        // a client result becomes ToolResult. This test owns the remote row.
        let ticket = ResumeTicket {
            correlation_id: "a2a:remote-7:InputRequired".into(),
            run_id: RunId("run-7".into()),
            thread_id: ThreadId("thread-7".into()),
            snapshot_id: "snapshot-7".into(),
            catalog_fingerprint: "fingerprint-7".into(),
            delegation_origin: None,
            data_subject_id: None,
            reason: AwaitReason::UserInput,
            call_id: Some("remote-7".into()),
            pending_tool: None,
            deadline_ms: None,
        };

        let pending = pending_from_ticket(&ticket).expect("visible input");
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
        let mut concrete = ticket;
        concrete.pending_tool = Some(awaken_agent_contract::agent::awaiting::PendingTool {
            tool_id: "submit_answer".into(),
            arguments: serde_json::json!({"answer": 42}),
        });
        concrete.reason = AwaitReason::ExternalEvent;
        assert!(
            pending_from_ticket(&concrete).unwrap().client_executed,
            "P2"
        );
        concrete.reason = AwaitReason::ToolPermission;
        assert!(
            !pending_from_ticket(&concrete).unwrap().client_executed,
            "P3"
        );
    }
}
