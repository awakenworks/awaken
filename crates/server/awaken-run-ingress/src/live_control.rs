//! Fail-closed live run control: cancel and wake by correlation-id (G5/G18).
//!
//! [`LiveRunControlService`] is the `LiveRunControl` seam described in G18:
//! it owns active-run steering and nothing else. It never starts runs, publishes
//! config, or owns a second commit mechanism. Cancellation tries the runtime live
//! channel first (for in-flight runs), then the dispatch store for queued or
//! awaiting runs. Wake is live-only and fail-closed: if no live subscriber accepts
//! the command, [`Error::NoSubscriber`] is returned rather than silently
//! succeeding — callers on direct ingress that receive this error must treat the
//! operation as undelivered (G5: durable-only operations fail closed on direct
//! ingress).

use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};

use crate::dispatch::Dispatch;
use crate::worker::DispatchWorker;

/// Errors produced by [`LiveRunControlService`] operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// No live or durable run matched the supplied correlation identifier.
    #[error("run not found: {0}")]
    NotFound(String),
    /// The run exists but has no live subscriber to accept the wake command.
    /// Wake is live-only; durable-only callers must treat this as a hard failure.
    #[error("no live subscriber for run: {0}")]
    NoSubscriber(String),
    /// A storage or runtime error prevented the operation from completing.
    #[error("dispatch error: {0}")]
    Dispatch(String),
}

/// Fail-closed live-control service (G18).
///
/// Routes `Cancel` and `Wake` to the run identified by a correlation identifier
/// (treated as a `RunId` in this MVP). Cancellation is fail-closed: if no live
/// or queued run matches, [`Error::NotFound`] is returned. Wake is live-only and
/// fail-closed: [`Error::NoSubscriber`] is returned when no active run exists,
/// rather than silently succeeding (G5).
pub struct LiveRunControlService<S> {
    worker: Arc<DispatchWorker<S>>,
}

