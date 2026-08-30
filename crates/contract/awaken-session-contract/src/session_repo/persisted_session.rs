use super::*;


impl PersistedSession {
    /// Construct a complete Session root after the creation compiler has
    /// consumed every authoring input. New creation persists this shape in one
    /// insert. Historical interrupted rows remain representable only through
    /// deserialization; no creation API can produce another one.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn frozen_with_budget(
        session_id: impl Into<String>,
        baseline: crate::SessionBaseline,
        resources: crate::SessionResourceState,
        mcp: crate::SessionMcpAttachmentSet,
        title: Option<String>,
        metadata: BTreeMap<String, String>,
        tools: crate::SessionToolConfiguration,
        budget: crate::SessionBudgetState,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            revision: SessionRevision::default(),
            baseline: crate::SessionBaselineState::Frozen(baseline),
            title,
            metadata,
            tools,
            event_batches: Vec::new(),
            activity_epoch: 0,
            active_activity_epochs: BTreeSet::new(),
            running_interval: None,
            closed_runtime_intervals: Vec::new(),
            runtime_active_millis: 0,
            usage_cursor: Default::default(),
            budget,
            environment: Default::default(),
            mcp,
            resources,
            realization: None,
            realization_progress: Default::default(),
            execution: SessionExecutionState::Preparing,
            disposition: Default::default(),
            terminal_cleanup: Default::default(),
        }
    }

    /// Install one complete create-time Event plan before the Session root is
    /// inserted. The root activity begins in this same value so a nonempty batch
    /// is observably Running without a follow-up mutation.
    pub fn install_initial_event_plan(
        &mut self,
        mut plan: crate::SessionInitialEventPlan,
    ) -> Result<(), crate::SessionEventBatchError> {
        if !self.event_batches.is_empty() {
            return Err(crate::SessionEventBatchError::ProgressMismatch);
        }
        let activity_epoch = self
            .begin_activity_epoch()
            .ok_or(crate::SessionEventBatchError::ProgressMismatch)?;
        plan.batch.admitted_revision = SessionRevision(
            self.revision
                .0
                .checked_add(1)
                .ok_or(crate::SessionEventBatchError::ProgressMismatch)?,
        );
        plan.batch.wake_activity_epoch = Some(activity_epoch);
        self.event_batches.push(plan.batch);
        Ok(())
    }

    /// Apply the sole durable Session execution transition function.
    ///
    /// Returns `false` for an idempotent replay and leaves the aggregate
    /// untouched when the transition is invalid.
    pub fn transition_execution(
        &mut self,
        next: SessionExecutionState,
    ) -> Result<bool, SessionExecutionTransitionError> {
        let from = self.execution;
        if !from.can_transition_to(next) {
            return Err(SessionExecutionTransitionError { from, to: next });
        }
        if from == next {
            return Ok(false);
        }
        if next.is_terminal() {
            self.active_activity_epochs.clear();
        }
        self.execution = next;
        Ok(true)
    }

    /// Advance the monotonic activity fence and record the newly admitted
    /// activity. An empty set on a historical Running row is deliberately not
    /// backfilled here: after crash recovery the predecessor has no durable
    /// completion owner, so the successor becomes the sole explicit activity.
    pub fn begin_activity_epoch(&mut self) -> Option<u64> {
        let next = self.activity_epoch.checked_add(1)?;
        self.activity_epoch = next;
        self.active_activity_epochs.insert(next);
        Some(next)
    }

    /// Open an activity at the exact root revision reserved by an idempotent
    /// application mutation. The revision is monotonic and therefore remains in
    /// the same fencing domain as ordinary activity epochs without an auxiliary
    /// operation-to-epoch registry.
    pub fn begin_activity_epoch_at(&mut self, epoch: u64) -> bool {
        if epoch == 0 || epoch <= self.activity_epoch {
            return false;
        }
        self.activity_epoch = epoch;
        self.active_activity_epochs.insert(epoch)
    }

    /// Settle one admitted activity epoch.
    ///
    /// `None` is an unknown, duplicate, or stale completion. `Some(false)`
    /// means another admitted activity remains; `Some(true)` means this was the
    /// last activity and the application may close the shared Running interval.
    /// A historical Running row with no explicit set treats its non-zero scalar
    /// epoch as a singleton for backward-compatible settlement.
    pub fn settle_activity_epoch(&mut self, expected_epoch: u64) -> Option<bool> {
        if self.active_activity_epochs.is_empty() {
            return (self.execution == SessionExecutionState::Running
                && expected_epoch != 0
                && expected_epoch == self.activity_epoch)
                .then_some(true);
        }
        self.active_activity_epochs
            .remove(&expected_epoch)
            .then_some(self.active_activity_epochs.is_empty())
    }

    /// Whether the current aggregate has explicit, unsettled activity truth.
    /// Legacy Running compatibility is intentionally handled only by
    /// [`Self::settle_activity_epoch`], where the caller supplies the epoch.
    #[must_use]
    pub fn has_active_activities(&self) -> bool {
        !self.active_activity_epochs.is_empty()
    }

    /// Whether the durable aggregate may be replaced by a compact tombstone.
    /// This is deliberately checked again by the store inside its transaction;
    /// callers cannot authorize physical deletion merely by constructing a
    /// [`SessionMutationPayload::Delete`].
    #[must_use]
    pub fn admits_tombstone(
        &self,
        asserted_session_id: &str,
        deleted_revision: SessionRevision,
    ) -> bool {
        session_tombstone_is_admitted(
            self.disposition.is_hidden(),
            self.execution.is_terminal(),
            self.has_verified_completed_cleanup(),
            !self.has_incomplete_event_batches(),
            self.session_id == asserted_session_id,
            self.revision
                .0
                .checked_add(1)
                .is_some_and(|next| deleted_revision == SessionRevision(next)),
        )
    }

    /// Archive one visible Session while terminating further execution.
    pub fn archive(
        &mut self,
        archived_at: impl Into<String>,
    ) -> Result<bool, SessionDispositionTransitionError> {
        match self.disposition {
            SessionDisposition::Deleting | SessionDisposition::Deleted => {
                return Err(SessionDispositionTransitionError::ArchiveAfterDelete);
            }
            SessionDisposition::Archived { .. } => return Ok(false),
            SessionDisposition::Active => {}
        }
        if !self.execution.is_terminal() {
            self.execution = SessionExecutionState::Terminated;
        }
        self.active_activity_epochs.clear();
        self.terminal_cleanup.request(&self.session_id);
        self.disposition = SessionDisposition::Archived {
            archived_at: archived_at.into(),
        };
        Ok(true)
    }

    /// Commit the hidden deletion phase before any external cleanup. Archived
    /// and activation-failed Sessions remain deletable because disposition is an
    /// orthogonal state axis.
    pub fn request_delete(&mut self) -> bool {
        let plan = session_delete_request_plan(
            SessionDeleteDispositionClass::from(&self.disposition),
            self.execution.is_terminal(),
        );
        if !plan.transition_to_deleting {
            return false;
        }
        if plan.terminalize_execution {
            self.execution = SessionExecutionState::Terminated;
        }
        self.active_activity_epochs.clear();
        if plan.request_cleanup {
            self.terminal_cleanup.request(&self.session_id);
        }
        self.disposition = SessionDisposition::Deleting;
        true
    }

    /// Install the durable admission fence required by terminal recovery.
    ///
    /// New Archive/Delete commands establish this fence in their root mutation.
    /// The explicit method exists for legacy terminal rows discovered by the
    /// reconciler, so no external cleanup effect needs to infer authority from a
    /// protocol projection.
    pub fn ensure_terminal_cleanup_fence(&mut self) -> bool {
        self.terminal_cleanup.request(&self.session_id)
    }

    /// Freeze the exact Runtime target set and begin release of the currently
    /// committed Resource generation in the same aggregate mutation.
    pub fn freeze_terminal_cleanup_targets(
        &mut self,
        thread_ids: impl IntoIterator<Item = String>,
        delegation_watermark: u64,
        runtime_commit_cursor: u64,
    ) -> Result<bool, crate::SessionCleanupError> {
        let mut changed = self.terminal_cleanup.freeze_targets(
            &self.session_id,
            thread_ids,
            delegation_watermark,
            runtime_commit_cursor,
        )?;
        if self.resources.pending.is_none() {
            let before = self.resources.clone();
            self.resources
                .begin_release()
                .expect("terminal release has no pending Resource generation");
            changed |= self.resources != before;
        }
        Ok(changed)
    }

    /// Accept the exact per-thread cleanup evidence and retire the Resource
    /// projection atomically with the cleanup completion fact.
    pub fn complete_terminal_cleanup(
        &mut self,
        receipts: &[crate::VerifiedSessionCleanupReceipt],
        release_reason: impl Into<String>,
    ) -> Result<bool, crate::SessionCleanupError> {
        let changed = self.terminal_cleanup.complete(&self.session_id, receipts)?;
        if changed {
            self.resources.complete_terminal_release(release_reason);
            self.environment = crate::SessionEnvironmentState::Unmaterialized;
        }
        Ok(changed)
    }

    /// Admit one exact remote Runtime receipt into the existing cleanup
    /// operation. This changes no target or phase and therefore cannot create a
    /// second cleanup queue beside [`crate::SessionCleanupOperation`].
    pub fn record_terminal_cleanup_completion(
        &mut self,
        completion: crate::SessionCleanupCompletion,
    ) -> Result<bool, crate::SessionCleanupError> {
        self.terminal_cleanup
            .record_completion(&self.session_id, completion)
    }

    /// Whether the root Session state forbids every new realization effect.
    /// Keep this classification on the aggregate so API rehydration, MCP recovery,
    /// and later reconcilers cannot grow different terminal-status lists.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.execution.is_terminal() || self.disposition.denies_activity()
    }

    #[must_use]
    pub const fn is_hidden(&self) -> bool {
        self.disposition.is_hidden()
    }

    /// Whether an ordinary protocol read may expose this aggregate.
    ///
    /// `ActivationFailed` is the durable terminal result of an accepted create
    /// intent. It remains exactly readable so an async caller can distinguish a
    /// transport timeout from background failure and inspect the aggregate-owned
    /// error; only the orthogonal hidden disposition removes public visibility.
    #[must_use]
    pub const fn is_publicly_readable(&self) -> bool {
        !self.is_hidden()
    }

    #[must_use]
    pub fn archived_at(&self) -> Option<&str> {
        self.disposition.archived_at()
    }

    /// Whether the Resource convergence driver owns work for this Session.
    ///
    /// A running or rescheduling Session with an active manifest is deliberately
    /// excluded: its resident Environment is live execution state, not terminal
    /// cleanup work. Keeping this predicate beside [`Self::is_terminal`] prevents
    /// reconcilers from recreating lifecycle status lists with string comparisons.
    #[must_use]
    pub fn needs_resource_reconciliation(&self) -> bool {
        matches!(
            self.disposition,
            SessionDisposition::Deleting | SessionDisposition::Deleted
        ) || self.resources.needs_reconciliation()
            || (self.is_terminal() && self.resources.has_active())
    }

    /// Whether this durable aggregate must be revisited by any Coordinator
    /// convergence driver. Keeping the union here prevents SQLite, Postgres,
    /// and future repositories from growing different recovery scans.
    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        self.needs_event_reconciliation()
            || self.needs_outcome_reconciliation()
            || self.needs_resource_reconciliation()
            || self.resources.has_references()
            || self.mcp.needs_reconciliation()
            || !matches!(
                self.environment,
                crate::SessionEnvironmentState::Unmaterialized
            )
            || self.verified_cleanup_needs_reconciliation()
            || self.needs_work_dispatch()
    }

    /// Whether the sole lifecycle supervisor must continue any retained
    /// root-owned Event batch. Completed batches remain provenance but do not
    /// produce repeated reconciliation work; an ordinary queued batch owns no
    /// aggregate activity while waiting behind an earlier Run.
    #[must_use]
    pub fn has_incomplete_event_batches(&self) -> bool {
        self.event_batches.iter().any(|batch| !batch.is_complete())
    }

    #[must_use]
    pub fn needs_event_reconciliation(&self) -> bool {
        self.has_incomplete_event_batches()
    }

    /// Whether retained root provenance can identify a Thread whose canonical
    /// Outcome aggregate may still need continuation. The root intentionally
    /// stores no active/terminal shadow flag: the lifecycle supervisor revisits
    /// this conservative candidate set and the Thread aggregate decides whether
    /// work exists. This trades a bounded read for one source of effect truth.
    #[must_use]
    pub fn needs_outcome_reconciliation(&self) -> bool {
        !self.is_terminal()
            && self.event_batches.iter().any(|batch| {
                batch.events.iter().any(|entry| {
                    matches!(
                        entry.event,
                        crate::SessionEventCommand::DefineOutcome { .. }
                    )
                })
            })
    }

    /// Whether the externally executed Session must have its one authoritative
    /// Environment WorkQueue projection.
    #[must_use]
    pub fn needs_work_dispatch(&self) -> bool {
        !self.is_terminal()
            && self
                .frozen_baseline()
                .is_some_and(|baseline| baseline.environment.self_hosted)
    }

    #[must_use]
    pub fn frozen_baseline(&self) -> Option<&crate::SessionBaseline> {
        match &self.baseline {
            crate::SessionBaselineState::Frozen(baseline) => Some(baseline),
            crate::SessionBaselineState::Preparing(_) => None,
        }
    }

    #[must_use]
    pub fn agent_id(&self) -> Option<&str> {
        self.frozen_baseline()
            .map(|baseline| baseline.agent_id.as_str())
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.frozen_baseline()
            .map(|baseline| baseline.model.as_str())
    }

    #[must_use]
    pub fn environment_id(&self) -> &str {
        match &self.baseline {
            crate::SessionBaselineState::Preparing(intent) => {
                &intent.control.environment.environment_id
            }
            crate::SessionBaselineState::Frozen(baseline) => &baseline.environment.environment_id,
        }
    }

    /// Managed wire projection derived from durably active generations only.
    #[must_use]
    pub fn visible_mcp_servers(&self) -> Vec<VisibleMcpServer> {
        self.mcp
            .visible()
            .into_iter()
            .map(|attachment| VisibleMcpServer {
                name: attachment.name.clone(),
                target: attachment.target.clone(),
                prompts_as_skills: attachment.prompts_as_skills,
            })
            .collect()
    }

    /// Secret-free MCP configuration accepted for this Session's current Agent
    /// snapshot. The Managed API must echo desired configuration even while its
    /// Runtime attachment is still Requested/Realizing or has failed closed;
    /// [`Self::visible_mcp_servers`] remains the separate execution-visibility
    /// projection and may legitimately be empty during those states.
    #[must_use]
    pub fn configured_mcp_servers(&self) -> Vec<VisibleMcpServer> {
        self.mcp
            .desired_attachments()
            .into_iter()
            .map(|attachment| VisibleMcpServer {
                name: attachment.name.clone(),
                target: attachment.target.clone(),
                prompts_as_skills: attachment.prompts_as_skills,
            })
            .collect()
    }
}
