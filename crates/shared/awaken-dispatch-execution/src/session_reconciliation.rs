// @formal surface obligation=session_execution.terminal_state_never_reopens
// @formal surface obligation=checkpoint.terminal_completion_clears_resume
// @formal reason: Dispatch-side terminal reconciliation from committed Run truth: the coordinator's
// @formal reason: maintenance loop, the Environment-free boundary worker and Worker evidence retention.
//! Environment-free terminal dispatch control and reconciliation. Cancellation
//! and committed-terminal repair share one boundary worker (dispatch fence and
//! Thread commit authority, never Session environment provisioning).

use crate::DEFAULT_LEASE_MS;
use crate::session_execution::execution_error;
use crate::session_worker_resolver::SessionWorkerResolver;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use std::sync::Arc;

/// The terminal scan state the coordinator-only maintenance loop drives.
/// Consumers: the claimed-Run resolver, which scans with it, and the process
/// dispatch, which owns the value. The loop itself is owned by the process
/// dispatch, never by this state, so the resolver the loop holds forms no
/// cycle back to its task.
#[derive(Default)]
pub struct TerminalReconciliationState {
    terminal_scan: crate::TerminalReconciliationScan,
    worker_evidence: std::sync::OnceLock<(
        Arc<dyn awaken_dispatch_contract::WorkerDirectory>,
        Arc<dyn crate::WorkerTerminalReconciliation>,
    )>,
    worker_evidence_after: tokio::sync::Mutex<Option<String>>,
}

impl TerminalReconciliationState {
    pub fn bind_worker_evidence(
        &self,
        directory: Arc<dyn awaken_dispatch_contract::WorkerDirectory>,
        source: Arc<dyn crate::WorkerTerminalReconciliation>,
    ) -> Result<(), crate::AdmissionError> {
        self.worker_evidence
            .set((directory, source))
            .map_err(|_| crate::AdmissionError::AlreadyBound("Worker evidence reconciliation"))
    }
}

impl SessionWorkerResolver {
    /// Settle up to `limit` quiescent or expired dispatches whose committed Run
    /// truth is terminal. Only a construction holding the Coordinator's
    /// committed truth scans; a database-less Worker delegates it upstream.
    pub(crate) async fn reconcile_committed_terminal_runs(
        &self,
        limit: usize,
    ) -> Result<Vec<(RunId, RunState)>, crate::Error> {
        let Some(terminals) = self.terminals.as_ref() else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }
        let store = &terminals.dispatch;
        self.terminal_state
            .terminal_scan
            .reconcile(store.as_ref(), limit, |row| {
                let store = &store;
                async move {
                    // The dispatch summary already carries the canonical Session affinity.
                    // Read committed truth from that physical partition before taking the
                    // maintenance claim: claiming every Awaiting/expired row and restoring
                    // non-terminal ones to Awaiting would steal a recoverable lease from a
                    // registered replacement Worker and advance its fencing epoch forever.
                    // `settle_claimed_terminal` repeats this check after the claim, closing
                    // the read/claim race without introducing another recovery registry.
                    let commit_thread = row.session_thread_id.as_ref().unwrap_or(&row.thread_id);
                    let commit = self.commits.open_commit(commit_thread).await?;
                    // Temporary recovery is held only during one candidate;
                    // the selected neutral adapter stores no recovery back-edge.
                    let recovery = self.commits.recovery_source()?;
                    let attempt_recovery =
                        crate::PhysicalAttemptRecoveryRead::new(recovery.as_ref());
                    let ready = match self.terminal_state.worker_evidence.get() {
                        Some((directory, terminal)) => tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            terminal.reconcile_terminal_attempt(
                                store.as_ref(),
                                directory.as_ref(),
                                commit.committed(),
                                &row,
                                Some(&attempt_recovery),
                                awaken_time::WallClockExt::now_ms(self.clock.as_ref()),
                            ),
                        )
                        .await
                        .map_err(|_| execution_error("terminal physical closure timed out"))?
                        .map_err(|error| execution_error(error.to_string()))?,
                        // A missing trusted role never observes/stops an occupied
                        // domain. Preserve only original canonical empty recovery;
                        // the atomic Store claim still excludes a concurrent slot.
                        None => {
                            crate::committed_terminal_ready(commit.committed(), &row)
                                .map_err(|error| execution_error(error.to_string()))?
                                && !row.physical_attempt_active
                        }
                    };
                    if !ready {
                        return Ok(None);
                    }

                    let Some(claimed) = store
                        .claim_for_terminal_recovery(
                            &row.run_id,
                            &terminals.lease_owner,
                            DEFAULT_LEASE_MS,
                        )
                        .await?
                    else {
                        return Ok(None);
                    };

                    let worker = match commit.boundary_worker(&claimed, None).await {
                        Ok(worker) => worker,
                        Err(error) => {
                            let _ = store
                                .settle(
                                    claimed.lease.run_id(),
                                    claimed.lease.epoch(),
                                    awaken_dispatch_contract::DispatchOutcome::Awaiting,
                                    &[],
                                )
                                .await;
                            return Err(error);
                        }
                    };
                    worker.settle_claimed_terminal(claimed).await
                }
            })
            .await
    }

    /// Retry infrastructure retention of closed idle Workers, one candidate
    /// per maintenance tick. Without a bound Worker directory there is nothing
    /// to release; a bound directory requires the Coordinator dispatch owner.
    pub(crate) async fn release_closed_worker_evidence(&self) -> Result<(), crate::Error> {
        let Some((directory, source)) = self.terminal_state.worker_evidence.get() else {
            return Ok(());
        };
        let dispatch = self
            .terminals
            .as_ref()
            .map(|terminals| &terminals.dispatch)
            .ok_or_else(|| {
                execution_error("Worker evidence reconciliation requires a Coordinator owner")
            })?;
        let now = awaken_time::WallClockExt::now_ms(self.clock.as_ref());
        let mut records = directory
            .list()
            .await
            .map_err(|e| execution_error(e.to_string()))?;
        let Ok(mut after) = self.terminal_state.worker_evidence_after.try_lock() else {
            return Ok(());
        };
        records.sort_by(|a, b| {
            a.snapshot
                .identity
                .worker_id
                .cmp(&b.snapshot.identity.worker_id)
        });
        let start = after.as_ref().map_or(0, |cursor| {
            records.partition_point(|r| r.snapshot.identity.worker_id <= *cursor)
        });
        let start = if start == records.len() { 0 } else { start };
        // One candidate per original maintenance tick. Advance scheduling before
        // provider I/O so cancellation/failure cannot starve a later Worker.
        for record in records.into_iter().skip(start).take(1) {
            let identity = &record.snapshot.identity;
            *after = Some(identity.worker_id.clone());
            // Ingress closes the exact Registry incarnation before optional
            // provider ownership. This scan owns only cadence and cursor.
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                source.release_closed_idle_worker_evidence(
                    dispatch.as_ref(),
                    directory.as_ref(),
                    identity,
                    now,
                ),
            )
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "exact Worker retention retry pending"),
                Err(_) => tracing::warn!("exact Worker retention retry timed out"),
            }
        }
        Ok(())
    }
}

use awaken_dispatch_contract::{DispatchControlClaims as _, DispatchSettlement as _};
