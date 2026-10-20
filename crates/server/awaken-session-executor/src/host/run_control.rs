// @formal surface obligation=run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal reason: Resident Run control records intent through the original Dispatch owner before
// @formal reason: nudging the existing pool and Runtime. Awaiting tickets remain committed Thread truth.
//! Run control of a resident Session: wake and pause its current attempt,
//! select the controls that interrupt it, and author the resume inputs that
//! durable delivery stages through the canonical dispatch control owner.
use super::{SessionCtx, SessionExecutionError};
use awaken_agent_contract::agent::{
    awaiting::{AwaitReason, ResumeTicket},
    run::Id as RunId,
    thread::Id as ThreadId,
};
use awaken_dispatch_execution::durable_control::DurableRunControl;
use awaken_dispatch_execution::{AdmissionError, LiveRunControlError};
use awaken_dispatch_contract::PendingInput;
use awaken_runtime::Runtime;
use awaken_runtime_contract::control::LiveCommand;
use awaken_runtime_contract::resume::ResumeResult;
use std::sync::Arc;

/// Cancels one durable Run of this process's dispatch owner and signals the
/// resident attempt serving it. Selected once from the topology: only a
/// process running the local claimer over a Coordinator dispatch owner can
/// cancel; any other process refuses with the same classification as before.
#[derive(Clone)]
pub(crate) struct DurableRunCancellation {
    installed_sessions: Arc<crate::InstalledSessions>,
    control: DurableCancellationControl,
}

#[derive(Clone)]
enum DurableCancellationControl {
    /// No local claimer runs here: durable dispatch is not enabled.
    NoLocalClaimer,
    /// A local claimer runs, but no Coordinator dispatch owner is local.
    NoDispatchOwner,
    Owner {
        owner: Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>,
        claimer: awaken_dispatch_execution::DispatchHint,
    },
}

impl DurableRunCancellation {
    pub(crate) fn new(
        installed_sessions: Arc<crate::InstalledSessions>,
        runs_local_claimer: bool,
        owner: Option<Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>>,
        claimer: awaken_dispatch_execution::DispatchHint,
    ) -> Self {
        let control = match (runs_local_claimer, owner) {
            (false, _) => DurableCancellationControl::NoLocalClaimer,
            (true, None) => DurableCancellationControl::NoDispatchOwner,
            (true, Some(owner)) => DurableCancellationControl::Owner { owner, claimer },
        };
        Self {
            installed_sessions,
            control,
        }
    }

    /// Persist cancellation of `run` and signal the resident attempt of `thread`.
    pub(crate) async fn cancel(
        &self,
        thread: &str,
        run: &str,
    ) -> Result<(), SessionExecutionError> {
        let resident = self
            .installed_sessions
            .read_run(thread, |run| run.context.clone())?
            .flatten();
        let (owner, claimer) = match &self.control {
            DurableCancellationControl::NoLocalClaimer => {
                return Err(SessionExecutionError::bad_request(
                    "durable dispatch not enabled (set typed durable ingress to run the pool)",
                ));
            }
            DurableCancellationControl::NoDispatchOwner => {
                return Err(SessionExecutionError::internal(
                    "operation requires a Coordinator dispatch owner",
                ));
            }
            DurableCancellationControl::Owner { owner, claimer } => (owner, claimer),
        };
        DurableRunControl::new(owner.clone(), Some(claimer.clone()))
            .cancel(
                &RunId(run.to_owned()),
                resident.as_ref().map(|ctx| ctx.live_control()),
            )
            .await
            .map_err(SessionExecutionError::from)
    }
}

/// Control only the current local attempt; no durable queue mutation authority is required.
pub(crate) struct LiveAttemptControl {
    runtime: Arc<Runtime>,
}

