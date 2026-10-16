// @formal surface obligation=session_execution.terminal_state_never_reopens
// @formal surface obligation=checkpoint.terminal_completion_clears_resume
// @formal reason: Dispatch-side terminal reconciliation from committed Run truth: the coordinator's
// @formal reason: maintenance loop, the Environment-free boundary worker and Worker evidence retention.
//! Environment-free terminal dispatch control and reconciliation. Cancellation
//! and committed-terminal repair share one boundary worker (dispatch fence and
//! Thread commit authority, never Session environment provisioning).

use crate::DEFAULT_LEASE_MS;
use crate::session_execution::execution_error;
use crate::session_execution::{RunWorkerFactory, SessionDispatch, TerminalRecovery};
use crate::session_worker_resolver::SessionWorkerResolver;
use crate::{DispatchWorker, Error};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_dispatch_erasure::AnyDispatchStore;
use awaken_session_realization::worker_projection::SessionProjectionExecution;
use std::sync::Arc;

/// The coordinator-only maintenance loop and the terminal scan it drives.
/// Consumer: this module and the dispatch capability, which owns the value.
#[derive(Default)]
pub struct TerminalReconciliationState {
    /// The run-ingress-owned maintenance loop used by a coordinator-only Host.
    /// Local pools already own the same canonical maintenance mechanism.
    dispatch_reconciler: std::sync::OnceLock<crate::DispatchReconciler>,
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
    ) -> Result<(), awaken_session_realization::host_error::HostError> {
        self.worker_evidence.set((directory, source)).map_err(|_| {
            awaken_session_realization::host_error::HostError::internal(
                "Worker evidence reconciliation already bound",
            )
        })
    }
    pub fn reconciler_started(&self) -> bool {
        self.dispatch_reconciler.get().is_some()
    }
    pub fn install_reconciler(&self, reconciler: crate::DispatchReconciler) -> bool {
        self.dispatch_reconciler.set(reconciler).is_ok()
    }
}

pub async fn reconcile_committed_terminals<T>(
    resolver: &SessionWorkerResolver<T>,
    limit: usize,
) -> Result<Vec<(RunId, RunState)>, crate::Error>
where
    T: SessionDispatch + RunWorkerFactory + SessionProjectionExecution + TerminalRecovery,
{
    let host = resolver.host()?;
    // A database-less Worker delegates committed truth upstream and has no
    // process-local registry from which to discover every Thread reader.
    if !host.scans_committed_terminals() || limit == 0 {
        return Ok(Vec::new());
    }

    let store = TerminalRecovery::coordinator_dispatch(host.as_ref())
        .ok_or_else(|| execution_error("terminal scan requires a Coordinator dispatch owner"))?;
    host.terminal_reconciliation()
        .terminal_scan
        .reconcile(store.as_ref(), limit, |row| {
            let host = &host;
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
                let commit = host.open_commit(commit_thread).await?;
                // Temporary recovery holds this Host only during one candidate;
                // the selected neutral adapter stores no Host/recovery back-edge.
                let recovery = host.worker_recovery_source();
                let attempt_recovery = crate::PhysicalAttemptRecoveryRead::new(recovery.as_ref());
                let ready = match host.terminal_reconciliation().worker_evidence.get() {
                    Some((directory, terminal)) => tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        terminal.reconcile_terminal_attempt(
                            store.as_ref(),
                            directory.as_ref(),
                            host.terminal_view(commit.as_ref()),
                            &row,
                            Some(&attempt_recovery),
                            awaken_time::WallClockExt::now_ms(host.realizer().clock.as_ref()),
                        ),
                    )
                    .await
                    .map_err(|_| execution_error("terminal physical closure timed out"))?
                    .map_err(|error| execution_error(error.to_string()))?,
                    // A missing trusted role never observes/stops an occupied
                    // domain. Preserve only original canonical empty recovery;
                    // the atomic Store claim still excludes a concurrent slot.
                    None => {
                        crate::committed_terminal_ready(host.terminal_view(commit.as_ref()), &row)
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
                        host.dispatch_owner(),
                        DEFAULT_LEASE_MS,
                    )
                    .await?
                else {
                    return Ok(None);
                };

                let worker = match host.boundary_worker(&claimed, commit, None, false).await {
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

use awaken_dispatch_contract::{DispatchControlClaims as _, DispatchSettlement as _};

pub async fn reconcile_worker_evidence<T>(host: &T) -> Result<(), crate::Error>
where
    T: SessionDispatch + RunWorkerFactory + SessionProjectionExecution + TerminalRecovery,
{
    let Some((directory, source)) = host.terminal_reconciliation().worker_evidence.get() else {
        return Ok(());
    };
    let dispatch = TerminalRecovery::coordinator_dispatch(host).ok_or_else(|| {
        execution_error("Worker evidence reconciliation requires a Coordinator owner")
    })?;
    let now = awaken_time::WallClockExt::now_ms(host.realizer().clock.as_ref());
    let mut records = directory
        .list()
        .await
        .map_err(|e| execution_error(e.to_string()))?;
    let Ok(mut after) = host
        .terminal_reconciliation()
        .worker_evidence_after
        .try_lock()
    else {
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

impl<T> SessionWorkerResolver<T>
where
    T: SessionDispatch + RunWorkerFactory + SessionProjectionExecution + TerminalRecovery,
{
    pub async fn cancellation_worker(
        &self,
        host: &T,
        claimed: &awaken_dispatch_contract::Claimed,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error> {
        let commit = host
            .open_commit(claimed.request.session_thread_id())
            .await?;
        Ok(Arc::new(
            host.boundary_worker(claimed, commit, None, true).await?,
        ))
    }
}
