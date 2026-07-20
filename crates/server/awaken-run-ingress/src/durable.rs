//! `DurableRunIngress`: the durable half of the `RunIngress` port (G5).
//!
//! Direct ingress executes inline and fails its durable-only operation closed.
//! Durable ingress instead *persists* an accepted run, then drives it through the
//! dispatch worker, so the submission survives a crash and is recovered. It adds
//! durability over the same runtime control a direct ingress uses (G6); it does
//! not own the loop, agent truth, or a second commit mechanism.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::thread::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_runtime::{RunIngress, RunService, Runtime};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::execution::{Error as ExecError, Result as ExecResult};
use awaken_runtime_contract::resume::ResumeCommand;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use crate::Error;
use crate::capability::RunIngressCapabilities;
use crate::clock::Clock;
use crate::dispatch::{Dispatch, DispatchSummary, PendingInput, SubmitOptions};
use crate::live_control::LiveRunControlService;
use crate::service::{DispatchService, DispatchServiceConfig};
use crate::worker::DispatchWorker;
use awaken_run_ingress_contract::RunDispatch;

/// Durable run ingress over a dispatch store. Holds the worker that turns durable
/// dispatches into runtime attempts; the worker is shared (`Arc`) so an
/// autonomous [`DispatchService`] can drain the same queue.
pub struct DurableRunIngress<S> {
    worker: Arc<DispatchWorker<S>>,
    /// Same committed-history source the worker consults. A replayed completed
    /// run is blocked by the dispatch tombstone, so submit returns this existing
    /// state rather than creating a second execution (ADR-0060).
    reader: Arc<dyn ThreadReader>,
    /// The per-session live inbox shared by the worker's drive (drained at safe
    /// loop boundaries) and the offer side (ADR-0054 P2). Neutral `Message`s only;
    /// this is why durable steer needs no protocol type in the worker.
    live_inbox: awaken_runtime_contract::live_inbox::LiveInbox,
}