impl LiveAttemptControl {
    pub(crate) fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    /// Select the original controls of one observed foreground. `foreground`
    /// is a value the caller read under its own lock; this call takes no
    /// Session lock and performs no await, so a caller may run it inside that
    /// same critical section. The Runtime registration is captured now: a
    /// later registration for the same Run cannot be signalled through it.
    pub(crate) fn select_interruption(
        self,
        foreground: super::ForegroundRun,
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

    async fn deliver(
        &self,
        correlation_id: &str,
        command: LiveCommand,
    ) -> Result<(), LiveRunControlError> {
        LiveRunControlError::of_attempt_delivery(
            correlation_id,
            self.runtime.deliver_to_current_attempt(command).await,
        )
    }

    pub(crate) async fn wake(&self, correlation_id: &str) -> Result<(), LiveRunControlError> {
        let run_id = RunId(correlation_id.to_owned());
        self.deliver(
            correlation_id,
            LiveCommand::Wake {
                run_id,
                reason: "live-wake".to_owned(),
            },
        )
        .await
    }

    pub(crate) async fn pause(&self, correlation_id: &str) -> Result<(), LiveRunControlError> {
        let run_id = RunId(correlation_id.to_owned());
        self.deliver(correlation_id, LiveCommand::Pause { run_id })
            .await
    }
}

/// Immutable local controls selected for one original attempt. It carries no
/// Dispatch administration or durable cancellation authority, and cannot
/// reselect another attempt when its operation is delivered after an await.
pub(crate) struct SelectedAttemptInterruption {
    foreground: super::ForegroundRun,
    registered_cancellation: Option<awaken_runtime_contract::CancellationToken>,
}

impl SelectedAttemptInterruption {
    /// Original foreground identity; absent for a claim-only attempt.
    pub(crate) fn active_run(&self) -> Option<&RunId> {
        self.foreground.run.as_ref()
    }

    /// Signal only the captured original registration and direct/ACP tokens.
    /// A signal is not confirmation that a process or external effect stopped.
    pub(crate) fn interrupt(&self) {
        if let Some(token) = &self.registered_cancellation {
            token.cancel();
        }
        if let Some(token) = &self.foreground.cancellation {
            token.cancel();
        }
    }
}

/// The resume input that delivers an approval decision to the awaiting Run.
fn decision_input(
    thread: ThreadId,
    awaiting: Option<(RunId, ResumeTicket)>,
    allow: bool,
) -> Result<PendingInput, AdmissionError> {
    let (run_id, ticket) = awaiting
        .ok_or_else(|| AdmissionError::rejected("no awaiting run on this thread to deliver to"))?;
    Ok(PendingInput {
        message_id: awaken_runtime::fresh_process_id("xthread"),
        run_id,
        thread_id: thread,
        correlation_id: ticket.correlation_id,
        available_at_ms: None,
        context_messages: Vec::new(),
        result: if allow {
            ResumeResult::allow()
        } else {
            ResumeResult::deny(None)
        },
    })
}

/// The resume input that continues a manually paused Run with text.
fn manual_resume_input(
    thread: ThreadId,
    awaiting: Option<(RunId, ResumeTicket)>,
    text: String,
) -> Result<PendingInput, AdmissionError> {
    let (run_id, ticket) = awaiting
        .ok_or_else(|| AdmissionError::rejected("no awaiting run on this thread to resume"))?;
    if ticket.reason() != AwaitReason::ManualPause {
        return Err(AdmissionError::rejected(
            "durable text resume requires a manual-pause ticket",
        ));
    }
    Ok(PendingInput {
        message_id: awaken_runtime::fresh_process_id("manual-resume"),
        run_id,
        thread_id: thread,
        correlation_id: ticket.correlation_id,
        available_at_ms: None,
        context_messages: Vec::new(),
        result: ResumeResult::Input(text),
    })
}

impl crate::SessionRuns {
    /// Cancel every run currently backed by a process-local Session projection.
    ///
    /// Worker authority loss must stop in-flight tool processes before local
    /// realization revocation detaches their process bindings. Otherwise a native
    /// process can continue writing after this Worker has dropped authority.
    pub async fn interrupt_all_session_runs(&self) -> Result<usize, SessionExecutionError> {
        let session_ids = self.installed_sessions.session_ids()?;
        let mut interrupted = 0;
        for session_id in session_ids {
            match self.interrupt(&session_id).await {
                Ok(()) => interrupted += 1,
                Err(error) => tracing::warn!(session_id, %error,
                    "Worker could not request Session interruption before local revocation"),
            }
        }
        Ok(interrupted)
    }

