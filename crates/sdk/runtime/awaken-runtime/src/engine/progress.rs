// @formal entrypoint background_task.never_replay_and_attempt_exhaustion_fail_closed
// @formal entrypoint checkpoint.durable_watermark_never_rolls_back
// @formal entrypoint tool_result.resume_receipt_is_atomic_with_ticket_consumption
//! Attempt progress, incremental commit watermarks, and state reconstruction.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

pub(super) use awaken_agent_contract::thread::commit::staged::ResumeCommit;

/// Native loop adapter over the shared ThreadCommit resume constructor. The
/// aggregate owns exact ticket replacement and any named ResumeApplied fact;
/// this adapter preserves Native's per-step state-change and error projection.
#[allow(clippy::too_many_arguments)]
pub(super) fn assemble_attempt_commit(
    thread_id: ThreadId,
    disposition: RunDisposition,
    state_changed: bool,
    messages: Vec<Message>,
    state: Vec<StateCommand>,
    audit: Vec<EventDraft>,
    resume: Option<&ResumeCommit>,
) -> Result<ThreadCommit> {
    let commit = ThreadCommit::assemble(
        thread_id,
        disposition,
        state_changed,
        messages,
        state,
        audit,
    );
    match resume {
        Some(resume) => commit
            .with_consumed_resume(resume)
            .map_err(|error| Error::Execution(error.to_string())),
        None => Ok(commit),
    }
}

/// What one attempt at driving the loop resolved to, ready to commit: the new
/// messages, the staged state, and where the run goes next.
pub(super) struct RunStepResult {
    pub(super) new_messages: Vec<Message>,
    pub(super) staged_state: Vec<StateCommand>,
    /// Permission-audit drafts produced this attempt, committed with the run's
    /// facts so an authorization decision is explainable (ADR-0030).
    pub(super) audit: Vec<EventDraft>,
    /// Uncommitted durable resume receipt, if this attempt was entered by a
    /// named delivery operation. The first successful commit consumes it.
    pub(super) resume: Option<ResumeCommit>,
    pub(super) disposition: RunDisposition,
}

impl RunStepResult {
    pub(super) fn cancelled(run_id: RunId) -> Self {
        Self::ended(run_id, EndCause::Cancelled)
    }

    pub(super) fn stopped(run_id: RunId, reason: String) -> Self {
        Self::ended(run_id, EndCause::Stopped(reason))
    }

    pub(super) fn capability_bound(run_id: RunId) -> Self {
        Self::ended(run_id, EndCause::Error(Failure::CapabilityBound))
    }

    pub(super) fn ended(run_id: RunId, cause: EndCause) -> Self {
        Self::ended_with_messages(run_id, cause, Vec::new())
    }

    pub(super) fn ended_with_messages(
        run_id: RunId,
        cause: EndCause,
        new_messages: Vec<Message>,
    ) -> Self {
        Self {
            new_messages,
            staged_state: Vec::new(),
            audit: Vec::new(),
            resume: None,
            disposition: RunDisposition::ended(run_id, cause),
        }
    }

    pub(super) fn with_resume(mut self, resume: Option<ResumeCommit>) -> Self {
        self.resume = resume;
        self
    }
}

/// The attempt's durable-progress accumulator. It owns the step-commit watermark
/// invariant: everything below a watermark is already durable, so a step commit
/// advances the watermark atomically with the delta it made durable, and the
/// returned checkpoint is only the tail beyond it. The model-facing `transcript`
/// (seeded with committed history) and the durable `new_messages` (only this
/// attempt's messages) grow together but stay distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WatermarkRegression;

/// Validate the only admissible successor of one durable prefix watermark.
/// The caller computes the candidate before slicing, then publishes it locally
/// only after the asynchronous commit succeeds.
pub(super) const fn advance_durable_watermark(
    current: usize,
    staged: usize,
) -> std::result::Result<usize, WatermarkRegression> {
    if staged < current {
        Err(WatermarkRegression)
    } else {
        Ok(staged)
    }
}

pub(super) struct StepLedger {
    pub(super) transcript: Vec<Message>,
    pub(super) new_messages: Vec<Message>,
    pub(super) staged_state: Vec<StateCommand>,
    /// Permission-audit drafts accumulated across the attempt's gate decisions.
    pub(super) audit: Vec<EventDraft>,
    resume: Option<ResumeCommit>,
    committed_messages: usize,
    committed_state: usize,
    committed_audit: usize,
    running_committed: bool,
}

