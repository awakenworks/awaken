//! Committed-terminal recovery and pre-execution terminalization for DispatchWorker.
//! This is the existing Worker implementation split by responsibility. Every
//! entry uses its original claim-fenced commit, observer and settlement path.

use super::DispatchWorker;
use crate::Error;
use crate::clock::Clock;
use crate::dispatch::{Claimed, Dispatch, DispatchOutcome, DispatchState, RunClaim};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::terminal::redeliver_committed_terminal;
use std::sync::Arc;
use tracing::Instrument;

impl<S: Dispatch + 'static> DispatchWorker<S> {
    /// Commit and settle a deterministic failure returned while resolving this
    /// exact claimed attempt, before an executor could be constructed. This is
    /// the same terminal commit and claim fence used after execution; it is not a
    /// dead-letter shortcut. If authority was lost, commit/settle fencing leaves
    /// the replacement owner untouched and the error remains recoverable.
    pub async fn fail_claimed_before_execution(
        &self,
        claimed: &Claimed,
        code: impl Into<String>,
        message: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        use awaken_agent_contract::agent::run::{EndCause, Failure};

        let cause = EndCause::Error(Failure::Inference {
            code: code.into(),
            message: message.into(),
        });
        let dispatch = Self::claimed_dispatch_span(claimed);
        self.prepare_and_end_claimed_before_execution(claimed, cause, clock)
            .instrument(dispatch)
            .await
    }

    /// Commit the one neutral terminal outcome for an exact retry-exhaustion
    /// claim, then reuse the ordinary observer and fenced `Done` settlement.
    /// The queue command owns only retry eligibility; this Worker method is the
    /// sole bridge into committed Run truth for every local, pooled, and remote
    /// automatic path.
    pub async fn terminalize_retry_exhausted(
        &self,
        claimed: &Claimed,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let dispatch = Self::claimed_dispatch_span(claimed);
        self.prepare_and_end_claimed_before_execution(
            claimed,
            awaken_agent_contract::agent::run::EndCause::Indeterminate,
            clock,
        )
        .instrument(dispatch)
        .await
    }

    /// Install the claim-fenced recovery projection once before either public
    /// pre-execution terminal cause enters the shared commit/observer/settle
    /// owner. Keeping this preparation here prevents resolution failure and
    /// retry exhaustion from growing parallel terminal protocols.
    async fn prepare_and_end_claimed_before_execution(
        &self,
        claimed: &Claimed,
        cause: awaken_agent_contract::agent::run::EndCause,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let claim = RunClaim::from(&claimed.lease);
        self.install_claimed_recovery_projection(&claim, claimed.request.thread_id())
            .await?;
        self.end_claimed_before_execution(claimed, cause, clock)
            .await
    }

    /// Claim and terminalize at most one exhausted dispatch for the per-Session
    /// service. Process pools claim centrally and call
    /// [`terminalize_retry_exhausted`](Self::terminalize_retry_exhausted) on the
    /// resolved boundary Worker instead.
    pub async fn resolve_one_retry_exhausted(
        &self,
        max_attempts: u64,
        clock: Arc<dyn Clock>,
    ) -> Result<bool, Error> {
        let now_ms = clock.now_ms();
        let Some(claimed) = self
            .store
            .claim_retry_exhausted(&self.owner, self.lease_ms, now_ms, max_attempts)
            .await?
        else {
            return Ok(false);
        };
        let claim = RunClaim::from(&claimed.lease);
        let _renewal = self.renew_claim_while_driving(&claim, clock.clone());
        let _ = self.terminalize_retry_exhausted(&claimed, clock).await?;
        Ok(true)
    }

    /// Commit one deterministic terminal outcome before invoking an executor.
    /// Callers install any recovery projection first; this function owns the one
    /// claim-fenced commit/observer/settle sequence shared by resolution failures
    /// and opaque ACP crash recovery.
    pub(super) async fn end_claimed_before_execution(
        &self,
        claimed: &Claimed,
        cause: awaken_agent_contract::agent::run::EndCause,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        use awaken_agent_contract::thread::commit::{RunDisposition, commit_run};

        let run_id = claimed.lease.run_id.clone();
        let thread_id = claimed.request.thread_id().clone();
        let consumed = claimed
            .pending
            .iter()
            .map(|pending| pending.message_id.clone())
            .collect::<Vec<_>>();
        let state = RunState::Ended(cause.clone());
        let claim = RunClaim::from(&claimed.lease);
        let context = self.execution_context_with(
            &claim,
            claimed.request.execution_scope.as_ref(),
            &None,
            &[],
            clock.clone(),
            None,
        );
        let coordinator = context.commit.ok_or_else(|| {
            Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                "claimed pre-execution terminal has no commit coordinator".to_string(),
            ))
        })?;
        if let Err(error) = commit_run(
            coordinator.as_ref(),
            &thread_id,
            RunDisposition::ended(run_id.clone(), cause),
            Vec::new(),
            Vec::new(),
        )
        .await
        {
            return self
                .settle_if_terminal_or_raise(
                    claimed,
                    &consumed,
                    Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                        error.to_string(),
                    )),
                    &clock,
                )
                .await;
        }
        self.redeliver_terminal_observers(&run_id, &thread_id)
            .await?;
        Ok(self
            .settle(
                claimed,
                Some(&state),
                DispatchOutcome::Done,
                &consumed,
                &clock,
            )
            .await?
            .applied()
            .then_some((run_id, state)))
    }

    /// Reconcile one quiescent pending/awaiting dispatch or expired running lease
    /// whose committed Run is already terminal. The committed reader is checked before
    /// the special claim, and
    /// [`settle_claimed_terminal`](Self::settle_claimed_terminal) checks it again
    /// under that claim before reusing the ordinary fenced Done settlement and
    /// completion tombstone.
    pub async fn reconcile_committed_terminal(
        &self,
        run_id: &RunId,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let now_ms = clock.now_ms();
        // A local active-active worker's process projection may predate the peer
        // that committed the terminal fact. Refresh the exact thread snapshot
        // before making the claim/no-claim authority decision.
        if self.recovery_source.is_some()
            && let Some(row) = self
                .store
                .list_dispatches()
                .await?
                .into_iter()
                .find(|row| &row.run_id == run_id)
        {
            self.refresh_local_recovery_projection(&row.thread_id, run_id)
                .await?;
        }
        if !matches!(self.reader.run_state(run_id), Some(RunState::Ended(_))) {
            return Ok(None);
        }
        let Some(claimed) = self
            .store
            .claim_for_terminal_recovery(run_id, &self.owner, self.lease_ms, now_ms)
            .await?
        else {
            return Ok(None);
        };
        self.settle_claimed_terminal(claimed, clock).await
    }

    /// Bounded scan used by both the per-Session daemon and process pool host
    /// adapter. Rows outside this Worker's committed reader naturally return no
    /// state, so one implementation owns selection and settlement semantics.
    pub async fn reconcile_committed_terminals(
        &self,
        clock: Arc<dyn Clock>,
        limit: usize,
    ) -> Result<Vec<(RunId, RunState)>, Error> {
        let rows = self.store.list_dispatches().await?;
        let mut processed = Vec::new();
        for row in rows.into_iter().filter(|row| {
            matches!(
                row.state,
                DispatchState::Pending | DispatchState::Awaiting | DispatchState::Leased
            )
        }) {
            if processed.len() >= limit {
                break;
            }
            if let Some(terminal) = self
                .reconcile_committed_terminal(&row.run_id, clock.clone())
                .await?
            {
                processed.push(terminal);
            }
        }
        Ok(processed)
    }

    /// Complete a terminal-recovery claim without executing the Run. If the
    /// caller raced stale or incorrect read evidence, restore the row to
    /// Awaiting; only exact committed `Ended` truth can reach Done.
    pub async fn settle_claimed_terminal(
        &self,
        claimed: Claimed,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let dispatch = Self::claimed_dispatch_span(&claimed);
        self.settle_claimed_terminal_inner(claimed, clock)
            .instrument(dispatch)
            .await
    }

    async fn settle_claimed_terminal_inner(
        &self,
        claimed: Claimed,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let run_id = claimed.lease.run_id.clone();
        // Decision table invariant: every terminal-recovery caller must check
        // the authoritative snapshot bound to this exact claim, never a stale
        // process projection. Keeping the refresh here makes claim + verify +
        // fenced settlement one indivisible protocol for every entry point.
        let claim = RunClaim::from(&claimed.lease);
        self.install_claimed_recovery_projection(&claim, claimed.request.thread_id())
            .await?;
        let Some(state @ RunState::Ended(_)) = self.reader.run_state(&run_id) else {
            let _ = self
                .settle(&claimed, None, DispatchOutcome::Awaiting, &[], &clock)
                .await?;
            return Ok(None);
        };
        let thread_id = claimed.request.thread_id().clone();
        self.redeliver_terminal_observers(&run_id, &thread_id)
            .await?;
        Ok(self
            .settle(&claimed, Some(&state), DispatchOutcome::Done, &[], &clock)
            .await?
            .applied()
            .then_some((run_id, state)))
    }

    pub(super) async fn redeliver_terminal_observers(
        &self,
        run_id: &RunId,
        thread_id: &ThreadId,
    ) -> Result<(), Error> {
        let context = self.execution_context();
        let Some(failures) = redeliver_committed_terminal(
            self.reader.as_ref(),
            &context.terminal_observers,
            run_id,
            thread_id,
        )
        .await
        else {
            return Ok(());
        };
        if failures.is_empty() {
            return Ok(());
        }

        for failure in &failures {
            tracing::warn!(
                observer.id = %failure.observer_id,
                awaken.run.id = %run_id.0,
                error = %failure.error,
                "terminal observer failed; dispatch settlement withheld for recovery"
            );
        }
        let failures = failures
            .into_iter()
            .map(|failure| format!("{}: {}", failure.observer_id, failure.error))
            .collect::<Vec<_>>()
            .join("; ");
        Err(Error::Execution(
            awaken_runtime_contract::execution::Error::Execution(format!(
                "committed-terminal observer delivery failed; dispatch settlement withheld for recovery: {failures}"
            )),
        ))
    }
}
