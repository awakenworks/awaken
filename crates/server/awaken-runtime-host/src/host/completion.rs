//! Durable-foreground observation: the [`SharedHost`] pool-submit/await methods
//! and the [`CompletionRegistry`] that owns temporary completion observers plus
//! best-effort live routes for each foreground Run.

pub(crate) use super::placement::requires_local_environment;
use super::placement::{
    InferencePlaintextHolderDecision, inference_plaintext_holder_decision, remote_worker_placement,
    self_hosted_holder_for_boundary, worker_local_credentials,
};
use super::*;
#[cfg(test)]
use awaken_run_ingress::{HOST_EXECUTOR_CAPABILITY, PROVIDER_CREDENTIAL_SOURCE_CAPABILITY};
use std::collections::HashMap;

impl SharedHost {
    /// Resolve the one exact inference plaintext holder used by both direct and
    /// dispatched attempts from the immutable candidate publication.
    pub(crate) fn inference_plaintext_holder(
        &self,
        activation: &RunActivation,
    ) -> Result<Option<awaken_runtime_contract::PlaintextHolder>, HostError> {
        Ok(match inference_plaintext_holder_decision(activation)? {
            InferencePlaintextHolderDecision::NotRequired => None,
            InferencePlaintextHolderDecision::Exact(holder) => Some(holder),
            InferencePlaintextHolderDecision::Boundary(boundary) => {
                let holder = self
                    .thread_credential_realization(&activation.thread_id.0)
                    .map_or_else(
                        || self_hosted_holder_for_boundary(boundary),
                        |profile| profile.inference_holder,
                    );
                if holder.boundary != boundary {
                    return Err(HostError::bad_request(
                        "the Session Environment credential holder conflicts with the selected model backend",
                    ));
                }
                Some(holder)
            }
        })
    }

    pub(crate) fn resolved_dispatch(
        &self,
        activation: RunActivation,
    ) -> Result<RunDispatch, HostError> {
        self.resolved_dispatch_with_traceparent_and_affinity(
            activation,
            awaken_observability::current_traceparent(),
            true,
        )
    }

    /// Decorate an extension-owned ordinary Run that shares a prepared
    /// Session's Thread and frozen execution inputs without converting it into
    /// a second Session-root admission. Outcome owns these Runs and their
    /// lifecycle; the Session aggregate remains only a conservative recovery
    /// candidate and must not acquire a parallel activity state machine.
    pub(crate) fn resolved_thread_extension_dispatch(
        &self,
        activation: RunActivation,
    ) -> Result<RunDispatch, HostError> {
        self.resolved_dispatch_with_traceparent_and_affinity(
            activation,
            awaken_observability::current_traceparent(),
            false,
        )
    }

    /// Decorate one activation using an explicit admission trace source. Session
    /// Event recovery supplies the context frozen in root provenance (including
    /// an explicit absence); ordinary foreground admission delegates here with
    /// its current validated OpenTelemetry context.
    pub(crate) fn resolved_dispatch_with_traceparent(
        &self,
        activation: RunActivation,
        traceparent: Option<String>,
    ) -> Result<RunDispatch, HostError> {
        self.resolved_dispatch_with_traceparent_and_affinity(activation, traceparent, true)
    }

