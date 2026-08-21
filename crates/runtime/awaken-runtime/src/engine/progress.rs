//! Attempt progress, incremental commit watermarks, and state reconstruction.

use super::*;

/// What one attempt at driving the loop resolved to, ready to commit: the new
/// messages, the staged state, and where the run goes next.
pub(super) struct RunStepResult {
    pub(super) new_messages: Vec<Message>,
    pub(super) staged_state: Vec<StateCommand>,
    /// Permission-audit drafts produced this attempt, committed with the run's
    /// facts so an authorization decision is explainable (ADR-0030).
    pub(super) audit: Vec<EventDraft>,
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
            disposition: RunDisposition::ended(run_id, cause),
        }
    }
}

/// The attempt's durable-progress accumulator. It owns the step-commit watermark
/// invariant: everything below a watermark is already durable, so a step commit
/// advances the watermark atomically with the delta it made durable, and the
/// returned checkpoint is only the tail beyond it. The model-facing `transcript`
/// (seeded with committed history) and the durable `new_messages` (only this
/// attempt's messages) grow together but stay distinct.
pub(super) struct StepLedger {
    pub(super) transcript: Vec<Message>,
    pub(super) new_messages: Vec<Message>,
    pub(super) staged_state: Vec<StateCommand>,
    /// Permission-audit drafts accumulated across the attempt's gate decisions.
    pub(super) audit: Vec<EventDraft>,
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
            committed_messages: 0,
            committed_state: 0,
            committed_audit: 0,
            running_committed: false,
        }
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
        commit_step_delta(
            context,
            thread_id,
            run_id,
            self.new_messages[self.committed_messages..].to_vec(),
            self.staged_state[self.committed_state..].to_vec(),
            self.audit[self.committed_audit..].to_vec(),
            !self.running_committed,
        )
        .await?;
        self.committed_messages = self.new_messages.len();
        self.committed_state = self.staged_state.len();
        self.committed_audit = self.audit.len();
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
            disposition,
        }
    }
}

/// Commit one step's staged delta under a `Running` fact — the durable record
/// that the run is mid-flight with these steps completed. The first step
/// commit also records the state transition into `Running`; terminal and
/// awaiting outcomes never come through here (they ride `finish`, where the
/// state and any awaiting ticket commit atomically).
async fn commit_step_delta(
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    run_id: &RunId,
    messages: Vec<Message>,
    state: Vec<StateCommand>,
    audit: Vec<EventDraft>,
    first: bool,
) -> Result<()> {
    let Some(coordinator) = &context.commit else {
        return Ok(());
    };
    // `first` is the per-step boundary's state transition (nothing → Running); a
    // later increment stays Running and emits no RunStateChanged.
    coordinator
        .commit(ThreadCommit::assemble(
            thread_id.clone(),
            RunDisposition::running(run_id.clone()),
            first,
            messages,
            state,
            audit,
        ))
        .await
        .map_err(|err| Error::Commit(err.to_string()))?;
    Ok(())
}

/// Seed the run's read-only state from committed thread truth. Thread/Shared/
/// Profile-scoped commands re-hydrate for every Run; Run-scoped commands only
/// re-hydrate for their committed owner. Within a Run, later commands are folded
/// into this store so gates and hooks read its accumulated state (G1/G13).
pub(super) fn store_from_commands(commands: Vec<StateCommand>, run_id: &RunId) -> Store {
    let kept: Vec<StateCommand> = commands
        .into_iter()
        .filter(|command| {
            if command.scope != Scope::Run {
                return true;
            }
            if let Some(owner) = &command.run_id {
                return owner == run_id;
            }
            // Legacy commands predate commit-time Run binding. Only the old
            // ToolBatch cell has an embedded Run id that can be migrated safely;
            // every new Run-scoped key is explicitly owned above.
            if command.key.0 != ActiveToolBatch::KEY {
                return false;
            }
            match &command.action {
                StateAction::Set(value) => {
                    serde_json::from_value::<Option<ToolBatch>>(value.clone())
                        .ok()
                        .flatten()
                        .is_some_and(|batch| batch.run_id() == run_id)
                }
                StateAction::Remove => false,
            }
        })
        .collect();
    Store::rebuild(&kept)
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
