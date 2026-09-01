//! Fail-closed live run control: cancel, pause and wake by correlation-id (G5/G18).
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

use crate::clock::SystemClock;
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
/// Routes `Cancel`, `Pause` and `Wake` to the run identified by a correlation identifier
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
                // The control edge owns the clock for this exact drive. Claim,
                // renewal, ownership verification, and settlement all retain this
                // source instead of pairing a timestamp with a private Worker clock.
                .tick_run(&run_id, Arc::new(SystemClock))
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

    /// Cooperatively pause the active run identified by `correlation_id` at its
    /// next safe boundary. Pause is live-only: once accepted, the executor commits
    /// a durable `ManualPause` ticket that may be resumed after process replacement.
    pub async fn pause(&self, correlation_id: &str) -> Result<(), Error> {
        let run_id = RunId(correlation_id.to_owned());
        match self
            .worker
            .runtime()
            .deliver_to_current_attempt(LiveCommand::Pause { run_id })
            .await
        {
            Ok(()) => Ok(()),
            Err(ControlError::NotActive) => Err(Error::NoSubscriber(correlation_id.to_owned())),
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
    pub async fn wake(&self, correlation_id: &str) -> Result<(), Error> {
        let run_id = RunId(correlation_id.to_owned());
        match self
            .worker
            .runtime()
            .deliver_to_current_attempt(LiveCommand::Wake {
                run_id,
                reason: "live-wake".to_owned(),
            })
            .await
        {
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
    use awaken_runtime_contract::activation::RunActivation;
    use awaken_runtime_contract::execution::{
        Result as ExecutionResult, RunAttemptExecutor, RunExecutor,
    };
    use awaken_runtime_contract::resolved::ModelBinding;
    use awaken_runtime_contract::resume::ResumeCommand;
    use awaken_runtime_contract::runtime_context::RuntimeRunContext;
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
    use awaken_store_inmem::MemoryCommitCoordinator;
    use tokio::sync::Notify;

    use crate::clock::Clock;
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

    struct BlockingCancel {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl RunExecutor for BlockingCancel {
        async fn execute(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<awaken_agent_contract::agent::run::RunState> {
            unreachable!("cancellation test never executes the run")
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for BlockingCancel {
        async fn resume(
            &self,
            _activation: RunActivation,
            _command: ResumeCommand,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<awaken_agent_contract::agent::run::RunState> {
            unreachable!("cancellation test never resumes the run")
        }

        async fn cancel(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<()> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(())
        }
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
        use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;

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
            CommittedThreadView::run(commit.as_ref(), &RunId("queued-cancel".into()))
                .expect("terminal record")
                .state,
            RunState::Ended(EndCause::Cancelled)
        );
    }

    #[tokio::test]
    async fn cancellation_claim_uses_wall_time_and_cannot_be_immediately_reclaimed() {
        let runtime = Arc::new(Runtime::new());
        let store = Arc::new(MemoryDispatchStore::new());
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let worker = Arc::new(DispatchWorker::new(
            runtime,
            store.clone(),
            commit,
            "cancel-owner",
        ));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        worker.install_attempt_executor(Arc::new(BlockingCancel {
            entered: entered.clone(),
            release: release.clone(),
        }));
        let service = Arc::new(LiveRunControlService::new(worker));
        store
            .enqueue(RunDispatch::new(activation("wall-clock-cancel")))
            .await
            .unwrap();

        let cancelling = {
            let service = service.clone();
            tokio::spawn(async move { service.cancel("wall-clock-cancel").await })
        };
        entered.notified().await;
        assert!(
            store
                .claim(
                    "competing-pool",
                    30_000,
                    SystemClock.now_ms(),
                    &Default::default(),
                )
                .await
                .unwrap()
                .is_none(),
            "the cancellation lease must not look expired to a wall-clock pool"
        );
        release.notify_one();
        cancelling.await.unwrap().expect("cancellation settles");
    }

    #[tokio::test]
    async fn wake_is_fail_closed_when_no_live_subscriber() {
        // Causes: C1 no attempt registration exists for the requested Run.
        // Effects: E1 the live-only service returns NoSubscriber. Constraint/
        // Invariant: a wake may not create a queue, store row, or side effect.
        // Decision rule W1: C1 -> E1 with zero durable fallback.
        let svc = make_service();
        let err = svc.wake("nonexistent-id").await.unwrap_err();
        assert!(
            matches!(err, Error::NoSubscriber(_)),
            "wake must be fail-closed when no live subscriber: got {err}"
        );
    }

    #[tokio::test]
    async fn pause_reaches_the_single_runtime_attempt_registry_and_fails_closed_after_attempt() {
        use awaken_runtime_contract::pause::PauseSignal;

        // Cause/effect graph: C1 the exact Run/Thread attempt is registered; C2
        // it carries a pause signal; C3 its exact tracking lifetime ends.
        // Effects: E1 C1+C2 requests that signal once; E2 C3 makes replay fail
        // closed as NoSubscriber. Constraint: the service and executor must use
        // the same Runtime registry, never an ingress-private pause map.
        // Decision rules: P1=C1+C2=>E1; P2=C3=>E2.
        let runtime = Arc::new(Runtime::new());
        let store = Arc::new(MemoryDispatchStore::new());
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let worker = Arc::new(DispatchWorker::new(
            runtime.clone(),
            store,
            commit,
            "pause-control-test",
        ));
        let service = LiveRunControlService::new(worker);
        let run_id = RunId("external-attempt".into());
        let thread_id = awaken_agent_contract::agent::thread::Id("external-thread".into());
        let pause = PauseSignal::new();
        let context = RuntimeRunContext::new().with_pause(pause.clone());
        let tracking = runtime.begin_active_attempt(
            &run_id,
            &thread_id,
            context,
            awaken_runtime_contract::execution::LiveInput::None,
        );

        service
            .pause(&run_id.0)
            .await
            .expect("active pause is accepted");
        assert!(
            pause.requested(),
            "the executor context observes the request"
        );

        drop(tracking);
        assert!(matches!(
            service.pause(&run_id.0).await,
            Err(Error::NoSubscriber(_))
        ));
    }
}
