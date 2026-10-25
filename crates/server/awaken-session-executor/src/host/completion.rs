// @formal surface obligation=session_run.execution_requires_dispatch_work_and_realization_owner
// @formal reason: Foreground submit and completion observe the one dispatch owner and exact committed Run result.
// @formal reason: This classifies the Host observation edge, not a proof claim.
//! Durable-foreground observation: the [`ExecutorComponents`] pool-submit/await
//! methods over the completion relay (`awaken-session-run-delivery`), and the
//! process's settlement sink that wakes the Session application.

use super::*;
#[cfg(test)]
use awaken_dispatch_contract::{HOST_EXECUTOR_CAPABILITY, PROVIDER_CREDENTIAL_SOURCE_CAPABILITY};
#[cfg(test)]
use awaken_dispatch_execution::placement::remote_worker_placement;
use awaken_session_execution_contract::{
    AdmittedSessionRun, SessionRunActivation, SessionRunDelivery,
};
use awaken_session_run_delivery::{activate_and_await_session_run, await_completion_state};

impl crate::SessionRuns {
    /// Register before `publish` delivers one Session-owned resume, then
    /// project only the next committed Step of `run_id`. Registering first
    /// means a local pool cannot settle the resumed ticket unobserved.
    pub(crate) async fn observe_published_resume(
        &self,
        session_id: &str,
        run_id: RunId,
        correlation_id: String,
        stream_sink: Option<Arc<dyn StreamSink>>,
        publish: impl std::future::Future<Output = Result<(), SessionExecutionError>>,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        let ctx = self
            .run_assembly
            .ctx_for_session_coordination(session_id, None)
            .await?;
        let messages_before = self
            .authoritative_step_snapshot(&ctx, &run_id)
            .await?
            .messages
            .len();
        let (settled, _waiter_guard) = self.completion.register(&run_id, stream_sink)?;
        publish.await?;
        let state = self
            .await_settled_event(&ctx, &run_id, Some(&correlation_id), settled)
            .await?;
        self.project_settled_session_resume(&ctx, run_id, state, messages_before)
            .await
    }

    /// Publish one Session-approved reservation through the existing dispatch
    /// row. This is the sole Host activation implementation used by both the
    /// non-blocking reconciler port and foreground observation.
    pub(crate) async fn activate_session_run_reservation(
        &self,
        delivery: SessionRunDelivery,
    ) -> Result<SessionRunActivation, SessionExecutionError> {
        let store = self.coordinator_dispatch()?;
        Ok(
            awaken_session_run_delivery::activate_session_run_reservation(
                store.as_ref(),
                &self.dispatch,
                delivery,
            )
            .await?,
        )
    }

    /// Register before activation and observe only the committed Run lifecycle.
    /// The registry is a wakeup/preview relay; peer settlement is recovered by
    /// the same authoritative Thread read used by other foreground operations.
    pub(crate) async fn activate_and_observe_session_run(
        &self,
        admission: AdmittedSessionRun,
        input_message_ids: Vec<String>,
        stream_sink: Option<Arc<dyn StreamSink>>,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
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
                    AdmittedSessionRun::Reserved(delivery)
                    | AdmittedSessionRun::AlreadyReserved(delivery)
                    | AdmittedSessionRun::AlreadyActivated(delivery) => {
                        self.activate_session_run_reservation(delivery).await
                    }
                    AdmittedSessionRun::RecoveryClaimed { .. } => {
                        Ok(SessionRunActivation::RecoveryClaimed)
                    }
                    AdmittedSessionRun::Completed { .. } => Ok(SessionRunActivation::Completed),
                }
            },
            || self.read_settled_phase(&ctx, &run_id, None),
        )
        .await?;
        self.project_settled_session_run(&ctx, run_id, state, &input_message_ids)
            .await
    }

    /// Submit a durable run and wait for the pool to drive it to a settled state.
    /// Under the shared queue a session's own worker must not claim (it would grab
    /// foreign threads' runs), so the foreground durable path enqueues, nudges the
    /// pool, and waits for the pool to signal completion — **by event**, not by
    /// polling committed truth, so it pays no poll-interval latency.
    pub(crate) async fn submit_durable_foreground(
        &self,
        ctx: &Arc<SessionCtx>,
        activation: RunActivation,
        stream_sink: Option<Arc<dyn StreamSink>>,
        associate_prepared_session: bool,
    ) -> Result<awaken_agent_contract::agent::run::RunState, SessionExecutionError> {
        let run_id = activation.run_id.clone();
        // Register for the settle event BEFORE enqueue, so the pool cannot drive and
        // settle the run before this caller is listening (no lost wakeup). The guard
        // removes the waiter if this future is dropped (client disconnect) before it
        // settles — held to the end of this method.
        let (settled, _waiter_guard) = self.completion.register(&run_id, stream_sink)?;
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
            return Err(SessionExecutionError::internal(
                "durable submit requires durable delivery",
            ));
        }
        let store = self.coordinator_dispatch()?;
        store
            .enqueue(request)
            .await
            .map_err(|e| SessionExecutionError::internal(e.to_string()))?;

        self.dispatch.hint_local_pool().await;
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
    ) -> Result<RunState, SessionExecutionError> {
        let run_id = command.run_id.clone();
        let correlation_id = command.correlation_id.clone();
        // Register before append so a local pool cannot settle between input
        // publication and waiter installation.
        let (settled, _waiter_guard) = self.completion.register(&run_id, None)?;
        let input = durable_resume_input(command);
        if !ctx.delivery.is_durable() {
            return Err(SessionExecutionError::internal(
                "durable resume requires durable delivery",
            ));
        }
        let store = self.coordinator_dispatch()?;
        store
            .append(input)
            .await
            .map_err(|error| SessionExecutionError::internal(error.to_string()))?;
        self.dispatch.hint_local_pool().await;
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
    ) -> Result<RunState, SessionExecutionError> {
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
    ) -> Result<Option<RunState>, SessionExecutionError> {
        Ok(awaken_session_run_delivery::read_settled_phase(
            ctx.commit.as_ref(),
            &ctx.thread_id,
            run_id,
            answered_correlation,
        )
        .await?)
    }
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

/// The process's one [`CompletionSink`]: it settles the foreground observers
/// of the completion relay, then wakes the Session application's lifecycle
/// reconciliation the composition bound. Both a local pool and the remote
/// Worker settle boundary settle through it.
pub(crate) struct SettlementWakeup {
    completion: Arc<CompletionRegistry>,
    committed_progress: crate::ApplicationPortBinding<Arc<dyn Fn() + Send + Sync>>,
}

impl SettlementWakeup {
    /// A fresh completion relay over `hub` that wakes `committed_progress`.
    pub(crate) fn over(
        hub: &Arc<crate::ThreadEventHub>,
        committed_progress: crate::ApplicationPortBinding<Arc<dyn Fn() + Send + Sync>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            completion: Arc::new(CompletionRegistry::new(hub.clone())),
            committed_progress,
        })
    }

    /// The completion relay foreground callers register with.
    pub(crate) fn completion(&self) -> Arc<CompletionRegistry> {
        self.completion.clone()
    }
}

impl CompletionSink for SettlementWakeup {
    fn settled(&self, run_id: &RunId, state: &RunState) {
        self.completion.settled(run_id, state);
        if let Some(wakeup) = self.committed_progress.get() {
            wakeup();
        }
    }
}

#[cfg(test)]
mod completion_tests;

use awaken_dispatch_contract::DispatchEnqueue as _;
