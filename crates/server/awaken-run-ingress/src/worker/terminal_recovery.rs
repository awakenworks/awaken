//! Committed-terminal recovery and pre-execution terminalization for DispatchWorker.
//! This is the existing Worker implementation split by responsibility. Every
//! entry uses its original claim-fenced commit, observer and settlement path.

use super::{DispatchWorker, PendingInputAdmission, pending_input_admission};
use crate::Error;
use crate::clock::Clock;
use crate::dispatch::{Claimed, Dispatch, DispatchOutcome, DispatchState, RunClaim, SettleOutcome};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::terminal::{
    CommittedTerminalProjection, CommittedTerminalRun, committed_terminal_projection,
    deliver_committed_terminal,
};
use std::sync::Arc;
use tracing::Instrument;

pub(super) enum CommittedTerminalSettlement {
    Applied(RunId, RunState),
    Fenced,
}

impl CommittedTerminalSettlement {
    pub(super) fn into_processed(self) -> Option<(RunId, RunState)> {
        match self {
            Self::Applied(run_id, state) => Some((run_id, state)),
            Self::Fenced => None,
        }
    }
}

impl<S: Dispatch + 'static> DispatchWorker<S> {
    /// Read the one Runtime-contract terminal projection for this exact claim.
    /// Identity conflicts are corrupt authority, not an ordinary nonterminal.
    pub(super) fn committed_terminal_for_claim(
        &self,
        claimed: &Claimed,
    ) -> Result<Option<CommittedTerminalRun>, Error> {
        let run_id = claimed.request.run_id();
        let thread_id = claimed.request.thread_id();
        match committed_terminal_projection(self.reader.as_ref(), run_id, thread_id) {
            CommittedTerminalProjection::Nonterminal => Ok(None),
            CommittedTerminalProjection::Exact(terminal) => Ok(Some(terminal)),
            CommittedTerminalProjection::IdentityConflict => {
                Err(Error::Dispatch(crate::DispatchError::Rejected(format!(
                    "committed terminal identity conflicts with dispatch Run {} Thread {}",
                    run_id.0, thread_id.0
                ))))
            }
        }
    }

    /// Deliver observers and settle exactly one already-projected terminal fact.
    /// No executor dependency is accepted by this owner; every terminal recovery
    /// entry reaches the same claim-fenced `Done` transition here.
    async fn settle_projected_terminal(
        &self,
        claimed: &Claimed,
        terminal: CommittedTerminalRun,
        consumed: &[String],
        clock: &Arc<dyn Clock>,
    ) -> Result<CommittedTerminalSettlement, Error> {
        self.deliver_terminal_observers(&terminal).await?;
        let run_id = terminal.run_id;
        let state = RunState::Ended(terminal.cause);
        let settled = self
            .settle(
                claimed,
                Some(&state),
                DispatchOutcome::Done,
                consumed,
                clock,
            )
            .await?;
        Ok(if settled.applied() {
            CommittedTerminalSettlement::Applied(run_id, state)
        } else {
            CommittedTerminalSettlement::Fenced
        })
    }

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
            RunDisposition::ended(run_id.clone(), cause.clone()),
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
        Ok(self
            .settle_projected_terminal(
                claimed,
                CommittedTerminalRun {
                    run_id,
                    thread_id,
                    cause,
                },
                &consumed,
                &clock,
            )
            .await?
            .into_processed())
    }

    /// Settle one claim from committed terminal truth without invoking an
    /// executor. This is the single owner of the crash-window inbox cleanup used
    /// both before backend recovery and after an awaiting-ticket race.
    pub(super) async fn settle_from_committed_terminal(
        &self,
        claimed: &Claimed,
        all_pending: &mut Vec<String>,
        clock: &Arc<dyn Clock>,
    ) -> Result<Option<CommittedTerminalSettlement>, Error> {
        let Some(terminal) = self.committed_terminal_for_claim(claimed)? else {
            return Ok(None);
        };

        // The committed terminal dominates the claimed signal set. Rebuild the
        // settlement deletion list from the same admission kernel used by fresh
        // execution and awaiting resume, so a future input class cannot bypass
        // the no-redelivery rule merely by being added to Claimed::pending.
        all_pending.clear();
        all_pending.extend(
            claimed
                .pending
                .iter()
                .filter(|input| {
                    pending_input_admission(true, None, input)
                        == PendingInputAdmission::ConsumedByCommittedTruth
                })
                .map(|input| input.message_id.clone()),
        );

        // A recovered fresh run that already committed a terminal record may
        // have drained unbound idle-thread input before dying prior to settle.
        // Consume exactly rows whose ids exist in committed transcript truth;
        // later, undelivered input remains available to the next Run.
        let delivered: std::collections::HashSet<String> = self
            .reader
            .committed_messages(&terminal.thread_id)
            .into_iter()
            .map(|message| message.id.0)
            .collect();
        let consumed_unbound = self
            .store
            .list(&terminal.thread_id)
            .await?
            .into_iter()
            .map(|record| record.input)
            .filter(|input| input.run_id.0.is_empty() && delivered.contains(&input.message_id))
            .map(|input| input.message_id);
        all_pending.extend(consumed_unbound);
        Ok(Some(
            self.settle_projected_terminal(claimed, terminal, all_pending, clock)
                .await?,
        ))
    }

    /// Convert a runtime-drive failure into a benign already-done when the
    /// canonical committed projection shows that a replacement already ended
    /// the Run. Any other error remains an execution failure.
    pub(super) async fn settle_if_terminal_or_raise(
        &self,
        claimed: &Claimed,
        consumed: &[String],
        err: impl Into<Error>,
        clock: &Arc<dyn Clock>,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let err = err.into();
        self.refresh_local_recovery_projection(
            claimed.request.thread_id(),
            claimed.request.run_id(),
        )
        .await?;
        if let Some(terminal) = self.committed_terminal_for_claim(claimed)? {
            // This path lost a terminal-commit race. Redelivery is expected:
            // the winning attempt may have crashed after commit and observers
            // suppress duplicate effects by `(observer_id, run_id)`.
            return Ok(self
                .settle_projected_terminal(claimed, terminal, consumed, clock)
                .await?
                .into_processed());
        }

        // A stale attempt can lose ownership before replacement terminal truth
        // is readable in this Worker's projection. The queue already rejected
        // its authority, so meter that exact decision through the same choke as
        // a stale settlement CAS. Ordinary Runs retain their original error; a
        // coordinated Run quietly yields to its replacement because it must not
        // manufacture an Error boundary for the parent Session.
        let started = std::time::Instant::now();
        let claim = RunClaim::from(&claimed.lease);
        if !self.store.claim_is_current(&claim, clock.now_ms()).await? {
            self.record_settlement_outcome(SettleOutcome::Fenced, "done", started.elapsed());
            if claimed.request.session_activity_epoch.is_some() {
                return Ok(None);
            }
        }
        Err(err)
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
        // Decision table invariant: every terminal-recovery caller must check
        // the authoritative snapshot bound to this exact claim, never a stale
        // process projection. Keeping the refresh here makes claim + verify +
        // fenced settlement one indivisible protocol for every entry point.
        let claim = RunClaim::from(&claimed.lease);
        self.install_claimed_recovery_projection(&claim, claimed.request.thread_id())
            .await?;
        let Some(terminal) = self.committed_terminal_for_claim(&claimed)? else {
            let _ = self
                .settle(&claimed, None, DispatchOutcome::Awaiting, &[], &clock)
                .await?;
            return Ok(None);
        };
        Ok(self
            .settle_projected_terminal(&claimed, terminal, &[], &clock)
            .await?
            .into_processed())
    }

    pub(super) async fn redeliver_terminal_observers(
        &self,
        run_id: &RunId,
        thread_id: &ThreadId,
    ) -> Result<(), Error> {
        let terminal = match committed_terminal_projection(self.reader.as_ref(), run_id, thread_id)
        {
            CommittedTerminalProjection::Nonterminal => return Ok(()),
            CommittedTerminalProjection::Exact(terminal) => terminal,
            CommittedTerminalProjection::IdentityConflict => {
                return Err(Error::Dispatch(crate::DispatchError::Rejected(format!(
                    "committed terminal identity conflicts with Run {} Thread {}",
                    run_id.0, thread_id.0
                ))));
            }
        };
        self.deliver_terminal_observers(&terminal).await
    }

    async fn deliver_terminal_observers(
        &self,
        terminal: &CommittedTerminalRun,
    ) -> Result<(), Error> {
        let context = self.execution_context();
        let failures = deliver_committed_terminal(&context.terminal_observers, terminal).await;
        if failures.is_empty() {
            return Ok(());
        }

        for failure in &failures {
            tracing::warn!(
                observer.id = %failure.observer_id,
                awaken.run.id = %terminal.run_id.0,
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