impl<S: Dispatch + 'static> LiveRunControlService<S> {
    /// Build from a dispatch worker. The worker carries both the runtime
    /// live-control handle and the durable dispatch store, keeping this service
    /// free of a second commit boundary (G6/G13).
    pub fn new(worker: Arc<DispatchWorker<S>>) -> Self {
        Self { worker }
    }

    /// Cancel the run identified by `correlation_id`.
    ///
    /// Resolution order:
    /// 1. Persist cancellation intent for a durable queued/awaiting/running run.
    /// 2. Signal a live attempt when present; otherwise claim the intent now.
    /// 3. Fall back to live-only delivery for an inline run with no dispatch row.
    ///
    /// Returns [`Error::NotFound`] when no matching run exists in either path.
    pub async fn cancel(&self, correlation_id: &str) -> Result<(), Error> {
        let run_id = RunId(correlation_id.to_owned());
        let thread_id = self
            .worker
            .store()
            .cancel(&run_id)
            .await
            .map_err(|e| Error::Dispatch(e.to_string()))?;
        if thread_id.is_some() {
            let live = match self.worker.runtime().deliver(LiveCommand::Cancel {
                run_id: run_id.clone(),
            }) {
                Ok(()) => true,
                Err(ControlError::NotActive) => false,
                Err(error) => return Err(Error::Dispatch(error.to_string())),
            };
            // Persisting a running cancel revoked its old epoch, so claim the
            // cancellation now. `None` means another pool worker won the claim;
            // the intent remains durable on that owner's lease.
            let driven = self
                .worker
                .tick_run(&run_id, 0)
                .await
                .map_err(|error| Error::Dispatch(error.to_string()))?;
            return match driven {
                Some((
                    _,
                    awaken_agent_contract::agent::run::RunState::Ended(
                        awaken_agent_contract::agent::run::EndCause::Cancelled,
                    ),
                ))
                | None => Ok(()),
                // A live owner may have completed concurrently after accepting the
                // signal. The signal was delivered, so cancellation was not lost.
                Some((_, _)) if live => Ok(()),
                Some((_, _)) => Err(Error::NotFound(correlation_id.to_owned())),
            };
        }

        match self
            .worker
            .runtime()
            .deliver(LiveCommand::Cancel { run_id })
        {
            Ok(()) => Ok(()),
            Err(ControlError::NotActive) => Err(Error::NotFound(correlation_id.to_owned())),
            Err(error) => Err(Error::Dispatch(error.to_string())),
        }
    }

    /// Deliver a `PendingBoundaryWake` nudge to the run identified by
    /// `correlation_id`.
    ///
    /// Wake is live-only: there is no durable fallback. Returns
    /// [`Error::NoSubscriber`] when no live subscriber accepts the command,
    /// rather than silently succeeding (G5: durable-only absent on direct
    /// ingress means fail-closed, not silent-drop).
    pub fn wake(&self, correlation_id: &str) -> Result<(), Error> {
        let run_id = RunId(correlation_id.to_owned());
        match self.worker.runtime().deliver(LiveCommand::Wake {
            run_id,
            reason: "live-wake".to_owned(),
        }) {
            Ok(()) => Ok(()),
            Err(ControlError::NotActive) => Err(Error::NoSubscriber(correlation_id.to_owned())),
            Err(e) => Err(Error::Dispatch(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use awaken_runtime::Runtime;
    use awaken_runtime::memory::MemoryCommitCoordinator;
    use awaken_runtime_contract::activation::RunActivation;
    use awaken_runtime_contract::resolved::ModelBinding;
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

    use crate::worker::DispatchWorker;
    use crate::{DispatchQueue, MemoryDispatchStore, RunDispatch};

    fn make_service() -> LiveRunControlService<MemoryDispatchStore> {
        let runtime = Arc::new(Runtime::new());
        let store = Arc::new(MemoryDispatchStore::new());
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let worker = Arc::new(DispatchWorker::new(
            runtime,
            store,
            commit,
            "live-control-test",
        ));
        LiveRunControlService::new(worker)
    }

    fn activation(run: &str) -> RunActivation {
        RunActivation::new(
            RunId(run.into()),
            awaken_agent_contract::agent::thread::Id("cancel-thread".into()),
            ExecutableAgentSnapshot::builder("cancel-snapshot")
                .model(ModelBinding::new("provider", "model", "backend"))
                .build(),
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn cancel_is_fail_closed_for_unknown_correlation_id() {
        let svc = make_service();
        let err = svc.cancel("nonexistent-id").await.unwrap_err();
        assert!(
            matches!(err, Error::NotFound(_)),
            "cancel must be fail-closed for unknown correlation id: got {err}"
        );
    }

    #[tokio::test]
    async fn queued_cancel_persists_then_commits_through_the_worker() {
        use awaken_agent_contract::agent::run::{EndCause, RunState};
        use awaken_agent_contract::thread::read::run_store::RunStore;

        let runtime = Arc::new(Runtime::new());
        let store = Arc::new(MemoryDispatchStore::new());
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let worker = Arc::new(DispatchWorker::new(
            runtime,
            store.clone(),
            commit.clone(),
            "live-control-test",
        ));
        let service = LiveRunControlService::new(worker);
        store
            .enqueue(RunDispatch::new(activation("queued-cancel")))
            .await
            .unwrap();

        service.cancel("queued-cancel").await.expect("cancel");

        assert_eq!(store.dispatch_count(), 0);
        assert_eq!(
            RunStore::get(commit.as_ref(), &RunId("queued-cancel".into()))
                .expect("terminal record")
                .state,
            RunState::Ended(EndCause::Cancelled)
        );
    }

    #[test]
    fn wake_is_fail_closed_when_no_live_subscriber() {
        let svc = make_service();
        let err = svc.wake("nonexistent-id").unwrap_err();
        assert!(
            matches!(err, Error::NoSubscriber(_)),
            "wake must be fail-closed when no live subscriber: got {err}"
        );
    }
}
