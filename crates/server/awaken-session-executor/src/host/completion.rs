// @formal surface obligation=session_run.execution_requires_dispatch_work_and_realization_owner
// @formal reason: Foreground submit and completion observe the one dispatch owner and exact committed Run result.
// @formal reason: This classifies the Host observation edge, not a proof claim.
//! Durable-foreground observation: the [`SharedHost`] pool-submit/await methods
//! and the [`CompletionRegistry`] that owns temporary completion observers plus
//! best-effort live routes for each foreground Run.

use super::*;
#[cfg(test)]
use awaken_dispatch_execution::placement::remote_worker_placement;
#[cfg(test)]
use awaken_dispatch_contract::{
    HOST_EXECUTOR_CAPABILITY, PROVIDER_CREDENTIAL_SOURCE_CAPABILITY,
};
use std::collections::HashMap;

impl SharedHost {
    /// The process dispatch pool, or a fail-closed error when durable ingress (and
    /// thus the pool) is not enabled.
    pub(crate) fn dispatch_pool_or_err(
        &self,
    ) -> Result<&Arc<DispatchPool<AnyDispatchStore>>, HostError> {
        self.dispatch.dispatch_pool.get().ok_or_else(|| {
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
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, HostError> {
        use awaken_dispatch_contract::Outbox as _;

        if delivery.session_activity_epoch == 0 {
            return Err(HostError::bad_request(
                "Session User Run activity epoch must be nonzero",
            ));
        }
        let store = self.coordinator_dispatch()?;
        let outcome = store
            .activate_session_run_reservation(
                &delivery.run_id,
                &ThreadId(delivery.session_id.into_string()),
                delivery.session_activity_epoch,
            )
            .await
            .map_err(|error| HostError::unavailable(error.to_string()))?;
        let projected = match outcome {
            awaken_dispatch_contract::SessionRunReservationActivation::Activated => {
                awaken_session_execution_contract::SessionRunActivation::Activated
            }
            awaken_dispatch_contract::SessionRunReservationActivation::AlreadyActivated {
                session_activity_epoch,
            } => awaken_session_execution_contract::SessionRunActivation::AlreadyActivated {
                session_activity_epoch,
            },
            awaken_dispatch_contract::SessionRunReservationActivation::RecoveryClaimed => {
                awaken_session_execution_contract::SessionRunActivation::RecoveryClaimed
            }
            awaken_dispatch_contract::SessionRunReservationActivation::Completed => {
                awaken_session_execution_contract::SessionRunActivation::Completed
            }
            awaken_dispatch_contract::SessionRunReservationActivation::MissingOrRejected => {
                return Err(HostError::bad_request(
                    "Session User Run reservation is missing or rejected",
                ));
            }
            awaken_dispatch_contract::SessionRunReservationActivation::Conflict => {
                return Err(HostError::bad_request(
                    "Session User Run reservation activation conflicts with durable truth",
                ));
            }
        };
        if matches!(
            projected,
            awaken_session_execution_contract::SessionRunActivation::Activated
                | awaken_session_execution_contract::SessionRunActivation::AlreadyActivated { .. }
        ) {
            if let Some(pool) = self.dispatch.dispatch_pool.get() {
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
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        stream_sink: Option<Arc<dyn StreamSink>>,
    ) -> Result<CommittedStepReceipt, HostError> {
        let session_id = admission.session_id().to_string();
        let run_id = admission.run_id().clone();
        let ctx = self
            .run_assembly
            .ctx_for_session_coordination(&session_id, None)
            .await?;
        let state = activate_and_await_session_run(
            &self.completion,
            &run_id,
            stream_sink,
            std::time::Duration::from_millis(250),
            || async move {
                match admission {
                    awaken_session_execution_contract::AdmittedSessionRun::Reserved(delivery)
                    | awaken_session_execution_contract::AdmittedSessionRun::AlreadyReserved(
                        delivery,
                    )
                    | awaken_session_execution_contract::AdmittedSessionRun::AlreadyActivated(
                        delivery,
                    ) => self.activate_session_run_reservation(delivery).await,
                    awaken_session_execution_contract::AdmittedSessionRun::RecoveryClaimed {
                        ..
                    } => Ok(awaken_session_execution_contract::SessionRunActivation::RecoveryClaimed),
                    awaken_session_execution_contract::AdmittedSessionRun::Completed { .. } => {
                        Ok(awaken_session_execution_contract::SessionRunActivation::Completed)
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
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<CommittedStepReceipt, HostError> {
        let session_id = delivery.command.session_id.clone();
        let run_id = delivery.command.expected_run_id.clone();
        let correlation_id = delivery.command.expected_correlation_id.clone();
        let ctx = self
            .run_assembly
            .ctx_for_session_coordination(session_id.as_str(), None)
            .await?;
        let messages_before = self
            .authoritative_step_snapshot(&ctx, &run_id)
            .await?
            .messages
            .len();
        let (settled, _waiter_guard) = self.completion.register(&run_id, stream_sink);
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
            self.run_assembly.prepare_dispatch(activation).fresh()?
        } else {
            self.run_assembly.prepare_dispatch(activation).extension()?
        };
        tracing::debug!(run_id = %run_id.0, scope = ?request.execution_scope,
            required_capabilities = ?request.placement.required_capabilities,
            placement = ?request.placement, holder = ?request.inference_plaintext_holder,
            "Submitting durable foreground dispatch");
        // Enqueue only — never drive here. Both a local pool and a remote Worker
        // claim from this same dispatch store; a local pool receives only a
        // best-effort wake after the durable write. `resolved_dispatch` already
        // captured the admitting trace before either topology reaches this point.
        if !ctx.delivery.is_durable() {
            return Err(HostError::internal(
                "durable submit requires durable delivery",
            ));
        }
        let store = self.coordinator_dispatch()?;
        if supersede {
            store
                .enqueue_with(
                    request,
                    SubmitOptions {
                        supersede: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
        } else {
            store
                .enqueue(request)
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
        }
        if let Some(pool) = self.dispatch.dispatch_pool.get() {
            pool.notify().await;
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
        let store = self.coordinator_dispatch()?;
        store
            .append(input)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        if let Some(pool) = self.dispatch.dispatch_pool.get() {
            pool.notify().await;
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
            Output = Result<awaken_session_execution_contract::SessionRunActivation, HostError>,
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
        awaken_session_execution_contract::SessionRunActivation::Completed
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
    /// The process composition's sole wakeup for committed Run progress.
    committed_progress: crate::ApplicationPortBinding<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for CompletionRegistry {
    fn default() -> Self {
        Self::new(Arc::new(crate::ThreadEventHub::new()), Default::default())
    }
}

struct ForegroundRegistration {
    id: u64,
    settled: tokio::sync::oneshot::Sender<RunState>,
    stream_sink: Option<Arc<dyn StreamSink>>,
}

impl CompletionRegistry {
    pub(crate) fn new(
        hub: Arc<crate::ThreadEventHub>,
        committed_progress: crate::ApplicationPortBinding<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self {
            waiters: Default::default(),
            next_registration_id: std::sync::atomic::AtomicU64::new(1),
            hub,
            committed_progress,
        }
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
mod completion_tests;

use awaken_dispatch_contract::{DispatchEnqueue as _, SessionRunReservations as _};
