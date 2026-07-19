//! Run driving for [`SharedHost`]: the neutral `run`/`resume`/`define_outcome`
//! entry points, step finalization, queries, and the durable-ops verbs.

use super::*;

impl SharedHost {
    /// All messages committed on `thread` so far (the source of history). Empty
    /// when the thread has not run yet. Resolves through `ctx_for`, so a durable
    /// thread is hydrated from its store on demand — a fresh process reads a
    /// awaiting thread's committed transcript even before any session touches it
    /// (ADR-0039), enabling post-restart session rehydration.
    /// True when the durable store already holds `thread` — WITHOUT building a
    /// session context (the layout probe lives with the commit boundary in
    /// [`crate::store`]).
    pub fn has_durable_thread(&self, thread: &str) -> bool {
        crate::store::durable_thread_exists(self.store_dir.as_deref(), thread)
    }

    pub async fn committed_messages(&self, thread: &str) -> Vec<Message> {
        match self.ctx_for(thread, None).await {
            Ok(ctx) => ctx.commit.committed_messages(&ctx.thread_id),
            Err(_) => Vec::new(),
        }
    }

    /// A thread's accumulated token usage, attributed per model (the run loop records
    /// it as committed thread state under [`THREAD_USAGE_STATE_KEY`]; each write is the
    /// running cumulative, so the last `Set` is the whole tally). Empty for a thread
    /// that has never run a real turn or whose provider reported no usage (the
    /// deterministic models). Callers use `.total()` for the session-level sum.
    pub async fn thread_usage(&self, thread: &str) -> awaken_runtime_contract::llm::ThreadUsage {
        use awaken_runtime_contract::llm::ThreadUsage;
        let Ok(ctx) = self.ctx_for(thread, None).await else {
            return ThreadUsage::default();
        };
        ThreadUsage::from_committed_state(&ctx.commit.committed_state(&ctx.thread_id))
    }

    /// True when `thread` has a run awaiting a decision.
    pub async fn is_awaiting(&self, thread: &str) -> bool {
        let ctx = match self.ctx_for(thread, None).await {
            Ok(ctx) => ctx,
            Err(_) => return false,
        };
        ctx.state.lock().await.awaiting_run.is_some()
    }

    /// The tool an awaiting run on `thread` is awaiting on, if any.
    pub async fn pending_tool(&self, thread: &str) -> Option<PendingTool> {
        let ctx = self.ctx_for(thread, None).await.ok()?;
        let st = ctx.state.lock().await;
        let run_id = st.awaiting_run.clone()?;
        pending_from_ticket(&ctx.commit.resume_ticket(&run_id)?, &self.client_tools)
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
    ) -> Result<RunResult, HostError> {
        self.deliver_run(agent, thread, input, false, None).await
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
    ) -> Result<RunResult, HostError> {
        self.deliver_run(agent, thread, input, false, Some(sink))
            .await
    }

    /// Submit a turn that *supersedes* the thread's prior pending/awaiting work
    /// (ADR-0022, slice E): the newest submission wins, stale dispatches are marked
    /// superseded and never claimed again, then the new run is driven. Requires
    /// durable ingress. Unlike `run` it does not fail closed on an awaiting
    /// thread — superseding an awaiting run is the point.
    pub(crate) async fn supersede_run(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
    ) -> Result<RunResult, HostError> {
        self.deliver_run(agent, thread, input, true, None).await
    }