    fn resident_context(
        &self,
        thread: &str,
    ) -> Result<Option<Arc<SessionCtx>>, SessionExecutionError> {
        Ok(self
            .installed_sessions
            .read_run(thread, |run| run.context.clone())?
            .flatten())
    }

    /// The canonical durable control owner over this process's dispatch queue and pool.
    pub(crate) fn durable_run_control(&self) -> Result<DurableRunControl, SessionExecutionError> {
        Ok(self
            .dispatch
            .durable_run_control(self.coordinator_dispatch()?))
    }

    pub async fn cancel_durable(
        &self,
        thread: &str,
        run: &str,
    ) -> Result<(), SessionExecutionError> {
        self.run_assembly
            .durable_run_cancellation
            .cancel(thread, run)
            .await
    }

    /// Wake the resident Run's current attempt. A cold Session has no
    /// subscriber; this never materializes one.
    pub async fn wake_durable(&self, thread: &str, run: &str) -> Result<(), SessionExecutionError> {
        self.require_durable_delivery()?;
        let runtime = self
            .resident_context(thread)?
            .filter(|ctx| ctx.delivery.is_durable())
            .map(|ctx| ctx.claimed_worker.runtime().clone())
            .ok_or_else(|| {
                AdmissionError::from(LiveRunControlError::NoSubscriber(run.to_owned()))
            })?;
        LiveAttemptControl::new(runtime)
            .wake(run)
            .await
            .map_err(|error| SessionExecutionError::from(AdmissionError::from(error)))
    }

    /// Pause the resident Run's current attempt (`run`, or the active one).
    pub async fn pause_durable(
        &self,
        thread: &str,
        run: Option<&str>,
    ) -> Result<String, SessionExecutionError> {
        self.require_durable_delivery()?;
        let resident = self.resident_context(thread)?;
        let no_active = || AdmissionError::rejected("thread has no locally owned active run");
        let runtime = resident
            .as_ref()
            .map(|ctx| ctx.runtime.clone())
            .ok_or_else(|| {
                run.map_or_else(no_active, |run| {
                    AdmissionError::from(LiveRunControlError::NoSubscriber(run.to_owned()))
                })
            })?;
        let run = match run {
            Some(run) => run.to_owned(),
            None => runtime
                .active_attempt_run_id(&ThreadId(thread.to_owned()))
                .await
                .map(|run| run.0)
                .ok_or_else(no_active)?,
        };
        let durable = resident
            .as_ref()
            .filter(|ctx| ctx.delivery.is_durable())
            .ok_or_else(|| AdmissionError::rejected("pause requires durable delivery"))?;
        LiveAttemptControl::new(durable.claimed_worker.runtime().clone())
            .pause(&run)
            .await
            .map_err(AdmissionError::from)?;
        Ok(run)
    }

    pub async fn stage_decision(
        &self,
        thread: &str,
        allow: bool,
    ) -> Result<String, SessionExecutionError> {
        let ctx = self.run_assembly.ctx_for(thread, None).await?;
        self.dispatch_pool_or_err()?;
        let thread = ThreadId(thread.to_owned());
        let awaiting = ctx
            .commit
            .open_wait_for_thread(&thread)
            .await
            .map_err(SessionExecutionError::internal)?;
        let input = decision_input(thread, awaiting, allow)?;
        self.durable_run_control()?
            .stage_input(input)
            .await
            .map_err(SessionExecutionError::from)
    }

