use super::*;
use awaken_dispatch::RunDispatch;
use awaken_runtime_contract::execution::{
    Error as ExecutionError, Result as ExecutionResult, RunAttemptExecutor, RunExecutor,
};
use awaken_runtime_contract::resume::ResumeCommand;

/// An opaque Managed attempt: it blocks until its registered cancellation token
/// fires, records whether the durable intent was already visible, and returns
/// `Cancelled`. `hold_after_signal` keeps it inside its physical slot after the
/// signal until the test releases it. It keeps the default
/// `RequirePredecessorQuiescence` recovery, so a replacement claim can take over
/// only an acknowledged slot.
struct BlockingManagedAttempt {
    dispatch: Arc<awaken_dispatch_erasure::AnyDispatchStore>,
    executions: AtomicUsize,
    entered: tokio::sync::Notify,
    live_cancel_observed: tokio::sync::Notify,
    intent_preceded_signal: AtomicBool,
    hold_after_signal: bool,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl RunExecutor for BlockingManagedAttempt {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        let cancellation = context.cancellation.ok_or_else(|| {
            ExecutionError::Execution("Managed attempt has no cancellation token".into())
        })?;
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        cancellation.cancelled().await;
        let persisted = self
            .dispatch
            .list_dispatches()
            .await
            .map_err(|error| ExecutionError::Execution(error.to_string()))?
            .into_iter()
            .find(|row| row.run_id == activation.run_id)
            .is_some_and(|row| row.cancellation_requested);
        self.intent_preceded_signal
            .store(persisted, Ordering::SeqCst);
        self.live_cancel_observed.notify_one();
        if self.hold_after_signal {
            self.release.notified().await;
        }
        Ok(RunState::Ended(EndCause::Cancelled))
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for BlockingManagedAttempt {
    async fn resume(
        &self,
        _activation: RunActivation,
        _command: ResumeCommand,
        _context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        Err(ExecutionError::Execution(
            "fresh Managed interruption test must not resume".into(),
        ))
    }
}

struct InterruptedManagedRun {
    host: Arc<SharedHost>,
    dispatch: Arc<awaken_dispatch_erasure::AnyDispatchStore>,
    attempt: Arc<BlockingManagedAttempt>,
    thread: &'static str,
    run_id: RunId,
}

impl InterruptedManagedRun {
    /// The exact dispatch row, read from the durable authority.
    async fn row(&self) -> Option<awaken_dispatch::DispatchSummary> {
        self.dispatch
            .list_dispatches()
            .await
            .expect("inspect dispatch authority")
            .into_iter()
            .find(|row| row.run_id == self.run_id)
    }

    /// Settle on one fact of the exact durable row (absent = settled).
    async fn settle_row(
        &self,
        fact: impl Fn(Option<&awaken_dispatch::DispatchSummary>) -> bool,
        label: &str,
    ) {
        let settled = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if fact(self.row().await.as_ref()) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        if settled.is_err() {
            panic!("{label}: durable row is {:?}", self.row().await);
        }
    }

    /// Settle on the exact Run's committed terminal.
    async fn committed_terminal(&self, label: &str) -> RunState {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let committed = self
                    .host
                    .commit_for_read(self.thread)
                    .await
                    .expect("committed truth")
                    .run_state(&self.run_id);
                if let Some(state @ RunState::Ended(_)) = committed {
                    break state;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{label}: the Run committed no terminal"))
    }
}

/// Drive one Managed Run into its opaque attempt, suspend new claims, and
/// interrupt it. Returns once the live signal reached the attempt; the
/// replacement cancellation claim cannot be admitted until the caller resumes
/// pool admission.
async fn interrupt_with_admission_suspended(
    thread: &'static str,
    hold_after_signal: bool,
) -> InterruptedManagedRun {
    let run_id = RunId(format!("{thread}-run"));
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(
            awaken_lease_contract::system_clock(),
        )
        .expect("dispatch store"),
    );
    let attempt = Arc::new(BlockingManagedAttempt {
        dispatch: dispatch.clone(),
        executions: AtomicUsize::new(0),
        entered: tokio::sync::Notify::new(),
        live_cancel_observed: tokio::sync::Notify::new(),
        intent_preceded_signal: AtomicBool::new(false),
        hold_after_signal,
        release: tokio::sync::Notify::new(),
    });
    let mut host = SharedHost::new(Arc::new(OkModel), "stub")
        .with_dispatch_store(dispatch.clone())
        .with_remote_attempt_executor(RemoteAttemptInstallation {
            executor: attempt.clone(),
            credential_realization: Default::default(),
        });
    // Managed Session Events use the dispatch authority independently of the
    // ordinary foreground-ingress choice. Mirror the scenario-host topology:
    // direct foreground plus a co-located process pool.
    host.deployment.durable = false;
    let host = Arc::new(host);
    install_recording_session_application(&host);
    let managed = install_test_dispatch_runtime(&host);
    host.ensure_dispatch_pool();

    let mut activation =
        crate::host::worker_resolver::test_support::test_activation(thread, &run_id.0);
    activation.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::try_remote(
            awaken_runtime_contract::resolved::ModelBinding::new(
                "remote",
                "",
                "a2a:http://blocking.invalid",
            ),
            awaken_tenancy::ScopeId::from(host.local_workspace()),
            None,
            "sha256:blocking-remote",
        )
        .expect("coherent blocking remote candidate");
    activation
        .snapshot
        .recompute_fingerprint()
        .expect("coherent blocking remote snapshot");
    crate::host::worker_resolver::test_support::install_complete_projection_for_snapshot(
        &managed,
        thread,
        host.local_workspace(),
        crate::host::worker_resolver::test_support::deferred_environment(),
        &activation.snapshot,
    )
    .await;
    dispatch
        .enqueue(
            RunDispatch::new(activation)
                .for_session(ThreadId(thread.into()))
                .with_session_activity_epoch(1),
        )
        .await
        .expect("Managed Run admission");
    let pool = host.dispatch_pool_or_err().expect("pool").clone();
    pool.notify().await;

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        attempt.entered.notified(),
    )
    .await
    .expect("C1+C2 exact attempt entered");
    let ctx = host.ctx_for(thread, None).await.expect("resident Session");
    assert!(
        !ctx.delivery.is_durable()
            && ctx.active_run.lock().unwrap().is_none()
            && ctx.cancel.lock().unwrap().is_none(),
        "C3 Managed Event execution has no foreground control hint"
    );
    let interrupted = InterruptedManagedRun {
        host: host.clone(),
        dispatch,
        attempt,
        thread,
        run_id,
    };
    let active = interrupted.row().await.expect("C1 one exact dispatch");
    assert_eq!(
        active.state,
        awaken_dispatch::DispatchState::Leased,
        "C1 cancellation must land after the attempt is executing"
    );
    assert!(
        !active.cancellation_requested && active.physical_attempt_active,
        "C1 precondition"
    );

    // C5 synchronization point: the replacement claim is admitted only when the
    // caller resumes the pool, after the physical-slot fact it wants to test.
    pool.suspend_admission().await;
    tokio::time::timeout(
        std::time::Duration::from_millis(250),
        host.interrupt(thread),
    )
    .await
    .expect("interrupt admission is non-blocking")
    .expect("interrupt accepted");
    tokio::time::timeout(
        std::time::Duration::from_millis(250),
        interrupted.attempt.live_cancel_observed.notified(),
    )
    .await
    .expect("E2 registered attempt receives live cancellation");
    assert!(
        interrupted
            .attempt
            .intent_preceded_signal
            .load(Ordering::SeqCst),
        "E1 durable intent must precede the live signal"
    );
    interrupted
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_interrupt_signals_the_registered_attempt_only_after_durable_intent() {
    // Causes: C1 an exact opaque attempt is executing under the first claim;
    // C2 the Worker registered its cancellation token in the Session Runtime;
    // C3 the Managed Event path has direct foreground delivery and no
    // `active_run`/`cancel` hint, while its Run executes from the process
    // dispatch authority; C4 user.interrupt selects that row; C5 the durable
    // physical-attempt slot is acknowledged (row `physical_attempt_active`
    // false) or still occupied when the replacement cancellation claim is
    // admitted.
    // Effects: E1 the dispatch authority records the cancellation intent (and
    // revokes the first claim) before E2 the Runtime signals the registered
    // attempt; E3 the replacement claim commits exactly one terminal.
    // Constraint: the replacement runs on its own boundary Runtime, so only the
    // persisted slot orders it after the predecessor; a cancellation claim never
    // waits for a physical ACK (run-ingress R4, lease_semantics.rs), so C5 alone
    // decides E3. Admission is suspended across the interrupt so each row fixes
    // C5 deterministically instead of racing the predecessor's ACK.
    //
    // | Rule | C5 at replacement admission | E3 committed terminal        | row after E3 |
    // |------|-----------------------------|------------------------------|--------------|
    // | L1   | acknowledged                | Cancelled                    | settled      |
    // | L2   | occupied                    | Indeterminate, no re-entry   | retained     |
    let l1 = interrupt_with_admission_suspended("managed-live-cancel", false).await;
    l1.settle_row(
        |row| row.is_some_and(|row| row.cancellation_requested && !row.physical_attempt_active),
        "L1/C5 predecessor quiescence ACK",
    )
    .await;
    let pool = l1.host.dispatch_pool_or_err().expect("pool").clone();
    assert!(pool.resume_admission().await, "L1 admission resumes");
    l1.settle_row(|row| row.is_none(), "L1/E3 cancellation settlement")
        .await;
    assert_eq!(
        l1.committed_terminal("L1/E3").await,
        RunState::Ended(EndCause::Cancelled),
        "L1/E3"
    );

    let l2 = interrupt_with_admission_suspended("managed-live-cancel-held", true).await;
    assert!(
        l2.row()
            .await
            .is_some_and(|row| row.cancellation_requested && row.physical_attempt_active),
        "L2/C5 the held predecessor still occupies its slot"
    );
    let pool = l2.host.dispatch_pool_or_err().expect("pool").clone();
    assert!(pool.resume_admission().await, "L2 admission resumes");
    assert_eq!(
        l2.committed_terminal("L2/E3").await,
        RunState::Ended(EndCause::Indeterminate),
        "L2/E3 an unacknowledged predecessor cannot be reported Cancelled"
    );
    assert!(
        l2.row()
            .await
            .is_some_and(|row| row.physical_attempt_active),
        "L2 the replacement neither clears the slot nor consumes the dispatch"
    );
    assert_eq!(
        l2.attempt.executions.load(Ordering::SeqCst),
        1,
        "L2 no second executor entry"
    );
    l2.attempt.release.notify_one();
    l2.settle_row(
        |row| row.is_none_or(|row| !row.physical_attempt_active),
        "L2 predecessor ACK after release",
    )
    .await;
    assert_eq!(
        l2.committed_terminal("L2 terminal is final").await,
        RunState::Ended(EndCause::Indeterminate),
        "L2 a late ACK never rewrites committed truth"
    );
}
