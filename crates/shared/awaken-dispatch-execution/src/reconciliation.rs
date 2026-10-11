// @formal entrypoint remote_worker.terminal_scan_is_bounded_and_preserves_authority
//! Bounded discovery shared by Worker and Host terminal reconciliation.
//! The cursor schedules reads only; Dispatch and ThreadCommit retain authority.

use awaken_dispatch_contract::DispatchControlReads;
use std::future::Future;

use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use tokio::sync::Mutex;

use crate::{DispatchState, DispatchSummary, Error};

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
        S: DispatchControlReads + ?Sized,
        F: FnMut(DispatchSummary) -> Fut,
        Fut: Future<Output = Result<Option<(RunId, RunState)>, Error>>,
    {
        let budget = inspection_budget(limit);
        if budget == 0 {
            return Ok(Vec::new());
        }
        let Ok(mut after) = self.after.try_lock() else {
            return Ok(Vec::new());
        };
        let ids = store.scan_dispatch_ids(after.as_ref(), budget).await?;
        let mut completed = Vec::new();
        for run_id in TerminalScanPage::new(&ids, &mut after, budget) {
            let result = async {
                let Some(row) = store.dispatch_summary(run_id).await? else {
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
        Ok(completed)
    }
}

fn inspection_budget(limit: usize) -> usize {
    limit.min(256)
}

/// The one production page traversal. Only scheduling identity is mutable here;
/// the caller retains Dispatch/ThreadCommit authority. Keeping traversal over a
/// borrowed slice also makes its exact control flow verifiable independently of
/// adapter allocation, locking and asynchronous observation.
struct TerminalScanPage<'a, Id> {
    ids: &'a [Id],
    after: &'a mut Option<Id>,
    budget: usize,
    visited: usize,
}

impl<'a, Id> TerminalScanPage<'a, Id> {
    fn new(ids: &'a [Id], after: &'a mut Option<Id>, limit: usize) -> Self {
        Self {
            ids,
            after,
            budget: inspection_budget(limit),
            visited: 0,
        }
    }
}

impl<'a, Id: Clone> Iterator for TerminalScanPage<'a, Id> {
    type Item = &'a Id;

    fn next(&mut self) -> Option<&'a Id> {
        if self.visited < self.budget && self.visited < self.ids.len() {
            let run_id = &self.ids[self.visited];
            self.visited += 1;
            // Advance before the caller performs per-row I/O. Dropping a page
            // while its observer is pending must preserve this position.
            *self.after = Some(run_id.clone());
            Some(run_id)
        } else {
            // Wrap only after consuming a short/empty positive-budget page.
            // A full final page needs a subsequent empty scan. In particular,
            // neither cancellation (Drop) nor a zero budget resets the cursor.
            if self.budget > 0 && self.ids.len() < self.budget {
                *self.after = None;
            }
            None
        }
    }
}

#[cfg(any(test, kani))]
mod page_proofs;
