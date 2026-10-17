// @formal surface obligation=run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal reason: Resident Run control records intent through the original Dispatch owner before
// @formal reason: nudging the existing pool and Runtime. Awaiting tickets remain committed Thread truth.
//! Run control of a resident Session: wake and pause its current attempt,
//! select the controls that interrupt it, and author the resume inputs that
//! durable delivery stages through the canonical dispatch control owner.
use super::{HostError, SessionCtx, SharedHost};
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

impl SharedHost {
    fn resident_context(&self, thread: &str) -> Option<Arc<SessionCtx>> {
        self.installed_sessions
            .read_run(thread, |run| run.context.clone())
            .flatten()
    }

    /// The canonical durable control owner over this process's dispatch queue and pool.
    pub(crate) fn durable_run_control(&self) -> Result<DurableRunControl, HostError> {
        Ok(DurableRunControl::new(
            self.coordinator_dispatch()?,
            self.dispatch.dispatch_pool.get().cloned(),
        ))
    }

    pub async fn cancel_durable(&self, thread: &str, run: &str) -> Result<(), HostError> {
        let resident = self.resident_context(thread);
        self.dispatch_pool_or_err()?;
        self.durable_run_control()?
            .cancel(
                &RunId(run.to_owned()),
                resident.as_ref().map(|ctx| ctx.live_control()),
            )
            .await
            .map_err(HostError::from)
    }

    /// Wake the resident Run's current attempt. A cold Session has no
    /// subscriber; this never materializes one.
    pub async fn wake_durable(&self, thread: &str, run: &str) -> Result<(), HostError> {
        self.require_durable_delivery()?;
        let runtime = self
            .resident_context(thread)
            .filter(|ctx| ctx.delivery.is_durable())
            .map(|ctx| ctx.claimed_worker.runtime().clone())
            .ok_or_else(|| {
                AdmissionError::from(LiveRunControlError::NoSubscriber(run.to_owned()))
            })?;
        LiveAttemptControl::new(runtime)
            .wake(run)
            .await
            .map_err(|error| HostError::from(AdmissionError::from(error)))
    }

    /// Pause the resident Run's current attempt (`run`, or the active one).
    pub async fn pause_durable(
        &self,
        thread: &str,
        run: Option<&str>,
    ) -> Result<String, HostError> {
        self.require_durable_delivery()?;
        let resident = self.resident_context(thread);
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

    pub async fn stage_decision(&self, thread: &str, allow: bool) -> Result<String, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        self.dispatch_pool_or_err()?;
        let thread = ThreadId(thread.to_owned());
        let awaiting = ctx
            .commit
            .open_wait_for_thread(&thread)
            .await
            .map_err(HostError::internal)?;
        let input = decision_input(thread, awaiting, allow)?;
        self.durable_run_control()?
            .stage_input(input)
            .await
            .map_err(HostError::from)
    }

    pub async fn stage_manual_resume(
        &self,
        thread: &str,
        text: String,
    ) -> Result<String, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        self.dispatch_pool_or_err()?;
        let thread = ThreadId(thread.to_owned());
        let awaiting = ctx
            .commit
            .open_wait_for_thread(&thread)
            .await
            .map_err(HostError::internal)?;
        let input = manual_resume_input(thread, awaiting, text)?;
        self.durable_run_control()?
            .stage_input(input)
            .await
            .map_err(HostError::from)
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
