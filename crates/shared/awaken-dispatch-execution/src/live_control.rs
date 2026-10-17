// @formal surface obligation=run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal surface obligation=live_inbox.attempt_scope_is_local_current_capability_gated_and_never_carried_over
// @formal reason: Static owner: the single protocol-neutral live-control adapter over canonical dispatch
// @formal reason: and current-attempt authority. Dynamic path: cancel first records intent through the
// @formal reason: Dispatch store; for a matching cancellable dispatch it then signals the live Runtime
// @formal reason: when present and drives that same durable row through DispatchWorker. A live-delivery or
// @formal reason: drive failure is surfaced while the committed intent remains durable, and a terminal or
// @formal reason: unknown store result falls back only to inline live delivery. Pause and wake target only
// @formal reason: the current attempt and map NotActive to NoSubscriber. It adds no second inbox, Run
// @formal reason: state, or durable control store.
//! Fail-closed live run control: cancel, pause and wake by correlation-id (G5/G18).
//!
//! [`LiveRunControlService`] is the protocol-neutral `LiveRunControl` seam and
//! owns no Run truth or second commit mechanism. Cancellation first records intent
//! through the authoritative Dispatch store. For a matching cancellable dispatch,
//! it then signals the live Runtime when present and drives that same durable row
//! through [`DispatchWorker`]; a live-delivery or drive failure is surfaced while
//! the committed intent remains durable. When the store reports a terminal or
//! unknown run, cancellation falls back only to inline live delivery. Pause and
//! wake target the current attempt only and return [`Error::NoSubscriber`] when no
//! subscriber accepts them.

use awaken_dispatch_erasure::{AnyCoordinatorDispatch, AnyDispatchStore, Dispatch};
use awaken_dispatch_contract::DispatchAdministration;
use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};

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
    administration: Arc<dyn DispatchAdministration>,
    /// The control edge's clock for the cancellation drive it performs. Claim,
    /// renewal, ownership verification and settlement are judged by the store
    /// that owns the row; this clock times only what the attempt itself reads.
    clock: Arc<dyn awaken_time::WallClock>,
}

impl<S: Dispatch + 'static> LiveRunControlService<S> {
    /// Build from a dispatch worker. The worker carries both the runtime
    /// live-control handle and the durable dispatch store, keeping this service
    /// free of a second commit boundary (G6/G13). `clock` is the composing
    /// Host's clock.
    pub fn new(worker: Arc<DispatchWorker<S>>, clock: Arc<dyn awaken_time::WallClock>) -> Self
    where
        S: DispatchAdministration,
    {
        let administration = worker.store().clone();
        Self {
            worker,
            clock,
            administration,
        }
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
            .administration
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
                .tick_run(&run_id, self.clock.clone())
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
        LiveAttemptControl::new(self.worker.runtime().clone())
            .pause(correlation_id)
            .await
    }

    /// Deliver a `PendingBoundaryWake` nudge to the run identified by
    /// `correlation_id`.
    ///
    /// Wake is live-only: there is no durable fallback. Returns
    /// [`Error::NoSubscriber`] when no live subscriber accepts the command,
    /// rather than silently succeeding (G5: durable-only absent on direct
    /// ingress means fail-closed, not silent-drop).
    pub async fn wake(&self, correlation_id: &str) -> Result<(), Error> {
        LiveAttemptControl::new(self.worker.runtime().clone())
            .wake(correlation_id)
            .await
    }
}

impl LiveRunControlService<AnyDispatchStore> {
    pub fn from_local_owner(
        worker: Arc<DispatchWorker<AnyDispatchStore>>,
        owner: Arc<AnyCoordinatorDispatch>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, Error> {
        if !Arc::ptr_eq(worker.store(), &owner.worker_view()) {
            return Err(Error::Dispatch(
                "live control must retain the Worker's exact bound owner".into(),
            ));
        }
        Ok(Self {
            worker,
            administration: owner,
            clock,
        })
    }
}

/// Control only the current local attempt; no durable queue mutation authority is required.
pub struct LiveAttemptControl {
    runtime: Arc<awaken_runtime::Runtime>,
}
impl LiveAttemptControl {
    pub fn new(runtime: Arc<awaken_runtime::Runtime>) -> Self {
        Self { runtime }
    }
    /// Select the original controls of one observed foreground. `foreground`
    /// is a value the caller read under its own lock; this call takes no
    /// Session lock and performs no await, so a caller may run it inside that
    /// same critical section. The Runtime registration is captured now: a
    /// later registration for the same Run cannot be signalled through it.
    pub fn select_interruption(
        self,
        foreground: ForegroundRun,
        installed_claim: Option<RunId>,
    ) -> SelectedAttemptInterruption {
        let registered_cancellation = foreground
            .run
            .as_ref()
            .or(installed_claim.as_ref())
            .and_then(|run| self.runtime.registered_attempt_cancellation(run));
        SelectedAttemptInterruption {
            foreground,
            registered_cancellation,
        }
    }

