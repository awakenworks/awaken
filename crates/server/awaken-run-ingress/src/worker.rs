//! The dispatch worker: claim one runnable dispatch, run it, settle it.
//!
//! The worker is the only place that turns a durable dispatch into a runtime
//! attempt. It is additive over runtime control (G6): it never reaches into the
//! loop, it calls the same `RunExecutor`/`Runtime::resume` a direct caller would,
//! and it decides execute-vs-resume from *committed truth* — the awaiting ticket
//! and the run record — not from a duplicated status in the queue. That keeps the
//! DispatchQueue aggregate free of run-outcome truth.

use std::sync::Arc;
use std::time::Instant;

use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::thread::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_runtime::Runtime;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::Error;
use crate::commit_fence::{ClaimedCommitCoordinator, ClaimedRunCommit, GuardedRunCommit};
use crate::dispatch::{Claimed, Dispatch, DispatchOutcome, PendingInput, RunClaim, SettleOutcome};
use crate::worker_context::WorkerContext;

/// Default lease: how long a claimed dispatch is owned before it is reclaimable.
pub const DEFAULT_LEASE_MS: u64 = 30_000;

/// Runs durable dispatches against a runtime. Generic over the store so the same
/// engine drives the in-memory reference and the Postgres backend unchanged.
pub struct DispatchWorker<S> {
    runtime: Arc<Runtime>,
    store: Arc<S>,
    exec: WorkerContext,
    reader: Arc<dyn ThreadReader>,
    claimed_commit: Arc<dyn ClaimedRunCommit>,
    owner: String,
    lease_ms: u64,
    cancellation: Option<CancellationToken>,
}