impl StepLedger {
    pub(super) fn new(
        transcript: Vec<Message>,
        new_messages: Vec<Message>,
        seed_state: Vec<StateCommand>,
        seed_audit: Vec<EventDraft>,
    ) -> Self {
        Self {
            transcript,
            new_messages,
            staged_state: seed_state,
            audit: seed_audit,
            resume: None,
            committed_messages: 0,
            committed_state: 0,
            committed_audit: 0,
            running_committed: false,
        }
    }

    pub(super) fn attach_resume(&mut self, resume: Option<ResumeCommit>) {
        self.resume = resume;
    }

    /// Append one message to both the model-facing transcript and the durable
    /// new-message accumulation — the two always grow in lock-step.
    pub(super) fn push_message(&mut self, message: Message) {
        self.transcript.push(message.clone());
        self.new_messages.push(message);
    }

    /// Whether any completed step staged messages/state/audit not yet made durable.
    pub(super) fn has_uncommitted(&self) -> bool {
        self.new_messages.len() > self.committed_messages
            || self.staged_state.len() > self.committed_state
            || self.audit.len() > self.committed_audit
    }

    /// Drop staged state back to the committed watermark: a batch that fails
    /// cumulative validation must not ride the final commit, but steps committed
    /// while the batch was still valid stay committed.
    pub(super) fn rollback_state(&mut self) {
        self.staged_state.truncate(self.committed_state);
    }

    /// Commit the tail beyond every watermark under a `Running` fact, then advance
    /// the watermarks. `first` (the first delta) announces the state change.
    pub(super) async fn commit_delta(
        &mut self,
        context: &RuntimeRunContext,
        thread_id: &ThreadId,
        run_id: &RunId,
    ) -> Result<()> {
        // Validate every candidate before indexing a tail. A corrupted local
        // watermark must fail closed rather than panic or reinterpret an older
        // staged prefix as new durable work.
        let next_messages =
            advance_durable_watermark(self.committed_messages, self.new_messages.len())
                .map_err(|_| Error::Execution("message commit watermark regressed".to_string()))?;
        let next_state =
            advance_durable_watermark(self.committed_state, self.staged_state.len())
                .map_err(|_| Error::Execution("state commit watermark regressed".to_string()))?;
        let next_audit = advance_durable_watermark(self.committed_audit, self.audit.len())
            .map_err(|_| Error::Execution("audit commit watermark regressed".to_string()))?;
        commit_step_delta(
            context,
            thread_id,
            run_id,
            self.new_messages[self.committed_messages..].to_vec(),
            self.staged_state[self.committed_state..].to_vec(),
            self.audit[self.committed_audit..].to_vec(),
            !self.running_committed,
            self.resume.as_ref(),
        )
        .await?;
        self.committed_messages = next_messages;
        self.committed_state = next_state;
        self.committed_audit = next_audit;
        self.resume = None;
        self.running_committed = true;
        Ok(())
    }