    pub async fn wake(&self, correlation_id: &str) -> Result<(), Error> {
        let run_id = RunId(correlation_id.to_owned());
        match self
            .runtime
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
    pub async fn pause(&self, correlation_id: &str) -> Result<(), Error> {
        let run_id = RunId(correlation_id.to_owned());
        match self
            .runtime
            .deliver_to_current_attempt(LiveCommand::Pause { run_id })
            .await
        {
            Ok(()) => Ok(()),
            Err(ControlError::NotActive) => Err(Error::NoSubscriber(correlation_id.to_owned())),
            Err(error) => Err(Error::Dispatch(error.to_string())),
        }
    }
}

/// One observation of the foreground Run a Session drives and the signal that
/// interrupts it. Both fields come from the same write of the observer's
/// record, so the signal always belongs to the attempt that installed it: to
/// `run` when present, or to an auxiliary attempt that has no Run of this
/// Session. Holding the value grants no lock and is never re-read.
#[derive(Clone, Debug, Default)]
pub struct ForegroundRun {
    pub run: Option<RunId>,
    pub cancellation: Option<tokio_util::sync::CancellationToken>,
}

/// Immutable local controls selected for one original attempt. It carries no
/// Dispatch administration or durable cancellation authority, and cannot
/// reselect another attempt when its operation is delivered after an await.
pub struct SelectedAttemptInterruption {
    foreground: ForegroundRun,
    registered_cancellation: Option<tokio_util::sync::CancellationToken>,
}

impl SelectedAttemptInterruption {
    /// Original foreground identity; absent for a claim-only attempt.
    pub fn active_run(&self) -> Option<&RunId> {
        self.foreground.run.as_ref()
    }

    /// Signal only the captured original registration and direct/ACP tokens.
    /// A signal is not confirmation that a process or external effect stopped.
    pub fn interrupt(&self) {
        if let Some(token) = &self.registered_cancellation {
            token.cancel();
        }
        if let Some(token) = &self.foreground.cancellation {
            token.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_dispatch_contract::DispatchClaims;
    use awaken_dispatch_contract::DispatchEnqueue;
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

    use crate::RunDispatch;
    use crate::worker::DispatchWorker;
    use awaken_dispatch::MemoryDispatchStore;

    fn make_service() -> LiveRunControlService<MemoryDispatchStore> {
        let runtime = Arc::new(Runtime::new());
        let store = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let worker = Arc::new(DispatchWorker::new(
            runtime,
            store,
            commit,
            "live-control-test",
        ));
        LiveRunControlService::new(worker, awaken_time::system_clock())
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

    #[test]
    fn selected_local_interruption_cannot_cancel_a_replacement_registration_for_the_same_run() {
        selected_interruption_preserves_registration_successor(true, true);
    }

    #[test]
    fn selected_claim_interruption_keeps_its_original_registered_cancellation() {
        selected_interruption_preserves_registration_successor(true, false);
    }

    #[test]
    fn absent_registered_interruption_cannot_cancel_a_later_registration() {
        selected_interruption_preserves_registration_successor(false, false);
    }

    fn selected_interruption_preserves_registration_successor(
        originally_registered: bool,
        direct_token: bool,
    ) {
        use awaken_agent_contract::agent::thread::Id as ThreadId;
        use awaken_runtime_contract::execution::LiveInput;
        use tokio_util::sync::CancellationToken;

        // C1 real Runtime registration A or absence; C2 capture original local
        // controls; C3 end A and register B for the very same Run ID; C4 signal
        // C2. E1 B stays live; E2 an existing A receives its captured signal.
        // R1 direct + A => E1+E2; R2 claim-only + A => E1+E2;
        // R3 claim-only + absent A => E1. No registry, epoch or token is assigned
        // through private test fields: both scopes use Runtime's public SDK API.
        let runtime = Arc::new(Runtime::new());
        let run = RunId("selected-local-attempt".into());
        let thread = ThreadId("selected-local-attempt-thread".into());
        let original = CancellationToken::new();
        let first = originally_registered.then(|| {
            runtime.begin_active_attempt(
                &run,
                &thread,
                RuntimeRunContext::new().with_cancellation(original.clone()),
                LiveInput::None,
            )
        });
        let foreground = ForegroundRun {
            run: direct_token.then(|| run.clone()),
            cancellation: direct_token.then(|| original.clone()),
        };
        let selected = LiveAttemptControl::new(runtime.clone())
            .select_interruption(foreground, Some(run.clone()));
        drop(first);
        let successor = CancellationToken::new();
        let _second = runtime.begin_active_attempt(
            &run,
            &thread,
            RuntimeRunContext::new().with_cancellation(successor.clone()),
            LiveInput::None,
        );
        assert!(!original.is_cancelled() && !successor.is_cancelled());
        selected.interrupt();
        assert!(
            !successor.is_cancelled(),
            "selected local interruption reselected the successor registration for the same Run ID"
        );
        assert_eq!(original.is_cancelled(), originally_registered);
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
        let store = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let worker = Arc::new(DispatchWorker::new(
            runtime,
            store.clone(),
            commit.clone(),
            "live-control-test",
        ));
        let service = LiveRunControlService::new(worker, awaken_time::system_clock());
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
        let store = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
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
        let service = Arc::new(LiveRunControlService::new(
            worker,
            awaken_time::system_clock(),
        ));
        store
            .enqueue(RunDispatch::new(activation("wall-clock-cancel")))
            .await
            .unwrap();

        let cancelling = {
            let service = service.clone();
            tokio::spawn(async move { service.cancel("wall-clock-cancel").await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), entered.notified())
            .await
            .expect("timed out waiting for entered.notified()");
        assert!(
            store
                .claim("competing-pool", 30_000, &Default::default())
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
        let store = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let worker = Arc::new(DispatchWorker::new(
            runtime.clone(),
            store,
            commit,
            "pause-control-test",
        ));
        let service = LiveRunControlService::new(worker, awaken_time::system_clock());
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
