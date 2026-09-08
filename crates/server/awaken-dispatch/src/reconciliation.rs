//! Bounded discovery shared by Worker and Host terminal reconciliation.
//! The cursor schedules reads only; Dispatch and ThreadCommit retain authority.

use std::future::Future;

use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use tokio::sync::Mutex;

use crate::{DispatchQueue, DispatchState, DispatchSummary, Error};

/// Process-local scan position, never a lease, receipt or durable recovery log.
#[derive(Default)]
pub struct TerminalReconciliationScan {
    after: Mutex<Option<RunId>>,
}

impl TerminalReconciliationScan {
    /// Inspect at most `min(limit, 256)` IDs, including nonterminal, disappeared
    /// and unreadable rows. A concurrent scan yields rather than queues. Each
    /// candidate must still be verified and settled through its current claim.
    pub async fn reconcile<S, F, Fut>(
        &self,
        store: &S,
        limit: usize,
        mut reconcile: F,
    ) -> Result<Vec<(RunId, RunState)>, Error>
    where
        S: DispatchQueue + ?Sized,
        F: FnMut(DispatchSummary) -> Fut,
        Fut: Future<Output = Result<Option<(RunId, RunState)>, Error>>,
    {
        let budget = limit.min(256);
        if budget == 0 {
            return Ok(Vec::new());
        }
        let Ok(mut after) = self.after.try_lock() else {
            return Ok(Vec::new());
        };
        let ids = store.scan_dispatch_ids(after.as_ref(), budget).await?;
        let at_end = ids.len() < budget;
        let mut completed = Vec::new();
        for run_id in ids {
            // Advance before per-row I/O: a failed/cancelled observation may be
            // retried after wrap, but cannot pin every later row behind it.
            *after = Some(run_id.clone());
            let result = async {
                let Some(row) = store.dispatch_summary(&run_id).await? else {
                    return Ok(None);
                };
                if !matches!(
                    row.state,
                    DispatchState::Pending | DispatchState::Awaiting | DispatchState::Leased
                ) {
                    return Ok(None);
                }
                reconcile(row).await
            }
            .await;
            match result {
                Ok(Some(terminal)) => completed.push(terminal),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(run_id = %run_id.0, %error, "terminal candidate reconciliation failed; retained for a later scan")
                }
            }
        }
        if at_end {
            *after = None;
        }
        Ok(completed)
    }
}
