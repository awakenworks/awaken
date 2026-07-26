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
        crate::store::durable_thread_exists_with_store(
            self.deployment.store,
            self.store_dir.as_deref(),
            thread,
        )
    }

    pub async fn committed_messages(&self, thread: &str) -> Vec<Message> {
        match self.ctx_for(thread, None).await {
            Ok(ctx) => ctx.commit.committed_messages(&ctx.thread_id),
            Err(error) => {
                tracing::warn!(thread, error = %error, "failed to open committed thread history");
                Vec::new()
            }
        }
    }

    /// Durable committed-truth lifecycle feed for the partition containing
    /// `thread`. A database-less Worker has only a non-authoritative recovery
    /// projection and therefore cannot expose this Control-side feed.
    pub async fn run_lifecycle_feed(
        &self,
        thread: &str,
    ) -> Result<awaken_agent_contract::CheckpointRunLifecycleFeed, HostError> {
        self.ctx_for(thread, None)
            .await?
            .commit
            .lifecycle_feed()
            .ok_or_else(|| {
                HostError::bad_request(
                    "run lifecycle feed is available only from committed-truth authority",
                )
            })
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
        let _execution = ctx.execution.lock().await;
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
        drop(st);
        let (generated_run_id, mut activation) =
            ctx.runtime
                .prepare(&ctx.config, ctx.thread_id.0.clone(), messages);
        let run_id = if ctx.durable {
            RunId(format!(
                "run-{}-{}",
                now_ms(),
                BASE_SEQ.fetch_add(1, Ordering::SeqCst)
            ))
        } else {
            generated_run_id
        };
        activation.run_id = run_id.clone();
        activation.model_ref_override = self.inference_routing.override_for(thread);
        let executor = crate::run_exec::BoundRunExecutor::new(self, ctx.clone())
            .with_supersede(supersede)
            .with_stream_sink(sink);
        let state = awaken_runtime_contract::execution::RunExecutor::execute(
            &executor,
            activation,
            awaken_runtime_contract::RuntimeRunContext::new(),
        )
        .await
        .map_err(|error| HostError::internal(error.to_string()))?;
        let mut st = ctx.state.lock().await;
        let result = self.finish_step(&ctx, &mut st, run_id, state, before, thread)?;
        Ok(result)
    }

    /// Resume through the same in-flight identity slot as a fresh foreground
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
        let result = ctx.ingress.resume(activation, command, ctx.context()).await;
        {
            let mut active = ctx.active_run.lock().expect("active run mutex poisoned");
            if active.as_ref() == Some(&run_id) {
                *active = None;
            }
        }
        result.map_err(|error| HostError::internal(error.to_string()))
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
        let (_run_id, mut activation) =
            ctx.runtime
                .prepare(&ctx.config, thread.to_string(), messages);
        // Stamp the thread's per-turn model override (R2/R5) off the fingerprinted
        // snapshot, so the claiming worker resolves the effective model itself.
        activation.model_ref_override = self.inference_routing.override_for(thread);
        // Restart-unique run id, same rationale as the foreground durable path.
        let uid = RunId(format!(
            "run-{}-{}",
            now_ms(),
            BASE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
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
        let _execution = ctx.execution.lock().await;
        let run_id = ctx
            .state
            .lock()
            .await
            .awaiting_run
            .clone()
            .ok_or_else(|| HostError::bad_request("no awaiting run to resume"))?;
        let ticket = ctx
            .commit
            .resume_ticket(&run_id)
            .ok_or_else(|| HostError::internal("awaiting run has no awaiting ticket"))?;

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
            let before = ctx.commit.committed_messages(&ctx.thread_id).len();
            let activation = ctx.resume_activation(&ticket);
            let command = ResumeCommand::from_ticket(&ticket, ResumeResult::Input(content), 0);
            let state = self.drive_resume(&ctx, activation, command).await?;
            let mut st = ctx.state.lock().await;
            let result = self.finish_step(&ctx, &mut st, run_id, state, before, thread)?;
            return Ok(result);
        }

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
            let activation = ctx.resume_activation(&ticket);
            let command = ResumeCommand::from_ticket(&ticket, result, 0);
            let state = self.drive_resume(&ctx, activation, command).await?;
            let mut st = ctx.state.lock().await;
            let result = self.finish_step(&ctx, &mut st, run_id, state, before, thread)?;
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
        let activation = ctx.resume_activation(&ticket);
        let command = ResumeCommand::from_ticket(&ticket, result, 0);
        let state = self.drive_resume(&ctx, activation, command).await?;
        let mut st = ctx.state.lock().await;
        let result = self.finish_step(&ctx, &mut st, run_id, state, before, thread)?;
        Ok(result)
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

/// Read the pending tool off an awaiting ticket, classifying it client-executed
/// when its id is in `client_tools`.
fn pending_from_ticket(
    ticket: &ResumeTicket,
    client_tools: &HashSet<String>,
) -> Option<PendingTool> {
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
    let client_executed = client_tools.contains(&tool.tool_id);
    Some(PendingTool {
        tool_use_id,
        name: tool.tool_id,
        input: tool.arguments,
        client_executed,
    })
}

#[cfg(test)]
mod ticket_projection_tests {
    use super::*;

    #[test]
    fn remote_input_wait_projects_as_a_client_executed_agent_input() {
        let ticket = ResumeTicket {
            correlation_id: "a2a:remote-7:InputRequired".into(),
            run_id: RunId("run-7".into()),
            thread_id: ThreadId("thread-7".into()),
            snapshot_id: "snapshot-7".into(),
            catalog_fingerprint: "fingerprint-7".into(),
            delegation_origin: None,
            reason: AwaitReason::UserInput,
            call_id: Some("remote-7".into()),
            pending_tool: None,
            deadline_ms: None,
        };

        let pending = pending_from_ticket(&ticket, &HashSet::new()).expect("visible input");
        assert_eq!(pending.tool_use_id, "remote-7");
        assert_eq!(pending.name, "agent_input");
        assert!(pending.client_executed);
        assert_eq!(pending.input["reason"], "user_input");
    }
}
