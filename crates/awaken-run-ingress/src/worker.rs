//! The dispatch worker: claim one runnable dispatch, run it, settle it.
//!
//! The worker is the only place that turns a durable dispatch into a runtime
//! attempt. It is additive over runtime control (G6): it never reaches into the
//! loop, it calls the same `RunExecutor`/`Runtime::resume` a direct caller would,
//! and it decides execute-vs-resume from *committed truth* — the waiting ticket
//! and the run record — not from a duplicated status in the queue. That keeps the
//! RunDispatch aggregate free of run-outcome truth.

use std::sync::Arc;

use awaken_agent_contract::agent::run::{Id as RunId, Phase};
use awaken_agent_contract::agent::waiting::WaitingTicket;
use awaken_agent_contract::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime::Runtime;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use tokio_util::sync::CancellationToken;

use crate::Error;
use crate::dispatch::{DispatchOutcome, DispatchStore};
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

impl<S: DispatchStore> DispatchWorker<S> {
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
            exec: RunExecutionContext::new(commit.clone()),
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

    /// Claim and process at most one runnable dispatch. Returns the processed
    /// run's id and resulting phase, or `None` when the queue is idle.
    pub async fn tick(&self, now_ms: u64) -> Result<Option<(RunId, Phase)>, Error> {
        let Some(claimed) = self.store.claim(&self.owner, self.lease_ms, now_ms).await? else {
            return Ok(None);
        };
        let run_id = claimed.request.run_id().clone();
        let context = self.exec.runtime_context(CancellationToken::new());

        let phase = match self.reader.waiting_ticket(&run_id) {
            // The run is parked. A wake delivers pending input -> resume. A claim
            // with no input is a recovered park with nothing to do; re-park it.
            Some(ticket) => match claimed.pending.into_iter().next() {
                Some(input) => {
                    let command = resume_command(&ticket, input.result, now_ms);
                    self.runtime
                        .resume(command, self.reader.as_ref(), context)
                        .await?
                }
                None => {
                    self.store.settle(&run_id, DispatchOutcome::Parked).await?;
                    return Ok(Some((run_id, Phase::Waiting)));
                }
            },
            // No ticket: a fresh run, or a recovered run that already finished.
            // The committed run record disambiguates so recovery never re-runs a
            // terminal run.
            None => match self.runs.get(&run_id) {
                Some(record) if matches!(record.phase, Phase::Ended(_)) => {
                    self.store.settle(&run_id, DispatchOutcome::Done).await?;
                    return Ok(Some((run_id, record.phase)));
                }
                _ => {
                    self.runtime
                        .execute(claimed.request.activation, context)
                        .await?
                }
            },
        };

        let outcome = match &phase {
            Phase::Waiting => DispatchOutcome::Parked,
            Phase::Ended(_) => DispatchOutcome::Done,
        };
        self.store.settle(&run_id, outcome).await?;
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

/// Build a resume command from the committed ticket plus the delivered input.
/// Every identity comes from the ticket, so the runtime's resume validation
/// (G5/G28) checks the resume against the same correlation it parked on.
fn resume_command(ticket: &WaitingTicket, result: ResumeResult, now_ms: u64) -> ResumeCommand {
    ResumeCommand {
        correlation_id: ticket.correlation_id.clone(),
        run_id: ticket.run_id.clone(),
        thread_id: ticket.thread_id.clone(),
        snapshot_id: ticket.snapshot_id.clone(),
        catalog_fingerprint: ticket.catalog_fingerprint.clone(),
        result,
        now_ms,
    }
}