    async fn deliver_run(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
        supersede: bool,
        sink: Option<Arc<dyn StreamSink>>,
    ) -> Result<RunResult, HostError> {
        let ctx = self.ctx_for(thread, agent).await?;
        let mut st = ctx.state.lock().await;
        if st.awaiting_run.is_some() && !supersede {
            return Err(HostError::bad_request("thread is awaiting a tool decision"));
        }
        if supersede && ctx.durable_ingress.is_none() {
            return Err(HostError::bad_request(
                "supersede requires durable ingress (set AWAKEN_INGRESS=durable)",
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
                        MessageId(format!("sys-{}", BASE_SEQ.fetch_add(1, Ordering::SeqCst))),
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
        let before = ctx.commit.committed_messages(&ctx.thread_id).len();
        // Baseline the compaction-fold count at the turn's start; a fold during the
        // turn grows it and the terminal step surfaces the marker. Set here (not on
        // resume) so it spans an awaiting→resumed turn.
        st.compactions_before =
            awaken_ext_compact::compaction_count(&ctx.commit.committed_state(&ctx.thread_id));
        // Prepare the activation (install catalog + register snapshot + mint ids),
        // then deliver it through the ingress seam. Direct ingress executes inline,
        // so this is behavior-identical to the former `start_run` call.
        let (mut run_id, mut activation) = ctx
            .runtime
            .prepare(&ctx.config, thread.to_string(), messages)
            .map_err(|e| HostError::internal(e.to_string()))?;
        // Stamp the thread's per-turn model override (R2/R5) onto the activation, OFF
        // the fingerprinted snapshot, so the resolve seam (here or on a claiming
        // worker) picks the effective model without a session-level registry.
        activation.model_ref_override = self.model_route.override_for(thread);
        if ctx.durable {
            // The durable path needs a run id that is unique across a restart: the
            // runtime's in-process id counter resets to 1 on restart and would
            // collide with an already-committed terminal run, which the dispatch
            // worker's terminal-run guard then skips (never re-running a finished
            // run) — silently dropping the turn. A wall-clock + sequence id cannot
            // collide with a prior process's ids.
            let uid = RunId(format!(
                "run-{}-{}",
                now_ms(),
                BASE_SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            activation.run_id = uid.clone();
            run_id = uid;
        }
        // Durable ingress queues the run through the dispatch store and drives it
        // via the worker (`submit_background`); a superseding submit first marks the
        // thread's stale pending/awaiting dispatches superseded (ADR-0022); direct
        // ingress runs it inline (`submit`). All drive to the same terminal/awaiting
        // state and commit through the same boundary, so `finish_step` is identical.
        // R3/R4: route to the ACP executor for acp:* threads, else the native
        // ingress (direct / durable / superseding). See `crate::run_exec`.
        let state = self
            .execute_activation(&ctx, thread, activation, supersede, sink)
            .await?;
        let result = self.finish_step(&ctx, &mut st, run_id, state, before, thread)?;
        drop(st);
        self.run_aux_after_step(&ctx, thread, &result.state).await;
        Ok(result)
    }

    /// Fire the out-of-band auxiliary agents (memory extraction) after a step reaches
    /// a terminal state. Shared by `run` and `resume`, so a turn that ended via a
    /// tool/delegation resume gets the same treatment as one that ended directly.
    /// No-op while the run is still awaiting. (Compaction is not out-of-band: it runs
    /// inline as the `compact` plugin's `BeforeInference` hook.)
    async fn run_aux_after_step(&self, ctx: &Arc<SessionCtx>, thread: &str, state: &RunState) {
        self.maybe_extract_memory(ctx, thread, state).await;
    }

    /// Fire out-of-band memory extraction when a turn reaches a terminal state
    /// (not awaiting) and memory is enabled. Seeds the extractor with only the
    /// messages committed since the last extraction (a per-thread cursor), so a
    /// long conversation is not re-processed every turn. Fire-and-forget (drained
    /// at shutdown). The cursor advances optimistically on trigger.
    async fn maybe_extract_memory(&self, ctx: &SessionCtx, thread: &str, state: &RunState) {
        if matches!(state, RunState::Awaiting) {
            return;
        }
        let Some(mem) = &self.memory else {
            return;
        };
        let committed = ctx.commit.committed_messages(&ctx.thread_id);
        let mut st = ctx.state.lock().await;
        let cursor = st.last_extracted_len.min(committed.len());
        if committed.len() <= cursor {
            return; // no new messages since the last extraction
        }
        let delta = committed[cursor..].to_vec();
        st.last_extracted_len = committed.len();
        drop(st);
        mem.trigger(thread, delta).await;
    }

    /// The durable ingress for `thread`, building the session if needed. Errors
    /// unless the server runs in durable mode (`AWAKEN_INGRESS=durable`). This is
    /// the operational entry for the ADR-0009 follow-on verbs (slice E): recover
    /// (ADR-0011), reap / dead-letter GC (ADR-0015), and superseding submit
    /// (ADR-0022).
    pub(crate) async fn durable_ingress(
        &self,
        thread: &str,
    ) -> Result<Arc<DurableRunIngress<AnyDispatchStore>>, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        ctx.durable_ingress.clone().ok_or_else(|| {
            HostError::bad_request("durable ingress not enabled (set AWAKEN_INGRESS=durable)")
        })
    }

    /// Reconcile `thread`'s dispatch queue (ADR-0011, slice E): reclaim and re-run
    /// any dispatch left runnable by a crash. Returns the recovered run ids.
    pub(crate) async fn reconcile(&self, thread: &str) -> Result<Vec<String>, HostError> {
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
    pub(crate) async fn reap(
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
    pub(crate) async fn dead_letters(&self, thread: &str) -> Result<Vec<String>, HostError> {
        let ids = self
            .durable_ingress(thread)
            .await?
            .dead_letters()
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(ids.into_iter().map(|id| id.0).collect())
    }

    /// Operator GC: purge every dead-lettered dispatch on `thread` (ADR-0015,
    /// slice E). Returns how many were removed.
    pub(crate) async fn purge_dead_letters(&self, thread: &str) -> Result<usize, HostError> {
        self.durable_ingress(thread)
            .await?
            .purge_dead_letters()
            .await
            .map_err(|e| HostError::internal(e.to_string()))
    }

    /// An operational snapshot of `thread`'s dispatch queue (ADR-0025): every row
    /// in enqueue order with its status and attempt count — the monitoring surface.
    pub(crate) async fn list_dispatches(
        &self,
        thread: &str,
    ) -> Result<Vec<(String, String, u64)>, HostError> {
        let rows = self
            .durable_ingress(thread)
            .await?
            .list_dispatches()
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|d| (d.run_id.0, format!("{:?}", d.state), d.attempt_count))
            .collect())
    }

    /// The run ids superseded by a newer submission on `thread` (ADR-0022,
    /// slice E).
    pub(crate) async fn superseded(&self, thread: &str) -> Result<Vec<String>, HostError> {
        let ids = self
            .durable_ingress(thread)
            .await?
            .superseded()
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(ids.into_iter().map(|id| id.0).collect())
    }

    /// Enqueue a run for the process dispatch pool to drive autonomously (ADR-0011,
    /// slice E follow-up): prepare the activation and hand it to the pool via
    /// `DispatchPool::submit` (durable enqueue + wake), returning immediately with
    /// the run id. The pool drains it out of band — no foreground request drives it
    /// — so the caller observes completion by polling committed truth. Requires
    /// durable ingress (`AWAKEN_INGRESS=durable`), which spawns the pool.
    pub(crate) async fn submit_background_async(
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
                        MessageId(format!("sys-{}", BASE_SEQ.fetch_add(1, Ordering::SeqCst))),
                        Role::System,
                        text,
                    )
                })
                .collect()
        };
        messages.extend(input);
        let (_run_id, mut activation) = ctx
            .runtime
            .prepare(&ctx.config, thread.to_string(), messages)
            .map_err(|e| HostError::internal(e.to_string()))?;
        // Stamp the thread's per-turn model override (R2/R5) off the fingerprinted
        // snapshot, so the claiming worker resolves the effective model itself.
        activation.model_ref_override = self.model_route.override_for(thread);
        // Restart-unique run id, same rationale as the foreground durable path.
        let uid = RunId(format!(
            "run-{}-{}",
            now_ms(),
            BASE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        activation.run_id = uid.clone();
        match self.dispatch_pool_or_err() {
            // Normal server: the local pool claims and drives it.
            Ok(pool) => pool
                .submit(activation)
                .await
                .map_err(|e| HostError::internal(e.to_string()))?,
            // Coordinator-only durable server (no local pool): enqueue straight into
            // the shared store so a remote database-less worker drains it over the
            // dispatch transport. Non-durable keeps the original "enable the pool" error.
            Err(e) if self.deployment.durable => {
                use awaken_run_ingress::DispatchQueue;
                let _ = e;
                let store =
                    crate::dispatch_backend::shared_durable_store(self.store_dir.as_deref())?;
                store
                    .enqueue(awaken_run_ingress::RunDispatch::new(activation))
                    .await
                    .map_err(|e| HostError::internal(e.to_string()))?;
            }
            Err(e) => return Err(e),
        }
        Ok(uid.0)
    }

    /// Resume the run awaiting on `thread`, answering `tool_use_id` with `resume`.
    /// Fails closed unless `tool_use_id` names the pending tool and its binding
    /// (built-in vs client-executed) matches the resume variant.
    pub async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: HostResume,
    ) -> Result<RunResult, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        let mut st = ctx.state.lock().await;
        let run_id = st
            .awaiting_run
            .clone()
            .ok_or_else(|| HostError::bad_request("no awaiting run to resume"))?;
        let ticket = ctx
            .commit
            .resume_ticket(&run_id)
            .ok_or_else(|| HostError::internal("awaiting run has no awaiting ticket"))?;

        // A awaiting delegation resumes through the kernel resolver with the user's
        // typed answer; the kernel routes it through the parent relationship to the
        // child's own Run service. The user never resumes the child directly.
        if ticket.reason == AwaitReason::Delegation {
            let mut parent_store = Store::new();
            for command in ctx.commit.committed_state(&ctx.thread_id) {
                if command.scope == Scope::Run && command.run_id.as_ref() == Some(&run_id) {
                    parent_store.apply(&command);
                }
            }
            let registry = RunDelegations::load(&parent_store)
                .map_err(|error| HostError::internal(error.to_string()))?;
            let child = child_ticket(ctx.as_ref(), registry.as_ref(), ticket.call_id.as_deref());
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
                        let output = if is_error {
                            ToolOutput::error(tool_use_id, content)
                        } else {
                            ToolOutput::ok(tool_use_id, content)
                        };
                        ResumeResult::ToolResult(output)
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
                ResumeResult::Input(content)
            };
            let before = ctx.commit.committed_messages(&ctx.thread_id).len();
            let command = ResumeCommand::from_ticket(&ticket, result, 0);
            let state = ctx
                .ingress
                .resume(command, ctx.context())
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
            let result = self.finish_step(&ctx, &mut st, run_id, state, before, thread)?;
            drop(st);
            self.run_aux_after_step(&ctx, thread, &result.state).await;
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
                    ToolOutput::error(call_id, content)
                } else {
                    ToolOutput::ok(call_id, content)
                };
                ResumeResult::ToolResult(output)
            }
        };
        let before = ctx.commit.committed_messages(&ctx.thread_id).len();
        let command = ResumeCommand::from_ticket(&ticket, result, 0);
        let state = ctx
            .ingress
            .resume(command, ctx.context())
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        let result = self.finish_step(&ctx, &mut st, run_id, state, before, thread)?;
        drop(st);
        self.run_aux_after_step(&ctx, thread, &result.state).await;
        Ok(result)
    }

    /// Define an outcome and drive the grade->revise loop over `thread`, bounded
    /// by `max_iterations`. Revision rounds auto-approve tools (the goal loop
    /// drives to a deliverable).
    pub async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<HostOutcomeReport, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        let mut st = ctx.state.lock().await;
        let goal = GoalSpec::new(description, rubric, max_iterations);

        // The runtime owns the grade->revise loop: a goal-enabled runtime whose
        // run-end guard steers revisions until the goal is met or the budget is
        // spent. The host drives one run and projects the rounds it committed. The
        // guard shares the thread's committed history and sandbox root.
        let goal_runtime = build_runtime(self.llm.clone(), &ctx.env)
            .with_plugin(Arc::new(GoalPlugin::new(goal, self.grader.clone())));
        // The goal run auto-approves tools to drive to a deliverable, so it does not
        // advertise `agent_run` (which awaits and is host-fulfilled, not auto-run).
        // The outcome/goal run does not offer skills (ADR-0036): it auto-approves
        // tools to drive a deliverable and does not register the `Skill` tool.
        let config = server_config(
            "assistant",
            &self.model_ref,
            &self.client_tools,
            &HashSet::new(),
            &["goal".to_string()],
            &self.plugin_config,
            &[],
            awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        );

        // One run: the guard re-derives and grades the deliverable, then steers
        // revisions. Outcome rounds auto-approve tools. Empty input re-infers over
        // the committed history. A concurrent `interrupt` cancels this run.
        let state = goal_runtime
            .run_to_completion(
                &config,
                thread,
                Vec::<Message>::new(),
                ctx.context(),
                |_| ResumeResult::allow(),
            )
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;

        // Project from DURABLE truth: the committed `Continuation` events the run
        // recorded, each carrying the round's opaque detail (result + explanation).
        // A `consumed_rounds` cursor scopes this to the rounds this call produced.
        let rounds: Vec<serde_json::Value> = ctx.commit.continuation_payloads(&ctx.thread_id);
        let fresh = &rounds[st.consumed_rounds.min(rounds.len())..];
        st.consumed_rounds = rounds.len();

        let outcome_id = format!("outc_{thread}");
        let all = ctx.commit.committed_messages(&ctx.thread_id);
        let mut iterations: Vec<HostOutcomeIteration> = fresh
            .iter()
            .enumerate()
            .map(|(i, detail)| HostOutcomeIteration {
                messages: if i == 0 { all.clone() } else { Vec::new() },
                outcome_id: outcome_id.clone(),
                iteration: i as u32 + 1,
                result: detail_str(detail, "result"),
                explanation: detail_str(detail, "explanation"),
            })
            .collect();
        // An interrupted run ends `Cancelled` before the guard can conclude, so
        // no terminal `Continuation` was committed. Report the outcome as
        // `interrupted` — distinct from satisfied/failed/max_iterations.
        if matches!(state, RunState::Ended(EndCause::Cancelled)) {
            iterations.push(HostOutcomeIteration {
                messages: Vec::new(),
                outcome_id: outcome_id.clone(),
                iteration: iterations.len() as u32 + 1,
                result: "interrupted".to_string(),
                explanation: "the outcome was interrupted".to_string(),
            });
        }
        Ok(HostOutcomeReport { iterations })
    }

    /// Project the step's delta, update the awaiting position, and publish the
    /// delta to the thread hub for any observing protocol.
    fn finish_step(
        &self,
        ctx: &SessionCtx,
        st: &mut SessionState,
        run_id: RunId,
        state: RunState,
        before: usize,
        thread: &str,
    ) -> Result<RunResult, HostError> {
        let all = ctx.commit.committed_messages(&ctx.thread_id);
        let new_messages = all[before.min(all.len())..].to_vec();
        let mut run_store = Store::new();
        for command in ctx.commit.committed_state(&ctx.thread_id) {
            if command.scope == Scope::Run && command.run_id.as_ref() == Some(&run_id) {
                run_store.apply(&command);
            }
        }
        let delegation_registry = RunDelegations::load(&run_store)
            .map_err(|error| HostError::internal(error.to_string()))?;
        let (pending, awaiting) = match &state {
            RunState::Awaiting => {
                st.awaiting_run = Some(run_id.clone());
                let pending = ctx.commit.resume_ticket(&run_id).and_then(|ticket| {
                    // A parent waiting on a child exposes the CHILD's ordinary
                    // interaction request. The protocol still addresses the
                    // parent session; it never obtains a bypass around the
                    // parent-child relationship.
                    let visible_child = if ticket.reason == AwaitReason::Delegation {
                        child_ticket(ctx, delegation_registry.as_ref(), ticket.call_id.as_deref())
                    } else {
                        None
                    };
                    match visible_child {
                        Some(child) => pending_from_ticket(&child, &self.client_tools),
                        None if ticket.reason == AwaitReason::Delegation => {
                            // A remote Agent may return an opaque InputRequired
                            // continuation without a local child ticket. The parent
                            // session mediates that user interaction, so it is
                            // client-executed even though the initiating agent_run
                            // tool itself is host-executed.
                            pending_from_ticket(&ticket, &self.client_tools).map(|mut pending| {
                                pending.client_executed = true;
                                pending
                            })
                        }
                        None => pending_from_ticket(&ticket, &self.client_tools),
                    }
                });
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
            && awaken_ext_compact::compaction_count(&ctx.commit.committed_state(&ctx.thread_id))
                > st.compactions_before;
        // The run's transient-retry counter: non-zero ⇒ the inference seam
        // transparently retried at least once, so the turn was auto-recovered.
        let rescheduled = ctx
            .reschedule
            .lock()
            .expect("reschedule mutex poisoned")
            .as_ref()
            .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed) > 0);
        let delegated_runs = delegation_registry
            .into_iter()
            .flat_map(|registry| {
                registry
                    .delegations()
                    .map(|delegation| DelegatedRun {
                        run_id: delegation.child_run_id.clone(),
                        parent_call_id: delegation.parent_call_id.clone(),
                        agent_id: delegation.target_agent_id.clone(),
                        status: delegation.status,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok(RunResult {
            new_messages,
            state,
            pending,
            compacted,
            rescheduled,
            delegated_runs,
        })
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
        if ticket.call_id.as_deref() != Some(tool_use_id) {
            return Err(HostError::bad_request(format!(
                "tool_use_id {tool_use_id:?} does not match the pending tool"
            )));
        }
        let pending_tool_id = ticket
            .pending_tool
            .as_ref()
            .map(|t| t.tool_id.as_str())
            .ok_or_else(|| HostError::internal("awaiting run has no pending tool"))?;
        let is_client = self.client_tools.contains(pending_tool_id);
        if is_client != want_client {
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

/// Resolve the ordinary ticket owned by the child named by a parent's pending
/// delegation call. Relationship identity stays in `RunDelegations`; the ticket
/// stays in the child Run. This function only joins those two committed facts for
/// the parent-facing interaction projection.
fn child_ticket(
    ctx: &SessionCtx,
    registry: Option<&awaken_agent_contract::agent::delegation::DelegationRegistry>,
    parent_call_id: Option<&str>,
) -> Option<ResumeTicket> {
    let parent_call_id = parent_call_id?;
    let child_run_id = registry?
        .delegations()
        .find(|relationship| relationship.parent_call_id == parent_call_id)?
        .child_run_id
        .clone();
    ctx.commit.resume_ticket(&child_run_id)
}

/// Read a string field from an opaque round detail, defaulting to empty.
fn detail_str(detail: &serde_json::Value, key: &str) -> String {
    detail
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Read the pending tool off an awaiting ticket, classifying it client-executed
/// when its id is in `client_tools`.
fn pending_from_ticket(
    ticket: &ResumeTicket,
    client_tools: &HashSet<String>,
) -> Option<PendingTool> {
    let tool_use_id = ticket.call_id.clone()?;
    let tool = ticket.pending_tool.clone()?;
    let client_executed = client_tools.contains(&tool.tool_id);
    Some(PendingTool {
        tool_use_id,
        name: tool.tool_id,
        input: tool.arguments,
        client_executed,
    })
}