    /// The tail beyond the watermarks, ready for the final commit: the step result
    /// must never re-append what step commits already made durable.
    pub(super) fn into_step_result(mut self, disposition: RunDisposition) -> RunStepResult {
        RunStepResult {
            new_messages: self.new_messages.split_off(self.committed_messages),
            staged_state: self.staged_state.split_off(self.committed_state),
            audit: self.audit.split_off(self.committed_audit),
            resume: self.resume,
            disposition,
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::{WatermarkRegression, advance_durable_watermark};

    /// @obligation checkpoint.durable_watermark_never_rolls_back
    #[kani::proof]
    fn durable_watermark_successor_is_exact_or_rejected_without_rollback() {
        let current = kani::any::<usize>();
        let staged = kani::any::<usize>();

        match advance_durable_watermark(current, staged) {
            Ok(next) => {
                assert!(staged >= current);
                assert_eq!(next, staged);
                assert!(next >= current);
            }
            Err(error) => {
                assert_eq!(error, WatermarkRegression);
                assert!(staged < current);
            }
        }
    }
}

/// Commit one step's staged delta under a `Running` fact — the durable record
/// that the run is mid-flight with these steps completed. The first step
/// commit also records the state transition into `Running`; terminal and
/// awaiting outcomes never come through here (they ride `finish`, where the
/// state and any awaiting ticket commit atomically).
#[allow(clippy::too_many_arguments)]
async fn commit_step_delta(
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    run_id: &RunId,
    messages: Vec<Message>,
    state: Vec<StateCommand>,
    audit: Vec<EventDraft>,
    first: bool,
    resume: Option<&ResumeCommit>,
) -> Result<()> {
    let Some(coordinator) = &context.commit else {
        return Ok(());
    };
    // `first` is the per-step boundary's state transition (nothing → Running); a
    // later increment stays Running and emits no RunStateChanged.
    coordinator
        .commit(assemble_attempt_commit(
            thread_id.clone(),
            RunDisposition::running(run_id.clone()),
            first,
            messages,
            state,
            audit,
            resume,
        )?)
        .await
        .map_err(Error::from)?;
    Ok(())
}

/// Fold a delegate's accumulated [`ThreadUsage`] into the parent thread's committed
/// tally under [`THREAD_USAGE_STATE_KEY`]. Load-merge-store on the live `store` plus
/// the staged batch, exactly like the per-step inference recording — so the parent
/// thread's usage (and thus a session's) counts sub-agent tokens. A no-op when the
/// delegate reported none (a remote delegate or a deterministic model).
/// Load-merge-store the thread-usage cell fail-closed (ADR-0055): read the
/// committed tally, apply `mutate`, write it back to the live `store` and stage
/// the command. On a shape drift it logs `drift` and leaves the tally untouched
/// (never resets an accumulated total). The one place the usage cell is folded —
/// both per-step recording and sub-agent rollup go through here.
pub(super) fn fold_thread_usage(
    store: &mut Store,
    staged_state: &mut Vec<StateCommand>,
    drift: &str,
    mutate: impl FnOnce(&mut ThreadUsage),
) {
    match ThreadUsageKey::load(store) {
        Ok(mut usage) => {
            mutate(&mut usage);
            let usage_cmd = ThreadUsageKey::write(&usage);
            store.apply(&usage_cmd);
            staged_state.push(usage_cmd);
        }
        Err(error) => tracing::error!(%error, "{drift}"),
    }
}

pub(super) fn merge_thread_usage(
    store: &mut Store,
    staged_state: &mut Vec<StateCommand>,
    delta: &ThreadUsage,
) {
    if delta.is_empty() {
        return;
    }
    fold_thread_usage(
        store,
        staged_state,
        "thread usage state drifted; skipping sub-agent rollup",
        |usage| usage.merge(delta),
    );
}

#[cfg(test)]
mod resume_commit_tests {
    use super::*;
    use awaken_agent_contract::agent::awaiting::{AwaitTarget, PauseReason};
    use awaken_agent_contract::audit::kind::Kind;

    fn ticket(correlation: &str) -> ResumeTicket {
        ResumeTicket::new(
            correlation,
            RunId("run".into()),
            ThreadId("thread".into()),
            "snapshot",
            "catalog",
            AwaitTarget::Pause(PauseReason::Manual),
        )
    }

    #[test]
    fn resume_commit_contains_exact_receipt_only_after_answered_ticket_replacement() {
        // Causes: C1 exact/foreign Thread; C2 exact/foreign Run; C3 the next
        // disposition removes, replaces, or retains the answered correlation.
        // Effects: E1 exact identity + removal/replacement constructs one commit
        // with the exact ResumeApplied fact; E2 foreign identity or retained
        // answered ticket rejects construction. Rules R1=C1+C2+!C3=>E1 and
        // R2=!C1|!C2|C3=>E2. The Kani proof covers the complete Boolean kernel;
        // this test cross-checks real ResumeTicket, RunDisposition and Draft.
        let answered = ticket("answered");
        let resume = ResumeCommit::new(Some("operation".into()), &answered);
        for disposition in [
            RunDisposition::running(RunId("run".into())),
            RunDisposition::awaiting(ticket("next")),
        ] {
            let commit = assemble_attempt_commit(
                ThreadId("thread".into()),
                disposition,
                true,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Some(&resume),
            )
            .expect("R1 exact replacement");
            let receipt = commit
                .events
                .iter()
                .find(|event| event.kind == Kind::ResumeApplied)
                .expect("R1 exact receipt");
            assert_eq!(receipt.payload["operation_id"], "operation");
            assert_eq!(receipt.payload["correlation_id"], "answered");
        }

        for (thread, disposition) in [
            (
                ThreadId("other".into()),
                RunDisposition::running(RunId("run".into())),
            ),
            (
                ThreadId("thread".into()),
                RunDisposition::running(RunId("other".into())),
            ),
            (
                ThreadId("thread".into()),
                RunDisposition::awaiting(answered),
            ),
        ] {
            assert!(
                assemble_attempt_commit(
                    thread,
                    disposition,
                    true,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Some(&resume),
                )
                .is_err(),
                "R2 incoherent commit is unconstructible"
            );
        }
    }
}
