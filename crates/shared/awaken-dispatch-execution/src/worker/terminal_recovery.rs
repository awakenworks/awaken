// @formal entrypoint remote_worker.committed_terminal_recovery_skips_execution_and_settles_dispatch
// @formal entrypoint remote_worker.pending_signal_is_consumed_by_commit
// @formal entrypoint tool_result.terminal_run_rejects_late_result
//! Shared settlement, committed-terminal recovery and pre-execution terminalization for DispatchWorker.
//! This is the existing Worker implementation split by responsibility. Every
//! entry uses its original claim-fenced commit, observer and settlement path.
//! The settlement operations the claimed drive awaits at many sites return
//! heap [`ClaimFuture`]s so the drive's poll frame holds only their pointers.

use super::{
    ClaimFuture, DispatchWorker, ExecutionAuthority, PendingInputAdmission,
    pending_input_admission, settle_outcome,
};
use crate::Error;
use crate::control_dispatch::TerminalRecoveryDispatch;
use crate::dispatch::{Claimed, DispatchOutcome, DispatchSummary, RunClaim, SettleOutcome};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_erasure::Dispatch;
use awaken_runtime_contract::terminal::{
    CommittedTerminalProjection, CommittedTerminalRun, committed_terminal_projection,
    deliver_committed_terminal,
};
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
    ) -> Result<CommittedTerminalSettlement, Error> {
        self.deliver_terminal_observers(&terminal).await?;
        let run_id = terminal.run_id;
        let state = RunState::Ended(terminal.cause);
        let settled = self
            .settle(claimed, Some(&state), DispatchOutcome::Done, consumed)
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
    ) -> Result<Option<(RunId, RunState)>, Error> {
        use awaken_agent_contract::agent::run::{EndCause, Failure};

        let cause = EndCause::Error(Failure::Inference {
            code: code.into(),
            message: message.into(),
        });
        let dispatch = Self::claimed_dispatch_span(claimed);
        self.prepare_and_end_claimed_before_execution(claimed, cause)
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
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let dispatch = Self::claimed_dispatch_span(claimed);
        self.prepare_and_end_claimed_before_execution(
            claimed,
            awaken_agent_contract::agent::run::EndCause::Indeterminate,
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
    ) -> Result<Option<(RunId, RunState)>, Error> {
        self.install_claimed_recovery_projection(claimed, claimed.request.thread_id())
            .await?;
        self.end_claimed_before_execution(claimed, cause).await
    }

    /// Claim and terminalize at most one exhausted dispatch for the per-Session
    /// service. Process pools claim centrally and call
    /// [`terminalize_retry_exhausted`](Self::terminalize_retry_exhausted) on the
    /// resolved boundary Worker instead.
    pub async fn resolve_one_retry_exhausted(&self, max_attempts: u64) -> Result<bool, Error> {
        let Some(claimed) = self
            .store
            .claim_retry_exhausted(&self.owner, self.lease_ms, max_attempts)
            .await?
        else {
            return Ok(false);
        };
        let claim = RunClaim::from(&claimed.lease);
        let _renewal = self.renew_claim_while_driving(&claim);
        let _ = self.terminalize_retry_exhausted(&claimed).await?;
        Ok(true)
    }

    /// Commit one deterministic terminal outcome before invoking an executor.
    /// Callers install any recovery projection first; this function owns the one
    /// claim-fenced commit/observer/settle sequence shared by resolution failures
    /// and opaque ACP crash recovery.
    pub(super) fn end_claimed_before_execution<'a>(
        &'a self,
        claimed: &'a Claimed,
        cause: awaken_agent_contract::agent::run::EndCause,
    ) -> ClaimFuture<'a, Option<(RunId, RunState)>> {
        Box::pin(async move {
            use awaken_agent_contract::thread::commit::{RunDisposition, commit_run};

            let run_id = claimed.lease.run_id().clone();
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
                &[],
                None,
            )?;
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
                )
                .await?
                .into_processed())
        })
    }

    /// Settle one claim from committed terminal truth without invoking an
    /// executor. This is the single owner of the crash-window inbox cleanup used
    /// both before backend recovery and after an awaiting-ticket race.
    pub(super) fn settle_from_committed_terminal<'a>(
        &'a self,
        claimed: &'a Claimed,
        all_pending: &'a mut Vec<String>,
    ) -> ClaimFuture<'a, Option<CommittedTerminalSettlement>> {
        Box::pin(async move {
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
                self.settle_projected_terminal(claimed, terminal, all_pending)
                    .await?,
            ))
        })
    }

    /// Convert a runtime-drive failure into a benign already-done when the
    /// canonical committed projection shows that a replacement already ended
    /// the Run. Any other error remains an execution failure.
    pub(super) fn settle_if_terminal_or_raise<'a>(
        &'a self,
        claimed: &'a Claimed,
        consumed: &'a [String],
        err: impl Into<Error>,
    ) -> ClaimFuture<'a, Option<(RunId, RunState)>> {
        let err = err.into();
        Box::pin(async move {
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
                    .settle_projected_terminal(claimed, terminal, consumed)
                    .await?
                    .into_processed());
            }

            // A stale attempt can lose ownership before replacement terminal truth
            // is readable in this Worker's projection. The queue already rejected
            // its authority, so meter that exact decision through the same choke as
            // a stale settlement CAS. Ordinary Runs retain their original error; a
            // coordinated Run quietly yields to its replacement because it must not
            // manufacture an Error boundary for the parent Session.
            let started = awaken_time::mono::Stopwatch::start();
            let claim = RunClaim::from(&claimed.lease);
            if !self.store.claim_is_current(&claim).await? {
                self.record_settlement_outcome(SettleOutcome::Fenced, "done", started.elapsed());
                if claimed.request.session_activity_epoch.is_some() {
                    return Ok(None);
                }
            }
            Err(err)
        })
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
        control: &dyn TerminalRecoveryDispatch,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let Some(row) = control.dispatch_summary(run_id).await? else {
            return Ok(None);
        };
        self.reconcile_terminal_row(row, control).await
    }

    async fn reconcile_terminal_row(
        &self,
        row: DispatchSummary,
        control: &dyn TerminalRecoveryDispatch,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let run_id = &row.run_id;
        // A local active-active worker's process projection may predate the peer
        // that committed the terminal fact. Refresh the exact thread snapshot
        // before making the claim/no-claim authority decision.
        if self.recovery_source.is_some() {
            self.refresh_local_recovery_projection(&row.thread_id, run_id)
                .await?;
        }
        if !matches!(self.reader.run_state(run_id), Some(RunState::Ended(_))) {
            return Ok(None);
        }
        let Some(claimed) = control
            .claim_for_terminal_recovery(run_id, &self.owner, self.lease_ms)
            .await?
        else {
            return Ok(None);
        };
        self.settle_claimed_terminal(claimed).await
    }

    /// Inspect a bounded page through the same scan owner used by the Host.
    /// The budget counts inspected IDs, not successful terminal settlements.
    pub async fn reconcile_committed_terminals(
        &self,
        limit: usize,
        control: &dyn TerminalRecoveryDispatch,
    ) -> Result<Vec<(RunId, RunState)>, Error> {
        self.terminal_scan
            .reconcile(control, limit, |row| {
                self.reconcile_terminal_row(row, control)
            })
            .await
    }

    /// Complete a terminal-recovery claim without executing the Run. If the
    /// caller raced stale or incorrect read evidence, restore the row to
    /// Awaiting; only exact committed `Ended` truth can reach Done.
    pub async fn settle_claimed_terminal(
        &self,
        claimed: Claimed,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        let dispatch = Self::claimed_dispatch_span(&claimed);
        self.settle_claimed_terminal_inner(claimed)
            .instrument(dispatch)
            .await
    }

    async fn settle_claimed_terminal_inner(
        &self,
        claimed: Claimed,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        // Decision table invariant: every terminal-recovery caller must check
        // the authoritative snapshot bound to this exact claim, never a stale
        // process projection. Keeping the refresh here makes claim + verify +
        // fenced settlement one indivisible protocol for every entry point.
        self.install_claimed_recovery_projection(&claimed, claimed.request.thread_id())
            .await?;
        let Some(terminal) = self.committed_terminal_for_claim(&claimed)? else {
            let _ = self
                .settle(&claimed, None, DispatchOutcome::Awaiting, &[])
                .await?;
            return Ok(None);
        };
        match self
            .settle_projected_terminal(&claimed, terminal, &[])
            .await
        {
            Ok(settled) => Ok(settled.into_processed()),
            Err(error) if error.is_settlement_observation_pending() => {
                // The Session observer has not yet accepted the committed
                // terminal boundary. No Done receipt was written. Release only
                // this exact mutation claim behind the Pool's existing retry
                // deadline so a registered Worker can recover it instead of
                // letting the Coordinator scanner re-own it indefinitely.
                let claim = RunClaim::from(&claimed.lease);
                let retry_after_ms =
                    crate::pool::claim_failure_backoff(claim.epoch).as_millis() as u64;
                crate::pool::defer_failed_claim(self.store.as_ref(), &claim, retry_after_ms)
                    .await?;
                Err(error)
            }
            Err(error) => Err(error),
        }
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

    /// Settle a claimed dispatch and record the operational `runs.settled` counter
    /// (labelled by outcome) on the runtime's metrics recorder. The single choke all
    /// of `drive_claimed`'s settle arms route through, so every settle is metered
    /// exactly once regardless of which branch reached it.
    pub(super) fn settle<'a>(
        &'a self,
        claimed: &'a Claimed,
        expected_committed_state: Option<&'a RunState>,
        outcome: DispatchOutcome,
        consumed: &'a [String],
    ) -> ClaimFuture<'a, SettleOutcome> {
        Box::pin(async move {
            let run_id = claimed.request.run_id();
            let claim = RunClaim::from(&claimed.lease);
            let mut coordinated_committed_state = None;
            let coordinated_claim_is_stale = if claimed.request.session_activity_epoch.is_some()
                && let Some(expected) = expected_committed_state
            {
                let committed = self.reader.run_state(run_id).ok_or_else(|| {
                    Error::Dispatch(crate::DispatchError::Rejected(format!(
                        "coordinated child Run {} has no committed settlement state",
                        run_id.0
                    )))
                })?;
                if &committed != expected || settle_outcome(&committed)? != outcome {
                    return Err(Error::Dispatch(crate::DispatchError::Rejected(format!(
                        "coordinated child Run {} settlement does not match committed truth",
                        run_id.0
                    ))));
                }
                if !self.store.claim_is_current(&claim).await? {
                    true
                } else {
                    let observer = self.settlement_observer.as_ref().ok_or_else(|| {
                        Error::Dispatch(crate::DispatchError::Rejected(
                            "coordinated child settlement observer is not installed".to_string(),
                        ))
                    })?;
                    // Pre-settlement Session effects need the same physical gate as
                    // the queue CAS. A remote observer checks its authority-side
                    // guard; a local observer consumes the colocated locked facts.
                    let guard = if observer.owns_dispatch_fence() {
                        None
                    } else {
                        let ExecutionAuthority::Local(store) = &self.ports else {
                            return Err(Error::Dispatch(crate::DispatchError::Rejected(
                                "remote settlement observer must own its dispatch fence".into(),
                            )));
                        };
                        let Some(guard) = store.lock_commit_epoch(&claim).await? else {
                            return Ok(SettleOutcome::Fenced);
                        };
                        if !guard.permits_settlement() {
                            guard.release().await.map_err(|error| {
                                Error::Dispatch(crate::DispatchError::Unavailable(error))
                            })?;
                            return Ok(SettleOutcome::Fenced);
                        }
                        Some(guard)
                    };
                    // Do not hold Dispatch's lock across a Session application
                    // callback: report/interrupt handling may re-enter Dispatch.
                    // This is the same release boundary as the remote coordinator.
                    // The callback is exact/idempotent and final queue CAS still
                    // rejects a superseding epoch; logical Ended cannot restart.
                    if let Some(guard) = guard {
                        guard.release().await.map_err(|error| {
                            Error::Dispatch(crate::DispatchError::Unavailable(error))
                        })?;
                    }
                    let observed = observer
                        .before_settle(
                            &claimed.request,
                            &claim,
                            &committed,
                            claimed.cancellation_requested,
                        )
                        .await;
                    observed
                        .map_err(|error| Error::SettlementObservationPending(error.to_string()))?;
                    coordinated_committed_state = Some(committed);
                    false
                }
            } else if claimed.request.session_activity_epoch.is_some()
                && outcome != DispatchOutcome::Awaiting
            {
                return Err(Error::Dispatch(crate::DispatchError::Rejected(
                    "coordinated child settlement has no committed boundary".to_string(),
                )));
            } else {
                false
            };
            let label = match outcome {
                DispatchOutcome::Done => "done",
                DispatchOutcome::Awaiting => "awaiting",
            };
            let started = awaken_time::mono::Stopwatch::start();
            let settlement = if coordinated_claim_is_stale {
                Ok(SettleOutcome::Fenced)
            } else {
                self.store
                    .settle(run_id, claimed.lease.epoch(), outcome, consumed)
                    .await
            };
            match settlement {
                Ok(result) => {
                    self.record_settlement_outcome(result, label, started.elapsed());
                    if result == SettleOutcome::Applied
                        && let Some(committed) = coordinated_committed_state.as_ref()
                    {
                        self.settlement_observer
                            .as_ref()
                            .expect("coordinated settlement installed its observer")
                            .after_settle(
                                &claimed.request,
                                &claim,
                                committed,
                                claimed.cancellation_requested,
                            )
                            .await
                            .map_err(|error| {
                                Error::Dispatch(crate::DispatchError::Rejected(error.to_string()))
                            })?;
                    }
                    Ok(result)
                }
                Err(error) => {
                    self.runtime
                        .metrics()
                        .record_dispatch_commit("error", started.elapsed());
                    Err(error.into())
                }
            }
        })
    }

    /// Record one authoritative settlement decision. Queue CAS fencing and the
    /// coordinated pre-observer ownership fence both converge here, preventing
    /// transport/topology-specific metric paths from drifting or double-counting.
    fn record_settlement_outcome(
        &self,
        result: SettleOutcome,
        applied_label: &str,
        duration: std::time::Duration,
    ) {
        let commit_outcome = match result {
            SettleOutcome::Applied => {
                self.runtime
                    .metrics()
                    .record_dispatch_settled(applied_label);
                "applied"
            }
            SettleOutcome::Fenced => {
                self.runtime.metrics().record_dispatch_fenced();
                "fenced"
            }
        };
        self.runtime
            .metrics()
            .record_dispatch_commit(commit_outcome, duration);
    }
}
