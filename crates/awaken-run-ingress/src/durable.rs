//! `DurableRunIngress`: the durable half of the `RunIngress` port (G5).
//!
//! Direct ingress executes inline and fails its durable-only operation closed.
//! Durable ingress instead *persists* an accepted run, then drives it through the
//! dispatch worker, so the submission survives a crash and is recovered. It adds
//! durability over the same runtime control a direct ingress uses (G6); it does
//! not own the loop, agent truth, or a second commit mechanism.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::{Id as RunId, Phase};
use awaken_agent_contract::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime::{RunIngress, Runtime};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::execution::{Error as ExecError, Result as ExecResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use crate::Error;
use crate::capability::RunIngressCapabilities;
use crate::clock::Clock;
use crate::dispatch::{DispatchStore, PendingInput};
use crate::request::RunExecutionRequest;
use crate::service::{DispatchService, DispatchServiceConfig};
use crate::worker::DispatchWorker;

/// Durable run ingress over a dispatch store. Holds the worker that turns durable
/// dispatches into runtime attempts; the worker is shared (`Arc`) so an
/// autonomous [`DispatchService`] can drain the same queue.
pub struct DurableRunIngress<S> {
    worker: Arc<DispatchWorker<S>>,
}

impl<S: DispatchStore + 'static> DurableRunIngress<S> {
    /// Build durable ingress from a runtime, a dispatch store, and the durable
    /// commit boundary. The commit handle is the single source of truth shared by
    /// the runtime's writes and the worker's reads (G6 same-source wiring).
    pub fn new<C>(runtime: Arc<Runtime>, store: Arc<S>, commit: Arc<C>) -> Self
    where
        C: CommitCoordinator + ThreadReader + RunStore + Send + Sync + 'static,
    {
        Self {
            worker: Arc::new(DispatchWorker::new(
                runtime,
                store,
                commit,
                "durable-run-ingress",
            )),
        }
    }

    /// The durable guarantees this ingress reports (G5): durable, recoverable,
    /// replayable. A direct ingress would report [`RunIngressCapabilities::DIRECT`].
    pub fn capabilities(&self) -> RunIngressCapabilities {
        RunIngressCapabilities::DURABLE
    }

    /// The worker, for out-of-band driving (recovery sweeps, background loops).
    pub fn worker(&self) -> &DispatchWorker<S> {
        &self.worker
    }

    /// Start an autonomous daemon that drains this queue against its runtime,
    /// recovering crashed leases on its poll cadence (ADR-0011). The daemon shares
    /// the same worker and store, so runs submitted synchronously here and runs
    /// submitted to the daemon all flow through one queue.
    pub fn spawn_service(
        &self,
        clock: Arc<dyn Clock>,
        config: DispatchServiceConfig,
    ) -> DispatchService<S> {
        DispatchService::spawn(self.worker.clone(), clock, config)
    }

    /// Deliver durable input to a parked run and drive its resume. The input is
    /// appended idempotently (duplicate `message_id` is a no-op), then the worker
    /// resumes against the committed ticket — but only if `input.correlation_id`
    /// matches the run's current ticket; input for a superseded or already-resumed
    /// ticket is dropped, never re-applied (ADR-0010). Returns the run's resulting
    /// phase. Only runs submitted durably (with a dispatch row) can be woken this
    /// way.
    pub async fn deliver_resume(&self, input: PendingInput, now_ms: u64) -> Result<Phase, Error> {
        let run_id = input.run_id.clone();
        self.worker.store().append(input).await?;
        let processed = self.worker.run_until_idle(now_ms).await?;
        phase_of(&processed, &run_id).ok_or_else(|| {
            Error::from(ExecError::Execution("resumed run was not processed".into()))
        })
    }

    /// Reclaim and re-run any dispatch whose lease expired (crash recovery), plus
    /// any work that became runnable. Returns each processed run and its phase.
    pub async fn recover(&self, now_ms: u64) -> Result<Vec<(RunId, Phase)>, Error> {
        self.worker.run_until_idle(now_ms).await
    }

    /// Stage a cross-thread delivery to another thread's parked run (M3b). It is
    /// relayed by [`relay_outbox`](Self::relay_outbox) or the daemon. Returns
    /// whether it was newly staged (idempotent by `message_id`).
    pub async fn stage_cross_thread(&self, input: PendingInput) -> Result<bool, Error> {
        Ok(self.worker.store().stage(input).await?)
    }

    /// Relay staged cross-thread deliveries to their target pending input, then
    /// drive any run that became wakeable. Returns each processed run and phase.
    pub async fn relay_outbox(&self, now_ms: u64) -> Result<Vec<(RunId, Phase)>, Error> {
        self.worker.store().relay().await?;
        self.worker.run_until_idle(now_ms).await
    }

    /// Dead-letter crashed runs that have exhausted their crash-retry budget, so
    /// a poison run is not reclaimed forever (ADR-0015). Returns how many were
    /// dead-lettered.
    pub async fn reap(&self, max_attempts: u64, now_ms: u64) -> Result<usize, Error> {
        Ok(self.worker.store().reap(max_attempts, now_ms).await?)
    }

    /// The run ids currently dead-lettered, for operations.
    pub async fn dead_letters(&self) -> Result<Vec<RunId>, Error> {
        Ok(self.worker.store().dead_letters().await?)
    }

    /// Return a dead-lettered run to the queue at a fresh budget.
    pub async fn requeue(&self, run_id: &RunId) -> Result<bool, Error> {
        Ok(self.worker.store().requeue(run_id).await?)
    }

    /// Durably cancel a not-running run: remove its dispatch and pending input,
    /// then commit a terminal `Cancelled` fact so its committed phase reflects the
    /// cancellation (clearing any waiting ticket). Returns `true` if cancelled; a
    /// currently-running run is not cancelled here — use `cancel` (live control).
    pub async fn cancel_durable(&self, run_id: &RunId) -> Result<bool, Error> {
        let Some(thread_id) = self.worker.store().cancel(run_id).await? else {
            return Ok(false);
        };
        self.worker
            .runtime()
            .cancel_run(run_id.clone(), thread_id, self.worker.execution_context())
            .await?;
        Ok(true)
    }
}

