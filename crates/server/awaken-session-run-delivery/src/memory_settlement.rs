// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: The existing pre-settlement adapter moves beside committed reads. Dispatch keeps
// @formal reason: claim fencing; the SDK keeps terminal delivery and Memory intent idempotency.
//! Observe committed Memory terminal facts before the existing Dispatch settlement.

use std::future::Future;
use std::sync::Arc;

use awaken_agent_contract::agent::run::RunState;
use awaken_dispatch_contract::{DispatchSettlementError, RunDispatch};
use awaken_runtime_contract::terminal::{
    CommittedTerminalRun, RunTerminalObserver, deliver_committed_terminal,
};
use awaken_worker_contract::RunClaim;

use crate::{CommittedReads, SessionCommit};

/// The common pre-settlement sequence for embedded and Worker-dispatched Memory.
/// The caller supplies only its frozen-publication observer selection. This
/// function neither settles Dispatch nor executes extraction: observer delivery
/// must first persist the existing intent, and a failure leaves the guarded row
/// available to the original settlement retry.
pub async fn observe_dispatched_memory_terminal<F, O>(
    reads: &CommittedReads,
    dispatch: &RunDispatch,
    claim: &RunClaim,
    committed_state: &RunState,
    observer_for_commit: F,
) -> Result<(), DispatchSettlementError>
where
    F: FnOnce(Arc<SessionCommit>) -> O + Send,
    O: Future<Output = Result<Option<Arc<dyn RunTerminalObserver>>, DispatchSettlementError>>
        + Send,
{
    if dispatch.run_id() != &claim.run_id {
        return Err(DispatchSettlementError(
            "Memory settlement dispatch does not match the guarded Run claim".into(),
        ));
    }
    let cause = match committed_state {
        RunState::Ended(cause) => cause.clone(),
        RunState::Awaiting => return Ok(()),
        RunState::Running => {
            return Err(DispatchSettlementError(
                "Memory settlement requires committed Awaiting or Ended truth".into(),
            ));
        }
    };
    let commit = reads
        .commit_for_read(&dispatch.session_thread_id().0)
        .await
        .map_err(|error| DispatchSettlementError(error.to_string()))?;
    let observers = observer_for_commit(commit)
        .await?
        .into_iter()
        .collect::<Vec<_>>();
    let terminal = CommittedTerminalRun {
        run_id: dispatch.run_id().clone(),
        thread_id: dispatch.thread_id().clone(),
        cause,
    };
    let failures = deliver_committed_terminal(&observers, &terminal).await;
    if failures.is_empty() {
        return Ok(());
    }
    Err(DispatchSettlementError(
        failures
            .into_iter()
            .map(|failure| format!("{}: {}", failure.observer_id, failure.error))
            .collect::<Vec<_>>()
            .join("; "),
    ))
}

#[cfg(test)]
mod tests;