impl<S: Dispatch + 'static> DurableRunIngress<S> {
    /// Build durable ingress from a runtime, a dispatch store, and the durable
    /// commit boundary. The commit handle is the single source of truth shared by
    /// the runtime's writes and the worker's reads (G6 same-source wiring).
    pub fn new<C>(runtime: Arc<Runtime>, store: Arc<S>, commit: Arc<C>) -> Self
    where
        C: CommitCoordinator + ThreadReader + RunStore + Send + Sync + 'static,
    {
        Self::with_owner(runtime, store, commit, "durable-run-ingress", None)
    }

    /// Like [`new`](Self::new) but with an explicit claim `owner`. Each process in
    /// a multi-node fleet MUST pass a unique owner: the lease is owner-scoped
    /// (`renew_lease`/`renew_owned_leases`), so a shared owner would let peers
    /// renew each other's leases and break the single-owner-per-run guarantee
    /// (ADR-0019/0024). Single-process deployments can keep the default owner.
    pub fn with_owner<C>(
        runtime: Arc<Runtime>,
        store: Arc<S>,
        commit: Arc<C>,
        owner: impl Into<String>,
        stream_checkpoint: Option<
            Arc<dyn awaken_agent_contract::stream::checkpoint::StreamCheckpointStore>,
        >,
    ) -> Self
    where
        C: CommitCoordinator + ThreadReader + RunStore + Send + Sync + 'static,
    {
        Self::with_owner_and_resolver(runtime, store, commit, owner, stream_checkpoint, None)
    }

    /// Like [`with_owner`](Self::with_owner) but also installs the model→executor
    /// resolver (R1), so this ingress's worker runs the run's configured model without
    /// a config service — the provider owns how the model is reached (local
    /// credentials or a gateway offering). A `None` resolver leaves the worker on the
    /// runtime's bound (host default) executor.
    pub fn with_owner_and_resolver<C>(
        runtime: Arc<Runtime>,
        store: Arc<S>,
        commit: Arc<C>,
        owner: impl Into<String>,
        stream_checkpoint: Option<
            Arc<dyn awaken_agent_contract::stream::checkpoint::StreamCheckpointStore>,
        >,
        inference_materializer: Option<crate::worker_context::InferenceMaterializerFn>,
    ) -> Self
    where
        C: CommitCoordinator + ThreadReader + RunStore + Send + Sync + 'static,
    {
        let reader: Arc<dyn ThreadReader> = commit.clone();
        let live_inbox = awaken_runtime_contract::live_inbox::LiveInbox::new();
        let mut worker =
            DispatchWorker::new(runtime, store, commit, owner).with_live_inbox(live_inbox.clone());
        if let Some(store) = stream_checkpoint {
            worker = worker.with_stream_checkpoint(store);
        }
        if let Some(inference_materializer) = inference_materializer {
            worker = worker.with_inference_materializer(inference_materializer);
        }
        Self {
            worker: Arc::new(worker),
            reader,
            live_inbox,
        }
    }

    /// The per-session live inbox the worker drains at boundaries — the offer side
    /// queues External steer here so it reaches a worker-driven run (ADR-0054 P2).
    pub fn live_inbox(&self) -> &awaken_runtime_contract::live_inbox::LiveInbox {
        &self.live_inbox
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

    /// A shared handle to this session's worker, for a process-level
    /// [`DispatchPool`](crate::DispatchPool) to drive claimed runs on the runtime
    /// that carries this thread's model/tools/config.
    pub fn worker_handle(&self) -> Arc<DispatchWorker<S>> {
        self.worker.clone()
    }

    /// Replace the local guarded commit service with another atomic claimed-run
    /// implementation. Database-less workers use this to send one combined
    /// claim-and-commit request to the store-owning server.
    #[must_use]
    pub fn with_claimed_commit(mut self, commit: Arc<dyn crate::ClaimedRunCommit>) -> Self {
        let worker = Arc::into_inner(self.worker)
            .expect("claimed commit must be configured before sharing the worker")
            .with_claimed_commit(commit);
        self.worker = Arc::new(worker);
        self
    }

    /// A fail-closed live-control service over this ingress's worker (G18): cancel
    /// a live/queued/awaiting run, or wake a live one, by correlation id (ADR-0018).
    /// Shares the same worker/store/runtime, so it owns no second commit boundary.
    pub fn live_control(&self) -> LiveRunControlService<S> {
        LiveRunControlService::new(self.worker.clone())
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

    /// Submit a run that supersedes the thread's prior pending/awaiting work
    /// (ADR-0022): the newest submission wins, the stale dispatches are marked
    /// superseded and never claimed again, then drive the new run. Superseded
    /// runs are observable via [`superseded`](Self::superseded).
    pub async fn submit_superseding(&self, activation: RunActivation) -> Result<RunState, Error> {
        let run_id = activation.run_id.clone();
        self.worker
            .store()
            .enqueue_with(
                RunDispatch::new(activation)
                    .with_traceparent(awaken_observability::current_traceparent()),
                SubmitOptions {
                    supersede: true,
                    ..Default::default()
                },
            )
            .await?;
        let processed = self.worker.run_until_idle(0).await?;
        state_of(&processed, &run_id).ok_or_else(|| {
            Error::from(ExecError::Execution(
                "superseding run was not processed".into(),
            ))
        })
    }

    /// The run ids superseded by a newer submission on their thread (ADR-0022).
    pub async fn superseded(&self) -> Result<Vec<RunId>, Error> {
        Ok(self.worker.store().superseded().await?)
    }

    /// An operational summary of every dispatch row, in enqueue order — the query
    /// surface for monitoring and maintenance (ADR-0025).
    pub async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, Error> {
        Ok(self.worker.store().list_dispatches().await?)
    }

    /// Deliver durable input to an awaiting run and drive its resume. The input is
    /// appended idempotently (duplicate `message_id` is a no-op), then the worker
    /// resumes against the committed ticket — but only if `input.correlation_id`
    /// matches the run's current ticket; input for a superseded or already-resumed
    /// ticket is dropped, never re-applied (ADR-0010). Returns the run's resulting
    /// state. Only runs submitted durably (with a dispatch row) can be woken this
    /// way.
    pub async fn deliver_resume(
        &self,
        input: PendingInput,
        now_ms: u64,
    ) -> Result<RunState, Error> {
        let run_id = input.run_id.clone();
        self.worker.store().append(input).await?;
        let processed = self.worker.run_until_idle(now_ms).await?;
        state_of(&processed, &run_id).ok_or_else(|| {
            Error::from(ExecError::Execution("resumed run was not processed".into()))
        })
    }

    /// Reclaim and re-run any dispatch whose lease expired (crash recovery), plus
    /// any work that became runnable. Returns each processed run and its state.
    pub async fn recover(&self, now_ms: u64) -> Result<Vec<(RunId, RunState)>, Error> {
        self.worker.run_until_idle(now_ms).await
    }

    /// Stage a cross-thread delivery to another thread's awaiting run (M3b). It is
    /// relayed by [`relay_outbox`](Self::relay_outbox) or the daemon. Returns
    /// whether it was newly staged (idempotent by `message_id`).
    pub async fn stage_cross_thread(&self, input: PendingInput) -> Result<bool, Error> {
        Ok(self.worker.store().stage(input).await?)
    }

    /// Relay staged cross-thread deliveries to their target pending input, then
    /// drive any run that became wakeable. Returns each processed run and state.
    pub async fn relay_outbox(&self, now_ms: u64) -> Result<Vec<(RunId, RunState)>, Error> {
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

    /// Operator GC: remove every dead-lettered dispatch and its pending input.
    /// Returns how many were purged.
    pub async fn purge_dead_letters(&self) -> Result<usize, Error> {
        Ok(self.worker.store().purge_dead_letters().await?)
    }

    /// Time-windowed GC: remove dead-letters dead-lettered at or before `cutoff_ms`
    /// (ADR-0023). The daemon runs this on its cadence when a ttl is configured.
    pub async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, Error> {
        Ok(self
            .worker
            .store()
            .purge_dead_letters_before(cutoff_ms)
            .await?)
    }

    /// Return a dead-lettered run to the queue at a fresh budget.
    pub async fn requeue(&self, run_id: &RunId) -> Result<bool, Error> {
        Ok(self.worker.store().requeue(run_id).await?)
    }

    /// Durably cancel a not-running run: remove its dispatch and pending input,
    /// then commit a terminal `Cancelled` fact so its committed state reflects the
    /// cancellation (clearing any awaiting ticket). Returns `true` if cancelled; a
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
impl<S: Dispatch + 'static> RunService for DurableRunIngress<S> {
    /// Foreground submit is additive over runtime control: it executes inline
    /// through the same `RunExecutor` a direct ingress uses (G6).
    async fn start(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecResult<RunState> {
        use awaken_runtime_contract::execution::RunExecutor;
        self.worker.runtime().execute(activation, context).await
    }

    async fn resume(
        &self,
        command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> ExecResult<RunState> {
        let reader = context.reader.clone().ok_or_else(|| {
            ExecError::Execution("RunService::resume requires committed-history wiring".to_string())
        })?;
        self.worker
            .runtime()
            .resume(command, reader.as_ref(), context)
            .await
    }

    fn cancel(&self, run_id: &RunId) -> Result<(), ControlError> {
        self.worker.runtime().deliver(LiveCommand::Cancel {
            run_id: run_id.clone(),
        })
    }
}

#[async_trait]
impl<S: Dispatch + 'static> RunIngress for DurableRunIngress<S> {
    /// Durable submit: persist the accepted run first (so it survives a crash),
    /// then drive it. A direct ingress fails this closed; durable ingress does
    /// not (G5).
    async fn submit_background(&self, activation: RunActivation) -> ExecResult<RunState> {
        let run_id = activation.run_id.clone();
        self.worker
            .store()
            .enqueue(
                RunDispatch::new(activation)
                    .with_traceparent(awaken_observability::current_traceparent()),
            )
            .await
            .map_err(|err| ExecError::Execution(err.to_string()))?;
        let processed = self.worker.run_until_idle(0).await.map_err(exec_error)?;
        state_of(&processed, &run_id)
            .or_else(|| self.reader.run_state(&run_id))
            .ok_or_else(|| ExecError::Execution("submitted run was not processed".into()))
    }
}

fn state_of(processed: &[(RunId, RunState)], run_id: &RunId) -> Option<RunState> {
    processed
        .iter()
        .rev()
        .find(|(id, _)| id == run_id)
        .map(|(_, state)| state.clone())
}

fn exec_error(err: Error) -> ExecError {
    match err {
        Error::Execution(err) => err,
        Error::Dispatch(err) => ExecError::Execution(err.to_string()),
    }
}