impl<S: Dispatch + 'static> DispatchWorker<S> {
    /// Wire a worker to its runtime, dispatch store, and durable commit boundary.
    /// The `commit` handle is the single source of durable truth: it is the
    /// commit coordinator the runtime writes through *and* the read port the
    /// worker consults for the ticket and run record (G6 same-source wiring).
    pub fn new<C>(
        runtime: Arc<Runtime>,
        store: Arc<S>,
        commit: Arc<C>,
        owner: impl Into<String>,
    ) -> Self
    where
        C: CommitCoordinator + ThreadReader + Send + Sync + 'static,
    {
        let base_commit: Arc<dyn CommitCoordinator> = commit.clone();
        let reader: Arc<dyn ThreadReader> = commit;
        Self::from_parts(runtime, store, base_commit, reader, owner)
    }

    /// Build a worker when the write and read sides are already erased behind
    /// their domain interfaces. This is the normal composition for a child Run:
    /// it inherits the parent's durable commit/history authority but owns a
    /// distinct dispatch row, claim, lease, and recovery lifecycle.
    pub fn from_parts(
        runtime: Arc<Runtime>,
        store: Arc<S>,
        commit: Arc<dyn CommitCoordinator>,
        reader: Arc<dyn ThreadReader>,
        owner: impl Into<String>,
    ) -> Self {
        let dispatch: Arc<dyn crate::dispatch::DispatchQueue> = store.clone();
        Self {
            runtime,
            store,
            exec: WorkerContext::new(commit.clone()).with_reader(reader.clone()),
            reader,
            claimed_commit: Arc::new(GuardedRunCommit::new(commit, dispatch)),
            owner: owner.into(),
            lease_ms: DEFAULT_LEASE_MS,
            cancellation: None,
        }
    }

    /// Inherit the same attempt capabilities as direct execution. The worker
    /// still replaces commit authority with the exact claim fence; a supplied
    /// cancellation token is preserved so parent cancellation reaches a live
    /// child Run.
    #[must_use]
    pub fn with_context(mut self, context: RuntimeRunContext) -> Self {
        self.cancellation = context.cancellation.clone();
        self.exec = self.exec.with_context(context);
        self
    }

    /// Override how a claimed worker commit is applied. Database-less workers
    /// inject the server-side atomic implementation; local workers keep the
    /// guarded store implementation installed by [`new`](Self::new).
    #[must_use]
    pub fn with_claimed_commit(mut self, commit: Arc<dyn ClaimedRunCommit>) -> Self {
        self.claimed_commit = commit;
        self
    }

    /// Attach a best-effort live stream sink to every attempt this worker runs.
    #[must_use]
    pub fn with_stream_sink(
        mut self,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Self {
        self.exec = self.exec.with_stream_sink(sink);
        self
    }

    /// Attach the durable interrupted-stream checkpoint store to every attempt, so
    /// a dispatch re-executed after a crash resumes its in-flight step (Phase 3).
    #[must_use]
    pub fn with_stream_checkpoint(
        mut self,
        store: Arc<dyn awaken_agent_contract::stream::checkpoint::StreamCheckpointStore>,
    ) -> Self {
        self.exec = self.exec.with_stream_checkpoint(store);
        self
    }

    /// Provide the per-session live inbox so this worker's runs drain mid-run
    /// steer at their safe loop boundaries (ADR-0054 P2).
    #[must_use]
    pub fn with_live_inbox(
        mut self,
        inbox: awaken_runtime_contract::live_inbox::LiveInbox,
    ) -> Self {
        self.exec = self.exec.with_live_inbox(inbox);
        self
    }

    /// Install the model→executor resolver (R1), so a worker-driven run resolves its
    /// effective model ref to the configured executor without a config service.
    #[must_use]
    pub fn with_model_resolver(mut self, resolve: crate::worker_context::ModelResolverFn) -> Self {
        self.exec = self.exec.with_model_resolver(resolve);
        self
    }

    /// Override the lease duration.
    #[must_use]
    pub fn with_lease_ms(mut self, lease_ms: u64) -> Self {
        self.lease_ms = lease_ms;
        self
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    /// This worker's lease owner id — the daemon renews this owner's leases.
    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }

    /// This worker's lease duration, for the daemon's renewal heartbeat.
    pub(crate) fn lease_ms(&self) -> u64 {
        self.lease_ms
    }

    /// A runtime context bound to this worker's commit boundary, for an
    /// out-of-band commit such as a durable cancel.
    pub(crate) fn execution_context(&self) -> RuntimeRunContext {
        self.exec
            .runtime_context(self.cancellation.clone().unwrap_or_default())
    }

    /// A run context for the attempt on `run_id` claimed under lease `epoch`, whose
    /// commit boundary is FENCED by that epoch (a stale owner cannot double-apply side
    /// effects — the commit twin of the settle fence) and whose inference routes
    /// through `model_executor` when the run resolved one (its provider-resolved
    /// model), else the runtime's bound (host default) executor.
    fn execution_context_with(
        &self,
        claim: &RunClaim,
        model_executor: &Option<Arc<dyn awaken_runtime_contract::llm::LlmExecutor>>,
    ) -> RuntimeRunContext {
        // The base commit boundary, wrapped per drive because the fence epoch is per
        // claim. `self.store` (the dispatch queue) reports the run's current epoch, so
        // a superseded owner's per-step commits are rejected.
        let fenced: Arc<dyn CommitCoordinator> = Arc::new(ClaimedCommitCoordinator::new(
            self.claimed_commit.clone(),
            claim.clone(),
        ));
        let ctx = self.execution_context().with_commit(fenced);
        match model_executor {
            Some(exec) => ctx.with_model_executor(exec.clone()),
            None => ctx,
        }
    }

    /// Perform a committed ScheduledAction in-process (ADR-0020), routing any
    /// inference it triggers through the run's resolved model executor when one applies
    /// and fencing its commits by the claim's lease `epoch`.
    async fn perform_scheduled(
        &self,
        claim: &RunClaim,
        now_ms: u64,
        model_executor: &Option<Arc<dyn awaken_runtime_contract::llm::LlmExecutor>>,
    ) -> Result<RunState, Error> {
        Ok(self
            .runtime
            .perform_scheduled_action(
                &claim.run_id,
                self.reader.as_ref(),
                self.execution_context_with(claim, model_executor),
                now_ms,
            )
            .await?)
    }

    /// Claim at most one runnable dispatch for this worker's owner, without
    /// driving it. Returns the claimed work (request, lease, pending input) or
    /// `None` when the queue is idle. A process-level pool claims here and then
    /// drives the result on the *owning session's* worker via
    /// [`drive_claimed`](Self::drive_claimed), so a run always executes on the
    /// runtime carrying its thread's model/tools/config (not whichever worker
    /// happened to claim it).
    pub async fn claim_one(&self, now_ms: u64) -> Result<Option<Claimed>, Error> {
        Ok(self.store.claim(&self.owner, self.lease_ms, now_ms).await?)
    }

    /// Claim one known Run under the same lease/fence policy without taking work
    /// belonging to another session. Parent-mediated child execution uses this
    /// after scheduling the child's stable identity.
    pub async fn claim_run(&self, run_id: &RunId, now_ms: u64) -> Result<Option<Claimed>, Error> {
        Ok(self
            .store
            .claim_run(run_id, &self.owner, self.lease_ms, now_ms)
            .await?)
    }

    /// Atomically admit, claim, and drive a newly created Run. This closes the
    /// enqueue/claim race with the process pool while preserving the same lease,
    /// fencing, recovery, and settlement path as every other dispatch.
    pub async fn start_run(
        &self,
        request: awaken_run_ingress_contract::RunDispatch,
        now_ms: u64,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let Some(claimed) = self
            .store
            .claim_new_run(request, &self.owner, self.lease_ms, now_ms)
            .await?
        else {
            return Ok(None);
        };
        self.drive_claimed(claimed, now_ms).await
    }

    /// Atomically deliver one durable input, claim its exact awaiting Run, and
    /// drive the resume boundary. This is the resume-side counterpart of
    /// [`start_run`](Self::start_run).
    pub async fn resume_run(
        &self,
        input: PendingInput,
        now_ms: u64,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let Some(claimed) = self
            .store
            .deliver_and_claim(input, &self.owner, self.lease_ms, now_ms)
            .await?
        else {
            return Ok(None);
        };
        self.drive_claimed(claimed, now_ms).await
    }

    /// Claim and process at most one runnable dispatch. Returns the processed
    /// run's id and resulting state, or `None` when the queue is idle.
    pub async fn tick(&self, now_ms: u64) -> Result<Option<(RunId, RunState)>, Error> {
        let Some(claimed) = self.claim_one(now_ms).await? else {
            return Ok(None);
        };
        self.drive_claimed(claimed, now_ms).await
    }

    /// Claim and drive one exact Run. This preserves queue isolation for callers
    /// that synchronously await a child boundary while the process pool remains
    /// free to drain every other Run.
    pub async fn tick_run(
        &self,
        run_id: &RunId,
        now_ms: u64,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let Some(claimed) = self.claim_run(run_id, now_ms).await? else {
            return Ok(None);
        };
        self.drive_claimed(claimed, now_ms).await
    }

    /// Drive an already-[`claim_one`](Self::claim_one)ed dispatch to a settled
    /// outcome on *this* worker's runtime and commit boundary, then settle it.
    /// Splitting claim from drive lets a process-level pool claim centrally and
    /// route each run to its owning session's worker, so the run executes with
    /// its thread's model/tools/config. Returns the run's id and resulting state.
    pub async fn drive_claimed(
        &self,
        claimed: Claimed,
        now_ms: u64,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        // Operational metrics (off the critical path): count this claim and time the
        // whole drive on the SAME recorder the runtime meters model/tool calls with,
        // so `awaken.dispatch.*` exports on the one OTLP pipeline. The timer records
        // `drive.duration` on every exit path (including the early `?`/return arms).
        self.runtime.metrics().record_dispatch_claimed();
        let _drive_timer = DriveTimer::new(self.runtime.metrics());

        let run_id = claimed.request.run_id().clone();
        // The fence token this drive holds. Every settle below carries it so a stale
        // owner (whose lease lapsed and was re-claimed under a higher epoch) is
        // rejected and abandons instead of clobbering the reclaimer's dispatch.
        let lease_epoch = claimed.lease.epoch;
        let claim = RunClaim::from(&claimed.lease);
        // Resolve this run's model to an executor once, before the activation is
        // consumed, and route every inference in this drive through it: the run's
        // effective model (its per-run override, else its snapshot binding) resolved
        // through the injected provider — which owns how the model is reached (local
        // credentials or a gateway offering). `None` leaves the runtime's bound (host
        // default) executor, so a single-model deployment is unaffected.
        let effective_model = claimed.request.activation.effective_model_ref().to_string();
        let model_executor = self
            .exec
            .resolve_model(&effective_model, claimed.request.model_access.as_ref());
        // Continue the admitting request's trace across the durable queue boundary:
        // this `wake.dispatch` span's remote parent is the persisted traceparent, so
        // the run driven below (`runtime.run` → …) nests under the trace that
        // submitted it — even when a daemon in another task/process drains it.
        let dispatch = awaken_observability::dispatch_span(claimed.request.traceparent.as_deref());
        let mut all_pending: Vec<String> = claimed
            .pending
            .iter()
            .map(|p| p.message_id.clone())
            .collect();

        let mut state = match self.reader.resume_ticket(&run_id) {
            // A committed ScheduledAction (ADR-0020): the system performs the
            // deferred action, not waits for external input. This also covers a
            // crash recovery of a scheduled await (no pending input is expected).
            Some(ticket) if ticket.reason == AwaitReason::ScheduledAction => {
                match self
                    .perform_scheduled(&claim, now_ms, &model_executor)
                    .instrument(dispatch.clone())
                    .await
                {
                    Ok(state) => state,
                    Err(err) => {
                        return self
                            .settle_if_terminal_or_raise(&run_id, lease_epoch, &all_pending, err)
                            .await;
                    }
                }
            }
            // The run is awaiting. Deliver only input whose correlation matches the
            // committed ticket; input for a superseded ticket (stale) is dropped
            // without delivery. Input that already drove a committed resume left a
            // ticket with a different correlation (or none), so it is never
            // re-applied (ADR-0010).
            Some(ticket) => {
                let matched = claimed
                    .pending
                    .iter()
                    .find(|p| p.correlation_id == ticket.correlation_id)
                    .cloned();
                match matched {
                    Some(input) => {
                        let command = ResumeCommand::from_ticket(&ticket, input.result, now_ms);
                        match self
                            .runtime
                            .resume(
                                command,
                                self.reader.as_ref(),
                                self.execution_context_with(&claim, &model_executor),
                            )
                            .instrument(dispatch.clone())
                            .await
                        {
                            Ok(state) => state,
                            Err(err) => {
                                return self
                                    .settle_if_terminal_or_raise(
                                        &run_id,
                                        lease_epoch,
                                        &all_pending,
                                        err,
                                    )
                                    .await;
                            }
                        }
                    }
                    None => {
                        // No input answers the current ticket; drop stale input
                        // and leave the run awaiting for a later wake.
                        return Ok(self
                            .settle(
                                &run_id,
                                lease_epoch,
                                DispatchOutcome::Awaiting,
                                &all_pending,
                            )
                            .await?
                            .applied()
                            .then_some((run_id, RunState::Awaiting)));
                    }
                }
            }
            // No ticket: a fresh run, or a recovered run that already finished.
            // The committed run record disambiguates so recovery never re-runs a
            // terminal run, and any orphan pending is dropped on settle.
            None => match self.reader.run_state(&run_id) {
                Some(state @ RunState::Ended(_)) => {
                    // A recovered fresh run that already committed a terminal record:
                    // its crashed prior attempt may have drained unbound idle-thread
                    // input (ADR-0021) into this run's committed transcript but died
                    // before recording that consumption in the settle. Consume exactly
                    // those unbound inbox rows whose message id IS in the committed
                    // transcript — committed truth is authority: their presence proves
                    // the crashed attempt delivered them, so they must not be
                    // re-delivered to a later run. An unbound row NOT in the transcript
                    // arrived after the terminal commit and was never delivered, so it
                    // is left for a future run (no loss). Without this, the run's own
                    // bound pending is dropped but a delivered unbound row lingers and
                    // is drained a SECOND time by the next fresh run (a duplicate).
                    let thread = claimed.request.thread_id().clone();
                    let delivered: std::collections::HashSet<String> = self
                        .reader
                        .committed_messages(&thread)
                        .into_iter()
                        .map(|message| message.id.0)
                        .collect();
                    let consumed_unbound = self
                        .store
                        .list(&thread)
                        .await?
                        .into_iter()
                        .map(|record| record.input)
                        .filter(|input| {
                            input.run_id.0.is_empty() && delivered.contains(&input.message_id)
                        })
                        .map(|input| input.message_id);
                    all_pending.extend(consumed_unbound);
                    return Ok(self
                        .settle(&run_id, lease_epoch, DispatchOutcome::Done, &all_pending)
                        .await?
                        .applied()
                        .then_some((run_id, state)));
                }
                _ => {
                    // Drain the thread inbox: input addressed to this thread with
                    // no run yet (ADR-0021) becomes new input to this fresh run.
                    let thread = claimed.request.thread_id().clone();
                    let unbound: Vec<PendingInput> = self
                        .store
                        .list(&thread)
                        .await?
                        .into_iter()
                        .map(|r| r.input)
                        .filter(|input| input.run_id.0.is_empty())
                        .collect();
                    let mut activation = claimed.request.activation;
                    // Prepend each delivered unbound *input* in arrival order. The
                    // insert position tracks how many were actually inserted, not the
                    // raw scan index — a non-`Input` unbound row (e.g. a stray
                    // decision) is skipped without shifting the target, so a skipped
                    // entry never desyncs the index and pushes a later insert past the
                    // vector's end (which would panic).
                    let mut at = 0;
                    for input in &unbound {
                        if let ResumeResult::Input(text) = &input.result {
                            activation.input.insert(
                                at,
                                Message {
                                    id: MessageId(input.message_id.clone()),
                                    role: Role::User,
                                    content: vec![ContentBlock::text(text)],
                                },
                            );
                            at += 1;
                        }
                    }
                    // Consumed on settle, not on read, so a crash re-delivers them.
                    all_pending.extend(unbound.into_iter().map(|input| input.message_id));
                    match self
                        .runtime
                        .execute(
                            activation,
                            self.execution_context_with(&claim, &model_executor),
                        )
                        .instrument(dispatch.clone())
                        .await
                    {
                        Ok(state) => state,
                        Err(err) => {
                            return self
                                .settle_if_terminal_or_raise(
                                    &run_id,
                                    lease_epoch,
                                    &all_pending,
                                    err,
                                )
                                .await;
                        }
                    }
                }
            },
        };

        // Drive any further scheduled actions to completion in-process: a run that
        // ends a step by committing a ScheduledAction is performed immediately,
        // until it ends or awaits on a wait that needs external input.
        while state == RunState::Awaiting {
            match self.reader.resume_ticket(&run_id) {
                Some(ticket) if ticket.reason == AwaitReason::ScheduledAction => {
                    state = match self
                        .perform_scheduled(&claim, now_ms, &model_executor)
                        .await
                    {
                        Ok(state) => state,
                        Err(err) => {
                            return self
                                .settle_if_terminal_or_raise(
                                    &run_id,
                                    lease_epoch,
                                    &all_pending,
                                    err,
                                )
                                .await;
                        }
                    };
                }
                _ => break,
            }
        }

        let outcome = settle_outcome(&state)?;
        Ok(self
            .settle(&run_id, lease_epoch, outcome, &all_pending)
            .await?
            .applied()
            .then_some((run_id, state)))
    }

    /// Settle a claimed dispatch and record the operational `runs.settled` counter
    /// (labelled by outcome) on the runtime's metrics recorder. The single choke all
    /// of `drive_claimed`'s settle arms route through, so every settle is metered
    /// exactly once regardless of which branch reached it.
    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, Error> {
        let label = match outcome {
            DispatchOutcome::Done => "done",
            DispatchOutcome::Awaiting => "awaiting",
        };
        self.runtime.metrics().record_dispatch_settled(label);
        Ok(self.store.settle(run_id, epoch, outcome, consumed).await?)
    }

    /// Convert a runtime-drive failure into a benign already-done when committed
    /// truth shows the run has reached a terminal state.
    ///
    /// The commit coordinators enforce terminal-is-final: once a run's committed
    /// state is `Ended`, a later commit for that run is rejected. That fence keeps
    /// the committed LOG exactly-once when a *stale* owner (slow-but-alive, its
    /// lease lapsed and superseded by a reclaimer that already drove the run to
    /// `Ended`) re-executes and tries to append a duplicate transcript — its commit
    /// fails. Surfacing that as a fatal error would strand the dispatch: it would
    /// sit un-settled until its lease lapsed, then be re-driven into the same
    /// rejected commit, burning crash-retries until it dead-lettered — even though
    /// the run is already complete. Instead the worker treats the lost race as
    /// already-done and settles the dispatch `Done` from committed truth, exactly
    /// as the "terminal committed record" recovery branch does. Any other error is
    /// a genuine fault and is re-raised unchanged.
    async fn settle_if_terminal_or_raise(
        &self,
        run_id: &RunId,
        epoch: u64,
        consumed: &[String],
        err: impl Into<Error>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        match self.reader.run_state(run_id) {
            Some(state @ RunState::Ended(_)) => Ok(self
                .settle(run_id, epoch, DispatchOutcome::Done, consumed)
                .await?
                .applied()
                .then_some((run_id.clone(), state))),
            _ => Err(err.into()),
        }
    }

    /// Drain the queue until no dispatch is runnable, returning every processed
    /// run and its state. A settled run becomes non-runnable, so this terminates.
    pub async fn run_until_idle(&self, now_ms: u64) -> Result<Vec<(RunId, RunState)>, Error> {
        let mut processed = Vec::new();
        while let Some(result) = self.tick(now_ms).await? {
            processed.push(result);
        }
        Ok(processed)
    }
}