#[async_trait]
impl<S: DispatchStore + 'static> RunIngress for DurableRunIngress<S> {
    /// Foreground submit is additive over runtime control: it executes inline
    /// through the same `RunExecutor` a direct ingress uses (G6).
    async fn submit(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecResult<Phase> {
        use awaken_runtime_contract::execution::RunExecutor;
        self.worker.runtime().execute(activation, context).await
    }

    /// Durable submit: persist the accepted run first (so it survives a crash),
    /// then drive it. A direct ingress fails this closed; durable ingress does
    /// not (G5).
    async fn submit_background(&self, activation: RunActivation) -> ExecResult<Phase> {
        let run_id = activation.run_id.clone();
        self.worker
            .store()
            .enqueue(RunExecutionRequest::new(activation))
            .await
            .map_err(|err| ExecError::Execution(err.to_string()))?;
        let processed = self.worker.run_until_idle(0).await.map_err(exec_error)?;
        phase_of(&processed, &run_id)
            .ok_or_else(|| ExecError::Execution("submitted run was not processed".into()))
    }

    fn cancel(&self, run_id: &RunId) -> Result<(), ControlError> {
        self.worker.runtime().deliver(LiveCommand::Cancel {
            run_id: run_id.clone(),
        })
    }
}

fn phase_of(processed: &[(RunId, Phase)], run_id: &RunId) -> Option<Phase> {
    processed
        .iter()
        .rev()
        .find(|(id, _)| id == run_id)
        .map(|(_, phase)| phase.clone())
}

fn exec_error(err: Error) -> ExecError {
    match err {
        Error::Execution(err) => err,
        Error::Dispatch(err) => ExecError::Execution(err.to_string()),
    }
}
