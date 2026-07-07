//! The dispatch worker: claim one runnable dispatch, run it, settle it.
//!
//! The worker is the only place that turns a durable dispatch into a runtime
//! attempt. It is additive over runtime control (G6): it never reaches into the
//! loop, it calls the same `RunExecutor`/`Runtime::resume` a direct caller would,
//! and it decides execute-vs-resume from *committed truth* — the waiting ticket
//! and the run record — not from a duplicated status in the queue. That keeps the
//! DispatchQueue aggregate free of run-outcome truth.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Phase};
use awaken_agent_contract::agent::waiting::WaitingReason;
use awaken_agent_contract::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime::Runtime;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use tokio_util::sync::CancellationToken;

use crate::Error;
use crate::dispatch::{Dispatch, DispatchOutcome, PendingInput};
use crate::request::RunExecutionContext;

/// Default lease: how long a claimed dispatch is owned before it is reclaimable.
pub const DEFAULT_LEASE_MS: u64 = 30_000;

/// Runs durable dispatches against a runtime. Generic over the store so the same
/// engine drives the in-memory reference and the Postgres backend unchanged.
pub struct DispatchWorker<S> {
    runtime: Arc<Runtime>,
    store: Arc<S>,
    exec: RunExecutionContext,
    reader: Arc<dyn ThreadReader>,
    runs: Arc<dyn RunStore + Send + Sync>,
    owner: String,
    lease_ms: u64,
}

impl<S: Dispatch> DispatchWorker<S> {
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
        C: CommitCoordinator + ThreadReader + RunStore + Send + Sync + 'static,
    {
        Self {
            runtime,
            store,
            // The same store is the commit boundary and the history reader, so a
            // durable fresh run continues the thread's conversation.
            exec: RunExecutionContext::new(commit.clone()).with_reader(commit.clone()),
            reader: commit.clone(),
            runs: commit,
            owner: owner.into(),
            lease_ms: DEFAULT_LEASE_MS,
        }
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
        store: Arc<dyn awaken_agent_contract::store::stream_checkpoint::StreamCheckpointStore>,
    ) -> Self {
        self.exec = self.exec.with_stream_checkpoint(store);
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
        self.exec.runtime_context(CancellationToken::new())
    }

    /// Perform a committed ScheduledAction in-process (ADR-0020).
    async fn perform_scheduled(&self, run_id: &RunId, now_ms: u64) -> Result<Phase, Error> {
        Ok(self
            .runtime
            .perform_scheduled_action(
                run_id,
                self.reader.as_ref(),
                self.execution_context(),
                now_ms,
            )
            .await?)
    }

    /// Claim and process at most one runnable dispatch. Returns the processed
    /// run's id and resulting phase, or `None` when the queue is idle.
    pub async fn tick(&self, now_ms: u64) -> Result<Option<(RunId, Phase)>, Error> {
        let Some(claimed) = self.store.claim(&self.owner, self.lease_ms, now_ms).await? else {
            return Ok(None);
        };
        let run_id = claimed.request.run_id().clone();
        let mut all_pending: Vec<String> = claimed
            .pending
            .iter()
            .map(|p| p.message_id.clone())
            .collect();

        let mut phase = match self.reader.waiting_ticket(&run_id) {
            // A committed ScheduledAction (ADR-0020): the system performs the
            // deferred action, not waits for external input. This also covers a
            // crash recovery of a scheduled park (no pending input is expected).
            Some(ticket) if ticket.reason == WaitingReason::ScheduledAction => {
                self.perform_scheduled(&run_id, now_ms).await?
            }
            // The run is parked. Deliver only input whose correlation matches the
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
                        self.runtime
                            .resume(command, self.reader.as_ref(), self.execution_context())
                            .await?
                    }
                    None => {
                        // No input answers the current ticket; drop stale input
                        // and leave the run parked for a later wake.
                        self.store
                            .settle(&run_id, DispatchOutcome::Parked, &all_pending)
                            .await?;
                        return Ok(Some((run_id, Phase::Waiting)));
                    }
                }
            }
            // No ticket: a fresh run, or a recovered run that already finished.
            // The committed run record disambiguates so recovery never re-runs a
            // terminal run, and any orphan pending is dropped on settle.
            None => match self.runs.get(&run_id) {
                Some(record) if matches!(record.phase, Phase::Ended(_)) => {
                    self.store
                        .settle(&run_id, DispatchOutcome::Done, &all_pending)
                        .await?;
                    return Ok(Some((run_id, record.phase)));
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
                    for (idx, input) in unbound.iter().enumerate() {
                        if let ResumeResult::Input(text) = &input.result {
                            activation.input.insert(
                                idx,
                                Message {
                                    id: MessageId(input.message_id.clone()),
                                    role: Role::User,
                                    content: vec![ContentBlock::text(text)],
                                },
                            );
                        }
                    }
                    // Consumed on settle, not on read, so a crash re-delivers them.
                    all_pending.extend(unbound.into_iter().map(|input| input.message_id));
                    self.runtime
                        .execute(activation, self.execution_context())
                        .await?
                }
            },
        };

        // Drive any further scheduled actions to completion in-process: a run that
        // ends a step by committing a ScheduledAction is performed immediately,
        // until it ends or parks on a wait that needs external input.
        while phase == Phase::Waiting {
            match self.reader.waiting_ticket(&run_id) {
                Some(ticket) if ticket.reason == WaitingReason::ScheduledAction => {
                    phase = self.perform_scheduled(&run_id, now_ms).await?;
                }
                _ => break,
            }
        }

        let outcome = match &phase {
            Phase::Waiting => DispatchOutcome::Parked,
            Phase::Ended(_) => DispatchOutcome::Done,
            // `execute`/`resume` only ever return a parked or ended phase;
            // `Running` exists as durable mid-flight truth, never as an
            // executor result. Fail loudly rather than settle a live run.
            Phase::Running => {
                return Err(Error::Execution(
                    awaken_runtime_contract::execution::Error::Execution(
                        "executor returned a non-settled Running phase".to_string(),
                    ),
                ));
            }
        };
        self.store.settle(&run_id, outcome, &all_pending).await?;
        Ok(Some((run_id, phase)))
    }

    /// Drain the queue until no dispatch is runnable, returning every processed
    /// run and its phase. A settled run becomes non-runnable, so this terminates.
    pub async fn run_until_idle(&self, now_ms: u64) -> Result<Vec<(RunId, Phase)>, Error> {
        let mut processed = Vec::new();
        while let Some(result) = self.tick(now_ms).await? {
            processed.push(result);
        }
        Ok(processed)
    }
}