    fn resolved_dispatch_with_traceparent_and_affinity(
        &self,
        activation: RunActivation,
        traceparent: Option<String>,
        associate_prepared_session: bool,
    ) -> Result<RunDispatch, HostError> {
        // `UNCONFIGURED_MODEL_REF` is a Coordinator-owned guidance executor, not a
        // remotely materializable model.  In a coordinator-only/all-in-one
        // deployment the registered Worker deliberately does not advertise the
        // generic host-executor capability, so enqueueing this activation can
        // never make progress.  Reject it before durable enqueue instead of
        // leaving a permanently pending run and reporting a misleading 60-second
        // dispatch-pool timeout.
        if activation.effective_model_ref() == crate::UNCONFIGURED_MODEL_REF {
            return Err(HostError::bad_request(
                "No model is configured. Connect a provider and select a model in Author > Quickstart before running this Agent.",
            ));
        }
        let thread = activation.thread_id.0.clone();
        let workspace = self.thread_workspace(&thread);
        let agent_publications = crate::agent_catalog::freeze_run_publications(
            &activation.snapshot,
            self.agent_publications.as_deref(),
            &workspace,
        )
        .map_err(|error| {
            HostError::bad_request(format!(
                "cannot freeze Agent execution publications: {error}"
            ))
        })?;
        let resources = self.thread_resource_manifest(&thread);
        let runtime_projection = self
            .session_slots
            .read(&thread, |slot| {
                slot.environment_snapshot.clone().map(|environment| {
                    let mcp_stages = slot
                        .mcp
                        .iter()
                        .filter(|projection| {
                            projection.state == crate::session_slot::McpProjectionState::Active
                        })
                        .map(|projection| projection.request.clone())
                        .collect();
                    (environment, slot.tools.clone(), mcp_stages)
                })
            })
            .flatten();
        // `SessionRuntime::install_session_projection` is the sole
        // Coordinator-side owner that installs this frozen runtime projection.
        // Preserve that existing
        // ownership fact in the dispatch contract so a registered Worker enters
        // the canonical Control-owned realization path before it constructs the
        // execution context. Resource manifests alone are deliberately
        // insufficient: ordinary Runs may carry one without being Sessions.
        let is_session_dispatch = self
            .session_slots
            .read(&thread, |slot| slot.session_dispatch)
            .unwrap_or(false)
            || runtime_projection.is_some();
        let environment_snapshot = runtime_projection
            .as_ref()
            .map(|(environment, _, _)| environment);
        let inference_holder = self.inference_plaintext_holder(&activation)?;
        // A mixed deployment may have both the local pool and remote workers.
        // Any carried manifest still needs capability admission: an explicit empty
        // successor can be the operation that removes a prior projection.
        let worker_local = !worker_local_credentials(&activation.snapshot.resolved_spec).is_empty();
        let placement = (self.deployment.disable_local_pool || resources.is_some() || worker_local)
            .then(|| {
                remote_worker_placement(
                    &activation.snapshot.resolved_spec,
                    environment_snapshot,
                    resources.as_ref(),
                    self.deployment.disable_local_pool,
                )
            });
        let mut request = RunDispatch::new(activation)
            .with_traceparent(traceparent)
            .with_agent_publications(agent_publications);
        if associate_prepared_session && is_session_dispatch {
            request = request.for_session(ThreadId(thread.clone()));
        }
        if let Some(holder) = inference_holder {
            request = request.with_inference_plaintext_holder(holder);
        }
        if let Some(resources) = resources {
            let envelope = awaken_run_ingress::SessionResourceEnvelope::from_manifest(&resources)
                .map_err(|error| {
                HostError::internal(format!("serialize Session resource manifest: {error}"))
            })?;
            request = request
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from(resources.workspace_id.clone()),
                ))
                .with_session_resources(envelope);
        }
        if let Some((environment, tools, mcp_stages)) = runtime_projection {
            // Remote warm capacity is container-backed and therefore preserves
            // the frozen network policy. Derive the preference from the same
            // canonical SandboxSpec identity used by Worker pool receipts.
            let preferred_shape =
                crate::provisioning::environment_capacity_projection(&environment, true).shape_id;
            let envelope = awaken_run_ingress::SessionRuntimeEnvelope::from_projection(
                environment,
                tools,
                mcp_stages,
            )
            .map_err(|error| {
                HostError::internal(format!("serialize Session runtime projection: {error}"))
            })?;
            request = request
                .with_session_runtime(envelope)
                .with_preferred_environment_shape(preferred_shape);
        }
        if let Some(placement) = placement {
            request = request.with_placement(placement);
        }
        Ok(request)
    }

    /// The process dispatch pool, or a fail-closed error when durable ingress (and
    /// thus the pool) is not enabled.
    pub(crate) fn dispatch_pool_or_err(
        &self,
    ) -> Result<&Arc<DispatchPool<AnyDispatchStore>>, HostError> {
        self.dispatch_pool.get().ok_or_else(|| {
            HostError::bad_request(
                "durable dispatch not enabled (set typed durable ingress to run the pool)",
            )
        })
    }

    /// Publish one Session-approved reservation through the existing dispatch
    /// row. This is the sole Host activation implementation used by both the
    /// non-blocking reconciler port and foreground observation.
    pub(crate) async fn activate_session_run_reservation(
        &self,
        delivery: awaken_session_contract::SessionRunDelivery,
    ) -> Result<awaken_session_contract::SessionRunActivation, HostError> {
        use awaken_run_ingress::Outbox as _;

        if delivery.session_activity_epoch == 0 {
            return Err(HostError::bad_request(
                "Session User Run activity epoch must be nonzero",
            ));
        }
        let store = self.dispatch_store()?;
        let outcome = store
            .activate_session_run_reservation(
                &delivery.run_id,
                &ThreadId(delivery.session_id),
                delivery.session_activity_epoch,
            )
            .await
            .map_err(|error| HostError::unavailable(error.to_string()))?;
        let projected = match outcome {
            awaken_run_ingress::SessionRunReservationActivation::Activated => {
                awaken_session_contract::SessionRunActivation::Activated
            }
            awaken_run_ingress::SessionRunReservationActivation::AlreadyActivated {
                session_activity_epoch,
            } => awaken_session_contract::SessionRunActivation::AlreadyActivated {
                session_activity_epoch,
            },
            awaken_run_ingress::SessionRunReservationActivation::RecoveryClaimed => {
                awaken_session_contract::SessionRunActivation::RecoveryClaimed
            }
            awaken_run_ingress::SessionRunReservationActivation::Completed => {
                awaken_session_contract::SessionRunActivation::Completed
            }
            awaken_run_ingress::SessionRunReservationActivation::MissingOrRejected => {
                return Err(HostError::bad_request(
                    "Session User Run reservation is missing or rejected",
                ));
            }
            awaken_run_ingress::SessionRunReservationActivation::Conflict => {
                return Err(HostError::bad_request(
                    "Session User Run reservation activation conflicts with durable truth",
                ));
            }
        };
        if matches!(
            projected,
            awaken_session_contract::SessionRunActivation::Activated
                | awaken_session_contract::SessionRunActivation::AlreadyActivated { .. }
        ) {
            if let Some(pool) = self.dispatch_pool.get() {
                pool.notify().await;
            } else {
                store
                    .relay()
                    .await
                    .map_err(|error| HostError::unavailable(error.to_string()))?;
            }
        }
        Ok(projected)
    }

    /// Register before activation and observe only the committed Run lifecycle.
    /// The registry is a wakeup/preview relay; peer settlement is recovered by
    /// the same authoritative Thread read used by other foreground operations.
    pub(crate) async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        stream_sink: Option<Arc<dyn StreamSink>>,
    ) -> Result<CommittedStepReceipt, HostError> {
        let session_id = admission.session_id().to_string();
        let run_id = admission.run_id().clone();
        let ctx = self.ctx_for(&session_id, None).await?;
        let state = activate_and_await_session_run(
            &self.completion,
            &run_id,
            stream_sink,
            std::time::Duration::from_millis(250),
            || async move {
                match admission {
                    awaken_session_contract::AdmittedSessionRun::Reserved(delivery)
                    | awaken_session_contract::AdmittedSessionRun::AlreadyReserved(delivery)
                    | awaken_session_contract::AdmittedSessionRun::AlreadyActivated(delivery) => {
                        self.activate_session_run_reservation(delivery).await
                    }
                    awaken_session_contract::AdmittedSessionRun::RecoveryClaimed { .. } => {
                        Ok(awaken_session_contract::SessionRunActivation::RecoveryClaimed)
                    }
                    awaken_session_contract::AdmittedSessionRun::Completed { .. } => {
                        Ok(awaken_session_contract::SessionRunActivation::Completed)
                    }
                }
            },
            || self.read_settled_phase(&ctx, &run_id, None),
        )
        .await?;
        self.project_settled_session_run(&ctx, run_id, state, &input_message_ids)
            .await
    }

    /// Register before publishing one Session-owned resume, then project only
    /// the next committed Step. The durable reply ingress and Worker execution
    /// are shared with background Managed Event reconciliation.
    pub(crate) async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
    ) -> Result<CommittedStepReceipt, HostError> {
        let session_id = delivery.command.session_id.clone();
        let run_id = delivery.command.expected_run_id.clone();
        let correlation_id = delivery.command.expected_correlation_id.clone();
        let ctx = self.ctx_for(&session_id, None).await?;
        let messages_before = self
            .authoritative_step_snapshot(&ctx, &run_id)
            .await?
            .messages
            .len();
        let (settled, _waiter_guard) = self.completion.register(&run_id, None);
        self.reply_session_thread_tool(delivery).await?;
        let state = self
            .await_settled_event(&ctx, &run_id, Some(&correlation_id), settled)
            .await?;
        self.project_settled_session_resume(&ctx, run_id, state, messages_before)
            .await
    }

    /// Submit a durable run and wait for the pool to drive it to a settled state.
    /// Under the shared queue a session's own worker must not claim (it would grab
    /// foreign threads' runs), so the foreground durable path enqueues, nudges the
    /// pool, and waits for the pool to signal completion — **by event**, not by
    /// polling committed truth, so it pays no poll-interval latency. `supersede`
    /// marks the thread's prior pending work superseded first (ADR-0022).
    pub(crate) async fn submit_durable_foreground(
        &self,
        ctx: &Arc<SessionCtx>,
        activation: RunActivation,
        supersede: bool,
        stream_sink: Option<Arc<dyn StreamSink>>,
        associate_prepared_session: bool,
    ) -> Result<awaken_agent_contract::agent::run::RunState, HostError> {
        let run_id = activation.run_id.clone();
        // Register for the settle event BEFORE enqueue, so the pool cannot drive and
        // settle the run before this caller is listening (no lost wakeup). The guard
        // removes the waiter if this future is dropped (client disconnect) before it
        // settles — held to the end of this method.
        let (settled, _waiter_guard) = self.completion.register(&run_id, stream_sink);
        let request = if associate_prepared_session {
            self.resolved_dispatch(activation)?
        } else {
            self.resolved_thread_extension_dispatch(activation)?
        };
        // Enqueue only — never drive here; the pool is the sole claimer. The common
        // path goes through `pool.submit` (which stamps the trace); a superseding
        // submit needs the supersede option, so it enqueues on the shared store and
        // nudges the pool directly.
        if !ctx.delivery.is_durable() {
            return Err(HostError::internal(
                "durable submit requires durable delivery",
            ));
        }
        let worker = &ctx.claimed_worker;
        if supersede {
            worker
                .store()
                .enqueue_with(
                    request,
                    SubmitOptions {
                        supersede: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
            if let Some(pool) = self.dispatch_pool.get() {
                pool.notify().await;
            }
        } else if let Some(pool) = self.dispatch_pool.get() {
            pool.submit_dispatch(request)
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
        } else {
            // A coordinator-only cell still authors the same durable dispatch;
            // registered remote Workers are its only claimers. Their authenticated
            // settle route wakes this Host's completion registry from committed
            // Run truth, so no local executor or polling compatibility path exists.
            worker
                .store()
                .enqueue(request)
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
        }
        self.await_settled_event(ctx, &run_id, None, settled).await
    }

    /// Deliver one foreground answer through the durable inbox and wait until the
    /// dispatch worker settles the exact resumed ticket. The queue remains the sole
    /// durable execution/settlement authority; this method only authors input and
    /// observes the resulting committed Run fact.
    pub(crate) async fn resume_durable_foreground(
        &self,
        ctx: &Arc<SessionCtx>,
        command: ResumeCommand,
    ) -> Result<RunState, HostError> {
        let run_id = command.run_id.clone();
        let correlation_id = command.correlation_id.clone();
        // Register before append so a local pool cannot settle between input
        // publication and waiter installation.
        let (settled, _waiter_guard) = self.completion.register(&run_id, None);
        let input = durable_resume_input(command);
        if !ctx.delivery.is_durable() {
            return Err(HostError::internal(
                "durable resume requires durable delivery",
            ));
        }
        let worker = &ctx.claimed_worker;
        if let Some(pool) = self.dispatch_pool.get() {
            pool.deliver(input)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
        } else {
            // Coordinator-only cells publish to the same shared store. Remote
            // Workers claim it on their ordinary wake/poll path and committed-truth
            // reconciliation below observes their settlement.
            worker
                .store()
                .append(input)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
        }
        self.await_settled_event(ctx, &run_id, Some(&correlation_id), settled)
            .await
    }

    /// Wait for the pool's settle signal for `run_id` (sub-millisecond wakeup), with
    /// committed-truth reconciliation for peer Coordinators. A foreground transport
    /// lifetime is not a Run deadline: long-running work remains `Running` until the
    /// runtime commits `Awaiting` or `Ended`, and client cancellation drops this
    /// future and its waiter guard.
    async fn await_settled_event(
        &self,
        ctx: &Arc<SessionCtx>,
        run_id: &RunId,
        answered_correlation: Option<&str>,
        settled: tokio::sync::oneshot::Receiver<RunState>,
    ) -> Result<RunState, HostError> {
        await_completion_state(settled, std::time::Duration::from_millis(250), || {
            self.read_settled_phase(ctx, run_id, answered_correlation)
        })
        .await
    }

    /// One committed-truth read: the run's state if it has settled (`Ended` or
    /// `Awaiting`), else `None`. The fallback path for `await_settled_event`.
    async fn read_settled_phase(
        &self,
        ctx: &Arc<SessionCtx>,
        run_id: &RunId,
        answered_correlation: Option<&str>,
    ) -> Result<Option<RunState>, HostError> {
        let record = ctx
            .commit
            .authoritative_run(run_id)
            .await
            .map_err(HostError::internal)?;
        match record {
            Some(
                record @ awaken_agent_contract::agent::run::Record {
                    state: RunState::Ended(_),
                    ..
                },
            ) => Ok(Some(record.state)),
            Some(
                record @ awaken_agent_contract::agent::run::Record {
                    state: RunState::Awaiting,
                    ..
                },
            ) => {
                let Some(answered_correlation) = answered_correlation else {
                    return Ok(Some(record.state));
                };
                // Before a remote Worker consumes the answer, committed truth is
                // still Awaiting on the answered ticket. That is not a settlement
                // of this resume. Only a newly committed ticket (or Ended above)
                // releases the foreground caller.
                let current = ctx
                    .commit
                    .open_wait_for_thread(&ctx.thread_id)
                    .await
                    .map_err(HostError::internal)?;
                Ok(awaiting_ticket_advanced(
                    run_id,
                    answered_correlation,
                    current
                        .as_ref()
                        .map(|(current_run, ticket)| (current_run, ticket.correlation_id.as_str())),
                )
                .then_some(record.state))
            }
            _ => Ok(None),
        }
    }
}

fn awaiting_ticket_advanced(
    observed_run: &RunId,
    answered_correlation: &str,
    current_wait: Option<(&RunId, &str)>,
) -> bool {
    current_wait.is_some_and(|(current_run, current_correlation)| {
        current_run == observed_run && current_correlation != answered_correlation
    })
}

/// A resume ticket is a one-answer idempotency boundary. Encoding the exact
/// `(run, correlation)` pair with a run-length prefix is collision-free for
/// arbitrary string contents; replaying another payload for that same ticket is
/// rejected by the Inbox's existing idempotency-conflict rule.
fn durable_resume_input(command: ResumeCommand) -> PendingInput {
    let message_id = format!(
        "foreground-resume:{}:{}{}",
        command.run_id.0.len(),
        command.run_id.0,
        command.correlation_id
    );
    PendingInput {
        message_id,
        run_id: command.run_id,
        thread_id: command.thread_id,
        correlation_id: command.correlation_id,
        available_at_ms: None,
        context_messages: Vec::new(),
        result: command.result,
    }
}

/// Await the existing completion signal while reconciling the one committed Run
/// authority. `None` means the Run is still live and must never be projected as a
/// timeout/error by this transport helper. Dispatch lease recovery and dead-letter
/// policy own genuinely abandoned execution; cancellation owns caller departure.
async fn await_completion_state<Read, ReadFuture>(
    settled: tokio::sync::oneshot::Receiver<RunState>,
    reconciliation_interval: std::time::Duration,
    mut read_settled: Read,
) -> Result<RunState, HostError>
where
    Read: FnMut() -> ReadFuture,
    ReadFuture: std::future::Future<Output = Result<Option<RunState>, HostError>>,
{
    let mut settled = std::pin::pin!(settled);
    // The local event is the fast path. A peer Coordinator can commit the same
    // shared PostgreSQL Run without owning this process's oneshot sender, so
    // committed-truth reconciliation is also required. One foreground waiter
    // performs one narrow read per interval; it never claims, settles, times out,
    // or creates a second completion authority.
    loop {
        tokio::select! {
            result = &mut settled => {
                if let Ok(state) = result {
                    return Ok(state);
                }
                return read_settled().await?.ok_or_else(|| {
                    HostError::internal(
                        "durable completion signal closed before committed Run settlement",
                    )
                });
            }
            _ = tokio::time::sleep(reconciliation_interval) => {
                if let Some(state) = read_settled().await? {
                    return Ok(state);
                }
            }
        }
    }
}

/// One register-before-activate composition shared by every Session User Run
/// foreground caller. Activation outcomes remain typed, while the only value
/// returned across the observation boundary is committed `RunState`.
async fn activate_and_await_session_run<Activate, ActivateFuture, Read, ReadFuture>(
    registry: &Arc<CompletionRegistry>,
    run_id: &RunId,
    stream_sink: Option<Arc<dyn StreamSink>>,
    reconciliation_interval: std::time::Duration,
    activate: Activate,
    mut read_settled: Read,
) -> Result<RunState, HostError>
where
    Activate: FnOnce() -> ActivateFuture,
    ActivateFuture: std::future::Future<
            Output = Result<awaken_session_contract::SessionRunActivation, HostError>,
        >,
    Read: FnMut() -> ReadFuture,
    ReadFuture: std::future::Future<Output = Result<Option<RunState>, HostError>>,
{
    // The guard spans activation and the complete committed-truth wait. Dropping
    // the caller at either phase removes only this observation; Dispatch/Thread
    // authorities continue independently.
    let (settled, _waiter_guard) = registry.register(run_id, stream_sink);
    let activation = activate().await?;
    if matches!(
        activation,
        awaken_session_contract::SessionRunActivation::Completed
    ) && let Some(state) = read_settled().await?
    {
        return Ok(state);
    }
    await_completion_state(settled, reconciliation_interval, read_settled).await
}

/// Wakes foreground durable submitters when the pool settles a Run and relays
/// that Run's best-effort live progress while each caller remains connected.
/// Exact replays share one run-id slot and retain independent drop guards;
/// neither is durable truth. Every exactly Thread-scoped observation is also
/// published through the existing Thread hub, so a background child needs no
/// completion registration.
pub(crate) struct CompletionRegistry {
    waiters: std::sync::Mutex<HashMap<String, Vec<ForegroundRegistration>>>,
    next_registration_id: std::sync::atomic::AtomicU64,
    hub: Arc<crate::ThreadEventHub>,
    committed_progress: std::sync::OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for CompletionRegistry {
    fn default() -> Self {
        Self::new(Arc::new(crate::ThreadEventHub::new()))
    }
}

struct ForegroundRegistration {
    id: u64,
    settled: tokio::sync::oneshot::Sender<RunState>,
    stream_sink: Option<Arc<dyn StreamSink>>,
}

impl CompletionRegistry {
    pub(crate) fn new(hub: Arc<crate::ThreadEventHub>) -> Self {
        Self {
            waiters: Default::default(),
            next_registration_id: std::sync::atomic::AtomicU64::new(1),
            hub,
            committed_progress: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn install_committed_progress_wakeup(
        &self,
        wakeup: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), &'static str> {
        self.committed_progress
            .set(wakeup)
            .map_err(|_| "committed Runtime progress wakeup is already installed")
    }

    /// Register interest in `run_id` BEFORE it is enqueued, so the pool cannot
    /// settle it before this caller is listening (no lost wakeup). Returns the
    /// receiver plus a [`WaiterGuard`] that removes the waiter if the caller's
    /// future is dropped before the run settles (e.g. a client disconnect), so an
    /// unwaited entry never lingers in the map.
    fn register(
        self: &Arc<Self>,
        run_id: &RunId,
        stream_sink: Option<Arc<dyn StreamSink>>,
    ) -> (tokio::sync::oneshot::Receiver<RunState>, WaiterGuard) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let has_stream_sink = stream_sink.is_some();
        let registration_id = self
            .next_registration_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.waiters
            .lock()
            .expect("completion registry poisoned")
            .entry(run_id.0.clone())
            .or_default()
            .push(ForegroundRegistration {
                id: registration_id,
                settled: tx,
                stream_sink,
            });
        tracing::trace!(
            run_id = %run_id.0,
            registration_id,
            has_stream_sink,
            "registered durable foreground observation"
        );
        let guard = WaiterGuard {
            registry: Arc::downgrade(self),
            run_id: run_id.0.clone(),
            registration_id,
        };
        (rx, guard)
    }

    async fn route_observation(
        &self,
        observation: awaken_agent_contract::stream::event::Observation,
    ) -> Result<(), awaken_agent_contract::stream::sink::Error> {
        if let Some(coordinate) = &observation.assistant_response {
            self.hub.publish(
                &coordinate.thread_id.0,
                crate::ThreadEvent::Live(observation.clone()),
            );
        }
        let sinks = self
            .waiters
            .lock()
            .expect("completion registry poisoned")
            .get(&observation.event.run_id.0)
            .into_iter()
            .flat_map(|registrations| registrations.iter())
            .filter_map(|registration| registration.stream_sink.clone())
            .collect::<Vec<_>>();
        tracing::trace!(
            run_id = %observation.event.run_id.0,
            matched = sinks.len(),
            "routed durable foreground stream event"
        );
        for sink in sinks {
            sink.send_observation(observation.clone()).await?;
        }
        Ok(())
    }
}

/// Removes a completion waiter on drop, so a foreground submit whose future is
/// dropped (client disconnect) or which timed out never leaves a stale sender in
/// the registry. On normal completion the sender is already gone (consumed by
/// [`CompletionSink::settled`]), so the removal is a harmless no-op.
struct WaiterGuard {
    registry: std::sync::Weak<CompletionRegistry>,
    run_id: String,
    registration_id: u64,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade()
            && let Ok(mut waiters) = registry.waiters.lock()
        {
            let mut removed = false;
            let mut remove_run = false;
            if let Some(registrations) = waiters.get_mut(&self.run_id) {
                let before = registrations.len();
                registrations.retain(|registration| registration.id != self.registration_id);
                removed = registrations.len() != before;
                remove_run = registrations.is_empty();
            }
            if remove_run {
                waiters.remove(&self.run_id);
            }
            tracing::trace!(
                run_id = %self.run_id,
                registration_id = self.registration_id,
                removed,
                "released durable foreground observation"
            );
        }
    }
}

impl CompletionSink for CompletionRegistry {
    fn settled(&self, run_id: &RunId, state: &RunState) {
        if let Some(registrations) = self
            .waiters
            .lock()
            .expect("completion registry poisoned")
            .remove(&run_id.0)
        {
            tracing::trace!(
                run_id = %run_id.0,
                observers = registrations.len(),
                ?state,
                "settled durable foreground observation"
            );
            for registration in registrations {
                // A receiver may have already gone — a dropped send is fine.
                let _ = registration.settled.send(state.clone());
            }
        }
        if let Some(wakeup) = self.committed_progress.get() {
            wakeup();
        }
    }
}

#[async_trait::async_trait]
impl StreamSink for CompletionRegistry {
    async fn send(
        &self,
        event: awaken_agent_contract::stream::event::Event,
    ) -> Result<(), awaken_agent_contract::stream::sink::Error> {
        self.route_observation(event.into()).await
    }

    async fn send_observation(
        &self,
        observation: awaken_agent_contract::stream::event::Observation,
    ) -> Result<(), awaken_agent_contract::stream::sink::Error> {
        self.route_observation(observation).await
    }
}

#[cfg(test)]
mod completion_tests {
    use super::{
        CompletionRegistry, HOST_EXECUTOR_CAPABILITY, PROVIDER_CREDENTIAL_SOURCE_CAPABILITY, RunId,
        activate_and_await_session_run, await_completion_state, awaiting_ticket_advanced,
        durable_resume_input, remote_worker_placement,
    };
    use awaken_agent_contract::agent::run::RunState;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::event::{AgentEvent, Delta};
    use awaken_agent_contract::stream::event::{
        Event as StreamEvent, Observation as StreamObservation,
    };
    use awaken_agent_contract::stream::sink::Sink as StreamSink;
    use awaken_run_ingress::CompletionSink;
    use awaken_runtime_contract::resolved::{
        CatalogFingerprint, ModelBinding, ResolvedModelCandidate,
    };
    use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshotId;
    use awaken_store_inmem::MemoryStreamSink;
    use std::sync::Arc;

    use crate::{NoModelConfiguredExecutor, SharedHost, UNCONFIGURED_MODEL_REF};

    fn host_models() -> awaken_runtime_contract::resolved::ResolvedSpec {
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("test")
            .model(ModelBinding::new("host", "primary", "native"))
            .build()
            .resolved_spec
    }

    fn environment() -> awaken_session_contract::EnvironmentSnapshot {
        awaken_session_contract::EnvironmentSnapshot {
            environment_id: "env-1".into(),
            revision: awaken_session_contract::EnvironmentRevision(3),
            self_hosted: false,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint("fp-env".into()),
            sandbox: serde_json::from_value(serde_json::json!({
                "isolation": "container",
                "requests": {"cpu_millis": 250, "memory_bytes": 33554432},
                "limits": {"memory_bytes": 67108864}
            }))
            .expect("valid SandboxOverride fixture"),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: awaken_session_contract::EnvironmentPackages {
                npm: vec!["tsx@4".into()],
                ..Default::default()
            },
            prepared_image: None,
            network: awaken_session_contract::SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.example.test".into()],
            },
            credential_realization:
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
        }
    }

    fn environment_with_isolation(
        isolation: awaken_provisioning_contract::IsolationClass,
    ) -> awaken_session_contract::EnvironmentSnapshot {
        let mut environment = environment();
        let isolation = match isolation {
            awaken_provisioning_contract::IsolationClass::Workdir => "workdir",
            awaken_provisioning_contract::IsolationClass::Namespace => "namespace",
            awaken_provisioning_contract::IsolationClass::Container => "container",
        };
        environment.sandbox = serde_json::from_value(serde_json::json!({
            "isolation": isolation,
        }))
        .expect("valid isolation-only SandboxOverride fixture");
        environment.packages = Default::default();
        environment.network = awaken_session_contract::SessionNetworkPolicy::Unrestricted;
        environment
    }

    fn projected_acp_models() -> awaken_runtime_contract::resolved::ResolvedSpec {
        let mut models = host_models();
        models.model_binding = ResolvedModelCandidate::try_provider_with_acp(
            ModelBinding::new("provider", "model", "acp:claude"),
            "provider@1",
            "route@1",
            "workspace",
            None,
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "anthropic".into(),
                api_dialect: "anthropic_messages".into(),
                base_url: "https://example.test".into(),
                upstream_model: "model".into(),
                processing_placement: None,
            },
            awaken_runtime_contract::resolved::AcpExecutionProfile {
                capability_fingerprint: "sha256:test-capability".into(),
                capability_adapter_version: "test".into(),
                session_configuration: Default::default(),
            },
        )
        .expect("coherent projected ACP candidate");
        models
    }

    fn repository_manifest() -> awaken_session_contract::SessionResourceManifest {
        use awaken_resource_contract::{
            BindingId, ClonePolicy, ConfigVersion, RepositoryConfigVersion, RepositoryId,
            ResourceAccess,
        };
        use awaken_session_contract::{
            ResolvedInput, ResolvedInputSource, ResolvedSessionResources, SessionResourceManifest,
        };

        SessionResourceManifest::new(
            "workspace-a",
            ResolvedSessionResources::try_new(
                vec![ResolvedInput {
                    binding_id: BindingId::new("repo-binding"),
                    source: ResolvedInputSource::Repository {
                        repository_id: RepositoryId::from("repo-a"),
                        config: RepositoryConfigVersion {
                            repository_id: "repo-a".into(),
                            version: ConfigVersion::INITIAL,
                            remote_url: "https://example.invalid/repo.git".into(),
                            credential_binding: Some("credential-a".into()),
                            initial_branch: None,
                            initial_commit: None,
                            clone_policy: ClonePolicy::default(),
                        },
                        credential: None,
                    },
                    mount_path: "/workspace/repo".into(),
                    access: ResourceAccess::ReadOnly,
                    instructions: None,
                }],
                Vec::new(),
            )
            .expect("valid frozen Repository manifest"),
        )
    }

    fn resume_command(run_id: &str, correlation_id: &str, answer: &str) -> ResumeCommand {
        ResumeCommand {
            operation_id: None,
            correlation_id: correlation_id.into(),
            run_id: RunId(run_id.into()),
            thread_id: ThreadId("thread-1".into()),
            snapshot_id: ExecutableAgentSnapshotId("snapshot-1".into()),
            catalog_fingerprint: CatalogFingerprint("catalog-1".into()),
            result: ResumeResult::Input(answer.into()),
            context_messages: Vec::new(),
            now_ms: 10,
        }
    }

    #[test]
    fn durable_resume_identity_is_exactly_the_answered_ticket() {
        // Cause/effect graph: C1 exact retry of one (Run, correlation, payload);
        // C2 same ticket with a conflicting payload; C3 delimiter-ambiguous raw
        // strings belonging to different tickets. Effects: E1 exact retry yields
        // one identical PendingInput; E2 conflict keeps the same message id but a
        // different payload so the canonical Inbox rejects it; E3 distinct ticket
        // pairs never alias. Constraint: caller time is not delivery identity.
        //
        // | Rule | ticket pair | payload | Effect |
        // | R1 | same | same | E1 identical input |
        // | R2 | same | different | E2 same id, conflicting input |
        // | R3 | different but delimiter-ambiguous | any | E3 different id |
        let exact = durable_resume_input(resume_command("run-a", "corr-a", "yes"));
        let retry = durable_resume_input(resume_command("run-a", "corr-a", "yes"));
        assert_eq!(exact, retry, "R1");

        let conflict = durable_resume_input(resume_command("run-a", "corr-a", "no"));
        assert_eq!(exact.message_id, conflict.message_id, "R2 identity");
        assert_ne!(exact, conflict, "R2 payload conflict");

        let left = durable_resume_input(resume_command("a", "bc", "yes"));
        let right = durable_resume_input(resume_command("ab", "c", "yes"));
        assert_ne!(left.message_id, right.message_id, "R3");
    }

    #[test]
    fn peer_completion_waits_for_exact_ticket_advancement() {
        // Cause/effect graph: C1 initial durable submit vs resumed wait; C2 the
        // committed Run is still Awaiting; C3 open ticket is the answered ticket,
        // a new ticket, absent, or belongs to another Run. Effects: E1 old truth
        // cannot prematurely release a resume waiter; E2 a new ticket releases it;
        // E3 missing/inconsistent truth remains fail-closed. Ended and initial
        // Awaiting are handled by the enclosing read-settled state match.
        //
        // | Rule | observed Run | current ticket | Effect |
        // | R1 | run-1 | same correlation | E1 false |
        // | R2 | run-1 | new correlation | E2 true |
        // | R3 | run-1 | absent | E3 false |
        // | R4 | run-1 | ticket for run-2 | E3 false |
        let run_1 = RunId("run-1".into());
        let run_2 = RunId("run-2".into());
        assert!(
            !awaiting_ticket_advanced(&run_1, "ticket-1", Some((&run_1, "ticket-1"))),
            "R1"
        );
        assert!(
            awaiting_ticket_advanced(&run_1, "ticket-1", Some((&run_1, "ticket-2"))),
            "R2"
        );
        assert!(!awaiting_ticket_advanced(&run_1, "ticket-1", None), "R3");
        assert!(
            !awaiting_ticket_advanced(&run_1, "ticket-1", Some((&run_2, "ticket-2"))),
            "R4"
        );
    }

    #[test]
    fn environment_and_backend_compile_one_worker_sandbox_requirement_vector() {
        use awaken_provisioning_contract::IsolationClass;
        use awaken_runtime_contract::resolved::{BackendModelSelection, ResolvedModelCandidate};

        // Cause/effect graph:
        // C1=Native; C2=projected ACP opaque process; C3=trusted BackendOwned ACP;
        // C4=A2A-only; C5=frozen Environment isolation/network/requests/limits/packages;
        // C6=prepared image. Effects: E1=one PlacementRequirements.sandbox vector;
        // E2=opaque ACP adds transparent Namespace semantics; E3=A2A adds no local
        // Sandbox demand; E4=image replaces package provisioning with rootfs demand.
        // Constraint: candidates are conjunctive for admission; any local candidate
        // keeps the local Environment requirement.
        //
        // Decision table:
        // R1 C1+C5 -> exact Environment enforcement, including the Environment's
        // Namespace-or-stronger path contract.
        // R2 C2+C5 -> R1 plus transparent/path-fidelity.
        // R3 C3+C5 -> trusted Workdir semantics; no artificial Namespace demand.
        // R4 C4+C5 -> default (no local Environment) vector.
        // R5 C1+C5+C6 -> custom-rootfs=true, package-provisioning=false.
        // R6 C4 plus any Native fallback -> R1 (the candidate set is not A2A-only).
        let frozen = environment();

        let native = remote_worker_placement(&host_models(), Some(&frozen), None, true);
        assert_eq!(native.sandbox.isolation, IsolationClass::Container, "R1");
        assert!(native.sandbox.network_isolation, "R1 network");
        assert!(native.sandbox.enforced_network_allowlist, "R1 allowlist");
        assert!(native.sandbox.resource_limits, "R1 limits");
        assert!(native.sandbox.package_provisioning, "R1 packages");
        assert_eq!(native.resources.cpu_millis, Some(250), "R1 cpu request");
        assert_eq!(
            native.resources.memory_bytes,
            Some(33_554_432),
            "R1 memory request"
        );
        assert!(
            native.sandbox.tool_transparent && native.sandbox.path_fidelity,
            "R1 Container paths"
        );

        let projected_acp = projected_acp_models();
        let acp = remote_worker_placement(&projected_acp, Some(&frozen), None, true);
        assert!(
            acp.sandbox.tool_transparent && acp.sandbox.path_fidelity,
            "R2"
        );

        let mut trusted = host_models();
        trusted.model_binding = ResolvedModelCandidate::try_backend_owned(
            ModelBinding::new("local", "", "acp:codex"),
            awaken_runtime_contract::CredentialRef {
                id: "local".into(),
                revision: 1,
            },
            BackendModelSelection::Default,
            "test",
            "sha256:test-capability",
            Default::default(),
        )
        .expect("coherent trusted ACP candidate");
        let mut workdir = frozen.clone();
        workdir.sandbox = Default::default();
        workdir.network = awaken_session_contract::SessionNetworkPolicy::Unrestricted;
        workdir.packages = Default::default();
        let trusted = remote_worker_placement(&trusted, Some(&workdir), None, true);
        assert_eq!(trusted.sandbox.isolation, IsolationClass::Workdir, "R3");
        assert!(!trusted.sandbox.tool_transparent, "R3");

        let mut remote = host_models();
        remote.model_binding = ResolvedModelCandidate::try_remote(
            ModelBinding::new("agent", "", "a2a:https://agent.test"),
            "workspace",
            None,
            "security-fp",
        )
        .expect("coherent remote candidate");
        let remote_placement = remote_worker_placement(&remote, Some(&frozen), None, true);
        assert_eq!(remote_placement.sandbox, Default::default(), "R4");
        assert_eq!(
            remote_placement.resources,
            Default::default(),
            "R4 resources"
        );

        remote.model_candidates.push(host_models().model_binding);
        let mixed = remote_worker_placement(&remote, Some(&frozen), None, true);
        assert_eq!(mixed.sandbox.isolation, IsolationClass::Container, "R6");

        let mut prepared = frozen;
        prepared.prepared_image = Some("image@sha256:abc".into());
        let image = remote_worker_placement(&host_models(), Some(&prepared), None, true);
        assert!(image.sandbox.custom_rootfs, "R5 rootfs");
        assert!(!image.sandbox.package_provisioning, "R5 packages");
    }

    #[test]
    fn session_path_causes_compile_into_worker_admission_once() {
        use awaken_provisioning_contract::{IsolationClass, SandboxCapabilities};

        // Cause/effect graph: C1 Environment asks for Workdir/Namespace; C2 the
        // frozen SessionResourceManifest has/has-not a typed Repository input;
        // C3 execution is cooperative Native/projected ACP; C4 the Worker
        // advertises path_fidelity=false/true while every unrelated capability
        // remains satisfied. E1 placement emits one SandboxRequirements vector;
        // E2 the exact Worker predicate accepts/rejects it. Repository presence
        // comes only from the frozen typed manifest, never from mount-path text.
        //
        // | Rule | Environment | Repository | Execution | Fidelity required |
        // |---|---|---|---|---|
        // | P1 | Workdir | no  | Native        | no  |
        // | P2 | Workdir | no  | projected ACP | yes |
        // | P3 | Workdir | yes | Native        | yes |
        // | P4 | Workdir | yes | projected ACP | yes |
        // | P5 | Namespace | no  | Native        | yes |
        // | P6 | Namespace | no  | projected ACP | yes |
        // | P7 | Namespace | yes | Native        | yes |
        // | P8 | Namespace | yes | projected ACP | yes |
        // For every P-row, C4=true accepts; C4=false accepts only P1 and rejects
        // P2-P8. This is the same `SandboxCapabilities::satisfies_requirements`
        // predicate consumed by Worker `can_claim`; no second admission table.
        for isolation in [IsolationClass::Workdir, IsolationClass::Namespace] {
            for has_repository in [false, true] {
                for projected_acp in [false, true] {
                    let environment = environment_with_isolation(isolation);
                    let models = if projected_acp {
                        projected_acp_models()
                    } else {
                        host_models()
                    };
                    let repository = has_repository.then(repository_manifest);
                    let placement = remote_worker_placement(
                        &models,
                        Some(&environment),
                        repository.as_ref(),
                        true,
                    );
                    let fidelity_required =
                        isolation >= IsolationClass::Namespace || has_repository || projected_acp;
                    let expected_isolation = if fidelity_required {
                        isolation.max(IsolationClass::Namespace)
                    } else {
                        isolation
                    };
                    assert_eq!(
                        placement.sandbox.isolation, expected_isolation,
                        "P isolation: {isolation:?}/{has_repository}/{projected_acp}"
                    );
                    assert_eq!(
                        placement.sandbox.tool_transparent, fidelity_required,
                        "P transparency: {isolation:?}/{has_repository}/{projected_acp}"
                    );
                    assert_eq!(
                        placement.sandbox.path_fidelity, fidelity_required,
                        "P fidelity: {isolation:?}/{has_repository}/{projected_acp}"
                    );

                    for path_fidelity in [false, true] {
                        let worker = SandboxCapabilities {
                            isolation: IsolationClass::Namespace,
                            tool_transparent: true,
                            path_fidelity,
                            enforced_readonly: true,
                            network_isolation: true,
                            enforced_network_allowlist: true,
                            secret_egress_substitution: true,
                            resource_limits: true,
                            custom_rootfs: true,
                            package_provisioning: true,
                            control_services: Default::default(),
                        };
                        assert_eq!(
                            worker.satisfies_requirements(&placement.sandbox),
                            path_fidelity || !fidelity_required,
                            "Worker claim: {isolation:?}/{has_repository}/{projected_acp}/{path_fidelity}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn hand_recovery_policy_compiles_into_the_existing_worker_requirement() {
        use awaken_runtime_contract::tool::{ToolRecoveryMode, ToolRecoveryPolicy};

        // Cause/effect decision table: C1=a selected canonical Hand descriptor
        // freezes DurableRequest; C2=a non-Hand coordination descriptor has the
        // same policy. R1 !C1+C2 => no Sandbox demand, because coordination
        // recovery is owned by its host executor; R2 C1+C2 => exactly one
        // DurableRequest demand.
        // This derives placement from the immutable snapshot without a second
        // Agent field or a concrete tool-id list in the placement owner.
        let mut models = host_models();
        let builtins = awaken_ext_builtin_tools::builtin_tools();
        let hand = builtins
            .iter()
            .find(|tool| tool.toolset() == awaken_ext_builtin_tools::Toolset::Hand)
            .expect("Hand catalog is non-empty")
            .descriptor()
            .clone();
        let coordination = builtins
            .iter()
            .find(|tool| tool.toolset() == awaken_ext_builtin_tools::Toolset::Coordination)
            .expect("Coordination catalog is non-empty")
            .descriptor()
            .clone()
            .with_recovery(ToolRecoveryPolicy::durable_request());
        models.tool_descriptors = vec![hand.clone(), coordination.clone()];
        let coordination_only = remote_worker_placement(&models, None, None, true);
        assert!(
            coordination_only.required_sandbox_tool_recovery.is_empty(),
            "R1"
        );

        models.tool_descriptors = vec![
            hand.with_recovery(ToolRecoveryPolicy::durable_request()),
            coordination,
        ];
        let resident_hand = remote_worker_placement(&models, None, None, true);
        assert_eq!(
            resident_hand.required_sandbox_tool_recovery,
            [ToolRecoveryMode::DurableRequest].into_iter().collect(),
            "R2"
        );
    }

    #[test]
    fn exported_dispatch_completion_sink_is_the_host_owned_projection() {
        // Cause graph: one SharedHost owns one CompletionRegistry; an embedding
        // requests the Worker-settle projection; cloning must preserve that exact
        // registry rather than constructing a peer notification path.
        // Decision table:
        // | host mode | exported sink | effect |
        // | local pool | exact host registry | local settle and export converge |
        // | coordinator-only | exact host registry | remote settle wakes Session |
        // Mode does not alter sink identity, so pointer identity covers both rules
        // without manufacturing two deployment configurations in this unit test.
        let host = SharedHost::new(Arc::new(NoModelConfiguredExecutor), UNCONFIGURED_MODEL_REF);
        let expected = host.completion.clone() as Arc<dyn CompletionSink>;
        let exported = host.dispatch_completion_sink();

        assert!(Arc::ptr_eq(&expected, &exported));
    }

    #[tokio::test]
    async fn unconfigured_model_is_rejected_before_durable_enqueue() {
        use awaken_agent_contract::agent::{run::Id as AgentRunId, thread::Id as ThreadId};
        use awaken_runtime_contract::RunActivation;

        // Cause/effect rule: C0 the ordinary Dispatch runtime composition is
        // present and C1 its frozen model is explicitly unconfigured. E1 context
        // creation succeeds through C0; E2 dispatch rejects C1 before enqueue.
        let host = Arc::new(SharedHost::new(
            Arc::new(NoModelConfiguredExecutor),
            UNCONFIGURED_MODEL_REF,
        ));
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let ctx = host
            .ctx_for("unconfigured-model", None)
            .await
            .expect("Session context");
        let activation = RunActivation::new(
            AgentRunId("run-unconfigured".into()),
            ThreadId("unconfigured-model".into()),
            ctx.config.clone(),
            Vec::new(),
        );

        let error = host
            .resolved_dispatch(activation)
            .expect_err("an unconfigured model must never enter durable dispatch");
        assert!(error.to_string().contains("No model is configured"));
        assert!(error.to_string().contains("Author > Quickstart"));
    }

    #[test]
    fn committed_completion_wakes_the_single_installed_convergence_owner() {
        // Cause/effect graph: C1 no foreground waiter exists; C2 an internal or
        // foreground Run settles; C3 one convergence owner is installed.
        // Effects: E1 C2 wakes C3 exactly once despite C1; E2 a parallel owner
        // cannot replace it. Both local and remote Workers publish through this
        // same CompletionSink, so deployment mode is not another cause.
        let registry = CompletionRegistry::default();
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = wakes.clone();
        registry
            .install_committed_progress_wakeup(std::sync::Arc::new(move || {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }))
            .expect("C3 installs the sole wakeup");

        awaken_run_ingress::CompletionSink::settled(
            &registry,
            &RunId("completion-wakeup".into()),
            &RunState::Ended(awaken_agent_contract::agent::run::EndCause::NaturalEnd),
        );
        assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 1, "E1");
        assert!(
            registry
                .install_committed_progress_wakeup(std::sync::Arc::new(|| {}))
                .is_err(),
            "E2"
        );
    }

    /// A3: dropping the guard (caller future dropped / timed out) removes the
    /// waiter, so a run that never settles does not leak an entry.
    #[tokio::test]
    async fn dropping_the_guard_removes_the_registration() {
        let registry = Arc::new(CompletionRegistry::default());
        let (rx, guard) = registry.register(&RunId("r".into()), None);
        assert_eq!(registry.waiters.lock().unwrap().len(), 1);
        drop(guard);
        drop(rx);
        assert!(
            registry.waiters.lock().unwrap().is_empty(),
            "the guard removed the leaked waiter"
        );
    }

    /// The happy path: `settled` delivers the state to the waiter and clears the
    /// slot, so the later guard drop is a no-op.
    #[tokio::test]
    async fn settled_delivers_the_state_and_clears_the_slot() {
        let registry = Arc::new(CompletionRegistry::default());
        let (rx, _guard) = registry.register(&RunId("r".into()), None);
        registry.settled(&RunId("r".into()), &RunState::Awaiting);
        assert!(matches!(rx.await, Ok(RunState::Awaiting)));
        assert!(registry.waiters.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn session_user_run_observation_covers_activation_and_peer_recovery() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        use awaken_agent_contract::agent::run::EndCause;
        use awaken_session_contract::SessionRunActivation;

        // Cause/effect graph: C1 activation returns Activated/AlreadyActivated/
        // RecoveryClaimed/Completed; C2 settlement is signalled locally during
        // activation or appears only in peer committed truth. Effects: E1 the
        // observer exists before activation; E2 a synchronous local settlement
        // is not lost; E3 peer truth releases the same wait; E4 only committed
        // Awaiting/Ended crosses the boundary.
        //
        // | Rule | Activation | Settlement source | Effect |
        // | O1 | Activated | local during activation | E1+E2+E4 |
        // | O2 | AlreadyActivated | peer committed fallback | E1+E3+E4 |
        // | O3 | RecoveryClaimed | peer committed fallback | E1+E3+E4 |
        // | O4 | Completed | immediate committed read | E1+E3+E4 |
        let rules = [
            (
                "O1",
                SessionRunActivation::Activated,
                Some(RunState::Awaiting),
                None,
            ),
            (
                "O2",
                SessionRunActivation::AlreadyActivated {
                    session_activity_epoch: 9,
                },
                None,
                Some(RunState::Awaiting),
            ),
            (
                "O3",
                SessionRunActivation::RecoveryClaimed,
                None,
                Some(RunState::Awaiting),
            ),
            (
                "O4",
                SessionRunActivation::Completed,
                None,
                Some(RunState::Ended(EndCause::NaturalEnd)),
            ),
        ];

        for (rule, activation, local_state, peer_state) in rules {
            let registry = Arc::new(CompletionRegistry::default());
            let run_id = RunId(format!("session-user-{rule}"));
            let activation_registry = registry.clone();
            let activation_run = run_id.clone();
            let expected_peer = peer_state.clone();
            let state = activate_and_await_session_run(
                &registry,
                &run_id,
                None,
                std::time::Duration::from_millis(2),
                move || async move {
                    assert_eq!(
                        activation_registry
                            .waiters
                            .lock()
                            .unwrap()
                            .get(&activation_run.0)
                            .map(Vec::len),
                        Some(1),
                        "{rule}/E1 register before activation"
                    );
                    if let Some(local_state) = local_state {
                        activation_registry.settled(&activation_run, &local_state);
                    }
                    Ok(activation)
                },
                move || {
                    let expected_peer = expected_peer.clone();
                    async move { Ok(expected_peer) }
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{rule}: {error}"));

            assert!(
                matches!(state, RunState::Awaiting | RunState::Ended(_)),
                "{rule}/E4"
            );
            assert!(registry.waiters.lock().unwrap().is_empty(), "{rule}");
        }
    }

    #[tokio::test]
    async fn dropping_session_user_run_observation_releases_only_its_waiter() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        use awaken_session_contract::SessionRunActivation;

        // Cause/effect graph: C1 an activated Run remains Running in committed
        // truth; C2 its foreground caller is dropped. Effects: E1 the observation
        // remains pending while C1 holds; E2 C2 removes its temporary registration;
        // E3 no Run/Dispatch state is changed. Decision rule D1=C1+C2=>E1+E2+E3.
        let registry = Arc::new(CompletionRegistry::default());
        let run_id = RunId("session-user-drop".into());
        let mut observation = Box::pin(activate_and_await_session_run(
            &registry,
            &run_id,
            None,
            std::time::Duration::from_millis(2),
            || std::future::ready(Ok(SessionRunActivation::Activated)),
            || std::future::ready(Ok(None)),
        ));

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(12), observation.as_mut())
                .await
                .is_err(),
            "D1/E1"
        );
        assert_eq!(
            registry
                .waiters
                .lock()
                .unwrap()
                .get(&run_id.0)
                .map(Vec::len),
            Some(1),
            "D1/E1"
        );
        drop(observation);
        assert!(registry.waiters.lock().unwrap().is_empty(), "D1/E2");
    }

    #[tokio::test]
    async fn exact_run_replay_observers_do_not_replace_each_other() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 two foreground callers observe the same stable
        // Run; C2 either caller drops or the Run settles. Effects: E1 both share
        // the one registry Run slot; E2 one drop preserves the peer; E3 settlement
        // wakes every remaining observer. Rules M1=C1=>E1, M2=C1+drop=>E2,
        // M3=C1+settle=>E3.
        let registry = Arc::new(CompletionRegistry::default());
        let run_id = RunId("session-user-replay".into());
        let (first, first_guard) = registry.register(&run_id, None);
        let (second, _second_guard) = registry.register(&run_id, None);
        assert_eq!(
            registry
                .waiters
                .lock()
                .unwrap()
                .get(&run_id.0)
                .map(Vec::len),
            Some(2),
            "M1/E1"
        );

        drop(first_guard);
        assert!(first.await.is_err(), "M2 dropped observer closes");
        assert_eq!(
            registry
                .waiters
                .lock()
                .unwrap()
                .get(&run_id.0)
                .map(Vec::len),
            Some(1),
            "M2/E2"
        );
        registry.settled(&run_id, &RunState::Awaiting);
        assert!(matches!(second.await, Ok(RunState::Awaiting)), "M3/E3");
        assert!(registry.waiters.lock().unwrap().is_empty(), "M3/E3");
    }

    #[tokio::test]
    async fn durable_foreground_registry_routes_only_the_current_runs_live_stream() {
        // Cause/effect graph: C1 a foreground durable Run registers a live sink;
        // C2 an emitted StreamEvent carries the exact vs another Run id; C3 the
        // registration remains active vs settles/drops. Effects: E1 exact active
        // progress reaches that connection; E2 foreign progress is ignored; E3
        // settlement delivers the authoritative state and removes both waiter and
        // live route; E4 later progress is ignored. Constraint: StreamEvent remains
        // best-effort observation and never changes committed RunState.
        //
        // | Rule | registered | event id | phase | Effects |
        // | R1 | yes | exact | active | E1 |
        // | R2 | yes | foreign | active | E2 |
        // | R3 | yes | exact | settled | E3+E4 |
        // | R4 | no/dropped | any | any | E2 |
        let registry = Arc::new(CompletionRegistry::default());
        let downstream = Arc::new(MemoryStreamSink::new());
        let run = RunId("run-live".into());
        let (settled, _guard) =
            registry.register(&run, Some(downstream.clone() as Arc<dyn StreamSink>));
        let event = |run_id: &str, delta: &str| StreamEvent {
            run_id: RunId(run_id.into()),
            kind: AgentEvent::Delta(Delta::TextDelta {
                delta: delta.into(),
            }),
        };

        registry
            .send(event("run-foreign", "ignored"))
            .await
            .unwrap();
        assert!(downstream.events().is_empty(), "R2");

        registry.send(event("run-live", "forwarded")).await.unwrap();
        assert_eq!(
            downstream.events(),
            vec![event("run-live", "forwarded")],
            "R1"
        );

        registry.settled(&run, &RunState::Awaiting);
        assert!(matches!(settled.await, Ok(RunState::Awaiting)), "R3 state");
        registry.send(event("run-live", "too-late")).await.unwrap();
        assert_eq!(downstream.events().len(), 1, "R3/R4 route removed");
    }

    /// Live observation cause/effect table: C1 a background Run has no
    /// foreground waiter; C2 its neutral event carries child Thread identity; C3
    /// an event omits Thread identity. E1 C1+C2 publishes once on the existing
    /// child Hub channel; E2 the primary/foreign channel sees nothing; E3 C3 is
    /// not guessed or published. Rules H1=C1+C2=>E1+E2, H2=C1+C3=>E3. This
    /// proves observation does not depend on a second background-run registry.
    /// Constraint/Invariant: exact Thread identity selects the existing Hub
    /// channel; absence never falls back to another Thread. Decision rule: H1 and
    /// H2 cover exact and absent Thread identity.
    #[tokio::test]
    async fn background_live_progress_uses_the_exact_thread_hub_channel() {
        let hub = Arc::new(crate::ThreadEventHub::new());
        let registry = CompletionRegistry::new(hub.clone());
        let mut child = hub.subscribe("child-thread");
        let mut primary = hub.subscribe("session-root");
        let event = StreamObservation::assistant_delta(
            RunId("background-run".into()),
            ThreadId("child-thread".into()),
            0,
            0,
            AgentEvent::Delta(Delta::TextDelta { delta: "x".into() }),
        );

        registry.send_observation(event.clone()).await.unwrap();
        assert!(
            matches!(
                child.try_recv(),
                Ok(crate::ThreadEvent::Live(observed)) if observed == event
            ),
            "H1/E1"
        );
        assert!(primary.try_recv().is_err(), "H1/E2");

        registry
            .send(StreamEvent {
                run_id: RunId("background-run".into()),
                kind: AgentEvent::Delta(Delta::TextDelta {
                    delta: "ambiguous".into(),
                }),
            })
            .await
            .unwrap();
        assert!(child.try_recv().is_err(), "H2/E3");
    }

    #[tokio::test]
    async fn a_live_run_outlives_foreground_reconciliation_without_a_transport_timeout() {
        // Durable foreground completion cause/effect table:
        // C1=the local completion sender remains open; C2=committed Run truth is
        // still Running (`read_settled -> None`); C3=multiple reconciliation
        // intervals pass; C4=the runtime later commits/sends Awaiting. Effects:
        // E1=the foreground waiter remains pending through C3; E2=no transport
        // timeout invents Error/Ended; E3=C4 alone releases the waiter with the
        // exact authoritative state. Rule F1=C1+C2+C3=>E1+E2;
        // F2=F1+C4=>E3. Caller cancellation is covered by the existing guard-drop
        // test and is intentionally independent of Run terminal authority.
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_reads = reads.clone();
        let future =
            await_completion_state(receiver, std::time::Duration::from_millis(2), move || {
                observed_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::ready(Ok(None))
            });
        tokio::pin!(future);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(15), &mut future)
                .await
                .is_err(),
            "F1/F2: live committed truth must keep the foreground wait open"
        );
        assert!(
            reads.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "F1: reconciliation observed live truth repeatedly"
        );

        sender
            .send(RunState::Awaiting)
            .expect("waiter remains live");
        let state = tokio::time::timeout(std::time::Duration::from_millis(100), &mut future)
            .await
            .expect("authoritative settlement wakes promptly")
            .expect("settlement succeeds");
        assert!(matches!(state, RunState::Awaiting), "F2");
    }

    #[test]
    fn coordinator_admission_pins_protocol_and_every_materialization_capability() {
        let mut models = host_models();
        models.model_candidates.push(
            ResolvedModelCandidate::try_provider(
                ModelBinding::new("provider", "fallback", "native"),
                "provider@1",
                "route@1",
                "workspace",
                None,
                awaken_runtime_contract::InferenceEndpoint {
                    adapter_kind: "openai".into(),
                    api_dialect: "open_ai_chat".into(),
                    base_url: "https://example.invalid".into(),
                    upstream_model: "fallback".into(),
                    processing_placement: None,
                },
            )
            .expect("coherent fallback provider candidate"),
        );
        let placement = remote_worker_placement(&models, None, None, true);
        assert_eq!(placement.contract_version, 1);
        assert_eq!(placement.dispatch_contract_version, 1);
        assert_eq!(placement.runtime_protocol_version, 1);
        assert!(placement.required_capabilities.contains("native-runtime"));
        assert!(
            placement
                .required_capabilities
                .contains(PROVIDER_CREDENTIAL_SOURCE_CAPABILITY)
        );
        assert!(
            placement
                .required_capabilities
                .contains(HOST_EXECUTOR_CAPABILITY)
        );
    }

    #[test]
    fn coordinator_admission_requires_exact_acp_and_generic_a2a_capabilities() {
        let mut models = host_models();
        let mut binding = models.model_binding.binding().clone();
        binding.backend_ref = "acp:claude".to_string();
        models.model_binding = ResolvedModelCandidate::host(binding);
        let remote = ResolvedModelCandidate::try_remote(
            ModelBinding::new("agent", "", "a2a:https://agent.example"),
            "workspace",
            None,
            "sha256:test-security",
        )
        .expect("coherent remote candidate");
        models.model_candidates.push(remote);

        let placement = remote_worker_placement(&models, None, None, true);
        assert!(
            placement.required_capabilities.contains("acp:claude"),
            "ACP CLI identity is part of claim compatibility"
        );
        assert!(
            placement
                .required_capabilities
                .contains(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY),
            "the generic A2A transport is part of claim compatibility"
        );
        assert!(
            !placement.required_capabilities.contains("native-runtime"),
            "native is not advertised as a substitute for exact external backends"
        );
    }

    #[test]
    fn worker_local_candidates_pin_every_exact_credential_for_claim_admission() {
        use awaken_runtime_contract::{
            CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
            CredentialUsage, InferenceEndpoint,
        };

        let worker_candidate =
            |model: &str, credential: &str, revision: u64| {
                ResolvedModelCandidate::try_provider(
                    ModelBinding::new("provider", model, "genai"),
                    "provider@1",
                    "route@1",
                    "workspace-a",
                    Some(
                        CredentialAccess::new(
                            CredentialRef {
                                id: credential.into(),
                                revision,
                            },
                            CredentialMaterialSource::WorkerReference,
                            CredentialUsage::ProviderAdapter,
                            CredentialExecutionPolicy::self_hosted_provider(),
                        )
                        .with_target(awaken_runtime_contract::CredentialTarget::new(
                            awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                            "provider",
                        )),
                    ),
                    InferenceEndpoint {
                        adapter_kind: "openai".into(),
                        api_dialect: "open_ai_chat".into(),
                        base_url: "https://example.invalid".into(),
                        upstream_model: model.into(),
                        processing_placement: None,
                    },
                )
                .expect("coherent Worker-local provider candidate")
            };
        let mut models = host_models();
        models.model_binding = worker_candidate("primary", "cred:primary", 2);
        models
            .model_candidates
            .push(worker_candidate("fallback", "cred:fallback", 5));

        let placement = remote_worker_placement(&models, None, None, false);
        assert_eq!(
            placement.location,
            awaken_run_ingress::ExecutionLocation::RemoteRequired
        );
        assert!(
            placement
                .required_capabilities
                .contains(awaken_run_ingress::WORKER_LOCAL_CREDENTIALS_CAPABILITY)
        );
        assert_eq!(
            placement.required_credentials,
            std::collections::BTreeSet::from([
                awaken_run_ingress::WorkerCredentialRevision {
                    id: "cred:fallback".into(),
                    revision: 5,
                },
                awaken_run_ingress::WorkerCredentialRevision {
                    id: "cred:primary".into(),
                    revision: 2,
                },
            ])
        );
    }

    #[test]
    fn backend_owned_candidate_uses_the_same_exact_worker_placement_fence() {
        // Cause graph: BackendOwned candidate -> WorkerLocal capability + exact
        // credential revision -> RemoteRequired placement -> existing heartbeat
        // selection and pre-launch revalidation. There is no materialization cause.
        //
        // Decision table: BackendOwned always requires its one exact observation;
        // HostExecutor requires neither this capability nor credential revision.
        let mut models = host_models();
        models.model_binding = ResolvedModelCandidate::try_backend_owned(
            ModelBinding::new("cred:local", "", "acp:codex"),
            awaken_runtime_contract::CredentialRef {
                id: "cred:local".into(),
                revision: 7,
            },
            awaken_runtime_contract::resolved::BackendModelSelection::Default,
            "test",
            "sha256:test-capability",
            Default::default(),
        )
        .expect("coherent backend-owned candidate");

        let placement = remote_worker_placement(&models, None, None, false);
        assert_eq!(
            placement.location,
            awaken_run_ingress::ExecutionLocation::RemoteRequired
        );
        assert!(
            placement
                .required_capabilities
                .contains(awaken_run_ingress::WORKER_LOCAL_CREDENTIALS_CAPABILITY)
        );
        assert_eq!(
            placement.required_credentials,
            std::collections::BTreeSet::from([awaken_run_ingress::WorkerCredentialRevision {
                id: "cred:local".into(),
                revision: 7,
            },])
        );
        assert_eq!(
            placement.required_acp_capabilities,
            std::collections::BTreeSet::from([
                awaken_run_ingress::WorkerAcpCapabilityRequirement {
                    backend_ref: "acp:codex".into(),
                    fingerprint: "sha256:test-capability".into(),
                },
            ])
        );
    }

    #[test]
    fn resource_placement_requires_resource_and_repository_credential_capabilities() {
        // Cause/effect graph: C1=resource manifest exists; C2=repository has a
        // credential; C3=input is read-only. Effects: E1=resource realization
        // capability; E2=repository credential capability; E3=enforced read-only
        // Sandbox capability. One row with C1+C2+C3 covers all conjunctive effects;
        // the following empty-manifest test owns the !C2/!C3 revocation row.
        let resources = repository_manifest();
        let placement = remote_worker_placement(&host_models(), None, Some(&resources), true);
        assert!(
            placement
                .required_capabilities
                .contains(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY)
        );
        assert!(
            placement
                .required_capabilities
                .contains(awaken_run_ingress::REPOSITORY_CREDENTIALS_CAPABILITY)
        );
        assert!(placement.sandbox.enforced_readonly);
    }

    #[test]
    fn explicit_empty_manifest_still_requires_the_revocation_capability() {
        let resources = awaken_session_contract::SessionResourceManifest::new(
            "workspace-a",
            awaken_session_contract::ResolvedSessionResources::default(),
        );
        let placement = remote_worker_placement(&host_models(), None, Some(&resources), true);
        assert!(
            placement
                .required_capabilities
                .contains(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY),
            "an empty successor manifest may need to remove prior sandbox material"
        );
        assert!(
            !placement
                .required_capabilities
                .contains(awaken_run_ingress::REPOSITORY_CREDENTIALS_CAPABILITY)
        );
    }
}