/// Records the `awaken.dispatch.drive.duration` histogram on drop, so the wall
/// time of `drive_claimed` is metered on every exit path — the happy return, an
/// early `?`, and each terminal-recovery arm — without threading a timer through
/// them. Borrows the runtime's metrics recorder for the lifetime of one drive.
struct DriveTimer<'a> {
    start: Instant,
    metrics: &'a dyn awaken_runtime_contract::metrics::MetricsRecorder,
}

impl<'a> DriveTimer<'a> {
    fn new(metrics: &'a dyn awaken_runtime_contract::metrics::MetricsRecorder) -> Self {
        Self {
            start: Instant::now(),
            metrics,
        }
    }
}

impl Drop for DriveTimer<'_> {
    fn drop(&mut self) {
        self.metrics.record_dispatch_drive(self.start.elapsed());
    }
}

/// Map a settled executor state to the dispatch outcome the worker commits.
///
/// `execute`/`resume` only ever return an awaiting or ended state; `Running` exists
/// as durable mid-flight truth, never as an executor result. A `Running` result
/// therefore signals a broken executor, and the worker fails loudly rather than
/// settle a live run to a terminal (`Done`) or awaiting outcome — committed truth,
/// not a bogus return, decides a run's fate (G1/G13).
fn settle_outcome(state: &RunState) -> Result<DispatchOutcome, Error> {
    match state {
        RunState::Awaiting => Ok(DispatchOutcome::Awaiting),
        RunState::Ended(_) => Ok(DispatchOutcome::Done),
        RunState::Running => Err(Error::Execution(
            awaken_runtime_contract::execution::Error::Execution(
                "executor returned a non-settled Running state".to_string(),
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::settle_outcome;
    use awaken_agent_contract::agent::run::{EndCause, RunState};

    use crate::Error;
    use crate::dispatch::DispatchOutcome;

    // Behavior 1: an illegal `Running` executor result fails loudly — the worker
    // must NOT settle a live run to Done/Awaiting. `settle_outcome` is the decision
    // point `drive_claimed` consults before it calls `store.settle`, so proving it
    // errors on `Running` proves the worker never settles a mid-flight run.
    #[test]
    fn running_state_result_is_rejected_not_settled() {
        let err = settle_outcome(&RunState::Running).expect_err("Running must fail loudly");
        // It is an execution error, not a dispatch/storage error — a broken executor
        // is not a queue fault.
        assert!(
            matches!(err, Error::Execution(_)),
            "a non-settled Running result is an execution error, got {err:?}"
        );
    }

    #[test]
    fn ended_settles_done_and_awaiting_settles_awaiting() {
        assert!(matches!(
            settle_outcome(&RunState::Ended(EndCause::NaturalEnd)).unwrap(),
            DispatchOutcome::Done
        ));
        assert!(matches!(
            settle_outcome(&RunState::Awaiting).unwrap(),
            DispatchOutcome::Awaiting
        ));
    }
}