    pub async fn stage_manual_resume(
        &self,
        thread: &str,
        text: String,
    ) -> Result<String, SessionExecutionError> {
        let ctx = self.run_assembly.ctx_for(thread, None).await?;
        self.dispatch_pool_or_err()?;
        let thread = ThreadId(thread.to_owned());
        let awaiting = ctx
            .commit
            .open_wait_for_thread(&thread)
            .await
            .map_err(SessionExecutionError::internal)?;
        let input = manual_resume_input(thread, awaiting, text)?;
        self.durable_run_control()?
            .stage_input(input)
            .await
            .map_err(SessionExecutionError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::runtime_context::RuntimeRunContext;

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
        use awaken_runtime_contract::CancellationToken;
        use awaken_runtime_contract::execution::LiveInput;

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
        let foreground = super::super::ForegroundRun {
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
}

#[cfg(test)]
mod durable_cancellation_tests {
    use super::*;
    use awaken_dispatch_contract::DispatchEnqueue as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct CountingWake(AtomicUsize);

    #[async_trait::async_trait]
    impl awaken_dispatch_contract::WakeSignal for CountingWake {
        async fn publish(&self) -> Result<(), awaken_dispatch_contract::DispatchError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn wait(&self) {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn durable_cancellation_follows_the_selected_topology() {
        // Decision table: causes = the topology selected at construction and
        // whether the Run is queued; effects = the cancellation outcome and
        // the hint published to the local claimer.
        // | Rule | local claimer | dispatch owner | Run queued | outcome                 | hints |
        // | D1   | no            | any            | any        | BadRequest (no claimer) | 0     |
        // | D2   | yes           | no             | any        | Internal (no owner)     | 0     |
        // | D3   | yes           | yes            | no         | BadRequest (not found)  | 0     |
        // | D4   | yes           | yes            | yes        | persisted               | 1     |
        let installed = Arc::new(crate::InstalledSessions::default());
        let owner: Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch> = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(awaken_time::system_clock())
                .await
                .unwrap(),
        );
        let cases: [(&str, bool, bool, bool); 5] = [
            ("D1", false, true, true),
            ("D1", false, false, false),
            ("D2", true, false, true),
            ("D3", true, true, false),
            ("D4", true, true, true),
        ];
        for (rule, claimer, has_owner, queued) in cases {
            let wake = Arc::new(CountingWake::default());
            let run = format!("cancel-{rule}-{claimer}-{has_owner}-{queued}");
            if queued && has_owner {
                owner
                    .enqueue(awaken_dispatch_contract::RunDispatch::new(
                        crate::host::test_activation("cancel-session", &run),
                    ))
                    .await
                    .unwrap();
            }
            let cancellation = DurableRunCancellation::new(
                installed.clone(),
                claimer,
                has_owner.then(|| owner.clone()),
                awaken_dispatch_execution::DispatchHint::new(wake.clone()),
            );
            let outcome = cancellation.cancel("cancel-session", &run).await;
            let hints = wake.0.load(Ordering::SeqCst);
            match rule {
                "D1" => {
                    let error = outcome.unwrap_err();
                    assert_eq!(
                        error.kind,
                        crate::SessionExecutionErrorKind::BadRequest,
                        "{rule}"
                    );
                    assert!(error.message.contains("not enabled"), "{rule}");
                    assert_eq!(hints, 0, "{rule}");
                }
                "D2" => {
                    let error = outcome.unwrap_err();
                    assert_eq!(
                        error.kind,
                        crate::SessionExecutionErrorKind::Internal,
                        "{rule}"
                    );
                    assert!(error.message.contains("dispatch owner"), "{rule}");
                    assert_eq!(hints, 0, "{rule}");
                }
                "D3" => {
                    let error = outcome.unwrap_err();
                    assert_eq!(
                        error.kind,
                        crate::SessionExecutionErrorKind::BadRequest,
                        "{rule}"
                    );
                    assert!(error.message.contains("not found"), "{rule}");
                    assert_eq!(hints, 0, "{rule}");
                }
                _ => {
                    outcome.unwrap();
                    assert_eq!(hints, 1, "{rule}");
                }
            }
        }
    }
}
